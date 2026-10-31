//! The loopback listener a plain foreground TUI offers for cross-session agent
//! messages.
//!
//! A terminal that hosts its session in-process holds the session lock and
//! publishes no `<sid>.owner.json`, so `resolve_owner` reports it as
//! `OwnedOpaque` and the desktop app keeps reaching it through the file
//! mailbox. That is right for control commands, but a mailbox entry cannot tell
//! an agent message's sender where the message landed: the sender would have to
//! wait for the TUI's next event-loop pass and read the ack back out of a status
//! file. This endpoint is the small synchronous door beside the mailbox, for the
//! one request whose caller needs an answer.
//!
//! It speaks the same wire as the session-host IPC — one compact JSON envelope
//! per connection, token authenticated, one JSON response — so an
//! `OwnerHandle`-shaped client can talk to it unchanged. It deliberately does
//! **not** publish into `owner.json`: the desktop app routes every session with
//! a reachable owner over that endpoint and would send it Reply, permission
//! answers, cancel and the rest, none of which a foreground event loop serves
//! here. The address lives in `.live.json` instead, where only a client looking
//! for agent-message delivery reads it.
//!
//! What it accepts is deliberately narrow: `Ping`, a minimal `Status`, and
//! `AgentMessage` (legacy envelope or the ACP `_session/agent_message`
//! spelling). Everything else is refused with a sentence saying so, because a
//! client that mistakes this for a general control plane must not be left to
//! discover that one request at a time.
//!
//! The listener lives exactly as long as the session lock: the TUI starts it
//! when it becomes the lock holder, drops it when the lock is lost or the
//! process exits, and the drop removes the token file.

use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rebon_proto::types::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use serde_json::Value;

use crate::agent_message::{AgentMessageDelivery, AgentMessageEnvelope, AgentMessageResult};
use crate::legacy_foreground::{
    write_foreground_agent_token, ForegroundAgentEndpointInfo, ForegroundCommand,
};
use crate::protocol::{BackgroundIpcEnvelope, BackgroundIpcRequest, BackgroundIpcResponse};
use crate::session_ext::{self, AgentMessageParams, RebonMeta};

/// Refusal sentence every unsupported request carries.
const NOT_SUPPORTED: &str =
    "not supported by a foreground session's agent endpoint; it accepts only Ping, Status and AgentMessage";

/// A request larger than this is not an agent message. The bound keeps a
/// malformed local client from making the listener allocate without limit.
const MAX_REQUEST_BYTES: u64 = 1 << 20;

/// How long one connection may take to send its line and read its answer.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// How many accepted message ids are remembered for idempotency.
const SEEN_HISTORY: usize = 512;

/// What the listener needs from the terminal it belongs to.
///
/// Delivery happens on the caller's thread, synchronously, so the answer can
/// name where the message went. The TUI's inbox ([`ForegroundAgentInbox`]) is
/// the implementation production uses; tests use a recorder.
pub trait ForegroundAgentSink: Send + Sync + 'static {
    /// Whether a model turn is in flight right now.
    fn busy(&self) -> bool;

    /// Accept one message and say where it landed. The same `message_id` must
    /// be accepted once.
    fn deliver_agent_message(&self, envelope: AgentMessageEnvelope) -> AgentMessageResult;
}

#[derive(Default)]
struct InboxState {
    busy: bool,
    blocked: bool,
    pending: VecDeque<AgentMessageEnvelope>,
    seen_order: VecDeque<String>,
    seen: HashSet<String>,
}

/// The terminal half of the endpoint: messages accepted, waiting for the event
/// loop to move them into a turn.
///
/// The listener thread never touches the engine. It records the message and
/// answers with the delivery the state at acceptance implies; the event loop
/// drains [`Self::take_pending`] on its next pass.
pub struct ForegroundAgentInbox {
    state: Mutex<InboxState>,
    /// The session's reply ledger, when this inbox belongs to a session.
    ///
    /// Waking an idle terminal from a cross-session message is what the hop and
    /// wake bounds are for; an inbox built without one (a test, a surface with
    /// no session) has no bounds to apply and admits every message.
    reply_ledger: Mutex<Option<Arc<rebon_tool::peer::AgentReplyLedger>>>,
}

