//! One client for talking to whoever owns a session.
//!
//! Before this there were three ways to reach a running session — a file
//! mailbox the desktop app wrote, this crate's worker IPC, and ACP over a
//! socket — and each grew its own command set. A command added to one of them
//! was missing from the other two until somebody noticed: the permission mode
//! never reached the worker, slash commands never reached the mirror, and the
//! effort selector only ever existed in the browser.
//!
//! So there is one protocol ([`BackgroundIpcRequest`]) and one way to find the
//! peer that speaks it. [`resolve_owner`] answers "who has this session, and
//! can I command them", and [`OwnerHandle`] is what a client holds afterwards.
//!
//! What this deliberately does **not** do is take a session over. A lock is
//! held by a live process or it is not held at all — the OS releases it when a
//! process dies — so "locked but not answering" can only mean a process that
//! is alive and busy or wedged. Claiming it would produce exactly the two
//! writers the lock exists to prevent.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rebon_session::{SessionOwnerDescriptor, SessionOwnerSurface};

use crate::session_host_client::HostCallError;
use crate::{
    BackgroundCommandResponse, BackgroundImageAttachment, BackgroundIpcCancelFence,
    BackgroundIpcEndpoint, BackgroundIpcEnvelope, BackgroundIpcRequest, BackgroundIpcResponse,
    BackgroundJobStatus, BackgroundStore, ClientLeaseKind, ForegroundQuestionAnswer, SessionEvent,
    SessionStatusSnapshot, BACKGROUND_IPC_PROTOCOL_VERSION, CLIENT_LEASE_RENEW_INTERVAL_MS,
};

/// How long a client waits to reach an endpoint before calling it unreachable.
///
/// Short on purpose: this runs on a path a user is waiting behind, the peer is
/// on loopback, and the answer to "no reply" is to fall back to read-only, not
/// to keep the interface stalled.
const OWNER_PING_TIMEOUT: Duration = Duration::from_millis(500);

/// What is known about the process holding a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerState {
    /// Nobody holds it. The caller may open it — as a client that spawns a
    /// worker, or in-process with `--local`.
    Free,
    /// Held, but by a process that published no reachable descriptor: a
    /// terminal hosting it in-process, or a build from before descriptors
    /// existed. The transcript can be read; the session cannot be commanded.
    OwnedOpaque {
        descriptor: Option<SessionOwnerDescriptor>,
    },
    /// Held by a process with an endpoint that did not answer. Alive (the lock
    /// says so) but busy, wedged, or not finished starting. Read-only, and
    /// never taken over.
    OwnedUnreachable { descriptor: SessionOwnerDescriptor },
    /// Held by a process that answered. Commands go to it.
    OwnedReachable { owner: OwnerHandle },
}

impl OwnerState {
    /// Whether commands can be sent.
    pub fn handle(&self) -> Option<&OwnerHandle> {
        match self {
            Self::OwnedReachable { owner } => Some(owner),
            _ => None,
        }
    }

    /// Whether some process holds the session, reachable or not.
    pub fn is_owned(&self) -> bool {
        !matches!(self, Self::Free)
    }

    /// A one-line explanation for a status bar. `None` when the session is
    /// free, which needs no explanation.
    pub fn describe(&self) -> Option<String> {
        match self {
            Self::Free => None,
            Self::OwnedOpaque { descriptor } => Some(match descriptor {
                Some(descriptor) if descriptor.surface == SessionOwnerSurface::Local => {
                    format!("open in a terminal (pid {})", descriptor.pid)
                }
                Some(descriptor) => format!("open in another process (pid {})", descriptor.pid),
                None => "open in another process".to_string(),
            }),
            Self::OwnedUnreachable { descriptor } => {
                Some(format!("host is not responding (pid {})", descriptor.pid))
            }
            Self::OwnedReachable { owner } => Some(format!("hosted (pid {})", owner.pid)),
        }
    }
}

/// A reachable owner, and everything a client may ask of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerHandle {
    pub session_id: String,
    pub job_id: Option<String>,
    pub pid: u32,
    pub port: u16,
    pub token: String,
    pub surface: SessionOwnerSurface,
}

/// When a session option a client asked for actually takes effect.
///
/// The owner is asked rather than told, and it answers honestly: some options
/// are a live switch on the running session, others are a field the next turn
/// or the next session build reads. A client that flattened all three into
/// "done" would be claiming a model change that has not happened yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOptionAppliesFrom {
    /// In force now.
    Immediately,
    /// Recorded; the session's next turn picks it up.
    NextTurn,
    /// Recorded; it lands when the session is next built.
    NextSession,
}

impl SessionOptionAppliesFrom {
    fn from_wire(value: Option<&str>) -> Self {
        match value {
            Some("nextTurn") => Self::NextTurn,
            Some("nextSession") => Self::NextSession,
            _ => Self::Immediately,
        }
    }
}

/// What became of an answer aimed at a prompt the owner was parked on.
///
/// The distinction matters to whoever is waiting: an answer that arrived after
/// the prompt was already resolved (from another client, or by the turn moving
/// on) is not a failure to retry — it is a race that somebody else won.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerOutcome {
    /// The owner took the answer.
    Applied,
    /// The owner is no longer parked on that prompt.
    AlreadyResolved,
}

impl OwnerHandle {
    /// The handle for a worker whose endpoint a job record already names.
    ///
    /// Job records carry the endpoint as three separate fields, and every
    /// client that wanted to talk to a worker used to re-assemble a handle from
    /// them by hand — five times over in the terminal alone, each spelling the
    /// surface out again. This is that assembly, once.
    pub fn for_worker(
        session_id: &str,
        job_id: Option<&str>,
        endpoint: &BackgroundIpcEndpoint,
    ) -> Self {
        Self {
            session_id: session_id.to_string(),
            job_id: job_id.map(str::to_string),
            pid: endpoint.pid,
            port: endpoint.port,
            token: endpoint.token.clone(),
            surface: SessionOwnerSurface::Worker,
        }
    }

    /// Where this owner listens, in the shape a job record spells it.
    pub fn endpoint(&self) -> BackgroundIpcEndpoint {
        BackgroundIpcEndpoint {
            pid: self.pid,
            port: self.port,
            token: self.token.clone(),
        }
    }

    /// Build a handle from a descriptor, if it names an endpoint.
    pub fn from_descriptor(session_id: &str, descriptor: &SessionOwnerDescriptor) -> Option<Self> {
        Some(Self {
            session_id: session_id.to_string(),
            job_id: descriptor.job_id.clone(),
            pid: descriptor.pid,
            port: descriptor.ipc_port?,
            token: descriptor.ipc_token.clone()?,
            surface: descriptor.surface,
        })
    }

    /// Send one request and hand back the whole response.
    ///
    /// `command_id` makes a retry safe: the owner remembers recent ids and
    /// answers a repeat from memory rather than performing the command twice.
    pub fn send(
        &self,
        request: BackgroundIpcRequest,
        command_id: Option<String>,
    ) -> anyhow::Result<BackgroundIpcResponse> {
        let timeout = crate::session_host_client::default_budget(&request);
        self.send_fallibly(request, command_id, timeout)
            .map_err(anyhow::Error::new)
    }

    /// [`Self::send`] with the deadline stated, and the failure as a value.
    ///
    /// The paired form: `send` takes the budget the request kind
    /// is documented to need, this one takes the caller's. Both report a
    /// deadline that passed as [`HostCallError::HostUnanswered`] rather than as
    /// an I/O error a caller would have to recognise by its message — that
    /// distinction is what lets [`crate::SessionHostConnection::call_with_timeout`]
    /// know when to send a `CancelCall`.
    pub fn send_fallibly(
        &self,
        request: BackgroundIpcRequest,
        command_id: Option<String>,
        timeout: Duration,
    ) -> Result<BackgroundIpcResponse, HostCallError> {
        if let Some(answer) = self.try_over_acp(&request, command_id.as_deref(), timeout) {
            return answer;
        }
        let envelope = BackgroundIpcEnvelope {
            protocol_version: BACKGROUND_IPC_PROTOCOL_VERSION,
            job_id: self.job_id.clone(),
            session_id: Some(self.session_id.clone()),
            command_id,
            token: self.token.clone(),
            request,
        };
        let response: BackgroundIpcResponse = send_envelope(self.port, &envelope, timeout)?;
        crate::session_host_client::reply_from_wire(response)
    }

