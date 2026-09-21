//! `GET /v1/sessions/{session_id}/stream` — the session WebSocket.
//!
//! The durable side of a session was already here (the work queue, the
//! events table); this is the live side. A worker attaches with the
//! session token its work item was leased with, control surfaces attach
//! with a device access token, and frames move between them as JSON text
//! in the [`SessionFrame`] envelope `rebon-bridge` defines.
//!
//! ## Who may attach
//!
//! The credential is resolved before the session is looked up, so an
//! unknown session id never tells a stranger which ids exist — the same
//! rule the HTTP routes follow. Both classes go through the existing
//! [`auth`] helpers rather than a second copy of their logic:
//!
//! | Credential | Role | Failure |
//! |---|---|---|
//! | session token for this session, still leased | worker | — |
//! | session token for **another** session | — | 403 |
//! | session token whose item is no longer leased | — | **409** |
//! | device access token on the session's account | controller | — |
//! | device access token on another account | — | 404 |
//! | anything else, or nothing | — | 401 |
//!
//! The 409 is the important one: a worker that lost its lease must not
//! get a socket, because the session has already been handed to somebody
//! else. [`auth::session`] answers that today, so this route inherits it.
//!
//! ## Routing
//!
//! A worker's frame fans out to every controller. A controller's frame
//! goes to the worker, and is mirrored to the *other* controllers —
//! without the mirror, a second control surface would see a different
//! session live than it would see on replay, since the backlog contains
//! both directions.
//!
//! The asymmetry between the two directions is deliberate. A worker
//! streams whether or not anybody is listening, and every frame is
//! persisted regardless — that is the point of the plaintext design:
//! a phone that attaches an hour later finds the content
//! already on the server instead of having to wake the machine and pull
//! it live. There is no "nobody is listening, drop it" path.
//!
//! A controller frame with no worker attached is the opposite case:
//! nothing can act on it. It is **not** queued and **not** persisted; it
//! comes back as a `stream_error` frame with code `no_worker` and the
//! socket stays open. Silently holding a prompt for a worker that may
//! never return would make the controller's "sent" state a lie; closing
//! the socket would punish a controller that is only reading history.
//!
//! ## First come, first served
//!
//! Several controllers can write to one session at once.
//! Each controller frame takes the session's turn
//! ([`SessionHub::in_arrival_order`]) before anything else and keeps it
//! until it has been routed, so frames are stored and handed to the
//! worker in the order they arrived. A prompt that arrives while a turn is
//! running is routed like any other: the session host queues it behind
//! the running turn, in that same order.
//!
//! An answer to a prompt — a `permission_response` or a
//! `question_response` — is stored only if no other connection's answer
//! holds that `request_id` ([`crate::store::Store::record_answer`]).
//! A later answer is not stored and not routed; its sender gets a
//! `stream_error` with code `already_answered`, the `request_id`, and
//! `answered_by` (device id, device label, connection tag and the
//! winner's event id). The winner is mirrored to the other controllers
//! as usual, which is how their prompt closes. A worker's
//! `control_response` error for that `request_id` means the runner
//! refused the answer, and releases the prompt in the same transaction
//! that stores the refusal.
//!
//! ## Frame identity
//!
//! Every persisted frame is delivered with the `event_id` it was stored
//! under spliced in as a top-level key (`rebon_bridge::session_stream::
//! stamp_event_id`), live and on replay alike, so a controller that also
//! pages history can drop the overlap. The stored payload stays the frame
//! as it arrived. A peer may not send `event_id` itself: that is a
//! protocol error.
//!
//! A worker frame with an idempotency key — a `session_message` with a
//! `message_id`, a `permission_request` — is stored at most once per
//! session. A resend of one already stored is dropped without a word:
//! not stored, not delivered, socket left open. An empty id, or one over
//! `MAX_FRAME_ID_BYTES`, is a protocol error.
//!
//! A worker's `session_bound` frame is stored the same way (keyed on the
//! Rebon session id it names) and, in the same transaction, recorded on
//! the session row, so later work for the session resumes that local
//! session (`Store::record_session_binding`).
//!
//! ## Replay
//!
//! On connect a controller is sent the last `session_replay_events`
//! persisted frames, oldest first, before any live frame. The order that
//! makes this gap-free is: register the slot, *then* read the backlog.
//! Live frames therefore queue behind the replay instead of being
//! dropped, and the overlap the two create is removed by id — every
//! frame is persisted before it is fanned out, so a queued frame at or
//! below the replayed-through id is one the replay already delivered.

