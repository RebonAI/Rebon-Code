//! The owner's half of a subscribed ACP connection.
//!
//! Two things write to a subscribed connection: the request loop, answering
//! what the client asked, and the event forwarder, pushing what the session
//! did. **They must not both hold a write handle.** Two `write_all` calls
//! interleaving on one socket tear a frame in half, and a torn frame is not a
//! decode error the peer can recover from -- it is a stream that never
//! resynchronises.
//!
//! So the write half has exactly one owner from the moment a connection is
//! classified as ACP: a writer thread, fed by a channel. Both producers send
//! already-framed bytes to it. There is no mode where that is not true, which
//! is the point -- a connection that switched owners when it subscribed would
//! have a window where it had two.
//!
//! The other thing here is the outbound id space. When the owner asks the
//! client something (`session/request_permission`), it is the *requester*, and
//! its ids must not collide with the client's. They cannot: the client numbers
//! its own however it likes, and these are strings shaped
//! `perm-<session>-<query>` -- the shape `rebon serve` already uses, readable
//! in a log, and derived from the query so the same question twice is the same
//! id twice.

use std::collections::HashMap;
use std::io::Write;
use std::net::TcpStream;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};

use rebon_proto::framing::{content_length_header, FramingMode};
use rebon_proto::types::{JsonRpcResponse, RequestId};
use rebon_session_host::session_ext::{method, GapParams, RebonMeta, TurnParams};
use rebon_session_host::SessionEvent;

/// How many frames may be queued for the writer before a producer waits.
///
/// Small on purpose. The queue is not a buffer for a slow client -- the event
/// ring upstream already is one, and it drops a subscriber that falls a whole
/// backlog behind. This only smooths the gap between "a frame was produced"
/// and "the socket took it".
const OUTBOUND_QUEUE: usize = 64;

/// The single writer on one ACP connection.
///
/// Cloneable, and every clone sends to the same one thread. Dropping the last
/// one closes the channel, which is how the writer thread learns to stop.
#[derive(Clone)]
pub(crate) struct Wire {
    outbound: SyncSender<Vec<u8>>,
    framing: FramingMode,
    shutdown: Arc<TcpStream>,
    order: Arc<Mutex<DeliveryOrder>>,
    /// Outbound `session/request_permission` calls awaiting an answer, by the
    /// id they went out under, each with the turn its question belonged to.
    pending: Arc<Mutex<HashMap<String, AskedPermission>>>,
    pub(crate) subscription: Arc<Mutex<Option<u64>>>,
    pub(crate) extensions: Arc<std::sync::atomic::AtomicBool>,
}

/// Only the event forwarder advances this watermark, after queuing a frame.
/// Completion threads hold this lock only for nonblocking queue operations.
#[derive(Default)]
struct DeliveryOrder {
    stream: Option<(u64, u64)>,
    replies: Vec<(u64, serde_json::Value)>,
}