    /// This request over the ACP link, when there is one and it carries it.
    ///
    /// `None` means "not this way" and the caller falls through to the legacy
    /// envelope. Three things produce that, and they are different:
    ///
    /// - **The owner predates ACP.** The probe says so, once per endpoint
    ///   generation, and every request goes the old way.
    /// - **A permission answer with no subscription to answer on.** The owner
    ///   is waiting for a response to a question it asked on a *subscribed*
    ///   connection; a client that is not subscribed has no such question and
    ///   nothing to respond to, so it answers the old way.
    /// - **The link could not be opened.** A worker that stopped between the
    ///   probe and now. Falling back gives the legacy path its own chance to
    ///   fail with the error its callers already understand.
    fn try_over_acp(
        &self,
        request: &BackgroundIpcRequest,
        command_id: Option<&str>,
        timeout: Duration,
    ) -> Option<Result<BackgroundIpcResponse, HostCallError>> {
        let endpoint = self.endpoint();
        if crate::protocol_probe::owner_protocol(&endpoint)
            != crate::protocol_probe::OwnerProtocol::Acp
        {
            return None;
        }
        // A permission answer is not a request at all: it is the response to
        // one the owner asked on the subscription, so it goes out there or not
        // over ACP at all.
        if let BackgroundIpcRequest::PermissionAnswer {
            query_id,
            option_id,
            extra_text,
            updated_input,
            ..
        } = request
        {
            let answerer = crate::acp_subscription::answerer_for(&endpoint)?;
            return answerer
                .answer(
                    *query_id,
                    option_id.clone(),
                    extra_text.clone(),
                    updated_input.clone(),
                )
                .then(|| Ok(BackgroundIpcResponse::ok()));
        }
        let link = self.acp_link(&endpoint)?;
        let (method, mut params) = match crate::session_ext::method_and_params(request) {
            Some(spelled) => spelled,
            // These need this session's content-block params, negotiated
            // delivery semantics, or a notification rather than a request.
            None => match request {
                BackgroundIpcRequest::Reply { message, images } => {
                    // Reply is delivery, not a standard ACP turn. Old workers
                    // advertised no enqueue extension and used prompt as an ack.
                    let method = if link.supports_method(crate::session_ext::method::ENQUEUE) {
                        crate::session_ext::method::ENQUEUE
                    } else {
                        "session/prompt"
                    };
                    (method, self.prompt_params(message, images))
                }
                BackgroundIpcRequest::Steer { message, images } => {
                    ("_session/steering", self.prompt_params(message, images))
                }
                BackgroundIpcRequest::Cancel { fence } => {
                    let mut params = serde_json::json!({ "sessionId": self.session_id });
                    if let (Some(object), Some(meta)) = (
                        params.as_object_mut(),
                        crate::session_ext::RebonMeta {
                            fence: Some(fence.clone()),
                            job_id: self.job_id.clone(),
                            session_id: Some(self.session_id.clone()),
                            ..crate::session_ext::RebonMeta::default()
                        }
                        .to_meta(),
                    ) {
                        object.insert("_meta".to_string(), meta);
                    }
                    return match link.notify("session/cancel", params) {
                        Ok(()) => Some(Ok(BackgroundIpcResponse::ok())),
                        // The same stale-link case as below: fall back rather
                        // than report a cancel as failed because the
                        // connection it would have gone on had already closed.
                        Err(_) => {
                            crate::acp_link::close_link(&endpoint);
                            None
                        }
                    };
                }
                _ => return None,
            },
        };
        // The fences travel per request, as they always have: the token said
        // who this client is, once, when the link was opened; these say which
        // worker it meant, which any client can get wrong after a replacement.
        let meta = crate::session_ext::RebonMeta {
            job_id: self.job_id.clone(),
            session_id: Some(self.session_id.clone()),
            command_id: command_id.map(str::to_string),
            ..crate::session_ext::RebonMeta::default()
        };
        if let (Some(object), Some(meta)) = (params.as_object_mut(), meta.to_meta()) {
            object.insert("_meta".to_string(), meta);
        }
        match link.call(method, params, timeout) {
            Ok(result) => Some(Ok(BackgroundIpcResponse {
                ok: true,
                error: None,
                data: unwrap_answer(request, result),
            })),
            // The connection broke under this call. That is not this request
            // failing -- a cached link outlives the socket it was opened on,
            // and an owner that restarted, was stopped, or reset the
            // connection leaves one behind that looks live until it is used.
            //
            // So the link is dropped and the call falls through to the legacy
            // envelope, which opens its own connection and will fail with the
            // error this caller already understands if the owner really is
            // gone. A request that had already run is protected by its command
            // id: the owner answers a repeat from its recent-results memory
            // rather than running it twice.
            Err(HostCallError::Transport(reason)) => {
                tracing::debug!(
                    %reason,
                    "rebon: the ACP link to this owner broke; falling back for this call"
                );
                crate::acp_link::close_link(&endpoint);
                None
            }
            Err(other) => Some(Err(other)),
        }
    }

    /// The link to this owner, or `None` after writing down that there is not
    /// one to be had.
    ///
    /// The probe said ACP and the connection would not hold. One call has
    /// already paid the handshake's whole timeout finding that out; writing
    /// the verdict down is what keeps the next one from paying it again.
    fn acp_link(
        &self,
        endpoint: &crate::state::BackgroundIpcEndpoint,
    ) -> Option<std::sync::Arc<crate::acp_link::AcpLink>> {
        match crate::acp_link::link_to(endpoint) {
            Ok(link) => Some(link),
            Err(_) => {
                crate::protocol_probe::remember(
                    endpoint,
                    crate::protocol_probe::OwnerProtocol::Legacy,
                );
                None
            }
        }
    }

    /// A message and its images as the content blocks a prompt carries.
    fn prompt_params(
        &self,
        message: &str,
        images: &[crate::state::BackgroundImageAttachment],
    ) -> serde_json::Value {
        let mut blocks = vec![serde_json::json!({ "type": "text", "text": message })];
        blocks.extend(images.iter().map(|image| {
            let mut block = serde_json::json!({
                "type": "image",
                "mimeType": image.media_type,
                "data": image.data,
            });
            if let (Some(object), Some(path)) = (block.as_object_mut(), image.source_path.as_ref())
            {
                object.insert("uri".to_string(), serde_json::json!(path));
            }
            block
        }));
        serde_json::json!({ "sessionId": self.session_id, "prompt": blocks })
    }

    /// Is the owner answering at all?
    pub fn ping(&self) -> bool {
        self.send(BackgroundIpcRequest::Ping, None).is_ok()
    }

    /// Everything this client must agree with the owner about (invariant I4).
    pub fn status(&self) -> anyhow::Result<SessionStatusSnapshot> {
        let response = self.send(BackgroundIpcRequest::Status, None)?;
        let data = response
            .data
            .ok_or_else(|| anyhow::anyhow!("the owner answered Status without a snapshot"))?;
        Ok(serde_json::from_value(data)?)
    }

    /// Take or renew this client's lease, keeping the owner alive.
    pub fn lease(&self, client_id: &str, kind: ClientLeaseKind) -> anyhow::Result<()> {
        self.send(
            BackgroundIpcRequest::Lease {
                client_id: client_id.to_string(),
                kind,
            },
            None,
        )
        .map(|_| ())
    }

    /// Hold a lease for as long as the returned guard lives.
    ///
    /// The renewal cadence is the owner's business, not each client's: a
    /// client that forgot to renew would watch its own host exit mid-session,
    /// and one that renewed too eagerly would write the job record hundreds of
    /// times a minute. Dropping the guard gives the lease up immediately
    /// rather than leaving the owner to wait out the TTL.
    pub fn hold_lease(&self, client_id: &str, kind: ClientLeaseKind) -> LeaseGuard {
        let stop = Arc::new(AtomicBool::new(false));
        // Starts false, so a client that is torn down without saying anything
        // — a panic, a dropped connection, a process killed — releases the way
        // it always did and the owner lingers. Only an exit that says out loud
        // it was meant gets the fast path.
        let deliberate = Arc::new(AtomicBool::new(false));
        let owner = self.clone();
        let id = client_id.to_string();
        let worker_stop = Arc::clone(&stop);
        let worker_deliberate = Arc::clone(&deliberate);
        let renewer = std::thread::Builder::new()
            .name("rebon-session-lease".to_string())
            .spawn(move || {
                while !worker_stop.load(Ordering::Relaxed) {
                    if let Err(err) = owner.lease(&id, kind) {
                        // A lease that cannot be renewed means the owner is
                        // gone or no longer ours. Say so once and stop; the
                        // client learns the rest from its own subscription.
                        tracing::debug!(error = %err, "rebon: session lease renewal stopped");
                        return;
                    }
                    // Short sleeps so dropping the guard is felt immediately
                    // rather than after a full renewal interval.
                    let mut waited = 0;
                    while waited < CLIENT_LEASE_RENEW_INTERVAL_MS
                        && !worker_stop.load(Ordering::Relaxed)
                    {
                        std::thread::sleep(Duration::from_millis(100));
                        waited += 100;
                    }
                }
                let deliberate = worker_deliberate.load(Ordering::Relaxed);
                match owner.release_lease(&id, deliberate) {
                    Ok(()) => {
                        tracing::info!(
                            client_id = %id,
                            deliberate,
                            "rebon: session lease released"
                        );
                    }
                    Err(err) => {
                        // A swallowed failure here is how a deliberate exit
                        // degrades into a full linger without a trace: the
                        // client is gone, the owner waits out the TTL, and
                        // nothing anywhere says why.
                        tracing::warn!(
                            client_id = %id,
                            deliberate,
                            error = %err,
                            "rebon: session lease release failed; the owner will wait out the TTL"
                        );
                    }
                }
            })
            .ok();
        LeaseGuard {
            stop,
            deliberate,
            renewer,
        }
    }