use std::{
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

use axum::{
    extract::{
        ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{HeaderMap, StatusCode},
    response::Response,
};
use rand::{rngs::OsRng, RngCore};
use rebon_bridge::session_stream::{
    close_code, error_code, stamp_event_id, AnsweredBy, DeliveredFrame, FrameOrigin, SessionFrame,
    MAX_FRAME_ID_BYTES,
};
use tracing::info;

use crate::{
    auth, db, error,
    hub::{Outbound, SessionHub, Slot, SlotTask},
    ids,
    store::{AnswerAttempt, AnswerClaim, AnswerHolder, Recorded, SessionEvent},
    RcState,
};

/// How often an idle socket is pinged.
const PING_INTERVAL: Duration = Duration::from_secs(25);
/// How long a ping may go unanswered before the socket is closed.
const PONG_DEADLINE: Duration = Duration::from_secs(10);
/// How long a socket may be silent altogether.
const IDLE_LIMIT: Duration = Duration::from_secs(75);
/// `code` of the `stream_error` a controller gets when nothing is there
/// to run its frame.
const NO_WORKER: &str = error_code::NO_WORKER;

/// Which side of the stream a connection authenticated as.
#[derive(Debug, Clone)]
enum StreamRole {
    /// The bridge worker holding this session's lease.
    Worker {
        /// Work item the session token belongs to.
        work_id: String,
    },
    /// A control surface on the session's account.
    Controller {
        /// Device the access token belongs to.
        device_id: String,
    },
}

impl StreamRole {
    fn origin(&self) -> FrameOrigin {
        match self {
            Self::Worker { .. } => FrameOrigin::Worker,
            Self::Controller { .. } => FrameOrigin::Controller,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Worker { .. } => "worker",
            Self::Controller { .. } => "controller",
        }
    }

    /// The id this connection authenticated as, for the log line. A work
    /// id for a worker, a device id for a controller — neither is a
    /// credential, and both are what an operator needs to match a socket
    /// to the thing that opened it.
    fn principal(&self) -> &str {
        match self {
            Self::Worker { work_id } => work_id,
            Self::Controller { device_id } => device_id,
        }
    }
}

/// Resolve the caller into a role, or into the status the HTTP routes
/// would have answered with.
async fn authorize(
    state: &Arc<RcState>,
    headers: &HeaderMap,
    session_id: &str,
) -> Result<StreamRole, Response> {
    // A session token is tried first. 401 from here means "this is not a
    // session token at all", which is exactly the case a controller's
    // device token falls into; 403 and 409 are verdicts about a genuine
    // session token and are final.
    match auth::session(state, headers, session_id).await {
        Ok(work) => {
            return Ok(StreamRole::Worker {
                work_id: work.work_id,
            })
        }
        Err(rejection) if rejection.status() != StatusCode::UNAUTHORIZED => return Err(rejection),
        Err(_) => {}
    }

    let device = auth::controller(state, headers).await?;
    let wanted = session_id.to_string();
    let found = db(state, move |state| state.store().session(&wanted)).await?;
    match found {
        Some((_, account_id, _)) if account_id == device.account_id => Ok(StreamRole::Controller {
            device_id: device.device_id,
        }),
        // A session on another account is not confirmed to exist.
        _ => Err(error(StatusCode::NOT_FOUND)),
    }
}

/// `GET /v1/sessions/{session_id}/stream`.
pub async fn stream(
    State(state): State<Arc<RcState>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let role = match authorize(&state, &headers, &session_id).await {
        Ok(role) => role,
        Err(rejection) => return rejection,
    };

    let hub = state.session_hub(&session_id);
    let slot_id = OsRng.next_u64();
    let work_id = match &role {
        StreamRole::Worker { work_id } => Some(work_id.clone()),
        StreamRole::Controller { .. } => None,
    };
    let (slot, task) = Slot::new(slot_id, work_id);

    let backlog_state = state.clone();
    let backlog_session = session_id.clone();
    let backlog_role = role.clone();
    let replay = match register_then_read_backlog(&hub, &role, slot, move || async move {
        // Only a controller is caught up. A worker is the thing that
        // *produced* the backlog, and replaying its own frames at it
        // would have it re-run its own history.
        if matches!(backlog_role, StreamRole::Worker { .. }) {
            return Ok(Vec::new());
        }
        let limit = backlog_state.config().session_replay_events;
        db(&backlog_state, move |state| {
            state.store().recent_session_events(&backlog_session, limit)
        })
        .await
    })
    .await
    {
        Ok(replay) => replay,
        Err(rejection) => return rejection,
    };
    let replayed_through = replay.last().map_or(0, |event| event.event_id);

    info!(
        session = %session_id,
        role = role.name(),
        principal = role.principal(),
        replay = replay.len(),
        "session stream attached"
    );
    let limit = state.config().max_body_bytes;
    upgrade
        // One byte past the application limit, so an oversized frame is
        // rejected with the protocol's own close code rather than
        // tungstenite's generic 1009.
        .max_message_size(limit + 1)
        .max_frame_size(limit + 1)
        .on_upgrade(move |socket| {
            let connection = Connection {
                state,
                hub,
                session_id,
                role,
                slot_id,
                max_frame_bytes: limit,
            };
            connection.run(
                socket,
                task,
                replay
                    .into_iter()
                    .map(|event| delivered_text(event.event_id, &event.payload_json))
                    .collect::<Vec<_>>(),
                replayed_through,
            )
        })
}

/// Register `slot` in the hub, **then** read the backlog.
///
/// The order is the whole point, which is why it is a function of its
/// own rather than two statements a later edit could swap. Registering
/// first means a frame published while the backlog is being read queues
/// for this socket instead of being missed; the overlap that creates is
/// removed by [`already_replayed`]. Reading first would leave a window in
/// which a frame is neither in the backlog nor in the queue.
///
/// On a failed read the slot is removed again, so a connection that is
/// about to be refused leaves nothing behind.
async fn register_then_read_backlog<Read, Pending>(
    hub: &SessionHub,
    role: &StreamRole,
    slot: Slot,
    read_backlog: Read,
) -> Result<Vec<SessionEvent>, Response>
where
    Read: FnOnce() -> Pending,
    Pending: std::future::Future<Output = Result<Vec<SessionEvent>, Response>>,
{
    let slot_id = slot.id();
    match role {
        StreamRole::Worker { .. } => hub.attach_worker(slot),
        StreamRole::Controller { .. } => hub.attach_controller(slot),
    }
    let backlog = read_backlog().await;
    if backlog.is_err() {
        detach(hub, role, slot_id);
    }
    backlog
}

fn detach(hub: &SessionHub, role: &StreamRole, slot_id: u64) {
    match role {
        StreamRole::Worker { .. } => hub.detach_worker(slot_id),
        StreamRole::Controller { .. } => hub.detach_controller(slot_id),
    }
}

/// Everything one attached socket needs. A struct rather than eight
/// positional arguments, all of which are fixed for the connection's
/// lifetime.
struct Connection {
    state: Arc<RcState>,
    hub: Arc<SessionHub>,
    session_id: String,
    role: StreamRole,
    slot_id: u64,
    max_frame_bytes: usize,
}

impl Connection {
    async fn run(
        self,
        mut socket: WebSocket,
        task: SlotTask,
        replay: Vec<String>,
        replayed_through: i64,
    ) {
        let SlotTask {
            mut receiver,
            queued_bytes,
            cancel,
            close_code,
        } = task;

        for payload in replay {
            if socket.send(Message::Text(payload.into())).await.is_err() {
                self.finish(socket, 1000).await;
                return;
            }
        }

        let mut liveness = tokio::time::interval(Duration::from_secs(1));
        liveness.tick().await;
        let mut next_ping = Instant::now() + PING_INTERVAL;
        let mut last_receive = Instant::now();
        let mut awaiting_pong: Option<Instant> = None;

        let close = loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break close_code.load(Ordering::Relaxed),
                _ = liveness.tick() => {
                    let now = Instant::now();
                    if now.duration_since(last_receive) >= IDLE_LIMIT
                        || awaiting_pong.is_some_and(|sent| now.duration_since(sent) >= PONG_DEADLINE)
                    {
                        break close_code::TIMEOUT;
                    }
                    if now >= next_ping {
                        if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                            break 1000;
                        }
                        awaiting_pong = Some(now);
                        next_ping = now + PING_INTERVAL;
                    }
                }
                outbound = receiver.recv() => {
                    let Some(outbound) = outbound else { break 1000 };
                    queued_bytes.fetch_sub(outbound.byte_len(), Ordering::AcqRel);
                    if already_replayed(&outbound, replayed_through) {
                        continue;
                    }
                    if socket.send(Message::Text(Utf8Bytes::from(outbound.text.as_ref()))).await.is_err() {
                        break 1000;
                    }
                }
                incoming = socket.recv() => {
                    let message = match incoming {
                        Some(Ok(message)) => message,
                        Some(Err(error)) => break read_error_code(&error),
                        None => break 1000,
                    };
                    last_receive = Instant::now();
                    match message {
                        Message::Pong(_) => awaiting_pong = None,
                        Message::Ping(payload) => {
                            if socket.send(Message::Pong(payload)).await.is_err() {
                                break 1000;
                            }
                        }
                        Message::Text(text) => {
                            if let Some(code) = self.accept(text.as_str()).await {
                                break code;
                            }
                        }
                        // The envelope is JSON text. A binary frame is
                        // not a newer protocol, it is the wrong one.
                        Message::Binary(_) => break close_code::PROTOCOL_ERROR,
                        Message::Close(_) => break 1000,
                    }
                }
            }
        };
        self.finish(socket, close).await;
    }

    /// Handle one inbound text frame. `Some(code)` closes the socket.
    async fn accept(&self, text: &str) -> Option<u16> {
        if text.len() > self.max_frame_bytes {
            return Some(close_code::RESOURCE_LIMIT);
        }
        let Ok(DeliveredFrame { event_id, frame }) = serde_json::from_str::<DeliveredFrame>(text)
        else {
            return Some(close_code::PROTOCOL_ERROR);
        };
        // `event_id` is the server's to assign; a peer that sends one is
        // either confused or trying to plant a fake id in the history.
        if event_id.is_some() {
            return Some(close_code::PROTOCOL_ERROR);
        }
        if frame
            .idempotency_id()
            .is_some_and(|id| id.is_empty() || id.len() > MAX_FRAME_ID_BYTES)
        {
            return Some(close_code::PROTOCOL_ERROR);
        }
        // A `type` this build does not know has no declared origin, so it
        // is routed by the sender's role. A known frame from the wrong
        // side is a bug: a worker that sends a prompt, or a controller
        // that sends a session message, is not a peer we can route for.
        match frame.origin() {
            Some(origin) if origin == self.role.origin() => {}
            None => {}
            Some(_) => return Some(close_code::PROTOCOL_ERROR),
        }

        // A controller frame waits for its turn, and keeps it until it is
        // routed; see the module docs.
        let _turn = match self.role {
            StreamRole::Controller { .. } => Some(self.hub.in_arrival_order().await),
            StreamRole::Worker { .. } => None,
        };
        // Refuse before persisting, so a prompt nobody can run never
        // enters the session's history.
        let worker_slot = match self.role {
            StreamRole::Controller { .. } => match self.hub.worker_slot_id() {
                Some(slot_id) => Some(slot_id),
                None => {
                    self.refuse(NO_WORKER, "no worker is attached to this session");
                    return None;
                }
            },
            StreamRole::Worker { .. } => None,
        };

        let write = Write::of(&frame, &self.role, self.slot_id, worker_slot);
        let session_id = self.session_id.clone();
        let payload = text.to_string();
        let now_unix = ids::now_unix();
        let recorded = db(&self.state, move |state| {
            write.run(state.store(), &session_id, &payload, now_unix)
        })
        .await;
        let event_id = match recorded {
            Ok(Stored::New(event_id)) => event_id,
            // Already stored and already delivered: a resend after a
            // reconnect. Nothing to do, and nothing to complain about.
            Ok(Stored::Duplicate) => return None,
            Ok(Stored::Taken { request_id, holder }) => {
                info!(
                    session = %self.session_id,
                    principal = self.role.principal(),
                    request_id = %request_id,
                    holder = %holder.device_id,
                    "a later answer was refused"
                );
                self.reply(SessionFrame::already_answered(
                    request_id,
                    answered_by(&holder),
                ));
                return None;
            }
            // The frame was not persisted, so it must not be delivered
            // either: a controller catching up later would never see it.
            Err(_) => return Some(1011),
        };

        let outbound = Outbound::persisted(event_id, delivered_text(event_id, text).into());
        match self.role {
            StreamRole::Worker { .. } => {
                self.hub.fan_out_to_controllers(outbound, None);
            }
            StreamRole::Controller { .. } => {
                if self.hub.send_to_worker(outbound.clone()) {
                    // Mirror to the other control surfaces so the live
                    // view and the replayed view agree.
                    self.hub
                        .fan_out_to_controllers(outbound, Some(self.slot_id));
                } else {
                    // The worker went away between the check above and
                    // here. The frame is already in the history, so a
                    // reattach will show it; the sender is told it did
                    // not run.
                    self.refuse(NO_WORKER, "the worker detached before the frame was routed");
                }
            }
        }
        None
    }

    /// Tell this socket its frame went nowhere, without closing it.
    ///
    /// Straight to this one socket: a rejection is about the sender's
    /// request, not about the session, so it is neither persisted nor
    /// shown to anybody else.
    fn refuse(&self, code: &str, message: &str) {
        self.reply(SessionFrame::stream_error(code, message));
    }

    /// Send `frame` to this one socket, unpersisted.
    fn reply(&self, frame: SessionFrame) {
        let text = serde_json::to_string(&frame).expect("stream error is serializable");
        self.hub
            .send_to_slot(self.slot_id, Outbound::transient(text));
    }

    async fn finish(self, mut socket: WebSocket, close: u16) {
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: close,
                reason: Utf8Bytes::from_static(close_reason(close)),
            })))
            .await;
        detach(&self.hub, &self.role, self.slot_id);
        info!(
            session = %self.session_id,
            role = self.role.name(),
            principal = self.role.principal(),
            close_code = close,
            "session stream detached"
        );
    }
}