impl Default for ForegroundAgentInbox {
    fn default() -> Self {
        Self::new()
    }
}

impl ForegroundAgentInbox {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(InboxState::default()),
            reply_ledger: Mutex::new(None),
        }
    }

    /// The inbox for one session, bounded by that session's ledger.
    pub fn for_session(session_id: &str) -> Self {
        Self {
            state: Mutex::new(InboxState::default()),
            reply_ledger: Mutex::new(Some(rebon_tool::peer::session_reply_ledger(session_id))),
        }
    }

    /// Point the bounds at another session's ledger — a session switch keeps one
    /// inbox for the process but must not attribute the new session's depth to
    /// the old one's.
    pub fn set_session(&self, session_id: &str) {
        *self
            .reply_ledger
            .lock()
            .expect("foreground agent inbox poisoned") =
            Some(rebon_tool::peer::session_reply_ledger(session_id));
    }

    /// Whether a turn is running, as the event loop last saw it.
    pub fn set_busy(&self, busy: bool) {
        self.state
            .lock()
            .expect("foreground agent inbox poisoned")
            .busy = busy;
    }

    /// Whether the terminal cannot start a turn yet — a resume dialog is open,
    /// or the session has not finished settling.
    pub fn set_blocked(&self, blocked: bool) {
        self.state
            .lock()
            .expect("foreground agent inbox poisoned")
            .blocked = blocked;
    }

    /// Everything accepted since the last call, in arrival order.
    pub fn take_pending(&self) -> Vec<AgentMessageEnvelope> {
        let mut state = self.state.lock().expect("foreground agent inbox poisoned");
        state.pending.drain(..).collect()
    }

    /// Whether this message may start a turn in this session.
    ///
    /// The bounds live in the ledger rather than here because the hosted half
    /// applies the same two to the same session id — see
    /// [`rebon_tool::peer::AgentReplyLedger::admit_wake`].
    fn may_wake(&self, envelope: &AgentMessageEnvelope) -> bool {
        let ledger = self
            .reply_ledger
            .lock()
            .expect("foreground agent inbox poisoned")
            .clone();
        let Some(ledger) = ledger else {
            return true;
        };
        let admission = ledger.admit_wake(
            &envelope.from_session_id,
            envelope.hops,
            crate::now_ms(),
        );
        if !admission.is_allowed() {
            tracing::info!(
                from = %envelope.from_session_id,
                hops = envelope.hops,
                ?admission,
                "rebon: a cross-session message will wait instead of starting a turn"
            );
        }
        admission.is_allowed()
    }

    /// Accept one message. A repeated `message_id` changes nothing and answers
    /// with the delivery the current state implies, matching the hosted owner's
    /// idempotency answer.
    pub fn accept(&self, envelope: AgentMessageEnvelope) -> AgentMessageResult {
        let mut state = self.state.lock().expect("foreground agent inbox poisoned");
        let outcome = if state.busy {
            // A person is watching this turn, so the bounds do not apply to it.
            AgentMessageDelivery::Injected
        } else if state.blocked || !state.pending.is_empty() {
            // Blocked, or behind a message the idle trigger has not picked up
            // yet: either way this one will join a turn that is already owed.
            AgentMessageDelivery::Queued
        } else if self.may_wake(&envelope) {
            AgentMessageDelivery::StartedNewTurn
        } else {
            // Too deep a chain, or a peer that keeps starting turns here: the
            // message waits for a person rather than starting another round.
            AgentMessageDelivery::Queued
        };
        if !state.seen.insert(envelope.message_id.clone()) {
            return AgentMessageResult {
                outcome,
                duplicate: true,
            };
        }
        state.seen_order.push_back(envelope.message_id.clone());
        if state.seen_order.len() > SEEN_HISTORY {
            if let Some(expired) = state.seen_order.pop_front() {
                state.seen.remove(&expired);
            }
        }
        state.pending.push_back(envelope);
        drop(state);
        AgentMessageResult {
            outcome,
            duplicate: false,
        }
    }
}

impl ForegroundAgentSink for ForegroundAgentInbox {
    fn busy(&self) -> bool {
        self.state
            .lock()
            .expect("foreground agent inbox poisoned")
            .busy
    }