    /// Give the lease up. What a clean exit does, instead of waiting out the
    /// TTL.
    ///
    /// `deliberate` says the user meant to be done rather than to detach; see
    /// [`BackgroundIpcRequest::ReleaseLease`] for what the owner does with it.
    pub fn release_lease(&self, client_id: &str, deliberate: bool) -> anyhow::Result<()> {
        self.send(
            BackgroundIpcRequest::ReleaseLease {
                client_id: client_id.to_string(),
                deliberate,
            },
            None,
        )
        .map(|_| ())
    }

    /// Open the session's live event stream.
    ///
    /// `since` is the last cursor this client saw. The stream begins with a
    /// [`SessionEvent::Hello`] carrying the state to render against, then
    /// whatever the owner still had from `since`, then live events. A
    /// [`SessionEvent::Gap`] means the client missed something and should
    /// re-read the transcript, which is the authority regardless.
    ///
    /// The returned iterator ends when the owner hangs up or the connection
    /// breaks; a client that wants to keep watching reconnects with the last
    /// cursor it saw.
    pub fn subscribe(&self, since: Option<u64>) -> anyhow::Result<SessionEventStream> {
        if crate::protocol_probe::owner_protocol(&self.endpoint())
            == crate::protocol_probe::OwnerProtocol::Acp
        {
            return self.subscribe_over_acp(since);
        }
        let envelope = BackgroundIpcEnvelope {
            protocol_version: BACKGROUND_IPC_PROTOCOL_VERSION,
            job_id: self.job_id.clone(),
            session_id: Some(self.session_id.clone()),
            command_id: None,
            token: self.token.clone(),
            request: BackgroundIpcRequest::Subscribe { since },
        };
        let mut stream = TcpStream::connect_timeout(
            &BackgroundStore::ipc_addr(self.port).parse()?,
            OWNER_PING_TIMEOUT,
        )?;
        stream.set_nonblocking(false)?;
        // No read timeout: a quiet session is not a broken one, and a
        // subscriber that timed out between turns would reconnect on a loop
        // for as long as nobody typed.
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        serde_json::to_writer(&mut stream, &envelope)?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        // Half-close: the owner reads the request with `from_reader`, which
        // only finishes when it sees end-of-input, and a subscriber has
        // nothing more to say anyway. The read half stays open for the stream.
        stream.shutdown(std::net::Shutdown::Write)?;
        // A second handle on the same socket, so a client that stops caring
        // can end the subscription itself. Without it a reader is parked
        // until the owner speaks again, and a quiet session speaks only when
        // somebody types — which may be never.
        let socket = Arc::new(stream.try_clone()?);
        Ok(SessionEventStream {
            source: EventSource::Legacy(BufReader::new(stream).lines()),
            socket,
        })
    }

    /// The same subscription, over ACP.
    ///
    /// Two round trips before the stream begins -- `initialize`, then
    /// `_session/subscribe` -- and the connection is the subscription from
    /// there. The write half stays open, which the legacy path half-closes:
    /// this one carries the answers to the permissions the owner asks here.
    fn subscribe_over_acp(&self, since: Option<u64>) -> anyhow::Result<SessionEventStream> {
        let stream = TcpStream::connect_timeout(
            &BackgroundStore::ipc_addr(self.port).parse()?,
            OWNER_PING_TIMEOUT,
        )?;
        stream.set_nonblocking(false)?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        // A read timeout only while the handshake is expected. A quiet session
        // is not a broken one, so the stream itself waits indefinitely.
        stream.set_read_timeout(Some(OWNER_PING_TIMEOUT))?;
        let writer = stream.try_clone()?;
        let socket = Arc::new(stream.try_clone()?);
        let mut reader = BufReader::new(stream);

        let mut handshake = writer.try_clone()?;
        acp_request(
            &mut handshake,
            "initialize",
            serde_json::json!({ "_meta": { "rebon": { "token": self.token } } }),
        )?;
        read_acp_result(&mut reader).map_err(|err| {
            anyhow::anyhow!("the owner refused the subscription's handshake: {err}")
        })?;

        let mut params = serde_json::json!({ "since": since });
        if let (Some(object), Some(meta)) = (
            params.as_object_mut(),
            crate::session_ext::RebonMeta {
                job_id: self.job_id.clone(),
                session_id: Some(self.session_id.clone()),
                ..crate::session_ext::RebonMeta::default()
            }
            .to_meta(),
        ) {
            object.insert("_meta".to_string(), meta);
        }
        acp_request(
            &mut handshake,
            crate::session_ext::method::SUBSCRIBE,
            params,
        )?;
        read_acp_result(&mut reader)
            .map_err(|err| anyhow::anyhow!("the owner refused the subscription: {err}"))?;

        // The handshake wanted a deadline; the stream that follows must not
        // have one, because a quiet session is not a broken one.
        //
        // Cleared on **the reader's own handle**. `try_clone` duplicates the
        // handle rather than sharing one, and the receive timeout is a
        // property of the handle: clearing it on `socket` (a copy kept only to
        // shut the connection down) left the reader on 500 ms, so the first
        // half-second in which the owner had nothing to say read as the owner
        // hanging up. `serve` then released its lease, slept, and resubscribed
        // -- about once a second, for as long as a tab was open. Measured on
        // this platform: after `clone.set_read_timeout(None)`, the original
        // handle still reports `Some(500ms)` and still times out.
        reader.get_ref().set_read_timeout(None)?;
        Ok(SessionEventStream {
            source: EventSource::Acp(crate::acp_subscription::AcpSubscription::new(
                reader,
                writer,
                self.endpoint(),
            )),
            socket,
        })
    }

    /// Run a slash command on the owner's turn loop and return what it printed.
    pub fn run_command(
        &self,
        name: &str,
        args: Vec<String>,
    ) -> anyhow::Result<crate::CommandOutput> {
        self.run_command_with_id(name, args, crate::generate_command_id())
    }

    /// [`Self::run_command`] with the idempotency key named.
    ///
    /// The id is what a `CancelCall` uses to find this call in the owner's
    /// in-flight registry. A command is the longest-waiting request
    /// there is, so it is also the one most worth being able to give up on.
    pub fn run_command_with_id(
        &self,
        name: &str,
        args: Vec<String>,
        command_id: String,
    ) -> anyhow::Result<crate::CommandOutput> {
        let envelope = BackgroundIpcEnvelope {
            protocol_version: BACKGROUND_IPC_PROTOCOL_VERSION,
            job_id: self.job_id.clone(),
            session_id: Some(self.session_id.clone()),
            command_id: Some(command_id),
            token: self.token.clone(),
            request: BackgroundIpcRequest::RunCommand {
                name: name.to_string(),
                args,
            },
        };
        let response: BackgroundCommandResponse =
            send_envelope(self.port, &envelope, crate::command_response_timeout(name))?;
        match (response.output, response.error) {
            (Some(output), None) => Ok(output),
            (_, Some(error)) => anyhow::bail!(error),
            _ => anyhow::bail!("the session owner returned no output"),
        }
    }

    /// Queue a prompt behind whatever the session is doing.
    ///
    /// The owner writes it to the job record itself, so this is the same
    /// durable queue a client could have written directly — reached through the
    /// owner rather than around it, which wakes a parked host now instead of at
    /// its next tick.
    pub fn reply(
        &self,
        message: String,
        images: Vec<BackgroundImageAttachment>,
        command_id: Option<String>,
    ) -> anyhow::Result<()> {
        self.send(BackgroundIpcRequest::Reply { message, images }, command_id)
            .map(|_| ())
    }