/// How one accepted frame is stored. Decided on the socket task, run on
/// the blocking pool, so it owns everything it needs.
#[derive(Debug, PartialEq)]
enum Write {
    /// Stored as it is, at most once per key when it has one.
    Event {
        kind: String,
        dedupe_key: Option<String>,
    },
    /// A worker's binding, which also moves the session row: later work
    /// for this session resumes the local session it names.
    Binding {
        dedupe_key: String,
        rebon_session_id: String,
    },
    /// A controller's answer to a prompt, stored only if it wins.
    Answer {
        kind: String,
        request_id: String,
        device_id: String,
        connection_id: u64,
        worker_connection_id: u64,
    },
    /// A worker's refusal of an answer, which reopens the prompt.
    Refusal { request_id: String },
}

/// What storing a frame came to.
enum Stored {
    New(i64),
    Duplicate,
    Taken {
        request_id: String,
        holder: AnswerHolder,
    },
}

impl Write {
    /// `slot_id` is the sending socket; `worker_slot` is the worker a
    /// controller frame is about to be routed to, `None` for a worker's
    /// own frame.
    fn of(frame: &SessionFrame, role: &StreamRole, slot_id: u64, worker_slot: Option<u64>) -> Self {
        let kind = frame.frame_type().to_string();
        match (role, frame) {
            (StreamRole::Worker { .. }, SessionFrame::SessionBound { rebon_session_id }) => {
                Self::Binding {
                    dedupe_key: frame.idempotency_key().expect("a binding always has a key"),
                    rebon_session_id: rebon_session_id.clone(),
                }
            }
            (StreamRole::Worker { .. }, SessionFrame::ControlResponse { response })
                if response.subtype == "error" =>
            {
                Self::Refusal {
                    request_id: response.request_id.clone(),
                }
            }
            (StreamRole::Controller { device_id }, _) => {
                match (frame.answered_request_id(), worker_slot) {
                    (Some(request_id), Some(worker_connection_id)) => Self::Answer {
                        kind,
                        request_id: request_id.to_string(),
                        device_id: device_id.clone(),
                        connection_id: slot_id,
                        worker_connection_id,
                    },
                    _ => Self::Event {
                        kind,
                        dedupe_key: frame.idempotency_key(),
                    },
                }
            }
            _ => Self::Event {
                kind,
                dedupe_key: frame.idempotency_key(),
            },
        }
    }

