//! One ACP server, many browser clients.
//!
//! The ACP server loop speaks to exactly one peer: it auto-detects the
//! peer's framing, refuses a second `initialize`, and answers a
//! `session/request_permission` by whoever holds the other end of the pipe.
//! A web page is not one peer — it is every tab the user has open, each of
//! which comes and goes — so the server's one peer is this multiplexer, and
//! the tabs are its clients.
//!
//! What it does with each direction:
//!
//! * A client **request** gets a server-side id of its own; the response is
//!   routed back to the client that asked, under the id it used. A client
//!   that disconnects mid-request loses the response and nothing else — the
//!   turn it started runs on, and its updates reach every other tab.
//! * `initialize` is answered by the mux itself: it initializes the server
//!   once, at start, and hands every client the same result. The server
//!   never sees a second one.
//! * Client **notifications** (`session/cancel`) pass through unchanged.
//! * Server **notifications** (`session/update`) are broadcast to every
//!   client. A tab filters by session id.
//! * Server **requests** (`session/request_permission`) are broadcast too,
//!   and the first answer wins: it is forwarded, later answers to the same
//!   id are dropped. A request nobody has answered is held, and a client
//!   that connects later is handed the backlog first — a permission prompt
//!   survives a reload.
//! * A **turn** is visible to every tab, not only the one that started it.
//!   ACP tells the prompting client when its turn ends (the response to
//!   `session/prompt`) and nobody else; a tab opened mid-turn, or reloaded,
//!   would otherwise never learn the session is busy or when it stops. The
//!   mux watches `session/prompt` requests and their responses and emits a
//!   `_serve/turn` notification (`running` / `idle`, with the stop reason
//!   or error) to every client, replays the running set to a client that
//!   connects, and keeps a per-session token ledger from the `token_usage`
//!   updates that pass through — the only place usage exists on this
//!   surface, since transcripts do not record it.
//!
//! Since RFC-0004 stage 4 the ACP server behind the mux is no longer where
//! a page's session lives: a [`HostedRouter`] takes the methods that belong
//! to the session's owning worker and answers them itself, and the same
//! router feeds turn state, updates and permission prompts in from the
//! owner's event stream ([`AcpMux::deliver_notification`],
//! [`AcpMux::note_turn`], [`AcpMux::issue_server_request`]). What the mux
//! does with a message is unchanged either way — it is the fan-out, not the
//! host.
//!
//! The mux is a state machine over JSON values; the wire tasks are thin.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use rebon_proto::transport::{StdioReader, StdioWriter};

pub type ClientId = u64;

/// The server-side id of the mux's own `initialize`. Client requests are
/// numbered from 1, so no client request can be mistaken for it.
const INITIALIZE_ID: i64 = 0;

/// What the mux does with a response the ACP server sent to a request the
/// mux made on its own account.
pub type ServerContinuation = Box<dyn FnOnce(Value) + Send>;

/// Whoever answers for sessions this process does not host.
///
/// The mux asks before every client message: a request or notification the
/// router takes is never forwarded to the ACP server, and a response to a
/// server request the router issued goes back to the router rather than out
/// the pipe. Implemented by [`super::hosted::HostedSessions`]; absent in the
/// mux's own tests, where the ACP server is the only peer.
pub trait HostedRouter: Send + Sync {
    /// Take a client request. `true` means the router owns the answer.
    fn take_request(&self, client: ClientId, id: &Value, method: &str, params: &Value) -> bool;
    /// Take a client notification (`session/cancel`).
    fn take_notification(&self, method: &str, params: &Value) -> bool;
    /// Whether `id` names a server request this router issued.
    fn owns_server_request(&self, id: &Value) -> bool;
    /// The winning answer to one of those requests.
    fn answer_server_request(&self, id: &Value, response: &Value);
    /// A client went away; drop whatever it was holding open.
    fn client_gone(&self, client: ClientId);
}

#[derive(Clone)]
pub struct AcpMux {
    state: Arc<Mutex<MuxState>>,
    to_server: mpsc::UnboundedSender<Value>,
    hosted: Arc<OnceLock<Arc<dyn HostedRouter>>>,
}

struct MuxState {
    next_client: ClientId,
    clients: HashMap<ClientId, mpsc::UnboundedSender<String>>,
    next_server_id: i64,
    /// Client requests in flight: server id → (client, the id it used).
    outstanding: HashMap<i64, (ClientId, Value)>,
    /// `session/prompt` requests in flight: server id → session id.
    prompt_turns: HashMap<i64, String>,
    /// Requests the mux made to the server for itself, and what to do with
    /// each answer. Used by the hosted router, which forwards `session/new`
    /// and `session/load` for the metadata the server owns (config options,
    /// the slash-command menu) before it starts the session's worker.
    server_continuations: HashMap<i64, ServerContinuation>,
    /// Sessions with at least one prompt in flight, with the count.
    running: HashMap<String, usize>,
    /// Token usage per session.
    usage: HashMap<String, SessionUsage>,
    /// Server requests nobody has answered yet, in arrival order.
    pending_server_requests: Vec<Value>,
    initialize_result: Option<Value>,
    /// Clients that asked to initialize before the server had answered.
    waiting_initialize: Vec<(ClientId, Value)>,
}