    /// Put a message into the turn that is already running, rather than behind
    /// it, and say which of the two it turned out to be.
    ///
    /// The outcome is the caller's to report rather than to guess. A steer
    /// that lands in a running turn is answered by that turn; one that arrives
    /// a moment late starts a fresh one whose output nobody is awaiting, and
    /// the client's contract with its own caller says which happened.
    ///
    /// It is read here because this is the function that knows the request. It
    /// used to be read by `serve`, which built the request by hand and pulled
    /// `data["outcome"]` out as a string with a default -- the same reading
    /// written a second time, in the one place that had no way to know whether
    /// the field it was defaulting for could ever be absent.
    pub fn steer(
        &self,
        message: String,
        images: Vec<BackgroundImageAttachment>,
        command_id: Option<String>,
    ) -> anyhow::Result<rebon_proto::types::SteeringOutcome> {
        let response = self.send(BackgroundIpcRequest::Steer { message, images }, command_id)?;
        let data = response
            .data
            .ok_or_else(|| anyhow::anyhow!("the owner answered a steer without an outcome"))?;
        // The declared result type of `_session/steering`, and what the legacy
        // envelope has carried in `data` since the protocol existed. An owner
        // that answers neither is not answering this method, and saying so is
        // better than reporting the likelier of the two outcomes as fact.
        let result: rebon_proto::types::SessionSteeringResult = serde_json::from_value(data)?;
        Ok(result.outcome)
    }