    fn run(
        self,
        store: &crate::store::Store,
        session_id: &str,
        payload: &str,
        now_unix: i64,
    ) -> rusqlite::Result<Stored> {
        let recorded = match self {
            Self::Event { kind, dedupe_key } => store.record_keyed_session_event(
                session_id,
                &kind,
                payload,
                dedupe_key.as_deref(),
                now_unix,
            )?,
            Self::Binding {
                dedupe_key,
                rebon_session_id,
            } => store.record_session_binding(
                session_id,
                payload,
                &dedupe_key,
                &rebon_session_id,
                now_unix,
            )?,
            Self::Refusal { request_id } => Recorded::New(store.record_answer_refusal(
                session_id,
                payload,
                &request_id,
                now_unix,
            )?),
            Self::Answer {
                kind,
                request_id,
                device_id,
                connection_id,
                worker_connection_id,
            } => {
                let attempt = AnswerAttempt {
                    request_id: &request_id,
                    device_id: &device_id,
                    connection_id,
                    worker_connection_id,
                };
                return Ok(
                    match store.record_answer(session_id, &kind, payload, attempt, now_unix)? {
                        AnswerClaim::Recorded(event_id) => Stored::New(event_id),
                        AnswerClaim::Taken(holder) => Stored::Taken { request_id, holder },
                    },
                );
            }
        };
        Ok(match recorded {
            Recorded::New(event_id) => Stored::New(event_id),
            Recorded::Duplicate(_) => Stored::Duplicate,
        })
    }
}