    fn deliver_agent_message(&self, envelope: AgentMessageEnvelope) -> AgentMessageResult {
        self.accept(envelope)
    }
}

/// A listening agent endpoint, owned for as long as the session lock is.
pub struct ForegroundAgentEndpoint {
    info: ForegroundAgentEndpointInfo,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    token_path: PathBuf,
}

impl ForegroundAgentEndpoint {
    /// Bind a loopback listener, write the token file, and start accepting.
    pub fn start(
        projects_root: &Path,
        cwd: &str,
        session_id: &str,
        sink: Arc<dyn ForegroundAgentSink>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let token = crate::generate_ipc_token()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let token_path = write_foreground_agent_token(projects_root, cwd, session_id, &token)?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = {
            let shutdown = Arc::clone(&shutdown);
            let token = token.clone();
            let session_id = session_id.to_string();
            std::thread::Builder::new()
                .name("rebon-foreground-agent-endpoint".to_string())
                .spawn(move || accept_loop(listener, token, session_id, sink, shutdown))
        };
        match thread {
            Ok(thread) => Ok(Self {
                info: ForegroundAgentEndpointInfo {
                    port,
                    token_path: token_path.to_string_lossy().into_owned(),
                },
                shutdown,
                thread: Some(thread),
                token_path,
            }),
            Err(error) => {
                let _ = std::fs::remove_file(&token_path);
                Err(error)
            }
        }
    }

    /// What `.live.json` publishes.
    pub fn info(&self) -> ForegroundAgentEndpointInfo {
        self.info.clone()
    }
}

impl Drop for ForegroundAgentEndpoint {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // The accept loop is blocked in `accept`. A loopback connection is how
        // it is woken; the request on it is never read.
        let address = format!("127.0.0.1:{}", self.info.port);
        let wake = address.parse().ok().and_then(|address| {
            TcpStream::connect_timeout(&address, Duration::from_millis(200)).ok()
        });
        drop(wake);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // After the listener is gone, nothing can authenticate with this token.
        let _ = std::fs::remove_file(&self.token_path);
    }
}

fn accept_loop(
    listener: TcpListener,
    token: String,
    session_id: String,
    sink: Arc<dyn ForegroundAgentSink>,
    shutdown: Arc<AtomicBool>,
) {
    for stream in listener.incoming() {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = stream else {
            continue;
        };
        let token = token.clone();
        let session_id = session_id.clone();
        let sink = Arc::clone(&sink);
        // One thread per connection so a client that connects and says
        // nothing cannot hold the accept loop, which would make every other
        // send wait out its timeout.
        let _ = std::thread::Builder::new()
            .name("rebon-foreground-agent-request".to_string())
            .spawn(move || {
                let _ = handle_connection(stream, &token, &session_id, sink.as_ref());
            });
    }
}