    /// Interrupt the running turn, leaving the session and its host alive.
    ///
    /// `Ok(false)` means there was nothing in flight — a no-op, not a failure,
    /// which is the difference between a stop button that does nothing and one
    /// that reports an error for doing nothing.
    ///
    /// The fence is built from a fresh [`Self::status`] rather than passed in:
    /// what it identifies is the turn being cancelled, and a caller holding a
    /// snapshot from several frames ago would be naming a turn that has since
    /// ended.
    pub fn cancel_turn(&self) -> anyhow::Result<bool> {
        let status = self.status()?;
        if !matches!(
            status.status,
            BackgroundJobStatus::Queued
                | BackgroundJobStatus::Running
                | BackgroundJobStatus::NeedsInput
        ) && status.pending_permission.is_none()
        {
            return Ok(false);
        }
        let fence = BackgroundIpcCancelFence {
            status: status.status,
            turn_generation: status.turn_generation,
            updated_at_ms: status.updated_at_ms,
            pending_permission_query_id: status
                .pending_permission
                .as_ref()
                .map(|permission| permission.query_id),
        };
        match self.send(BackgroundIpcRequest::Cancel { fence }, None) {
            Ok(_) => Ok(true),
            Err(err) if err.to_string() == "background turn is no longer cancellable" => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Answer the permission prompt the owner is parked on, if it is still
    /// `query_id`.
    ///
    /// The turn generation is read from the owner rather than carried by the
    /// caller. It has to match exactly or the owner refuses, and the only value
    /// that can match is the one the owner is holding right now — a client that
    /// remembered one from the frame it rendered would be fencing against its
    /// own staleness rather than against a replaced prompt. `query_id` is what
    /// identifies the prompt the user actually answered, and that is checked
    /// here before anything is sent.
    pub fn answer_pending_permission(
        &self,
        query_id: u64,
        option_id: Option<String>,
        extra_text: Option<String>,
        command_id: Option<String>,
    ) -> anyhow::Result<AnswerOutcome> {
        let status = self.status()?;
        let Some(pending) = status
            .pending_permission
            .as_ref()
            .filter(|pending| pending.query_id == query_id)
        else {
            return Ok(AnswerOutcome::AlreadyResolved);
        };
        self.send(
            BackgroundIpcRequest::PermissionAnswer {
                query_id,
                turn_generation: pending.turn_generation,
                option_id,
                extra_text,
                updated_input: None,
            },
            command_id,
        )?;
        Ok(AnswerOutcome::Applied)
    }

    /// Answer the interactive question prompt the owner is parked on, if it is
    /// still `query_id`. Fenced exactly as [`Self::answer_pending_permission`].
    pub fn answer_pending_questions(
        &self,
        query_id: u64,
        answers: Vec<ForegroundQuestionAnswer>,
        command_id: Option<String>,
    ) -> anyhow::Result<AnswerOutcome> {
        let status = self.status()?;
        let Some(pending) = status
            .pending_permission
            .as_ref()
            .filter(|pending| pending.query_id == query_id)
        else {
            return Ok(AnswerOutcome::AlreadyResolved);
        };
        self.send(
            BackgroundIpcRequest::AnswerQuestions {
                query_id,
                turn_generation: pending.turn_generation,
                answers,
            },
            command_id,
        )?;
        Ok(AnswerOutcome::Applied)
    }

    /// Put this client's permission mode in force on the session.
    pub fn set_permission_mode(&self, mode: &str) -> anyhow::Result<()> {
        self.send(
            BackgroundIpcRequest::SetPermissionMode {
                mode: mode.to_string(),
            },
            None,
        )
        .map(|_| ())
    }

    /// Change a session option the owner holds, and report back when it lands.
    pub fn set_session_option(
        &self,
        key: &str,
        value: &str,
    ) -> anyhow::Result<SessionOptionAppliesFrom> {
        let response = self.send(
            BackgroundIpcRequest::SetSessionOption {
                key: key.to_string(),
                value: value.to_string(),
            },
            None,
        )?;
        Ok(SessionOptionAppliesFrom::from_wire(
            response
                .data
                .as_ref()
                .and_then(|data| data.get("appliesFrom"))
                .and_then(|value| value.as_str()),
        ))
    }

    /// Follow the owner's event stream on a thread, delivering events to a
    /// channel the caller can drain without waiting.
    ///
    /// A thread rather than a task, because the clients are frame loops: a
    /// terminal redrawing and a window painting both need somewhere to put
    /// "whatever arrived since the last frame", and neither can await. The
    /// connection is opened *inside* the thread, so subscribing never stalls a
    /// frame on a peer that is not answering.
    ///
    /// A closed channel is the single signal for every way this ends — the
    /// owner hung up, it never spoke this protocol, or the connection broke —
    /// because the caller's answer to all three is the same: carry on polling.
    pub fn subscribe_in_background(
        &self,
        since: Option<u64>,
    ) -> Option<std::sync::mpsc::Receiver<SessionEvent>> {
        let owner = self.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("rebon-session-events".to_string())
            .spawn(move || {
                let stream = match owner.subscribe(since) {
                    Ok(stream) => stream,
                    Err(err) => {
                        tracing::debug!(
                            error = %err,
                            "rebon: the session owner does not serve an event stream"
                        );
                        return;
                    }
                };
                for event in stream {
                    if tx.send(event).is_err() {
                        return;
                    }
                }
            })
            .ok()?;
        Some(rx)
    }
}

/// A lease held for as long as this value lives.
///
/// Dropping it stops the renewal and releases the lease, so an owner learns
/// that its last client left at the moment it leaves rather than a TTL later.
pub struct LeaseGuard {
    stop: Arc<AtomicBool>,
    deliberate: Arc<AtomicBool>,
    renewer: Option<std::thread::JoinHandle<()>>,
}

impl LeaseGuard {
    /// Say this client is leaving because the user meant to be done.
    ///
    /// Call it before dropping the guard on a clean `/exit` or a top-level
    /// Ctrl+C. Anything that hands the session on instead — `/bg`, Ctrl+Z,
    /// detaching to the desktop app — must *not* call it: those exist so the
    /// session outlives this terminal, and the owner lingering is the point.
    ///
    /// Idempotent, and safe to call on a guard whose owner has already gone.
    pub fn mark_deliberate(&self) {
        self.deliberate.store(true, Ordering::Relaxed);
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Joined rather than detached: the release is the last thing the
        // thread does, and a client exiting should not race its own goodbye.
        if let Some(renewer) = self.renewer.take() {
            let _ = renewer.join();
        }
    }
}

/// A live subscription to a session, as the client reads it.
///
/// Iterating yields one [`SessionEvent`] per line the owner wrote. A line that
/// does not parse is skipped rather than ending the stream: a client and an
/// owner from different releases may disagree about a variant, and the right
/// answer to "I do not understand this event" is to keep rendering the ones I
/// do.
pub struct SessionEventStream {
    source: EventSource,
    socket: Arc<TcpStream>,
}

/// Which wire the events are arriving on.
///
/// The distinction stops here. Everything above reads `SessionEvent`, and the
/// `StreamWatermark` de-duplicates by a cursor that both wires carry, so no
/// follower learns that the protocol changed underneath it.
enum EventSource {
    /// One `SessionEvent` per line, as the builds predating ACP send them.
    Legacy(std::io::Lines<BufReader<TcpStream>>),
    /// ACP messages, translated back into the events above.
    Acp(crate::acp_subscription::AcpSubscription),
}

impl SessionEventStream {
    /// Which wire this subscription is reading.
    ///
    /// Only the tests ask, and what they ask is unanswerable any other way: a
    /// session event looks the same whichever protocol carried it, which is
    /// the point of the translation and also the reason a test cannot tell
    /// them apart by watching what arrives.
    #[doc(hidden)]
    pub fn speaks_acp(&self) -> bool {
        matches!(self.source, EventSource::Acp(_))
    }

    /// A handle that ends this subscription from another thread.
    ///
    /// Iteration blocks until the owner writes a line, and a session nobody
    /// is prompting writes nothing — so "I have stopped watching" has to be
    /// said on the socket, not on a flag the reader only checks between
    /// events.
    pub fn closer(&self) -> SessionStreamCloser {
        SessionStreamCloser(Arc::clone(&self.socket))
    }
}

/// Closes a [`SessionEventStream`] from wherever the decision was made.
#[derive(Clone)]
pub struct SessionStreamCloser(Arc<TcpStream>);

impl SessionStreamCloser {
    pub fn close(&self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

impl Iterator for SessionEventStream {
    type Item = SessionEvent;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.source {
            EventSource::Acp(subscription) => subscription.next_event(),
            EventSource::Legacy(lines) => loop {
                let line = lines.next()?.ok()?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str(&line) {
                    Ok(event) => return Some(event),
                    Err(err) => {
                        tracing::debug!(error = %err, "rebon: skipping an unreadable session event");
                        continue;
                    }
                }
            },
        }
    }
}

/// Who owns `session_id`, and can this client command them.
///
/// The lock answers "is it held" — it is the only thing that can, because it
/// is the only claim the OS releases on a crash. The descriptor answers "by
/// whom and where", and the ping answers "and is it listening". A missing
/// descriptor beside a held lock is not an error: it is what an in-process
/// terminal host looks like.
pub fn resolve_owner(projects_root: &Path, cwd: &str, session_id: &str) -> OwnerState {
    if !rebon_session::is_session_active(projects_root, cwd, session_id) {
        return OwnerState::Free;
    }
    let Some(descriptor) = rebon_session::read_session_owner(projects_root, cwd, session_id) else {
        return OwnerState::OwnedOpaque { descriptor: None };
    };
    let Some(owner) = OwnerHandle::from_descriptor(session_id, &descriptor) else {
        return OwnerState::OwnedOpaque {
            descriptor: Some(descriptor),
        };
    };
    if owner.ping() {
        OwnerState::OwnedReachable { owner }
    } else {
        OwnerState::OwnedUnreachable { descriptor }
    }
}

/// An owner's answer, in the shape the desktop already renders.
///
/// [`crate::ForegroundSessionStatus`] was the mailbox's answer to "what is this
/// session doing": a file a terminal republished every second, read by whoever
/// wanted to draw controls for it. The owner's [`SessionStatusSnapshot`] is the
/// same answer from the process that actually knows, and it is a superset —
/// which is why this translation is a projection and not a merge.
///
/// It lives here, in one function, so that retiring the mailbox is deleting it
/// and moving the readers onto the snapshot, rather than hunting for
/// field-by-field assumptions spread through a UI. Two fields are not carried
/// across but derived, and both say what they mean:
///
/// - `heartbeat_ms` was "when the publisher last proved it was alive". Liveness
///   is the lock now (invariant I5), so this is `updated_at_ms` — when the
///   owner last had something to say — and nothing gates on its age.
/// - `recent_command_results` was the mailbox's acknowledgement history. Over
///   an endpoint a command is applied before the call returns, so the only
///   entry here is whatever the owner last remembered.
///
/// A session that hosts itself in-process holds the lock and publishes no
/// endpoint, so the mailbox — and this projection onto its status type — is how
/// its status is read.
pub fn foreground_status_from_owner(
    session_id: &str,
    owner: &OwnerHandle,
    status: &SessionStatusSnapshot,
) -> crate::ForegroundSessionStatus {
    crate::ForegroundSessionStatus {
        pid: owner.pid,
        session_id: status
            .session_id
            .clone()
            .unwrap_or_else(|| session_id.to_string()),
        cwd: status.cwd.clone(),
        heartbeat_ms: status.updated_at_ms,
        busy: status.busy,
        pending_permission: status.pending_permission.clone(),
        ask_user_questions: status.ask_user_questions.clone(),
        last_command_id: status.last_command_id.clone(),
        last_command_at_ms: status.last_command_at_ms,
        last_command_error: status.last_command_error.clone(),
        recent_command_results: status
            .last_command_id
            .clone()
            .map(|command_id| {
                vec![crate::ForegroundCommandResult {
                    command_id,
                    processed_at_ms: status.last_command_at_ms,
                    outcome: match status.last_command_error {
                        None => crate::ForegroundCommandOutcome::Applied,
                        Some(_) => crate::ForegroundCommandOutcome::Rejected,
                    },
                    error: status.last_command_error.clone(),
                }]
            })
            .unwrap_or_default(),
    }
}

/// [`resolve_owner`], remembered for a moment.
///
/// Resolving is three trips out of the process — a lock probe, a descriptor
/// read, and a ping — and the clients that need the answer are frame loops that
/// would ask several times a second, for every session on screen. The answer
/// does not change that fast: a host that was reachable a few hundred
/// milliseconds ago is reachable now, and if it is not, the command that
/// discovers it says so.
///
/// An owner that did not answer is remembered for longer than one that did,
/// because that is the expensive answer to re-derive: the ping has to wait out
/// its whole timeout before it can be called a failure, and repeating that once
/// a frame would stall the caller behind a host that is already known to be
/// wedged.
pub struct OwnerCache {
    reachable_ttl: Duration,
    unreachable_ttl: Duration,
    entries: Mutex<HashMap<String, (Instant, OwnerState)>>,
}

impl OwnerCache {
    pub fn new(reachable_ttl: Duration, unreachable_ttl: Duration) -> Self {
        Self {
            reachable_ttl,
            unreachable_ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Who owns `session_id` — from the last answer if it is still fresh.
    pub fn resolve(&self, projects_root: &Path, cwd: &str, session_id: &str) -> OwnerState {
        if let Some(state) = self.cached(session_id) {
            return state;
        }
        let state = resolve_owner(projects_root, cwd, session_id);
        self.remember(session_id, state.clone());
        state
    }

    /// The handle for a reachable owner, or nothing.
    pub fn handle(&self, projects_root: &Path, cwd: &str, session_id: &str) -> Option<OwnerHandle> {
        match self.resolve(projects_root, cwd, session_id) {
            OwnerState::OwnedReachable { owner } => Some(owner),
            _ => None,
        }
    }

    /// Forget what is known about `session_id`.
    ///
    /// What a client calls after a command failed to reach the owner it was
    /// aimed at: the cached answer is the reason it aimed there, so keeping it
    /// would send the next command to the same dead endpoint.
    pub fn invalidate(&self, session_id: &str) {
        self.entries.lock().expect("poisoned").remove(session_id);
    }

    /// Forget everything. What a client calls when the whole job store moved
    /// under it — a different config home, or a test fixture between cases.
    pub fn invalidate_all(&self) {
        self.entries.lock().expect("poisoned").clear();
    }

    fn cached(&self, session_id: &str) -> Option<OwnerState> {
        let entries = self.entries.lock().expect("poisoned");
        let (at, state) = entries.get(session_id)?;
        let ttl = match state {
            OwnerState::OwnedUnreachable { .. } => self.unreachable_ttl,
            _ => self.reachable_ttl,
        };
        (at.elapsed() < ttl).then(|| state.clone())
    }

    fn remember(&self, session_id: &str, state: OwnerState) {
        self.entries
            .lock()
            .expect("poisoned")
            .insert(session_id.to_string(), (Instant::now(), state));
    }
}

/// The payload inside an ACP answer, in the shape the legacy response carried.
///
/// The owner wraps two answers in a declared result type — a status in
/// `{snapshot}`, a command's output in `{output}` — and every other method
/// answers with its payload directly. Undoing that here rather than at each
/// call site is what lets the two protocols hand their callers the same value.
///
/// Which wrapper to undo is decided by the *request*, never guessed from the
/// answer: `{}` is a valid answer under either shape, and guessing would turn
/// a command that printed nothing into a status with no snapshot.
fn unwrap_answer(
    request: &BackgroundIpcRequest,
    result: serde_json::Value,
) -> Option<serde_json::Value> {
    let payload = if matches!(request, BackgroundIpcRequest::Status) {
        result.get("snapshot").cloned()
    } else if crate::session_ext::answer_is_command_output(request) {
        result.get("output").cloned()
    } else {
        Some(result)
    };
    // An empty object is "nothing to say", which the legacy wire spells as an
    // absent `data` rather than as `{}`.
    payload.filter(|payload| !matches!(payload, serde_json::Value::Object(map) if map.is_empty()))
}

/// Write one JSON-RPC request on a subscription's socket.
fn acp_request(
    stream: &mut TcpStream,
    method: &str,
    params: serde_json::Value,
) -> anyhow::Result<()> {
    let mut body = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": method,
        "method": method,
        "params": params,
    }))?;
    body.push(b'\n');
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(())
}

/// Read one JSON-RPC answer, failing on an error answer.
///
/// Only the handshake uses this. After it, everything arriving is an event or
/// a question, and neither is something to wait for.
fn read_acp_result(reader: &mut BufReader<TcpStream>) -> anyhow::Result<serde_json::Value> {
    let mut line = String::new();
    if BufRead::read_line(reader, &mut line)? == 0 {
        anyhow::bail!("the owner closed the connection");
    }
    let message: serde_json::Value = serde_json::from_str(&line)?;
    if let Some(error) = message.get("error") {
        anyhow::bail!(
            "{}",
            error
                .get("message")
                .and_then(|message| message.as_str())
                .unwrap_or("the owner refused")
        );
    }
    Ok(message.get("result").cloned().unwrap_or_default())
}

/// One round trip to an owner's endpoint.
///
/// A deadline that passes comes back as [`HostCallError::HostUnanswered`] and
/// everything else as [`HostCallError::Transport`]. Keeping the two apart is
/// the whole point: only the first is a call the owner may still be holding,
/// and therefore the only one worth sending a `CancelCall` for.
fn send_envelope<T: serde::de::DeserializeOwned>(
    port: u16,
    envelope: &BackgroundIpcEnvelope,
    read_timeout: Duration,
) -> Result<T, HostCallError> {
    let address = BackgroundStore::ipc_addr(port)
        .parse()
        .map_err(|err| HostCallError::Transport(format!("{err}")))?;
    let mut stream =
        TcpStream::connect_timeout(&address, OWNER_PING_TIMEOUT).map_err(transport_error)?;
    stream.set_nonblocking(false).map_err(transport_error)?;
    stream
        .set_read_timeout(Some(read_timeout))
        .map_err(transport_error)?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(transport_error)?;
    serde_json::to_writer(&mut stream, envelope).map_err(encode_error)?;
    stream.write_all(b"\n").map_err(transport_error)?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    serde_json::from_reader(&mut stream).map_err(decode_error)
}

/// A socket error, sorted into "the deadline passed" and everything else.
///
/// Windows reports an expired `SO_RCVTIMEO` as `TimedOut` and Unix as
/// `WouldBlock`; both mean the same thing here, and a client that only knew one
/// of them would block out its own timeout on the other platform.
fn transport_error(err: std::io::Error) -> HostCallError {
    match err.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
            HostCallError::HostUnanswered
        }
        _ => HostCallError::Transport(err.to_string()),
    }
}