impl Wire {
    /// Take over the write half and start the one thread that owns it.
    pub(crate) fn spawn(stream: TcpStream, framing: FramingMode) -> std::io::Result<Self> {
        let (outbound, frames) = sync_channel::<Vec<u8>>(OUTBOUND_QUEUE);
        // Closing a stalled connection must always wake its reader and release
        // its subscriptions/reply sinks. Failure to clone is a setup failure.
        let shutdown = Arc::new(stream.try_clone()?);
        std::thread::Builder::new().spawn(move || write_frames(stream, frames))?;
        Ok(Self {
            outbound,
            framing,
            shutdown,
            order: Arc::new(Mutex::new(DeliveryOrder::default())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            subscription: Arc::new(Mutex::new(None)),
            extensions: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    pub(crate) fn forget_stream(&self) {
        let mut order = self.order.lock().expect("poisoned");
        order.stream = None;
        order.replies.clear();
    }

    pub(crate) fn close(&self) {
        self.forget_stream();
        let _ = self.shutdown.shutdown(std::net::Shutdown::Both);
    }

    pub(crate) fn begin_stream(&self, id: u64, cursor: u64) -> bool {
        let mut order = self.order.lock().expect("poisoned");
        if !order.replies.is_empty() {
            // Replacing a stream cannot claim its undelivered updates were
            // seen. Reconnect/replay rather than strand or misorder a reply.
            drop(order);
            self.close();
            return false;
        }
        order.stream = Some((id, cursor));
        true
    }

    pub(crate) fn forwarded(&self, id: u64, cursor: u64) -> bool {
        let mut order = self.order.lock().expect("poisoned");
        let Some((active, delivered)) = &mut order.stream else {
            return false;
        };
        if *active != id {
            return false;
        }
        *delivered = (*delivered).max(cursor);
        let delivered = *delivered;
        let mut sent = true;
        order.replies.retain(|(through, response)| {
            if *through > delivered {
                return true;
            }
            sent &= self.send(response, false);
            false
        });
        sent
    }

    /// Frame and queue one message. `false` once the connection is gone.
    fn send(&self, message: &serde_json::Value, wait: bool) -> bool {
        let Ok(body) = serde_json::to_vec(message) else {
            // A message that will not serialize cannot be reported as a
            // message either. Dropping it costs one frame; the peer's own
            // timeout handles what it was waiting for.
            return true;
        };
        let framed = match self.framing {
            FramingMode::ContentLength => {
                let mut framed = content_length_header(body.len()).into_bytes();
                framed.extend_from_slice(&body);
                framed
            }
            // `Auto` cannot reach here -- the probe resolves it before a
            // connection exists -- and NDJSON is what every rebon client on
            // this port speaks.
            FramingMode::Ndjson | FramingMode::Auto => {
                let mut framed = body;
                framed.push(b'\n');
                framed
            }
        };
        if wait {
            self.outbound.send(framed).is_ok()
        } else if self.outbound.try_send(framed).is_ok() {
            true
        } else {
            // A prompt completion runs on the worker, not the socket thread.
            // Never let one stalled peer hold up other turns or shutdown.
            let _ = self.shutdown.shutdown(std::net::Shutdown::Both);
            false
        }
    }

    pub(crate) fn send_response(&self, response: &JsonRpcResponse) -> bool {
        match serde_json::to_value(response) {
            Ok(value) => self.send(&value, true),
            Err(_) => true,
        }
    }

    /// Answer only after this subscription has queued every earlier update.
    /// The bounded waiting list never blocks an executor on a slow socket.
    pub(crate) fn send_prompt_response(
        &self,
        subscription: u64,
        through: u64,
        response: &JsonRpcResponse,
    ) -> bool {
        let response = serde_json::to_value(response).expect("prompt response serializes");
        let mut order = self.order.lock().expect("poisoned");
        if let Some((id, cursor)) = order.stream {
            if id == subscription {
                if cursor >= through {
                    return self.send(&response, false);
                }
                if order.replies.len() < OUTBOUND_QUEUE {
                    order.replies.push((through, response));
                    return true;
                }
            }
        }
        drop(order);
        self.close();
        false
    }

    pub(crate) fn send_notification(&self, method: &str, params: serde_json::Value) -> bool {
        self.send(
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }),
            true,
        )
    }

    /// Ask the client something, and remember what the answer will be about.
    ///
    /// The id is derived from the question rather than from a counter: the
    /// same pending permission re-announced on a reconnect goes out under the
    /// same id, so a client that answers the older copy is still answering
    /// this query rather than an unknown one.
    pub(crate) fn ask_permission(
        &self,
        session_id: &str,
        asked: AskedPermission,
        cursor: u64,
        mut params: serde_json::Value,
    ) -> bool {
        let id = permission_request_id(session_id, asked.query_id);
        self.pending
            .lock()
            .expect("poisoned")
            .insert(id.clone(), asked);
        // The standard params say what is being asked; these say which
        // question it is. A generic ACP client answers by id and needs none of
        // this, but a rebon client also follows the owner's event stream, and
        // has to match this question against the pending permission it is
        // already showing.
        if let (Some(object), Some(meta)) = (
            params.as_object_mut(),
            RebonMeta {
                query_id: Some(asked.query_id),
                turn_generation: Some(asked.turn_generation),
                cursor: Some(cursor),
                ..RebonMeta::default()
            }
            .to_meta(),
        ) {
            object.insert("_meta".to_string(), meta);
        }
        self.send(
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/request_permission",
                "params": params,
            }),
            true,
        )
    }

    /// Which question an inbound response is answering, if it is answering one
    /// of ours. Taken rather than read: an id is answered once.
    pub(crate) fn answered_query(&self, id: &RequestId) -> Option<AskedPermission> {
        let RequestId::String(id) = id else {
            // Every id this side issues is a string. A numeric one is the
            // client answering something it invented, which is not ours.
            return None;
        };
        self.pending.lock().expect("poisoned").remove(id)
    }
}