/// What the ledger knows about one session's tokens: the totals of the
/// turns that have ended, and the latest snapshot of the one running.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub turns: u64,
    pub current_input: u64,
    pub current_output: u64,
}

impl SessionUsage {
    fn fold_current_turn(&mut self) {
        self.input_tokens += self.current_input;
        self.output_tokens += self.current_output;
        self.current_input = 0;
        self.current_output = 0;
        self.turns += 1;
    }
}

/// The notification every client gets when a session's turn state changes.
fn turn_notification(session_id: &str, running: bool, response: Option<&Value>) -> Value {
    let stop_reason = response
        .and_then(|response| response.get("result"))
        .and_then(|result| result.get("stopReason"))
        .cloned();
    let error = response
        .and_then(|response| response.get("error"))
        .map(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("turn failed")
                .to_string()
        });
    turn_notification_parts(session_id, running, stop_reason, error)
}

/// The same notification from the parts an event stream reports, rather than
/// from an ACP response.
fn turn_notification_parts(
    session_id: &str,
    running: bool,
    stop_reason: Option<Value>,
    error: Option<String>,
) -> Value {
    let mut params =
        json!({ "sessionId": session_id, "state": if running { "running" } else { "idle" } });
    if let Some(reason) = stop_reason {
        params["stopReason"] = reason;
    }
    if let Some(error) = error {
        params["error"] = json!(error);
    }
    json!({ "jsonrpc": "2.0", "method": "_serve/turn", "params": params })
}