fn encode_error(err: serde_json::Error) -> HostCallError {
    match err.io_error_kind() {
        Some(kind) => transport_error(std::io::Error::from(kind)),
        None => HostCallError::InvalidRequest,
    }
}

/// A response that could not be read.
///
/// An owner that hung up without answering, or one whose answer arrived after
/// the deadline, both surface here as an I/O failure rather than as malformed
/// JSON — so the timeout classification has to happen on this path too.
fn decode_error(err: serde_json::Error) -> HostCallError {
    match err.io_error_kind() {
        Some(kind) => transport_error(std::io::Error::from(kind)),
        // A truncated or unreadable answer is not a timeout: the owner said
        // something, and it was not something this client can act on.
        None if err.is_eof() => HostCallError::Transport(err.to_string()),
        None => HostCallError::InvalidRequest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("rebon-owner-{tag}-"))
            .tempdir()
            .unwrap()
    }

    fn descriptor(port: Option<u16>) -> SessionOwnerDescriptor {
        SessionOwnerDescriptor {
            version: rebon_session::SESSION_OWNER_VERSION,
            pid: std::process::id(),
            pid_identity: None,
            surface: if port.is_some() {
                SessionOwnerSurface::Worker
            } else {
                SessionOwnerSurface::Local
            },
            job_id: Some("job-1".to_string()),
            ipc_port: port,
            ipc_token: port.map(|_| "token".to_string()),
            started_at_ms: 1,
        }
    }

    #[test]
    fn an_unheld_session_is_free() {
        let tmp = tempdir("free");
        assert_eq!(
            resolve_owner(tmp.path(), "/work/free", "sess-free"),
            OwnerState::Free
        );
    }

    /// A terminal hosting a session in its own process holds the lock and
    /// publishes no endpoint. That is not a broken owner — it is the escape
    /// hatch, and it must read as "someone has this", not as "free".
    #[test]
    fn a_held_session_without_a_descriptor_is_opaque() {
        let tmp = tempdir("opaque");
        let _lock = rebon_session::try_acquire_session_active_lock(
            tmp.path(),
            "/work/opaque",
            "sess-opaque",
        )
        .unwrap()
        .expect("the fixture session starts free");

        let state = resolve_owner(tmp.path(), "/work/opaque", "sess-opaque");

        assert_eq!(state, OwnerState::OwnedOpaque { descriptor: None });
        assert!(state.is_owned());
        assert!(state.handle().is_none());
    }

    #[test]
    fn a_descriptor_without_an_endpoint_is_opaque_too() {
        let tmp = tempdir("local");
        let _lock =
            rebon_session::try_acquire_session_active_lock(tmp.path(), "/work/local", "sess-local")
                .unwrap()
                .unwrap();
        rebon_session::write_session_owner(
            tmp.path(),
            "/work/local",
            "sess-local",
            &descriptor(None),
        )
        .unwrap();

        let state = resolve_owner(tmp.path(), "/work/local", "sess-local");

        assert!(matches!(
            state,
            OwnerState::OwnedOpaque {
                descriptor: Some(_)
            }
        ));
        assert!(state.describe().unwrap().contains("terminal"));
    }

    /// The case that must never become a takeover: a live owner whose endpoint
    /// does not answer. Port 1 is not listening, so this stands in for a
    /// wedged host.
    #[test]
    fn a_live_owner_that_does_not_answer_stays_owned() {
        let tmp = tempdir("wedged");
        let _lock = rebon_session::try_acquire_session_active_lock(
            tmp.path(),
            "/work/wedged",
            "sess-wedged",
        )
        .unwrap()
        .unwrap();
        rebon_session::write_session_owner(
            tmp.path(),
            "/work/wedged",
            "sess-wedged",
            &descriptor(Some(1)),
        )
        .unwrap();

        let state = resolve_owner(tmp.path(), "/work/wedged", "sess-wedged");

        assert!(matches!(state, OwnerState::OwnedUnreachable { .. }));
        assert!(
            state.handle().is_none(),
            "an unreachable owner takes no commands"
        );
    }

    /// A stand-in owner: one loopback listener that serves a scripted line for
    /// each connection it is given and then stops.
    ///
    /// Serving a fixed number of connections rather than looping forever is
    /// what makes it a fixture — the thread ends when the script does, so a
    /// test that sent one request too many fails on the missing answer rather
    /// than leaving a thread parked on `accept` for the rest of the run.
    struct StubOwner {
        handle: OwnerHandle,
        received: Arc<Mutex<Vec<BackgroundIpcEnvelope>>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl StubOwner {
        /// One entry per expected connection; each entry is the lines that
        /// connection is answered with.
        fn new(script: Vec<Vec<serde_json::Value>>) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let received = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&received);
            let thread = std::thread::spawn(move || {
                let mut script = script.into_iter().peekable();
                loop {
                    let Ok((stream, _)) = listener.accept() else {
                        return;
                    };
                    let mut writer = match stream.try_clone() {
                        Ok(writer) => writer,
                        Err(_) => return,
                    };
                    let mut line = String::new();
                    // One line, not to end-of-input: the protocol probe leaves
                    // its write half open, and reading to EOF would block on it
                    // until it gave up.
                    match std::io::BufRead::read_line(
                        &mut std::io::BufReader::new(stream),
                        &mut line,
                    ) {
                        // A connection that says nothing is the handle waking
                        // this thread so it can be joined. No real client opens
                        // one and stays silent.
                        Ok(0) => return,
                        Ok(_) => {}
                        Err(_) => continue,
                    }
                    let Ok(envelope) = serde_json::from_str::<BackgroundIpcEnvelope>(&line) else {
                        // Not an envelope: a client asking which protocol this
                        // is. A worker from before ACP answers exactly this —
                        // its own decoder's complaint, in the legacy shape,
                        // with the connection left whole — so this fake does
                        // too, and **does not consume a scripted connection**.
                        // A probe is not one of the requests a test scripted.
                        let refusal = serde_json::json!({
                            "ok": false,
                            "error": "missing field `token` at line 1 column 196",
                        });
                        if serde_json::to_writer(&mut writer, &refusal).is_ok() {
                            let _ = writer.write_all(b"\n");
                            let _ = writer.flush();
                        }
                        continue;
                    };
                    seen.lock().unwrap().push(envelope);
                    let Some(lines) = script.next() else {
                        return;
                    };
                    for line in lines {
                        if serde_json::to_writer(&mut writer, &line).is_err() {
                            break;
                        }
                        if writer.write_all(b"\n").is_err() {
                            break;
                        }
                    }
                    let _ = writer.flush();
                    if script.peek().is_none() {
                        // The last scripted connection has been answered. The
                        // old shape of this loop was `for lines in script`,
                        // which ended here too -- and it has to, because the
                        // handle joins this thread when it drops.
                        return;
                    }
                }
            });
            Self {
                handle: OwnerHandle {
                    session_id: "sess-stub".to_string(),
                    job_id: Some("job-stub".to_string()),
                    pid: std::process::id(),
                    port,
                    token: "token".to_string(),
                    surface: SessionOwnerSurface::Worker,
                },
                received,
                thread: Some(thread),
            }
        }

        fn requests(&self) -> Vec<BackgroundIpcRequest> {
            self.received
                .lock()
                .unwrap()
                .iter()
                .map(|envelope| envelope.request.clone())
                .collect()
        }
    }

    impl Drop for StubOwner {
        fn drop(&mut self) {
            if let Some(thread) = self.thread.take() {
                // Wake the accept before joining. A test that ends without
                // using every scripted connection -- which is what every test
                // that *fails* does, since it stops at its first assertion --
                // leaves this thread blocked in `accept`, and the join then
                // waits for a connection that is never coming. That turned a
                // one-line assertion failure into a sixty-second hang, and hid
                // the message behind it.
                //
                // The owner's own shutdown does the same thing for the same
                // reason: a blocking accept is returned by a connection, and
                // the listener belongs to the thread that is blocked in it.
                let _ = std::net::TcpStream::connect_timeout(
                    &format!("127.0.0.1:{}", self.handle.port)
                        .parse()
                        .expect("a local address"),
                    Duration::from_millis(250),
                );
                let _ = thread.join();
            }
        }
    }

    fn status_line(status: BackgroundJobStatus, turn_generation: u64) -> SessionStatusSnapshot {
        SessionStatusSnapshot {
            job_id: "job-stub".to_string(),
            session_id: Some("sess-stub".to_string()),
            cwd: "/work/stub".to_string(),
            status,
            busy: matches!(status, BackgroundJobStatus::Running),
            turn_generation,
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
            updated_at_ms: 1,
        }
    }

    fn ok_with(data: SessionStatusSnapshot) -> serde_json::Value {
        serde_json::json!({ "ok": true, "data": serde_json::to_value(data).unwrap() })
    }

    fn pending_permission(
        query_id: u64,
        turn_generation: u64,
    ) -> crate::BackgroundPermissionQuerySnapshot {
        crate::BackgroundPermissionQuerySnapshot {
            query_id,
            turn_generation,
            endpoint: None,
            tool: Some("Bash".to_string()),
            tool_call_id: None,
            session_id: Some("sess-stub".to_string()),
            title: None,
            message: None,
            tool_input: None,
            metadata: None,
            options: Vec::new(),
        }
    }

    /// The other row of the compatibility matrix: a client that speaks ACP,
    /// against an owner that does not.
    ///
    /// The ACP row is in `rebon-session-runtime`, where a real owner can be
    /// started. This row belongs here, because the thing it needs is a *fake*
    /// owner that answers the way a build from before ACP does -- which is
    /// exactly what `StubOwner` is, now that it answers a probe the way the
    /// real one was measured to.
    ///
    /// What is asserted is the transport, not the answer: an answer looks the
    /// same over either wire. The stub records the envelopes it decoded, so an
    /// envelope arriving is proof the legacy path carried this request.
    #[test]
    fn a_new_client_against_an_owner_that_predates_e3_falls_back_to_the_envelope() {
        let owner = StubOwner::new(vec![vec![serde_json::json!({ "ok": true })]]);
        let endpoint = owner.handle.endpoint();
        crate::protocol_probe::forget(&endpoint);

        assert_eq!(
            crate::protocol_probe::owner_protocol(&endpoint),
            crate::protocol_probe::OwnerProtocol::Legacy,
            "the probe should have read this owner as one that predates E3"
        );

        let started = std::time::Instant::now();
        owner
            .handle
            .send(BackgroundIpcRequest::Ping, None)
            .expect("the owner answers");
        let elapsed = started.elapsed();

        assert_eq!(
            owner.requests(),
            vec![BackgroundIpcRequest::Ping],
            "the request did not arrive as an envelope, so it did not take the legacy path"
        );
        // Not only *that* it fell back, but that it did not pay for the wrong
        // guess first. A client that opens an ACP link to a legacy owner waits
        // out the handshake before falling back, and the answer it finally
        // gets is correct -- which is why the arrival of the envelope alone
        // cannot catch it.
        assert!(
            elapsed < Duration::from_secs(2),
            "falling back took {elapsed:?}, which means the wrong wire was tried first"
        );
        crate::protocol_probe::forget(&endpoint);
    }

    /// And the probe is asked once per owner, not once per request: a second
    /// call to the same endpoint must not open another connection to ask again.
    ///
    /// `StubOwner` scripts one connection, so a second probe would consume the
    /// scripted answer and the second request would be left without one.
    #[test]
    fn the_probe_does_not_run_again_for_every_request() {
        let owner = StubOwner::new(vec![
            vec![serde_json::json!({ "ok": true })],
            vec![serde_json::json!({ "ok": true })],
        ]);
        let endpoint = owner.handle.endpoint();
        crate::protocol_probe::forget(&endpoint);

        owner
            .handle
            .send(BackgroundIpcRequest::Ping, None)
            .expect("the first answer");
        owner
            .handle
            .send(BackgroundIpcRequest::Ping, None)
            .expect("the second answer");

        assert_eq!(
            owner.requests(),
            vec![BackgroundIpcRequest::Ping, BackgroundIpcRequest::Ping]
        );
        crate::protocol_probe::forget(&endpoint);
    }

    #[test]
    fn a_worker_handle_carries_the_endpoint_it_was_built_from() {
        let endpoint = BackgroundIpcEndpoint {
            pid: 4242,
            port: 51234,
            token: "t".to_string(),
        };

        let handle = OwnerHandle::for_worker("sess-a", Some("job-a"), &endpoint);

        assert_eq!(handle.surface, SessionOwnerSurface::Worker);
        assert_eq!(handle.job_id.as_deref(), Some("job-a"));
        assert_eq!(handle.endpoint(), endpoint);
    }

    /// Nothing in flight is a no-op, not a failure — and the owner is not asked
    /// to cancel something the client already knows is not running.
    #[test]
    fn cancelling_an_idle_session_asks_only_what_it_is_doing() {
        let owner = StubOwner::new(vec![vec![ok_with(status_line(
            BackgroundJobStatus::Idle,
            7,
        ))]]);

        let cancelled = owner.handle.cancel_turn().unwrap();

        assert!(!cancelled);
        assert!(matches!(
            owner.requests().as_slice(),
            [BackgroundIpcRequest::Status]
        ));
    }

    /// The fence names the turn the owner is on right now, read one round trip
    /// before the cancel — not one a caller was holding from an earlier frame.
    #[test]
    fn a_cancel_fences_the_turn_the_owner_reported() {
        let mut running = status_line(BackgroundJobStatus::Running, 12);
        running.pending_permission = Some(pending_permission(5, 12));
        let owner = StubOwner::new(vec![
            vec![ok_with(running)],
            vec![serde_json::json!({ "ok": true })],
        ]);

        assert!(owner.handle.cancel_turn().unwrap());

        let requests = owner.requests();
        let Some(BackgroundIpcRequest::Cancel { fence }) = requests.get(1) else {
            panic!("expected a cancel after the status, got {requests:?}");
        };
        assert_eq!(fence.turn_generation, 12);
        assert_eq!(fence.status, BackgroundJobStatus::Running);
        assert_eq!(fence.pending_permission_query_id, Some(5));
    }

    /// An answer for a prompt the owner has moved past is a race somebody else
    /// won, not an error to retry. Nothing is sent.
    #[test]
    fn answering_a_prompt_the_owner_moved_past_is_not_an_error() {
        let mut parked = status_line(BackgroundJobStatus::NeedsInput, 3);
        parked.pending_permission = Some(pending_permission(9, 3));
        let owner = StubOwner::new(vec![vec![ok_with(parked)]]);

        let outcome = owner
            .handle
            .answer_pending_permission(7, Some("allow".into()), None, None)
            .unwrap();

        assert_eq!(outcome, AnswerOutcome::AlreadyResolved);
        assert!(matches!(
            owner.requests().as_slice(),
            [BackgroundIpcRequest::Status]
        ));
    }

    /// The owner refuses an answer whose turn generation is not the one it is
    /// parked on, so the client reads that generation off the owner rather than
    /// remembering one.
    #[test]
    fn an_answer_carries_the_generation_the_owner_is_parked_on() {
        let mut parked = status_line(BackgroundJobStatus::NeedsInput, 4);
        parked.pending_permission = Some(pending_permission(7, 4));
        let owner = StubOwner::new(vec![
            vec![ok_with(parked)],
            vec![serde_json::json!({ "ok": true })],
        ]);

        let outcome = owner
            .handle
            .answer_pending_permission(7, Some("allow".into()), Some("note".into()), None)
            .unwrap();

        assert_eq!(outcome, AnswerOutcome::Applied);
        let requests = owner.requests();
        let Some(BackgroundIpcRequest::PermissionAnswer {
            query_id,
            turn_generation,
            option_id,
            extra_text,
            ..
        }) = requests.get(1)
        else {
            panic!("expected a permission answer, got {requests:?}");
        };
        assert_eq!(*query_id, 7);
        assert_eq!(*turn_generation, 4);
        assert_eq!(option_id.as_deref(), Some("allow"));
        assert_eq!(extra_text.as_deref(), Some("note"));
    }

    #[test]
    fn questions_are_fenced_the_same_way_as_permissions() {
        let mut parked = status_line(BackgroundJobStatus::NeedsInput, 8);
        parked.pending_permission = Some(pending_permission(11, 8));
        let owner = StubOwner::new(vec![
            vec![ok_with(parked)],
            vec![serde_json::json!({ "ok": true })],
        ]);

        let outcome = owner
            .handle
            .answer_pending_questions(
                11,
                vec![ForegroundQuestionAnswer {
                    selected_options: vec![0],
                    other_text: None,
                }],
                None,
            )
            .unwrap();

        assert_eq!(outcome, AnswerOutcome::Applied);
        let requests = owner.requests();
        let Some(BackgroundIpcRequest::AnswerQuestions {
            query_id,
            turn_generation,
            ..
        }) = requests.get(1)
        else {
            panic!("expected an answer, got {requests:?}");
        };
        assert_eq!(*query_id, 11);
        assert_eq!(*turn_generation, 8);
    }

    /// The owner says when a change lands, and the client passes that through
    /// rather than softening it into "done".
    #[test]
    fn a_session_option_reports_when_it_takes_effect() {
        let owner = StubOwner::new(vec![
            vec![serde_json::json!({ "ok": true, "data": { "appliesFrom": "nextTurn" } })],
            vec![serde_json::json!({ "ok": true })],
        ]);

        assert_eq!(
            owner.handle.set_session_option("model", "gpt-5").unwrap(),
            SessionOptionAppliesFrom::NextTurn
        );
        assert_eq!(
            owner.handle.set_session_option("agent", "local").unwrap(),
            SessionOptionAppliesFrom::Immediately,
            "an owner that says nothing about timing has already applied it"
        );
    }

    #[test]
    fn a_background_subscription_delivers_the_owners_events() {
        // `turn_generation`, not `turnGeneration`: the enum's `rename_all`
        // renames variant names, not the fields inside a variant. The snapshot
        // nested under `status` is a struct with its own `rename_all`, so its
        // fields *are* camelCase — the two conventions really do meet here.
        let hello = serde_json::json!({
            "kind": "hello",
            "cursor": 4,
            "turn_generation": 2,
            "status": serde_json::to_value(status_line(BackgroundJobStatus::Running, 2)).unwrap(),
        });
        let turn = serde_json::json!({ "kind": "turn", "cursor": 5, "state": "idle" });
        let owner = StubOwner::new(vec![vec![hello, turn]]);

        let events = owner.handle.subscribe_in_background(Some(0)).unwrap();

        assert!(matches!(
            events.recv_timeout(Duration::from_secs(5)).unwrap(),
            SessionEvent::Hello { cursor: 4, .. }
        ));
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(5)).unwrap(),
            SessionEvent::Turn { cursor: 5, .. }
        ));
        assert!(
            matches!(
                owner.requests().as_slice(),
                [BackgroundIpcRequest::Subscribe { since: Some(0) }]
            ),
            "the subscription asks for everything the owner still holds"
        );
    }

    /// A cached answer is the whole point, and a cached answer is also how a
    /// client ends up aiming at an endpoint that has gone: invalidating is what
    /// a failed command does.
    #[test]
    fn the_cache_answers_from_memory_until_it_is_invalidated() {
        let tmp = tempdir("cache");
        let cache = OwnerCache::new(Duration::from_secs(60), Duration::from_secs(60));
        assert_eq!(
            cache.resolve(tmp.path(), "/work/cache", "sess-cache"),
            OwnerState::Free
        );

        let _lock =
            rebon_session::try_acquire_session_active_lock(tmp.path(), "/work/cache", "sess-cache")
                .unwrap()
                .unwrap();

        assert_eq!(
            cache.resolve(tmp.path(), "/work/cache", "sess-cache"),
            OwnerState::Free,
            "the lock was taken after the answer was cached"
        );
        cache.invalidate("sess-cache");
        assert_eq!(
            cache.resolve(tmp.path(), "/work/cache", "sess-cache"),
            OwnerState::OwnedOpaque { descriptor: None }
        );
    }

    /// A host that did not answer is the expensive answer to re-derive — the
    /// ping has to wait out its whole timeout — so it is kept for longer than a
    /// host that did.
    #[test]
    fn an_unreachable_owner_is_remembered_for_longer_than_a_live_one() {
        let tmp = tempdir("ttl");
        let cache = OwnerCache::new(Duration::ZERO, Duration::from_secs(60));
        let lock =
            rebon_session::try_acquire_session_active_lock(tmp.path(), "/work/ttl", "sess-ttl")
                .unwrap()
                .unwrap();
        rebon_session::write_session_owner(
            tmp.path(),
            "/work/ttl",
            "sess-ttl",
            &descriptor(Some(1)),
        )
        .unwrap();
        assert!(matches!(
            cache.resolve(tmp.path(), "/work/ttl", "sess-ttl"),
            OwnerState::OwnedUnreachable { .. }
        ));

        lock.release();

        assert!(
            matches!(
                cache.resolve(tmp.path(), "/work/ttl", "sess-ttl"),
                OwnerState::OwnedUnreachable { .. }
            ),
            "the wedged host is still remembered"
        );
        cache.invalidate("sess-ttl");
        assert_eq!(
            cache.resolve(tmp.path(), "/work/ttl", "sess-ttl"),
            OwnerState::Free
        );
    }

    /// Releasing the lock takes the descriptor with it, so a session that was
    /// hosted and closed does not leave behind an address that resolves.
    #[test]
    fn releasing_the_lock_removes_the_descriptor() {
        let tmp = tempdir("release");
        let lock = rebon_session::try_acquire_session_active_lock(
            tmp.path(),
            "/work/release",
            "sess-release",
        )
        .unwrap()
        .unwrap();
        rebon_session::write_session_owner(
            tmp.path(),
            "/work/release",
            "sess-release",
            &descriptor(Some(1)),
        )
        .unwrap();
        assert!(
            rebon_session::read_session_owner(tmp.path(), "/work/release", "sess-release")
                .is_some()
        );

        lock.release();

        assert!(
            rebon_session::read_session_owner(tmp.path(), "/work/release", "sess-release")
                .is_none()
        );
        assert_eq!(
            resolve_owner(tmp.path(), "/work/release", "sess-release"),
            OwnerState::Free
        );
    }

    fn held_lock(session_id: &str) -> (tempfile::TempDir, rebon_session::HeldSessionLock) {
        let dir = tempfile::tempdir().unwrap();
        let lock = rebon_session::try_acquire_session_active_lock(dir.path(), "/work", session_id)
            .unwrap()
            .expect("a fresh session is free");
        (
            dir,
            rebon_session::HeldSessionLock {
                session_id: session_id.to_string(),
                lock,
            },
        )
    }

    /// A `--local` session is the one host that publishes no endpoint: it
    /// holds the session's lock and nothing else. To every other process
    /// that reads as owned but opaque — not free, so nobody takes the
    /// session; not reachable, so nobody tries to drive it. Letting the lock go
    /// is what frees it.
    #[test]
    fn a_local_session_reads_as_owned_opaque_to_everyone_else() {
        let (dir, held) = held_lock("sess-local");

        assert_eq!(
            resolve_owner(dir.path(), "/work", "sess-local"),
            OwnerState::OwnedOpaque { descriptor: None }
        );

        drop(held);
        assert_eq!(
            resolve_owner(dir.path(), "/work", "sess-local"),
            OwnerState::Free
        );
    }
}