/// A question this side asked, and the turn it belonged to.
///
/// The turn is remembered here rather than read back from the client's answer.
/// It is a fence -- it exists to stop an answer landing on a turn it was not
/// meant for -- and a fence a client supplies is a fence the client can get
/// wrong. This side knew the answer when it asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AskedPermission {
    pub(crate) query_id: u64,
    pub(crate) turn_generation: u64,
}

/// The id one permission question goes out under.
///
/// `rebon serve` already spells it this way. Readable in a log, and a function
/// of the question, so it is stable across a re-announcement.
pub(crate) fn permission_request_id(session_id: &str, query_id: u64) -> String {
    format!("perm-{session_id}-{query_id}")
}

/// The one place bytes reach the socket.
fn write_frames(mut stream: TcpStream, frames: Receiver<Vec<u8>>) {
    // A subscriber writes for as long as the session runs, so the socket must
    // not carry the short timeout a request/response exchange uses.
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(30)));
    while let Ok(frame) = frames.recv() {
        if stream.write_all(&frame).is_err() || stream.flush().is_err() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        }
    }
}

/// What one session event becomes on an ACP connection.
///
/// A pure function of the event, so the mapping is testable without a socket:
/// the shapes are the contract the client reads, and every one of them
/// is easier to get wrong than to notice.
#[derive(Debug, PartialEq)]
pub(crate) enum Outbound {
    /// A notification: nothing comes back.
    Notify {
        method: String,
        params: serde_json::Value,
    },
    /// A question for the client, answered by a response carrying its id.
    AskPermission {
        asked: AskedPermission,
        cursor: u64,
        query: Box<rebon_session_host::BackgroundPermissionQuerySnapshot>,
    },
}

/// One message's params, with rebon's cursor hung in `_meta`.
///
/// The standard shapes have nowhere for it, and the client's de-duplication
/// reads it. One helper rather than one copy per event kind, because an event
/// that lost its cursor would be applied twice after a reconnect and nothing
/// else would notice.
fn with_cursor(mut params: serde_json::Value, cursor: u64) -> serde_json::Value {
    if let Some(object) = params.as_object_mut() {
        if let Some(meta) = (RebonMeta {
            cursor: Some(cursor),
            ..RebonMeta::default()
        })
        .to_meta()
        {
            object.insert("_meta".to_string(), meta);
        }
    }
    params
}