/// The opaque tag a connection is named by in `answered_by`. The slot id
/// is a random number that authenticates nothing.
fn connection_tag(slot_id: u64) -> String {
    format!("c-{slot_id:016x}")
}

/// Who holds a prompt, as a refused controller is told.
fn answered_by(holder: &AnswerHolder) -> AnsweredBy {
    AnsweredBy {
        device_id: holder.device_id.clone(),
        label: holder.label.clone(),
        connection_id: Some(connection_tag(holder.connection_id)),
        event_id: u64::try_from(holder.event_id).ok(),
    }
}

/// A persisted frame's text as it goes out: the stored payload with its
/// `event_id` spliced in.
///
/// A stored payload is always an object — it was parsed as a frame
/// before it was written — so the splice cannot fail; if it somehow did,
/// the frame still goes out, just without its id.
pub(crate) fn delivered_text(event_id: i64, payload_json: &str) -> String {
    u64::try_from(event_id)
        .ok()
        .and_then(|event_id| stamp_event_id(payload_json, event_id))
        .unwrap_or_else(|| payload_json.to_string())
}

/// Whether a queued frame was already delivered by this socket's replay.
///
/// Every frame is persisted before it is fanned out, and the slot is
/// registered before the backlog is read, so a queued frame whose id is
/// at or below the last replayed id is one the backlog already carried.
/// An unpersisted frame (id `0`) is never part of a backlog.
fn already_replayed(outbound: &Outbound, replayed_through: i64) -> bool {
    outbound.event_id != 0 && outbound.event_id <= replayed_through
}