fn handle_connection(
    mut stream: TcpStream,
    token: &str,
    session_id: &str,
    sink: &dyn ForegroundAgentSink,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(CONNECTION_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECTION_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    let read = reader
        .by_ref()
        .take(MAX_REQUEST_BYTES)
        .read_line(&mut line)?;
    if read == 0 || line.trim().is_empty() {
        return Ok(());
    }

    let value: Value = match serde_json::from_str(line.trim_end()) {
        Ok(value) => value,
        Err(error) => {
            return write_legacy(
                &mut stream,
                &BackgroundIpcResponse::failed(format!("could not decode the request: {error}")),
            )
        }
    };

    if value.get("jsonrpc").is_some() {
        return handle_acp_request(&mut stream, value, token, sink);
    }
    handle_legacy_request(&mut stream, value, token, session_id, sink)
}

fn handle_legacy_request(
    stream: &mut TcpStream,
    value: Value,
    token: &str,
    session_id: &str,
    sink: &dyn ForegroundAgentSink,
) -> std::io::Result<()> {
    let envelope: BackgroundIpcEnvelope = match serde_json::from_value(value) {
        Ok(envelope) => envelope,
        Err(error) => {
            return write_legacy(
                stream,
                &BackgroundIpcResponse::failed(format!("could not decode the request: {error}")),
            )
        }
    };
    if !rebon_types::constant_time_eq(&envelope.token, token) {
        return write_legacy(
            stream,
            &BackgroundIpcResponse::failed("the session owner did not accept this token"),
        );
    }
    if envelope
        .session_id
        .as_deref()
        .is_some_and(|addressed| addressed != session_id)
    {
        return write_legacy(
            stream,
            &BackgroundIpcResponse::failed(format!(
                "this agent endpoint belongs to session `{session_id}`, not `{}`",
                envelope.session_id.unwrap_or_default()
            )),
        );
    }
    if let Some(job_id) = envelope.job_id.as_deref() {
        return write_legacy(
            stream,
            &BackgroundIpcResponse::failed(format!(
                "a foreground session has no job record, so `{job_id}` cannot address it"
            )),
        );
    }
    let response = match envelope.request {
        BackgroundIpcRequest::Ping => BackgroundIpcResponse::ok(),
        BackgroundIpcRequest::Status => BackgroundIpcResponse::with_data(&serde_json::json!({
            "busy": sink.busy(),
        })),
        BackgroundIpcRequest::AgentMessage {
            message_id,
            from_session_id,
            from_agent,
            from_cwd,
            from_handle,
            from_name,
            summary,
            text,
            hops,
        } => BackgroundIpcResponse::with_data(&sink.deliver_agent_message(AgentMessageEnvelope {
            message_id,
            from_session_id,
            from_agent,
            from_cwd,
            from_handle,
            from_name,
            summary,
            text,
            hops,
        })),
        _ => BackgroundIpcResponse::failed(NOT_SUPPORTED),
    };
    write_legacy(stream, &response)
}

fn handle_acp_request(
    stream: &mut TcpStream,
    value: Value,
    token: &str,
    sink: &dyn ForegroundAgentSink,
) -> std::io::Result<()> {
    let request: JsonRpcRequest = match serde_json::from_value(value) {
        Ok(request) => request,
        Err(error) => {
            return write_json_rpc(
                stream,
                &JsonRpcResponse {
                    jsonrpc: rebon_proto::types::JsonRpcVersion,
                    id: None,
                    result: None,
                    error: Some(JsonRpcError::parse_error(error.to_string())),
                },
            )
        }
    };

    // The probe that decides whether an owner speaks ACP asks this exact
    // question. Answering it with JSON-RPC would classify this endpoint as an
    // ACP owner and send every later request down a protocol it does not hold
    // a connection open for. The legacy shape is the answer a pre-ACP worker
    // gives, so the probe reads it as legacy and falls back to the envelope.
    if request.method == "initialize" {
        return write_legacy(
            stream,
            &BackgroundIpcResponse::failed(
                "a foreground session's agent endpoint does not speak ACP; use the legacy envelope",
            ),
        );
    }

    let params = request.params.clone().unwrap_or(Value::Null);
    let meta = RebonMeta::from_meta(params.get("_meta"));
    let presented = meta.token.as_deref().unwrap_or_default();
    if !rebon_types::constant_time_eq(presented, token) {
        return write_json_rpc(
            stream,
            &JsonRpcResponse {
                jsonrpc: rebon_proto::types::JsonRpcVersion,
                id: Some(request.id),
                result: None,
                error: Some(JsonRpcError {
                    code: rebon_proto::types::error_code::UNAUTHENTICATED,
                    message: "the session owner did not accept this token".to_string(),
                    data: None,
                }),
            },
        );
    }

    let outcome = match request.method.as_str() {
        session_ext::method::PING => Ok(serde_json::json!({})),
        session_ext::method::STATUS => Ok(serde_json::json!({ "busy": sink.busy() })),
        session_ext::method::AGENT_MESSAGE => serde_json::from_value::<AgentMessageParams>(params)
            .map_err(|error| JsonRpcError::invalid_params(error.to_string()))
            .map(|params| {
                serde_json::to_value(sink.deliver_agent_message(AgentMessageEnvelope {
                    message_id: params.message_id,
                    from_session_id: params.from_session_id,
                    from_agent: params.from_agent,
                    from_cwd: params.from_cwd,
                    from_handle: params.from_handle,
                    from_name: params.from_name,
                    summary: params.summary,
                    text: params.text,
                    hops: params.hops,
                }))
                .unwrap_or(Value::Null)
            }),
        other => Err(JsonRpcError {
            code: rebon_proto::types::error_code::METHOD_NOT_FOUND,
            message: format!("`{other}` is {NOT_SUPPORTED}"),
            data: None,
        }),
    };
    let response = match outcome {
        Ok(result) => JsonRpcResponse {
            jsonrpc: rebon_proto::types::JsonRpcVersion,
            id: Some(request.id),
            result: Some(result),
            error: None,
        },
        Err(error) => JsonRpcResponse {
            jsonrpc: rebon_proto::types::JsonRpcVersion,
            id: Some(request.id),
            result: None,
            error: Some(error),
        },
    };
    write_json_rpc(stream, &response)
}

fn write_legacy(stream: &mut TcpStream, response: &BackgroundIpcResponse) -> std::io::Result<()> {
    let body = serde_json::to_vec(response)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    write_line(stream, &body)
}

fn write_json_rpc(stream: &mut TcpStream, response: &JsonRpcResponse) -> std::io::Result<()> {
    let body = serde_json::to_vec(response)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    write_line(stream, &body)
}

fn write_line(stream: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    stream.write_all(body)?;
    stream.write_all(b"\n")?;
    stream.flush()
}

impl ForegroundCommand {
    /// The envelope this command carries, when it is an agent message.
    pub fn into_agent_message(self) -> Option<AgentMessageEnvelope> {
        match self {
            ForegroundCommand::AgentMessage {
                message_id,
                from_session_id,
                from_agent,
                from_cwd,
                from_handle,
                from_name,
                summary,
                text,
                hops,
            } => Some(AgentMessageEnvelope {
                message_id,
                from_session_id,
                from_agent,
                from_cwd,
                from_handle,
                from_name,
                summary,
                text,
                hops,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use super::*;
    use crate::legacy_foreground::{foreground_agent_token_path, read_foreground_status};
    use crate::protocol_probe::{probe, OwnerProtocol};
    use crate::BACKGROUND_IPC_PROTOCOL_VERSION;
    use tempfile::TempDir;

    #[derive(Default)]
    struct RecordingSink {
        busy: AtomicBool,
        inbox: ForegroundAgentInbox,
        delivered: Mutex<Vec<AgentMessageEnvelope>>,
        deliveries: AtomicUsize,
    }

    impl ForegroundAgentSink for RecordingSink {
        fn busy(&self) -> bool {
            self.busy.load(Ordering::SeqCst)
        }

        fn deliver_agent_message(&self, envelope: AgentMessageEnvelope) -> AgentMessageResult {
            let result = self.inbox.accept(envelope.clone());
            if !result.duplicate {
                self.deliveries.fetch_add(1, Ordering::SeqCst);
                self.delivered
                    .lock()
                    .expect("recording sink poisoned")
                    .push(envelope);
            }
            result
        }
    }

    fn start(dir: &TempDir) -> (ForegroundAgentEndpoint, Arc<RecordingSink>, String) {
        let sink = Arc::new(RecordingSink::default());
        let endpoint =
            ForegroundAgentEndpoint::start(dir.path(), "/work/proj", "sess-agent", sink.clone())
                .expect("the endpoint binds");
        let token = std::fs::read_to_string(&endpoint.info().token_path)
            .expect("the token file is written");
        (endpoint, sink, token)
    }

    fn envelope(
        token: &str,
        session_id: &str,
        request: BackgroundIpcRequest,
    ) -> BackgroundIpcEnvelope {
        BackgroundIpcEnvelope {
            protocol_version: BACKGROUND_IPC_PROTOCOL_VERSION,
            job_id: None,
            session_id: Some(session_id.to_string()),
            command_id: None,
            token: token.to_string(),
            request,
        }
    }

    fn exchange(port: u16, body: Value) -> Value {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let mut bytes = serde_json::to_vec(&body).expect("serialize");
        bytes.push(b'\n');
        stream.write_all(&bytes).expect("write");
        let mut reply = String::new();
        BufReader::new(stream)
            .read_line(&mut reply)
            .expect("read the answer");
        serde_json::from_str(reply.trim_end()).expect("the answer is JSON")
    }

    fn agent_request(message_id: &str) -> BackgroundIpcRequest {
        BackgroundIpcRequest::AgentMessage {
            message_id: message_id.to_string(),
            from_session_id: "sess-sender".to_string(),
            from_agent: "main".to_string(),
            from_cwd: None,
            from_handle: None,
            from_name: None,
            summary: Some("greeting".to_string()),
            text: "hello".to_string(),
            hops: 0,
        }
    }

    #[test]
    fn accepts_ping_status_and_agent_message_and_refuses_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, sink, token) = start(&dir);
        let port = endpoint.info().port;

        let ping = exchange(
            port,
            serde_json::to_value(envelope(&token, "sess-agent", BackgroundIpcRequest::Ping))
                .unwrap(),
        );
        assert_eq!(ping["ok"], true);

        let status = exchange(
            port,
            serde_json::to_value(envelope(&token, "sess-agent", BackgroundIpcRequest::Status))
                .unwrap(),
        );
        assert_eq!(status["ok"], true);
        assert_eq!(status["data"]["busy"], false);

        let message = exchange(
            port,
            serde_json::to_value(envelope(&token, "sess-agent", agent_request("msg-1"))).unwrap(),
        );
        assert_eq!(message["ok"], true);
        assert_eq!(message["data"]["outcome"], "startedNewTurn");
        assert!(
            message["data"].get("duplicate").is_none(),
            "a fresh delivery has no duplicate flag: {message}"
        );
        assert_eq!(sink.deliveries.load(Ordering::SeqCst), 1);
        assert_eq!(sink.delivered.lock().unwrap()[0].message_id, "msg-1");

        let refused = exchange(
            port,
            serde_json::to_value(envelope(
                &token,
                "sess-agent",
                BackgroundIpcRequest::Reply {
                    message: "typed".to_string(),
                    images: Vec::new(),
                },
            ))
            .unwrap(),
        );
        assert_eq!(refused["ok"], false);
        assert!(
            refused["error"]
                .as_str()
                .unwrap_or_default()
                .contains("not supported by a foreground session's agent endpoint"),
            "{refused}"
        );
    }

    #[test]
    fn status_reports_a_running_turn_as_busy() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, sink, token) = start(&dir);
        sink.busy.store(true, Ordering::SeqCst);

        let status = exchange(
            endpoint.info().port,
            serde_json::to_value(envelope(&token, "sess-agent", BackgroundIpcRequest::Status))
                .unwrap(),
        );
        assert_eq!(status["data"]["busy"], true);
    }

    #[test]
    fn a_wrong_token_is_refused_before_the_request_runs() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, sink, _token) = start(&dir);

        let refused = exchange(
            endpoint.info().port,
            serde_json::to_value(envelope(
                "not-the-token",
                "sess-agent",
                agent_request("msg-1"),
            ))
            .unwrap(),
        );
        assert_eq!(refused["ok"], false);
        assert!(refused["error"]
            .as_str()
            .unwrap_or_default()
            .contains("did not accept this token"));
        assert_eq!(sink.deliveries.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_request_for_another_session_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, sink, token) = start(&dir);

        let refused = exchange(
            endpoint.info().port,
            serde_json::to_value(envelope(&token, "sess-other", agent_request("msg-1"))).unwrap(),
        );
        assert_eq!(refused["ok"], false);
        assert!(refused["error"]
            .as_str()
            .unwrap_or_default()
            .contains("sess-agent"));
        assert_eq!(sink.deliveries.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_repeated_message_id_is_answered_once_and_delivered_once() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, sink, token) = start(&dir);

        for _ in 0..2 {
            let reply = exchange(
                endpoint.info().port,
                serde_json::to_value(envelope(&token, "sess-agent", agent_request("msg-once")))
                    .unwrap(),
            );
            assert_eq!(reply["ok"], true);
        }
        assert_eq!(sink.deliveries.load(Ordering::SeqCst), 1);
        let duplicate = exchange(
            endpoint.info().port,
            serde_json::to_value(envelope(&token, "sess-agent", agent_request("msg-once")))
                .unwrap(),
        );
        assert_eq!(duplicate["data"]["duplicate"], true);
    }

    #[test]
    fn the_acp_spelling_is_accepted_while_initialize_still_reads_as_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, sink, token) = start(&dir);
        let port = endpoint.info().port;

        // The probe decides which protocol an owner speaks from this answer.
        // An endpoint that answered in JSON-RPC here would send every client
        // down an ACP connection it does not serve.
        assert_eq!(
            probe(port, &token, Duration::from_secs(1)),
            OwnerProtocol::Legacy
        );

        let reply = exchange(
            port,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": session_ext::method::AGENT_MESSAGE,
                "params": {
                    "messageId": "msg-acp",
                    "fromSessionId": "sess-sender",
                    "fromAgent": "main",
                    "text": "hello over acp",
                    "_meta": { "rebon": { "token": token } },
                },
            }),
        );
        assert_eq!(reply["result"]["outcome"], "startedNewTurn");
        assert_eq!(sink.deliveries.load(Ordering::SeqCst), 1);

        let refused = exchange(
            port,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 8,
                "method": "session/prompt",
                "params": { "_meta": { "rebon": { "token": token } } },
            }),
        );
        assert!(
            refused["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("not supported by a foreground session's agent endpoint"),
            "{refused}"
        );

        let unauthenticated = exchange(
            port,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 9,
                "method": session_ext::method::AGENT_MESSAGE,
                "params": { "messageId": "msg-bad", "fromSessionId": "x", "fromAgent": "y", "text": "z" },
            }),
        );
        assert_eq!(
            unauthenticated["error"]["code"],
            rebon_proto::types::error_code::UNAUTHENTICATED
        );
    }

    #[test]
    fn dropping_the_endpoint_removes_the_token_and_closes_the_port() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, _sink, _token) = start(&dir);
        let port = endpoint.info().port;
        let token_path = std::path::PathBuf::from(endpoint.info().token_path.clone());
        assert!(token_path.exists());
        assert_eq!(
            token_path,
            foreground_agent_token_path(dir.path(), "/work/proj", "sess-agent")
        );

        drop(endpoint);

        assert!(!token_path.exists());
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
        // A status file published before the endpoint started would be cleared
        // by the mailbox's own drop; this endpoint only owns its token.
        assert!(read_foreground_status(dir.path(), "/work/proj", "sess-agent").is_none());
    }

    #[test]
    fn the_inbox_queues_until_the_event_loop_drains_it() {
        let inbox = ForegroundAgentInbox::new();
        let accepted = inbox.accept(AgentMessageEnvelope {
            message_id: "m1".to_string(),
            from_session_id: "s".to_string(),
            from_agent: "a".to_string(),
            from_cwd: None,
            from_handle: None,
            from_name: None,
            summary: None,
            text: "hello".to_string(),
            hops: 0,
        });
        assert_eq!(accepted.outcome, AgentMessageDelivery::StartedNewTurn);
        assert!(!accepted.duplicate);
        assert_eq!(inbox.take_pending().len(), 1);
        assert!(inbox.take_pending().is_empty());

        let repeated = inbox.accept(AgentMessageEnvelope {
            message_id: "m1".to_string(),
            from_session_id: "s".to_string(),
            from_agent: "a".to_string(),
            from_cwd: None,
            from_handle: None,
            from_name: None,
            summary: None,
            text: "hello".to_string(),
            hops: 0,
        });
        assert!(repeated.duplicate);
        assert!(
            inbox.take_pending().is_empty(),
            "a retry must not queue a second copy, even after the first was drained"
        );

        inbox.set_busy(true);
        let accepted = inbox.accept(AgentMessageEnvelope {
            message_id: "m2".to_string(),
            from_session_id: "s".to_string(),
            from_agent: "a".to_string(),
            from_cwd: None,
            from_handle: None,
            from_name: None,
            summary: None,
            text: "hello".to_string(),
            hops: 0,
        });
        assert_eq!(accepted.outcome, AgentMessageDelivery::Injected);

        inbox.set_busy(false);
        inbox.set_blocked(true);
        let accepted = inbox.accept(AgentMessageEnvelope {
            message_id: "m3".to_string(),
            from_session_id: "s".to_string(),
            from_agent: "a".to_string(),
            from_cwd: None,
            from_handle: None,
            from_name: None,
            summary: None,
            text: "hello".to_string(),
            hops: 0,
        });
        assert_eq!(accepted.outcome, AgentMessageDelivery::Queued);
    }
}