/// Translate one event. `None` for an event with nothing to say on this wire.
pub(crate) fn outbound_for(event: SessionEvent) -> Option<Outbound> {
    Some(match event {
        SessionEvent::Hello {
            epoch,
            cursor,
            status,
            ..
        } => Outbound::Notify {
            method: method::HELLO.to_string(),
            params: serde_json::json!({
                "epoch": epoch,
                "cursor": cursor,
                "status": status,
            }),
        },
        // The one standard shape in the set, so it keeps the standard's params
        // exactly and puts rebon's cursor in `_meta` -- which is what the
        // client's watermark reads to de-duplicate.
        SessionEvent::SessionUpdate { cursor, update } => Outbound::Notify {
            method: "session/update".to_string(),
            params: with_cursor(update, cursor),
        },
        // The cursor rides on these too, not only on `session/update`. A
        // client rebuilding the event it came from needs the number it was
        // published at, and dropping it here would make the two protocols
        // carry different amounts of the same event.
        SessionEvent::Turn {
            cursor,
            state,
            stop_reason,
            stop_refused,
        } => Outbound::Notify {
            method: method::TURN.to_string(),
            params: with_cursor(
                serde_json::to_value(TurnParams {
                    state,
                    stop_reason,
                    stop_refused,
                })
                .ok()?,
                cursor,
            ),
        },
        SessionEvent::Status { cursor, snapshot } => Outbound::Notify {
            method: method::STATUS_CHANGED.to_string(),
            params: with_cursor(serde_json::json!({ "snapshot": snapshot }), cursor),
        },
        SessionEvent::Permission { cursor, query } => Outbound::AskPermission {
            asked: AskedPermission {
                query_id: query.query_id,
                turn_generation: query.turn_generation,
            },
            cursor,
            query,
        },
        SessionEvent::Gap { from, to } => Outbound::Notify {
            method: method::GAP.to_string(),
            params: serde_json::to_value(GapParams { from, to }).ok()?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_session_host::{SessionStatusSnapshot, TurnStreamState};

    fn queued_wire() -> (Wire, Receiver<Vec<u8>>, TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let (stream, _) = listener.accept().unwrap();
        let (outbound, frames) = sync_channel(OUTBOUND_QUEUE);
        // Hold the real writer queue without a consumer: backpressure is
        // deterministic, not dependent on OS send-buffer sizes or sleeps.
        let wire = Wire {
            outbound,
            framing: FramingMode::Ndjson,
            shutdown: Arc::new(stream),
            order: Arc::new(Mutex::new(DeliveryOrder::default())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            subscription: Arc::new(Mutex::new(None)),
            extensions: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        (wire, frames, client)
    }

    fn prompt_response() -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: rebon_proto::types::JsonRpcVersion,
            id: Some(RequestId::Number(7)),
            result: Some(serde_json::json!({"stopReason": "end_turn"})),
            error: None,
        }
    }

    #[test]
    fn prompt_completion_never_blocks_on_either_bounded_queue() {
        for waiting_for_updates in [false, true] {
            let (wire, frames, mut client) = queued_wire();
            assert!(wire.begin_stream(1, 0));
            let worker = wire.clone();
            let (finished, done) = std::sync::mpsc::channel();
            let task = std::thread::spawn(move || {
                let through = u64::from(waiting_for_updates);
                for _ in 0..OUTBOUND_QUEUE {
                    assert!(worker.send_prompt_response(1, through, &prompt_response()));
                }
                assert!(!worker.send_prompt_response(1, through, &prompt_response()));
                finished.send(()).unwrap();
            });
            done.recv_timeout(std::time::Duration::from_secs(5))
                .expect("worker never waits on a slow peer");
            task.join().unwrap();
            use std::io::Read;
            match client.read(&mut [0]) {
                Ok(0) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ) => {}
                result => panic!("stalled socket must close, got {result:?}"),
            }
            wire.forget_stream();
            assert!(wire.order.lock().unwrap().replies.is_empty());
            drop(frames);
        }
    }

    #[test]
    fn fresh_cursor_and_replay_progress_release_only_their_subscription() {
        let (wire, frames, _client) = queued_wire();
        assert!(wire.begin_stream(1, 41));
        assert!(wire.send_prompt_response(1, 41, &prompt_response()));
        assert!(
            frames.try_recv().is_ok(),
            "fresh streams need no historical events"
        );
        assert!(wire.send_prompt_response(1, 43, &prompt_response()));
        assert!(frames.try_recv().is_err());
        assert!(!wire.forwarded(2, 43));
        assert!(wire.forwarded(1, 42));
        assert!(frames.try_recv().is_err());
        assert!(wire.forwarded(1, 43));
        assert!(frames.try_recv().is_ok());
        assert!(wire.send_prompt_response(1, 44, &prompt_response()));
        assert!(
            !wire.begin_stream(2, 44),
            "replacement cannot skip pending updates"
        );
        assert!(wire.order.lock().unwrap().replies.is_empty());
        assert!(!wire.forwarded(1, 44));
    }

    fn snapshot() -> SessionStatusSnapshot {
        SessionStatusSnapshot {
            job_id: "bg-1".into(),
            session_id: Some("s-1".into()),
            cwd: ".".into(),
            status: rebon_session_host::BackgroundJobStatus::Running,
            busy: true,
            turn_generation: 3,
            permission_mode: None,
            plan_mode: false,
            model: None,
            effort: None,
            agent: None,
            pending_permission: None,
            ask_user_questions: None,
            usage: None,
            mcp: None,
            client_leases: Vec::new(),
            last_command_id: None,
            last_command_at_ms: 0,
            last_command_error: None,
            updated_at_ms: 7,
        }
    }

    /// The cursor rides in `_meta.rebon.cursor` and the standard params are
    /// otherwise untouched. The client de-duplicates on that cursor, so
    /// a delta that lost it would be applied twice on a reconnect.
    #[test]
    fn a_session_update_carries_its_cursor_in_meta() {
        let update = serde_json::json!({
            "sessionId": "s-1",
            "update": {"sessionUpdate": "agent_message_chunk"},
        });
        let outbound = outbound_for(SessionEvent::SessionUpdate {
            cursor: 12,
            update: update.clone(),
        })
        .expect("an update translates");
        match outbound {
            Outbound::Notify { method, params } => {
                assert_eq!(method, "session/update");
                assert_eq!(params["_meta"]["rebon"]["cursor"], serde_json::json!(12));
                assert_eq!(params["sessionId"], update["sessionId"]);
                assert_eq!(params["update"], update["update"]);
            }
            other => panic!("expected a notification, got {other:?}"),
        }
    }

    /// A gap says both ends, because the client reads the events file for what
    /// is between them and needs to know where to stop.
    #[test]
    fn a_gap_becomes_the_extension_notification() {
        let outbound = outbound_for(SessionEvent::Gap { from: 4, to: 9 }).expect("a gap");
        assert_eq!(
            outbound,
            Outbound::Notify {
                method: "_session/gap".to_string(),
                params: serde_json::json!({"from": 4, "to": 9}),
            }
        );
    }

    /// Every numbered event carries its number, not only `session/update`.
    /// A client rebuilding the event needs the cursor it was published at, and
    /// an event that arrived without one would make the two protocols carry
    /// different amounts of the same thing.
    #[test]
    fn a_turn_becomes_the_extension_notification_with_its_cursor() {
        let outbound = outbound_for(SessionEvent::Turn {
            cursor: 1,
            state: TurnStreamState::Idle,
            stop_reason: Some("end_turn".into()),
            stop_refused: None,
        })
        .expect("a turn");
        assert_eq!(
            outbound,
            Outbound::Notify {
                method: "_session/turn".to_string(),
                params: serde_json::json!({
                    "state": "idle",
                    "stopReason": "end_turn",
                    "_meta": {"rebon": {"cursor": 1}},
                }),
            }
        );
    }

    #[test]
    fn a_status_change_carries_its_cursor_too() {
        let outbound = outbound_for(SessionEvent::Status {
            cursor: 8,
            snapshot: Box::new(snapshot()),
        })
        .expect("a status");
        match outbound {
            Outbound::Notify { method, params } => {
                assert_eq!(method, "_session/status_changed");
                assert_eq!(params["_meta"]["rebon"]["cursor"], serde_json::json!(8));
                assert_eq!(params["snapshot"]["jobId"], serde_json::json!("bg-1"));
            }
            other => panic!("expected a notification, got {other:?}"),
        }
    }

    #[test]
    fn hello_carries_the_epoch_the_cursor_belongs_to() {
        let outbound = outbound_for(SessionEvent::Hello {
            cursor: 5,
            epoch: 99,
            turn_generation: 3,
            status: Box::new(snapshot()),
        })
        .expect("hello");
        match outbound {
            Outbound::Notify { method, params } => {
                assert_eq!(method, "_session/hello");
                assert_eq!(params["epoch"], serde_json::json!(99));
                assert_eq!(params["cursor"], serde_json::json!(5));
                assert_eq!(params["status"]["jobId"], serde_json::json!("bg-1"));
            }
            other => panic!("expected a notification, got {other:?}"),
        }
    }

    /// A permission is the one event that is a question rather than an
    /// announcement, and the only one the client answers.
    #[test]
    fn a_permission_is_a_question_not_a_notification() {
        let query = rebon_session_host::BackgroundPermissionQuerySnapshot {
            query_id: 42,
            turn_generation: 1,
            endpoint: None,
            tool: Some("Bash".into()),
            tool_call_id: Some("call-1".into()),
            session_id: Some("s-1".into()),
            title: None,
            message: None,
            tool_input: None,
            metadata: None,
            options: Vec::new(),
        };
        match outbound_for(SessionEvent::Permission {
            cursor: 2,
            query: Box::new(query),
        })
        .expect("a permission")
        {
            Outbound::AskPermission { asked, .. } => {
                assert_eq!(asked.query_id, 42);
                assert_eq!(asked.turn_generation, 1);
            }
            other => panic!("a permission must be a request, got {other:?}"),
        }
    }

    /// The id is a function of the question, so the same query re-announced on
    /// a reconnect goes out under the same id.
    #[test]
    fn a_permission_id_is_derived_from_the_question() {
        assert_eq!(permission_request_id("s-1", 42), "perm-s-1-42");
        assert_eq!(
            permission_request_id("s-1", 42),
            permission_request_id("s-1", 42)
        );
        assert_ne!(
            permission_request_id("s-1", 42),
            permission_request_id("s-1", 43)
        );
    }
}