impl AcpMux {
    /// A mux whose server side is `to_server`; server messages are fed in
    /// through [`Self::on_server_message`]. This is the state machine on its
    /// own, which is what the tests drive.
    pub fn new(to_server: mpsc::UnboundedSender<Value>) -> Self {
        let mux = Self {
            state: Arc::new(Mutex::new(MuxState {
                next_client: 1,
                clients: HashMap::new(),
                next_server_id: 1,
                outstanding: HashMap::new(),
                prompt_turns: HashMap::new(),
                server_continuations: HashMap::new(),
                running: HashMap::new(),
                usage: HashMap::new(),
                pending_server_requests: Vec::new(),
                initialize_result: None,
                waiting_initialize: Vec::new(),
            })),
            to_server,
            hosted: Arc::new(OnceLock::new()),
        };
        mux.send_to_server(json!({
            "jsonrpc": "2.0",
            "id": INITIALIZE_ID,
            "method": "initialize",
            "params": {
                "protocolVersion": rebon_acp::ACP_PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false }
                },
                "clientInfo": { "name": "rebon-serve", "version": env!("CARGO_PKG_VERSION") }
            }
        }));
        mux
    }

    /// Attach the wire tasks to an ACP server reachable through `reader` /
    /// `writer` (NDJSON framing; the server matches it).
    pub fn start<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (to_server, mut from_mux) = mpsc::unbounded_channel::<Value>();
        let mux = Self::new(to_server);

        tokio::spawn(async move {
            let mut writer = StdioWriter::new(writer);
            while let Some(message) = from_mux.recv().await {
                let bytes = match serde_json::to_vec(&message) {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        tracing::warn!(error = %err, "rebon serve: unserializable message to server");
                        continue;
                    }
                };
                if let Err(err) = writer.write_ndjson(&bytes).await {
                    tracing::warn!(error = %err, "rebon serve: ACP server pipe closed");
                    break;
                }
            }
        });

        let reader_mux = mux.clone();
        tokio::spawn(async move {
            let mut reader = StdioReader::new(reader);
            loop {
                match reader.read_message().await {
                    Ok(Some(body)) => match serde_json::from_slice::<Value>(&body) {
                        Ok(value) => reader_mux.on_server_message(value),
                        Err(err) => {
                            tracing::warn!(error = %err, "rebon serve: ACP server sent invalid JSON");
                        }
                    },
                    Ok(None) => {
                        tracing::info!("rebon serve: ACP server closed its side");
                        break;
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "rebon serve: ACP server read failed");
                        break;
                    }
                }
            }
        });

        mux
    }

    /// Put a hosted-session router in front of the ACP server. Once, at
    /// start-up: the router and the mux hold each other, so the mux is built
    /// first and told about the router after.
    pub fn attach_hosted(&self, router: Arc<dyn HostedRouter>) {
        let _ = self.hosted.set(router);
    }

    fn hosted(&self) -> Option<Arc<dyn HostedRouter>> {
        self.hosted.get().cloned()
    }

    /// The server's `initialize` result, once it has answered.
    pub fn initialize_result(&self) -> Option<Value> {
        self.lock().initialize_result.clone()
    }

    /// Ask the ACP server something on the mux's own account, and do
    /// `on_response` with the answer. The client never sees this id.
    pub fn request_server(&self, mut value: Value, on_response: ServerContinuation) {
        let server_id = {
            let mut state = self.lock();
            let server_id = state.next_server_id;
            state.next_server_id += 1;
            state.server_continuations.insert(server_id, on_response);
            server_id
        };
        value["id"] = json!(server_id);
        self.send_to_server(value);
    }

    /// Answer a client request the mux (or the hosted router) took itself.
    pub fn answer_client(&self, client: ClientId, id: Value, result: Value) {
        self.send_to_client(
            client,
            &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        );
    }

    /// Hand a client a response the ACP server produced for a request the
    /// mux made on its behalf, re-labelled with the id the client used.
    pub fn send_raw_to_client(&self, client: ClientId, mut response: Value, id: Value) {
        response["id"] = id;
        self.send_to_client(client, &response);
    }

    /// Replace a session's ledger totals with the owner's own count. The
    /// owner is the only process that sees every turn's usage, including the
    /// ones that ran before this server started.
    ///
    /// The running turn's snapshot is cleared with it: the owner's total
    /// already covers every turn it has finished, and the `token_usage`
    /// updates still arriving for the turn in flight refill it — so the two
    /// halves are never counted twice.
    pub fn set_usage(&self, session_id: &str, input_tokens: u64, output_tokens: u64) {
        let mut state = self.lock();
        let usage = state.usage.entry(session_id.to_string()).or_default();
        usage.input_tokens = input_tokens;
        usage.output_tokens = output_tokens;
        usage.current_input = 0;
        usage.current_output = 0;
    }

    /// Refuse one, in the shape a JSON-RPC client expects.
    pub fn fail_client(
        &self,
        client: ClientId,
        id: Value,
        code: i64,
        message: impl Into<String>,
        data: Option<Value>,
    ) {
        let mut error = json!({ "code": code, "message": message.into() });
        if let Some(data) = data {
            error["data"] = data;
        }
        self.send_to_client(
            client,
            &json!({ "jsonrpc": "2.0", "id": id, "error": error }),
        );
    }

    /// Put a server request in front of every client, held for whoever
    /// connects next until somebody answers it. What the hosted router does
    /// with a permission prompt the owner published.
    pub fn issue_server_request(&self, value: Value) {
        let clients: Vec<_> = {
            let mut state = self.lock();
            state.pending_server_requests.push(value.clone());
            state.clients.values().cloned().collect()
        };
        let text = value.to_string();
        for client in clients {
            let _ = client.send(text.clone());
        }
    }

    /// Take a pending server request back because it was resolved elsewhere
    /// — another client of the owner answered it, or the turn moved on.
    /// `true` when it was still pending, which is what makes the withdrawal
    /// worth announcing.
    pub fn withdraw_server_request(&self, id: &Value) -> bool {
        let mut state = self.lock();
        let before = state.pending_server_requests.len();
        state
            .pending_server_requests
            .retain(|request| &request["id"] != id);
        state.pending_server_requests.len() != before
    }

    /// Broadcast a notification that did not come from the ACP server —
    /// through the same ledger the server's own updates pass, so a hosted
    /// session's tokens are counted exactly like an in-process one's.
    pub fn deliver_notification(&self, value: Value) {
        self.note_token_usage(&value);
        self.broadcast(&value);
    }

    /// A session's turn state, as its owner reports it.
    ///
    /// The in-process path infers this from `session/prompt` and its
    /// response; an owned session is told. Idempotent, because a stream that
    /// reconnects says "running" again for a turn every client already knows
    /// about.
    pub fn note_turn(
        &self,
        session_id: &str,
        running: bool,
        stop_reason: Option<String>,
        error: Option<String>,
    ) {
        let changed = {
            let mut state = self.lock();
            if running {
                state.running.insert(session_id.to_string(), 1).is_none()
            } else if state.running.remove(session_id).is_some() {
                state
                    .usage
                    .entry(session_id.to_string())
                    .or_default()
                    .fold_current_turn();
                true
            } else {
                false
            }
        };
        if changed {
            self.broadcast(&turn_notification_parts(
                session_id,
                running,
                stop_reason.map(Value::String),
                error,
            ));
        }
    }

    /// Forget a session's ledger and running flag. What the router does when
    /// the last client of a session lets go, so a later reopen does not add
    /// its turns to a total nobody is looking at any more.
    pub fn forget_session(&self, session_id: &str) {
        let mut state = self.lock();
        state.running.remove(session_id);
        state.usage.remove(session_id);
    }

    /// Register a client. Returns its id and the stream of messages to
    /// deliver to it; the sessions with a turn running and the server
    /// requests still waiting for an answer are already queued on it.
    pub fn connect(&self) -> (ClientId, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut state = self.lock();
        let id = state.next_client;
        state.next_client += 1;
        let mut running: Vec<_> = state.running.keys().cloned().collect();
        running.sort();
        for session_id in running {
            let _ = tx.send(turn_notification(&session_id, true, None).to_string());
        }
        for request in &state.pending_server_requests {
            let _ = tx.send(request.to_string());
        }
        state.clients.insert(id, tx);
        (id, rx)
    }

    /// Sessions with a `session/prompt` in flight right now.
    pub fn active_turns(&self) -> Vec<String> {
        let mut sessions: Vec<_> = self.lock().running.keys().cloned().collect();
        sessions.sort();
        sessions
    }

    /// Whether `session_id` has a turn running.
    pub fn is_turn_running(&self, session_id: &str) -> bool {
        self.lock().running.contains_key(session_id)
    }

    /// The token ledger of a session, as far as this server has seen it.
    pub fn usage(&self, session_id: &str) -> SessionUsage {
        self.lock()
            .usage
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn disconnect(&self, client: ClientId) {
        {
            let mut state = self.lock();
            state.clients.remove(&client);
            state
                .waiting_initialize
                .retain(|(waiting, _)| *waiting != client);
            // Requests it had in flight stay outstanding; their responses are
            // dropped on arrival because the client is gone.
        }
        if let Some(hosted) = self.hosted() {
            hosted.client_gone(client);
        }
    }

    pub fn client_count(&self) -> usize {
        self.lock().clients.len()
    }

    /// A message from a client, as received on the wire.
    pub fn on_client_message(&self, client: ClientId, text: &str) {
        let value: Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(err) => {
                self.send_to_client(
                    client,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": { "code": -32700, "message": format!("Parse error: {err}") }
                    }),
                );
                return;
            }
        };
        if !value.is_object() {
            self.send_to_client(
                client,
                &json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32600, "message": "Invalid Request" }
                }),
            );
            return;
        }
        let has_method = value.get("method").is_some();
        let has_id = value.get("id").map(|id| !id.is_null()).unwrap_or(false);
        match (has_method, has_id) {
            (true, true) => self.on_client_request(client, value),
            (true, false) => self.on_client_notification(value),
            (false, true) => self.on_client_response(value),
            (false, false) => self.send_to_client(
                client,
                &json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32600, "message": "Invalid Request" }
                }),
            ),
        }
    }

    fn on_client_notification(&self, value: Value) {
        if let Some(hosted) = self.hosted() {
            let method = value["method"].as_str().unwrap_or_default().to_string();
            if hosted.take_notification(&method, &value["params"]) {
                return;
            }
        }
        self.send_to_server(value);
    }

    fn on_client_request(&self, client: ClientId, mut value: Value) {
        let client_id = value["id"].clone();
        if value["method"] == "initialize" {
            let cached = {
                let mut state = self.lock();
                match state.initialize_result.clone() {
                    Some(result) => Some(result),
                    None => {
                        state.waiting_initialize.push((client, client_id.clone()));
                        None
                    }
                }
            };
            if let Some(result) = cached {
                self.send_to_client(
                    client,
                    &json!({ "jsonrpc": "2.0", "id": client_id, "result": result }),
                );
            }
            return;
        }
        if let Some(hosted) = self.hosted() {
            let method = value["method"].as_str().unwrap_or_default().to_string();
            if hosted.take_request(client, &client_id, &method, &value["params"]) {
                return;
            }
        }
        let prompt_session = (value["method"] == "session/prompt")
            .then(|| value["params"]["sessionId"].as_str().map(str::to_owned))
            .flatten();
        let (server_id, started) = {
            let mut state = self.lock();
            let server_id = state.next_server_id;
            state.next_server_id += 1;
            state.outstanding.insert(server_id, (client, client_id));
            let mut started = None;
            if let Some(session_id) = prompt_session {
                state.prompt_turns.insert(server_id, session_id.clone());
                let count = state.running.entry(session_id.clone()).or_insert(0);
                *count += 1;
                if *count == 1 {
                    started = Some(session_id);
                }
            }
            (server_id, started)
        };
        value["id"] = json!(server_id);
        self.send_to_server(value);
        if let Some(session_id) = started {
            self.broadcast(&turn_notification(&session_id, true, None));
        }
    }

    fn on_client_response(&self, value: Value) {
        let id = value["id"].clone();
        let accepted = {
            let mut state = self.lock();
            let before = state.pending_server_requests.len();
            state
                .pending_server_requests
                .retain(|request| request["id"] != id);
            state.pending_server_requests.len() != before
        };
        if !accepted {
            tracing::debug!(
                ?id,
                "rebon serve: dropping a late or unsolicited client response"
            );
            return;
        }
        // First answer wins either way; who gets it depends on who asked.
        match self.hosted() {
            Some(hosted) if hosted.owns_server_request(&id) => {
                hosted.answer_server_request(&id, &value)
            }
            _ => self.send_to_server(value),
        }
    }

    /// A message from the server.
    pub fn on_server_message(&self, value: Value) {
        let has_method = value.get("method").is_some();
        let has_id = value.get("id").map(|id| !id.is_null()).unwrap_or(false);
        match (has_method, has_id) {
            (true, true) => {
                let clients: Vec<_> = {
                    let mut state = self.lock();
                    state.pending_server_requests.push(value.clone());
                    state.clients.values().cloned().collect()
                };
                let text = value.to_string();
                for client in clients {
                    let _ = client.send(text.clone());
                }
            }
            (true, false) => {
                self.note_token_usage(&value);
                self.broadcast(&value)
            }
            (false, true) => self.on_server_response(value),
            (false, false) => {
                tracing::warn!("rebon serve: ACP server sent a message that is neither request, notification nor response");
            }
        }
    }

    fn on_server_response(&self, mut value: Value) {
        if value["id"] == json!(INITIALIZE_ID) {
            let waiting = {
                let mut state = self.lock();
                match value.get("result") {
                    Some(result) => state.initialize_result = Some(result.clone()),
                    None => {
                        tracing::error!(error = %value["error"], "rebon serve: ACP server refused initialize");
                    }
                }
                std::mem::take(&mut state.waiting_initialize)
            };
            for (client, client_id) in waiting {
                let mut reply = value.clone();
                reply["id"] = client_id;
                self.send_to_client(client, &reply);
            }
            return;
        }
        let Some(server_id) = value["id"].as_i64() else {
            tracing::debug!(id = %value["id"], "rebon serve: response with an id the mux never issued");
            return;
        };
        let continuation = self.lock().server_continuations.remove(&server_id);
        if let Some(continuation) = continuation {
            continuation(value);
            return;
        }
        let (routed, ended) = {
            let mut state = self.lock();
            let routed = state.outstanding.remove(&server_id);
            let mut ended = None;
            if let Some(session_id) = state.prompt_turns.remove(&server_id) {
                let remaining = match state.running.get_mut(&session_id) {
                    Some(count) => {
                        *count = count.saturating_sub(1);
                        *count
                    }
                    None => 0,
                };
                if remaining == 0 {
                    state.running.remove(&session_id);
                    ended = Some(session_id.clone());
                }
                state
                    .usage
                    .entry(session_id)
                    .or_default()
                    .fold_current_turn();
            }
            (routed, ended)
        };
        if let Some(session_id) = ended {
            self.broadcast(&turn_notification(&session_id, false, Some(&value)));
        }
        match routed {
            Some((client, client_id)) => {
                value["id"] = client_id;
                self.send_to_client(client, &value);
            }
            None => {
                tracing::debug!(server_id, "rebon serve: response to an unknown request");
            }
        }
    }

    /// A `token_usage` update is the running turn's latest snapshot; it
    /// is folded into the session's totals when the turn ends.
    fn note_token_usage(&self, value: &Value) {
        if value["method"] != "session/update" {
            return;
        }
        let params = &value["params"];
        if params["update"]["sessionUpdate"] != "token_usage" {
            return;
        }
        let Some(session_id) = params["sessionId"].as_str() else {
            return;
        };
        let update = &params["update"];
        let mut state = self.lock();
        let usage = state.usage.entry(session_id.to_string()).or_default();
        if let Some(input) = update["inputTokens"].as_u64() {
            usage.current_input = input;
        }
        if let Some(output) = update["outputTokens"].as_u64() {
            usage.current_output = output;
        }
    }

    fn broadcast(&self, value: &Value) {
        let clients: Vec<_> = self.lock().clients.values().cloned().collect();
        let text = value.to_string();
        for client in clients {
            let _ = client.send(text.clone());
        }
    }

    fn send_to_client(&self, client: ClientId, value: &Value) {
        let sender = self.lock().clients.get(&client).cloned();
        if let Some(sender) = sender {
            let _ = sender.send(value.to_string());
        }
    }

    fn send_to_server(&self, value: Value) {
        if self.to_server.send(value).is_err() {
            tracing::warn!("rebon serve: ACP server is gone; dropping a client message");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MuxState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mux() -> (AcpMux, mpsc::UnboundedReceiver<Value>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (AcpMux::new(tx), rx)
    }

    fn recv(rx: &mut mpsc::UnboundedReceiver<String>) -> Value {
        serde_json::from_str(&rx.try_recv().expect("a message for the client")).unwrap()
    }

    fn initialized(mux: &AcpMux, to_server: &mut mpsc::UnboundedReceiver<Value>) {
        let init = to_server.try_recv().expect("the mux initializes first");
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["id"], 0);
        mux.on_server_message(json!({
            "jsonrpc": "2.0", "id": 0,
            "result": { "protocolVersion": 1, "agentCapabilities": {} }
        }));
    }

    #[test]
    fn the_mux_initializes_once_and_answers_every_client_itself() {
        let (mux, mut to_server) = mux();
        let (a, mut a_rx) = mux.connect();
        // A client asking before the server answered waits.
        mux.on_client_message(
            a,
            r#"{"jsonrpc":"2.0","id":"init-a","method":"initialize","params":{"protocolVersion":1}}"#,
        );
        assert!(a_rx.try_recv().is_err());
        initialized(&mux, &mut to_server);
        let reply = recv(&mut a_rx);
        assert_eq!(reply["id"], "init-a");
        assert_eq!(reply["result"]["protocolVersion"], 1);

        // A client asking afterwards is answered from the cache; the server
        // never sees a second initialize.
        let (b, mut b_rx) = mux.connect();
        mux.on_client_message(
            b,
            r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"protocolVersion":1}}"#,
        );
        assert_eq!(recv(&mut b_rx)["id"], 7);
        assert!(to_server.try_recv().is_err());
    }

    #[test]
    fn client_requests_are_renumbered_and_routed_back_to_their_client() {
        let (mux, mut to_server) = mux();
        initialized(&mux, &mut to_server);
        let (a, mut a_rx) = mux.connect();
        let (b, mut b_rx) = mux.connect();
        // Both clients use id 1.
        mux.on_client_message(
            a,
            r#"{"jsonrpc":"2.0","id":1,"method":"session/list","params":{}}"#,
        );
        mux.on_client_message(
            b,
            r#"{"jsonrpc":"2.0","id":1,"method":"session/list","params":{}}"#,
        );
        let first = to_server.try_recv().unwrap();
        let second = to_server.try_recv().unwrap();
        assert_ne!(first["id"], second["id"]);
        // Answer the second first: routing is by id, not order.
        mux.on_server_message(json!({"jsonrpc":"2.0","id":second["id"],"result":{"who":"b"}}));
        mux.on_server_message(json!({"jsonrpc":"2.0","id":first["id"],"result":{"who":"a"}}));
        let a_reply = recv(&mut a_rx);
        let b_reply = recv(&mut b_rx);
        assert_eq!(a_reply["id"], 1);
        assert_eq!(a_reply["result"]["who"], "a");
        assert_eq!(b_reply["id"], 1);
        assert_eq!(b_reply["result"]["who"], "b");
    }

    #[test]
    fn notifications_pass_through_in_both_directions() {
        let (mux, mut to_server) = mux();
        initialized(&mux, &mut to_server);
        let (a, mut a_rx) = mux.connect();
        let (_b, mut b_rx) = mux.connect();
        mux.on_client_message(
            a,
            r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s"}}"#,
        );
        assert_eq!(to_server.try_recv().unwrap()["method"], "session/cancel");
        mux.on_server_message(json!({
            "jsonrpc":"2.0","method":"session/update",
            "params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk"}}
        }));
        assert_eq!(recv(&mut a_rx)["method"], "session/update");
        assert_eq!(recv(&mut b_rx)["method"], "session/update");
    }

    #[test]
    fn a_server_request_is_answered_once_and_survives_a_reconnect() {
        let (mux, mut to_server) = mux();
        initialized(&mux, &mut to_server);
        let (a, mut a_rx) = mux.connect();
        let (b, mut b_rx) = mux.connect();
        mux.on_server_message(json!({
            "jsonrpc":"2.0","id":41,"method":"session/request_permission",
            "params":{"sessionId":"s","options":[]}
        }));
        assert_eq!(recv(&mut a_rx)["id"], 41);
        assert_eq!(recv(&mut b_rx)["id"], 41);

        // A tab opened while the prompt is pending gets it too.
        let (c, mut c_rx) = mux.connect();
        assert_eq!(recv(&mut c_rx)["method"], "session/request_permission");

        // First answer wins.
        mux.on_client_message(
            b,
            r#"{"jsonrpc":"2.0","id":41,"result":{"outcome":{"outcome":"selected","optionId":"allow"}}}"#,
        );
        mux.on_client_message(
            a,
            r#"{"jsonrpc":"2.0","id":41,"result":{"outcome":{"outcome":"cancelled"}}}"#,
        );
        mux.on_client_message(
            c,
            r#"{"jsonrpc":"2.0","id":41,"result":{"outcome":{"outcome":"cancelled"}}}"#,
        );
        let forwarded = to_server.try_recv().unwrap();
        assert_eq!(forwarded["result"]["outcome"]["optionId"], "allow");
        assert!(to_server.try_recv().is_err(), "later answers are dropped");

        // Nothing pending is replayed to a new tab now.
        let (_d, mut d_rx) = mux.connect();
        assert!(d_rx.try_recv().is_err());
    }

    #[test]
    fn a_disconnected_client_loses_only_its_own_response() {
        let (mux, mut to_server) = mux();
        initialized(&mux, &mut to_server);
        let (a, a_rx) = mux.connect();
        let (_b, mut b_rx) = mux.connect();
        mux.on_client_message(
            a,
            r#"{"jsonrpc":"2.0","id":9,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}"#,
        );
        let sent = to_server.try_recv().unwrap();
        assert_eq!(recv(&mut b_rx)["method"], "_serve/turn");
        drop(a_rx);
        mux.disconnect(a);
        assert_eq!(mux.client_count(), 1);
        mux.on_server_message(json!({
            "jsonrpc":"2.0","method":"session/update",
            "params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk"}}
        }));
        assert_eq!(recv(&mut b_rx)["method"], "session/update");
        // The response has nowhere to go and is dropped, not misrouted —
        // but the turn's end is still announced.
        mux.on_server_message(
            json!({"jsonrpc":"2.0","id":sent["id"],"result":{"stopReason":"end_turn"}}),
        );
        let idle = recv(&mut b_rx);
        assert_eq!(idle["method"], "_serve/turn");
        assert_eq!(idle["params"]["state"], "idle");
        assert!(b_rx.try_recv().is_err());
    }

    #[test]
    fn a_turn_is_announced_to_every_tab_and_its_usage_is_kept() {
        let (mux, mut to_server) = mux();
        initialized(&mux, &mut to_server);
        let (a, mut a_rx) = mux.connect();
        let (_b, mut b_rx) = mux.connect();
        mux.on_client_message(
            a,
            r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}"#,
        );
        let sent = to_server.try_recv().unwrap();
        // Both tabs learn the session is busy; the prompting tab too.
        let a_turn = recv(&mut a_rx);
        assert_eq!(a_turn["method"], "_serve/turn");
        assert_eq!(a_turn["params"]["state"], "running");
        assert_eq!(recv(&mut b_rx)["params"]["sessionId"], "s");
        assert_eq!(mux.active_turns(), vec!["s".to_string()]);
        assert!(mux.is_turn_running("s"));

        // A tab opened mid-turn is told first thing.
        let (_c, mut c_rx) = mux.connect();
        assert_eq!(recv(&mut c_rx)["params"]["state"], "running");

        // Usage snapshots are the running turn's; the last one counts.
        for (input, output) in [(100, 5), (100, 40)] {
            mux.on_server_message(json!({
                "jsonrpc":"2.0","method":"session/update",
                "params":{"sessionId":"s","update":{"sessionUpdate":"token_usage","inputTokens":input,"outputTokens":output}}
            }));
            recv(&mut a_rx);
            recv(&mut b_rx);
            recv(&mut c_rx);
        }
        assert_eq!(mux.usage("s").current_output, 40);
        assert_eq!(mux.usage("s").turns, 0);

        mux.on_server_message(
            json!({"jsonrpc":"2.0","id":sent["id"],"result":{"stopReason":"end_turn"}}),
        );
        let idle = recv(&mut b_rx);
        assert_eq!(idle["method"], "_serve/turn");
        assert_eq!(idle["params"]["state"], "idle");
        assert_eq!(idle["params"]["stopReason"], "end_turn");
        assert!(mux.active_turns().is_empty());
        let usage = mux.usage("s");
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.turns),
            (100, 40, 1)
        );
        assert_eq!(usage.current_output, 0);
        // The prompting tab gets the idle notice and then its response.
        assert_eq!(recv(&mut a_rx)["method"], "_serve/turn");
        assert_eq!(recv(&mut a_rx)["id"], 3);
    }

    #[test]
    fn a_failed_turn_reports_its_error_when_it_goes_idle() {
        let (mux, mut to_server) = mux();
        initialized(&mux, &mut to_server);
        let (a, mut a_rx) = mux.connect();
        mux.on_client_message(
            a,
            r#"{"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}"#,
        );
        let sent = to_server.try_recv().unwrap();
        recv(&mut a_rx);
        mux.on_server_message(
            json!({"jsonrpc":"2.0","id":sent["id"],"error":{"code":-32000,"message":"model refused"}}),
        );
        let idle = recv(&mut a_rx);
        assert_eq!(idle["params"]["state"], "idle");
        assert_eq!(idle["params"]["error"], "model refused");
        assert!(!mux.is_turn_running("s"));
    }

    /// A router that takes `session/prompt` and nothing else, and remembers
    /// what it was handed. Stands in for the hosted translation layer so the
    /// mux's half of the arrangement can be tested on its own.
    #[derive(Default)]
    struct StubRouter {
        requests: Mutex<Vec<(ClientId, String)>>,
        notifications: Mutex<Vec<String>>,
        answers: Mutex<Vec<(Value, Value)>>,
        gone: Mutex<Vec<ClientId>>,
        owned: Mutex<Vec<Value>>,
    }

    impl HostedRouter for StubRouter {
        fn take_request(
            &self,
            client: ClientId,
            _id: &Value,
            method: &str,
            _params: &Value,
        ) -> bool {
            if method != "session/prompt" {
                return false;
            }
            self.requests
                .lock()
                .unwrap()
                .push((client, method.to_string()));
            true
        }

        fn take_notification(&self, method: &str, _params: &Value) -> bool {
            if method != "session/cancel" {
                return false;
            }
            self.notifications.lock().unwrap().push(method.to_string());
            true
        }

        fn owns_server_request(&self, id: &Value) -> bool {
            self.owned.lock().unwrap().contains(id)
        }

        fn answer_server_request(&self, id: &Value, response: &Value) {
            self.answers
                .lock()
                .unwrap()
                .push((id.clone(), response.clone()));
        }

        fn client_gone(&self, client: ClientId) {
            self.gone.lock().unwrap().push(client);
        }
    }

    fn with_router() -> (AcpMux, mpsc::UnboundedReceiver<Value>, Arc<StubRouter>) {
        let (mux, mut to_server) = mux();
        initialized(&mux, &mut to_server);
        let router = Arc::new(StubRouter::default());
        mux.attach_hosted(router.clone());
        (mux, to_server, router)
    }

    #[test]
    fn what_the_router_takes_never_reaches_the_acp_server() {
        let (mux, mut to_server, router) = with_router();
        let (a, _a_rx) = mux.connect();
        mux.on_client_message(
            a,
            r#"{"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}"#,
        );
        mux.on_client_message(
            a,
            r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s"}}"#,
        );
        assert_eq!(router.requests.lock().unwrap().len(), 1);
        assert_eq!(router.notifications.lock().unwrap().len(), 1);
        assert!(
            to_server.try_recv().is_err(),
            "a hosted session's turn is not this server's to run"
        );
        // And what it declines still is.
        mux.on_client_message(
            a,
            r#"{"jsonrpc":"2.0","id":2,"method":"session/list","params":{}}"#,
        );
        assert_eq!(to_server.try_recv().unwrap()["method"], "session/list");
        assert!(
            !mux.is_turn_running("s"),
            "the stream says that, not the mux"
        );
    }

    #[test]
    fn a_permission_the_router_issued_is_answered_once_and_routed_back_to_it() {
        let (mux, mut to_server, router) = with_router();
        let (a, mut a_rx) = mux.connect();
        let (b, mut b_rx) = mux.connect();
        let id = json!("perm-s-7");
        router.owned.lock().unwrap().push(id.clone());
        mux.issue_server_request(json!({
            "jsonrpc": "2.0", "id": id, "method": "session/request_permission",
            "params": { "sessionId": "s", "options": [] }
        }));
        assert_eq!(recv(&mut a_rx)["id"], id);
        assert_eq!(recv(&mut b_rx)["id"], id);
        // A tab that arrives while it is pending is handed it too.
        let (_c, mut c_rx) = mux.connect();
        assert_eq!(recv(&mut c_rx)["method"], "session/request_permission");

        mux.on_client_message(
            b,
            r#"{"jsonrpc":"2.0","id":"perm-s-7","result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}"#,
        );
        mux.on_client_message(
            a,
            r#"{"jsonrpc":"2.0","id":"perm-s-7","result":{"outcome":{"outcome":"cancelled"}}}"#,
        );
        let answers = router.answers.lock().unwrap();
        assert_eq!(answers.len(), 1, "first answer wins");
        assert_eq!(answers[0].1["result"]["outcome"]["optionId"], "allow_once");
        assert!(
            to_server.try_recv().is_err(),
            "the ACP server never asked, so it is not told"
        );
        // Withdrawing it stops a later tab from being shown it.
        assert!(!mux.withdraw_server_request(&id), "already answered");
        let (_d, mut d_rx) = mux.connect();
        assert!(d_rx.try_recv().is_err());
    }

    #[test]
    fn a_withdrawn_permission_is_no_longer_replayed() {
        let (mux, _to_server, router) = with_router();
        let id = json!("perm-s-1");
        router.owned.lock().unwrap().push(id.clone());
        mux.issue_server_request(
            json!({ "jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {} }),
        );
        assert!(mux.withdraw_server_request(&id));
        let (_a, mut a_rx) = mux.connect();
        assert!(a_rx.try_recv().is_err());
    }

    #[test]
    fn a_turn_the_owner_reports_is_announced_and_folds_its_usage() {
        let (mux, _to_server, _router) = with_router();
        let (_a, mut a_rx) = mux.connect();
        mux.note_turn("s", true, None, None);
        let running = recv(&mut a_rx);
        assert_eq!(running["method"], "_serve/turn");
        assert_eq!(running["params"]["state"], "running");
        assert!(mux.is_turn_running("s"));
        // Saying it twice is not two turns: a stream that reconnects repeats
        // the state every client already has.
        mux.note_turn("s", true, None, None);
        assert!(a_rx.try_recv().is_err());

        mux.on_server_message(json!({
            "jsonrpc":"2.0","method":"session/update",
            "params":{"sessionId":"s","update":{"sessionUpdate":"token_usage","inputTokens":90,"outputTokens":12}}
        }));
        recv(&mut a_rx);
        mux.note_turn("s", false, Some("end_turn".into()), None);
        let idle = recv(&mut a_rx);
        assert_eq!(idle["params"]["state"], "idle");
        assert_eq!(idle["params"]["stopReason"], "end_turn");
        let usage = mux.usage("s");
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.turns),
            (90, 12, 1)
        );

        // The owner's own count replaces the ledger's, and clears the running
        // turn's snapshot with it so the two are never added together.
        mux.set_usage("s", 500, 40);
        let usage = mux.usage("s");
        assert_eq!((usage.input_tokens, usage.current_input), (500, 0));
        mux.forget_session("s");
        assert_eq!(mux.usage("s"), SessionUsage::default());
    }

    #[test]
    fn the_mux_can_ask_the_server_something_a_client_never_sees() {
        let (mux, mut to_server, _router) = with_router();
        let (a, mut a_rx) = mux.connect();
        let answered = Arc::new(Mutex::new(None));
        let sink = answered.clone();
        mux.request_server(
            json!({ "jsonrpc": "2.0", "method": "session/new", "params": { "cwd": "/w" } }),
            Box::new(move |response| *sink.lock().unwrap() = Some(response)),
        );
        let sent = to_server.try_recv().unwrap();
        assert_eq!(sent["method"], "session/new");
        mux.on_server_message(
            json!({ "jsonrpc":"2.0","id":sent["id"],"result":{"sessionId":"sess-1"} }),
        );
        assert_eq!(
            answered.lock().unwrap().as_ref().unwrap()["result"]["sessionId"],
            "sess-1"
        );
        assert!(a_rx.try_recv().is_err(), "no client asked for this");

        // The router answers the client itself, under the client's own id.
        mux.answer_client(a, json!(9), json!({ "sessionId": "sess-1" }));
        let reply = recv(&mut a_rx);
        assert_eq!(reply["id"], 9);
        assert_eq!(reply["result"]["sessionId"], "sess-1");
        mux.fail_client(
            a,
            json!(10),
            -32000,
            "held elsewhere",
            Some(json!({"owner":{"pid":7}})),
        );
        let refusal = recv(&mut a_rx);
        assert_eq!(refusal["error"]["code"], -32000);
        assert_eq!(refusal["error"]["data"]["owner"]["pid"], 7);
    }

    #[test]
    fn a_client_that_leaves_is_reported_to_the_router() {
        let (mux, _to_server, router) = with_router();
        let (a, _a_rx) = mux.connect();
        mux.disconnect(a);
        assert_eq!(*router.gone.lock().unwrap(), vec![a]);
    }

    #[test]
    fn malformed_client_input_gets_a_json_rpc_error_not_a_forward() {
        let (mux, mut to_server) = mux();
        initialized(&mux, &mut to_server);
        let (a, mut a_rx) = mux.connect();
        mux.on_client_message(a, "{not json");
        assert_eq!(recv(&mut a_rx)["error"]["code"], -32700);
        mux.on_client_message(a, "[1,2]");
        assert_eq!(recv(&mut a_rx)["error"]["code"], -32600);
        mux.on_client_message(a, r#"{"jsonrpc":"2.0"}"#);
        assert_eq!(recv(&mut a_rx)["error"]["code"], -32600);
        assert!(to_server.try_recv().is_err());
    }
}