/// The close code for a socket whose read failed.
///
/// The WebSocket layer is configured one byte past the application's
/// frame cap, so a frame of exactly `cap + 1` bytes reaches
/// [`Connection::accept`] and is refused there — but anything larger is
/// refused by the WebSocket layer itself, which surfaces as a read error
/// rather than a frame. Without this, the one oversize frame the peer
/// most needs to be told about would close as an ordinary 1000.
///
/// An I/O error is the one read failure that is not about what the peer
/// sent: the connection is gone, and the close frame will not arrive
/// anyway. Everything else a conforming client can trigger is a size
/// ceiling, so it gets the size code.
fn read_error_code(error: &axum::Error) -> u16 {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = cause {
        if current.downcast_ref::<std::io::Error>().is_some() {
            return 1000;
        }
        cause = current.source();
    }
    close_code::RESOURCE_LIMIT
}

/// The reason string that goes out with a close code. Fixed text per
/// code, like every other RC error: it tells the peer what class of
/// failure it was without describing this session's state.
pub fn close_reason(code: u16) -> &'static str {
    match code {
        close_code::TIMEOUT => "timeout",
        close_code::LEASE_GONE => "lease_gone",
        close_code::RESOURCE_LIMIT => "resource_limit",
        close_code::PROTOCOL_ERROR => "protocol_error",
        close_code::SUPERSEDED => "superseded",
        1011 => "internal_error",
        _ => "normal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persisted(event_id: i64) -> Outbound {
        Outbound::persisted(event_id, format!("{{\"n\":{event_id}}}").into())
    }

    /// The interleaving that makes replay dangerous, driven step by step:
    /// a frame published *after* the slot is registered but *before* the
    /// backlog is read lands both in the backlog and in the queue.
    #[test]
    fn a_frame_in_both_the_backlog_and_the_queue_is_delivered_once() {
        let hub = SessionHub::default();
        let (slot, mut task) = Slot::new(1, None);

        // 1. Register first.
        hub.attach_controller(slot);
        // 2. A frame is persisted as event 7 and fanned out — it queues.
        hub.fan_out_to_controllers(persisted(7), None);
        // 3. The backlog read now sees events up to and including 7.
        let replayed_through = 7;
        // 4. A later frame, persisted after the read.
        hub.fan_out_to_controllers(persisted(8), None);
        // 5. An unpersisted rejection addressed at this socket.
        hub.send_to_slot(1, Outbound::transient("{}".into()));

        let mut delivered = Vec::new();
        while let Ok(outbound) = task.receiver.try_recv() {
            if !already_replayed(&outbound, replayed_through) {
                delivered.push(outbound.event_id);
            }
        }
        // 7 came in the backlog; 8 and the unpersisted frame go live.
        assert_eq!(delivered, vec![8, 0]);
    }

    fn controller() -> StreamRole {
        StreamRole::Controller {
            device_id: "dev_test".into(),
        }
    }

    fn event(event_id: i64) -> SessionEvent {
        SessionEvent {
            event_id,
            kind: "session_message".into(),
            payload_json: format!("{{\"n\":{event_id}}}"),
            created_at_unix: 0,
        }
    }

    /// The handler's real ordering, not a simulation of it: a frame
    /// published *while the backlog is being read* must already find the
    /// new slot registered. Swapping the two steps fails this every time.
    #[tokio::test]
    async fn a_frame_published_during_the_backlog_read_is_not_missed() {
        let hub = SessionHub::default();
        let (slot, mut task) = Slot::new(1, None);
        let publisher = &hub;
        let backlog = register_then_read_backlog(&hub, &controller(), slot, move || async move {
            // A worker's frame lands mid-read: persisted as event 9 and
            // fanned out, and — because the read is still in progress —
            // also visible in the backlog.
            let delivered = publisher.fan_out_to_controllers(persisted(9), None);
            assert_eq!(delivered, 1, "the slot was registered before the read");
            Ok(vec![event(8), event(9)])
        })
        .await
        .expect("backlog");

        let replayed_through = backlog.last().map_or(0, |event| event.event_id);
        let queued = task.receiver.try_recv().expect("the mid-read frame queued");
        assert_eq!(queued.event_id, 9);
        // And it is recognised as already delivered by the backlog.
        assert!(already_replayed(&queued, replayed_through));
    }

    #[tokio::test]
    async fn a_failed_backlog_read_leaves_nothing_attached() {
        let hub = SessionHub::default();
        let (slot, _task) = Slot::new(1, None);
        let failed = register_then_read_backlog(&hub, &controller(), slot, || async {
            Err(error(StatusCode::INTERNAL_SERVER_ERROR))
        })
        .await;
        assert_eq!(
            failed.expect_err("read failed").status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert!(hub.is_idle());

        let worker = StreamRole::Worker {
            work_id: "wrk_test".into(),
        };
        let (slot, _task) = Slot::new(2, Some("wrk_test".into()));
        let failed = register_then_read_backlog(&hub, &worker, slot, || async {
            Err(error(StatusCode::INTERNAL_SERVER_ERROR))
        })
        .await;
        assert!(failed.is_err());
        assert_eq!(hub.worker_slot_id(), None);
    }

    #[test]
    fn with_no_backlog_every_queued_frame_goes_live() {
        for event_id in [0, 1, 42] {
            assert!(!already_replayed(&persisted(event_id), 0));
        }
        assert!(already_replayed(&persisted(3), 3));
        assert!(already_replayed(&persisted(2), 3));
        assert!(!already_replayed(&persisted(4), 3));
    }

    #[test]
    fn a_delivered_frame_carries_its_event_id() {
        let text = delivered_text(12, r#"{"type":"cancel"}"#);
        assert_eq!(text, r#"{"event_id":12,"type":"cancel"}"#);
        // Not an object: delivered as it is rather than dropped.
        assert_eq!(delivered_text(12, "[]"), "[]");
        assert_eq!(
            delivered_text(-1, "{\"type\":\"cancel\"}"),
            "{\"type\":\"cancel\"}"
        );
    }

    #[test]
    fn every_close_code_has_its_own_reason() {
        let codes = [
            1000,
            1011,
            close_code::TIMEOUT,
            close_code::LEASE_GONE,
            close_code::RESOURCE_LIMIT,
            close_code::PROTOCOL_ERROR,
            close_code::SUPERSEDED,
        ];
        let mut reasons: Vec<&str> = codes.iter().map(|code| close_reason(*code)).collect();
        reasons.sort_unstable();
        reasons.dedup();
        assert_eq!(reasons.len(), codes.len());
    }

    #[test]
    fn an_io_failure_is_not_blamed_on_the_peer() {
        let io = axum::Error::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ));
        assert_eq!(read_error_code(&io), 1000);

        #[derive(Debug)]
        struct TooLong;
        impl std::fmt::Display for TooLong {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("message too long")
            }
        }
        impl std::error::Error for TooLong {}
        assert_eq!(
            read_error_code(&axum::Error::new(TooLong)),
            close_code::RESOURCE_LIMIT
        );
    }
}
