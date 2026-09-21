//! `StdioMcpClient` — real MCP transport over child-process stdio.
//!
//! The MCP (Model Context Protocol) defines a JSON-RPC 2.0 wire
//! protocol over stdio. A typical configuration file lists one or
//! more servers as `{ command, args, env }` entries; the client is
//! responsible for:
//!
//! 1. Spawning the child process with piped stdin/stdout.
//! 2. Framing messages as NDJSON (one JSON message per line).
//! 3. Sending the `initialize` handshake followed by an
//!    `initialized` notification.
//! 4. Answering inbound requests via JSON-RPC response and pushing
//!    outbound tool calls.
//! 5. Correlating responses to outbound calls via the `id` field.
//! 6. Tearing the child down on drop / `remove_server`.
//!
//! This module implements the caller half of that protocol:
//! [`StdioMcpClient`] manages a set of named servers, each backed
//! by its own spawned child. It implements the `McpClient` trait
//! defined in [`rebon_tool::mcp`] so `McpTool` can dispatch calls to it
//! without knowing the transport.
//!
//! Scope — what this transport ships:
//!
//! - Spawn + initialize + `tools/call` + `tools/list`
//! - Per-server background reader task that pumps NDJSON lines
//!   into the correlator
//! - Response routing via oneshot channels keyed by request id
//! - Timeout on each request
//! - Graceful teardown on [`StdioMcpClient::shutdown`]
//!
//! Deferred:
//!
//! - Inbound `sampling/createMessage` requests (server → client)
//! - Prompts, resources, and roots lists
//! - HTTP + SSE transport (separate module, same trait)
//! - Reconnect / backoff on crash

use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use crate::runtime::{
    ChannelCapabilities, ChannelEntry, ChannelGateContext, ChannelGateResult, SubscriptionType,
};
use async_trait::async_trait;
use rebon_tools_core::ProcessTreeGuard;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

use crate::mcp::parse_mcp_tool_definitions;
use rebon_tool::{
    McpClient, McpClientError, McpShutdownReport, McpToolCall, McpToolDefinition, McpToolResult,
};

/// `CREATE_NO_WINDOW` — prevents a visible console on Windows.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Configuration for one stdio-backed MCP server.
#[derive(Debug, Clone)]
pub struct StdioServerConfig {
    /// Logical name the caller uses to address this server in
    /// [`McpToolCall::server`].
    pub name: String,
    /// Executable path or command name (looked up via PATH).
    pub command: String,
    /// CLI arguments passed to the command.
    pub args: Vec<String>,
    /// Environment variables merged onto the child process.
    pub env: Vec<(String, String)>,
    /// Optional working directory.
    pub cwd: Option<String>,
    /// Per-request timeout. Defaults to 30 seconds when `None`.
    pub request_timeout: Option<Duration>,
}

impl StdioServerConfig {
    /// Minimal constructor.
    pub fn new(name: impl Into<String>, command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args,
            env: Vec::new(),
            cwd: None,
            request_timeout: None,
        }
    }

    fn effective_timeout(&self) -> Duration {
        self.request_timeout
            .unwrap_or_else(|| Duration::from_secs(30))
    }
}

/// Runtime state for one live server connection.
struct StdioServer {
    config: StdioServerConfig,
    stdin: Mutex<ChildStdin>,
    child: Mutex<Option<Child>>,
    /// Kills every descendant of the server process when the server is
    /// torn down (or this handle is dropped). `Child::kill` /
    /// `kill_on_drop` only reach the direct child; MCP servers that
    /// spawn their own helpers (the `rust_lsp` server spawns
    /// rust-analyzer through a rustup proxy) would otherwise leak
    /// multi-gigabyte grandchildren past the session's lifetime.
    process_tree: StdMutex<Option<ProcessTreeGuard>>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<JsonRpcResponse, McpClientError>>>>,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    stderr_handle: Mutex<Option<JoinHandle<()>>>,
    tools_cache: Mutex<Option<Vec<McpToolDefinition>>>,
    channel_registered: AtomicBool,
    channel_queue: Arc<StdMutex<VecDeque<String>>>,
}

/// Guard a spawned server's whole process tree.
///
/// The guard itself lives in `rebon-tools-core`; what stays here is how
/// this caller reaches the child's raw identifier, and what it calls a
/// child that has none.
#[cfg(windows)]
fn guard_process_tree(child: &Child) -> std::io::Result<ProcessTreeGuard> {
    let handle = child
        .raw_handle()
        .ok_or_else(|| std::io::Error::other("child process has no process handle"))?;
    // Breakaway exempts nothing by default — children stay in the job
    // unless they explicitly request it. The rust_lsp bridge relies on
    // that to launch the per-workspace shared rust-analyzer daemon,
    // which must outlive this session's job.
    ProcessTreeGuard::for_raw_handle(handle, true)
}

#[cfg(unix)]
fn guard_process_tree(child: &Child) -> std::io::Result<ProcessTreeGuard> {
    let process_id = child
        .id()
        .ok_or_else(|| std::io::Error::other("child process has no pid"))?;
    ProcessTreeGuard::for_process_group(process_id)
}

/// Make the spawned server its own process-group leader so the whole
/// tree can be signalled at once. Windows needs no spawn-time setup —
/// job membership is inherited automatically.
#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
fn configure_process_group(_command: &mut Command) {}

impl std::fmt::Debug for StdioServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioServer")
            .field("name", &self.config.name)
            .field("command", &self.config.command)
            .finish()
    }
}

/// Minimal JSON-RPC 2.0 response shape used by the correlator.
#[derive(Debug, Clone)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    id: Option<i64>,
    result: Option<Value>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Clone)]
struct JsonRpcError {
    code: i64,
    message: String,
}

/// [`McpClient`] implementation that talks to one or more MCP
/// servers over stdio.
#[derive(Debug, Default, Clone)]
pub struct StdioMcpClient {
    servers: Arc<Mutex<HashMap<String, Arc<StdioServer>>>>,
    /// Configs parked by `disconnect_server`, so `/mcp reconnect` can bring a
    /// disconnected server back. `remove_server` stays a true removal.
    disconnected: Arc<Mutex<HashMap<String, StdioServerConfig>>>,
    channel_entries: Arc<Vec<ChannelEntry>>,
    channel_queue: Arc<StdMutex<VecDeque<String>>>,
}

impl StdioMcpClient {
    /// Construct an empty client. Servers are added on demand via
    /// [`Self::add_server`].
    pub fn new() -> Self {
        Self {
            servers: Arc::new(Mutex::new(HashMap::new())),
            disconnected: Arc::new(Mutex::new(HashMap::new())),
            channel_entries: Arc::new(Vec::new()),
            channel_queue: Arc::new(StdMutex::new(VecDeque::new())),
        }
    }

    /// Construct a client with session-approved channel entries.
    ///
    /// Entries are still gated per server during initialize; this only
    /// makes notification registration possible when the server also
    /// advertises the channel capability and the core gate allows it.
    pub fn with_channel_entries(channel_entries: Vec<ChannelEntry>) -> Self {
        Self {
            servers: Arc::new(Mutex::new(HashMap::new())),
            disconnected: Arc::new(Mutex::new(HashMap::new())),
            channel_entries: Arc::new(channel_entries),
            channel_queue: Arc::new(StdMutex::new(VecDeque::new())),
        }
    }

    /// Spawn a new server and run the `initialize` handshake.
    ///
    /// The server is registered under `config.name` and can be
    /// addressed via [`McpToolCall::server`] from that point on.
    /// If a server with the same name already exists it is
    /// replaced — the previous child is torn down first.
    pub async fn add_server(&self, config: StdioServerConfig) -> Result<(), McpClientError> {
        self.add_server_cancellable(config, std::future::pending())
            .await
    }

    pub(super) async fn add_server_cancellable(
        &self,
        config: StdioServerConfig,
        cancel: impl std::future::Future<Output = ()> + Send,
    ) -> Result<(), McpClientError> {
        // Bound to a local first: a guard created in the `if let` scrutinee
        // lives until the block ends, and tearing a child down under the table
        // lock blocks every concurrent tool call for the length of a process
        // shutdown. `remove_server` below is the shape this follows.
        let existing = self.servers.lock().await.remove(&config.name);
        if let Some(existing) = existing {
            shutdown_server(existing).await;
        }

        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        configure_process_group(&mut cmd);
        for (key, value) in &config.env {
            cmd.env(key, value);
        }
        if let Some(cwd) = &config.cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd
            .spawn()
            .map_err(|err| McpClientError::Transport(format!("spawn failed: {err}")))?;
        let process_tree = match guard_process_tree(&child) {
            Ok(guard) => Some(guard),
            Err(err) => {
                tracing::warn!(
                    server = %config.name,
                    error = %err,
                    "stdio mcp: failed to guard server process tree; descendants may outlive the session"
                );
                None
            }
        };
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpClientError::Transport("no stdin on child".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpClientError::Transport("no stdout on child".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| McpClientError::Transport("no stderr on child".into()))?;

        let server = Arc::new(StdioServer {
            config: config.clone(),
            stdin: Mutex::new(stdin),
            child: Mutex::new(Some(child)),
            process_tree: StdMutex::new(process_tree),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            reader_handle: Mutex::new(None),
            stderr_handle: Mutex::new(None),
            tools_cache: Mutex::new(None),
            channel_registered: AtomicBool::new(false),
            channel_queue: Arc::clone(&self.channel_queue),
        });

        // Spawn the reader task that pumps server → client messages.
        // The reader holds only a `Weak` reference: it blocks on the child's
        // stdout, which stays open for as long as the child lives — and the
        // child lives for as long as `StdioServer` (its `kill_on_drop` handle
        // and stdin) is alive. A strong `Arc` here would therefore form a
        // cycle (reader keeps server alive → server keeps child alive → child
        // keeps reader blocked) that leaks the whole server-process chain
        // when the client is dropped without an explicit `shutdown()`.
        let reader_server = Arc::downgrade(&server);
        let reader_name = config.name.clone();
        let reader_handle = tokio::spawn(async move {
            run_reader(reader_server, reader_name, stdout).await;
        });
        *server.reader_handle.lock().await = Some(reader_handle);

        // Spawn a bounded stderr drain so verbose servers cannot block
        // while writing diagnostics to their stderr pipe.
        let stderr_server_name = config.name.clone();
        let stderr_handle = tokio::spawn(async move {
            drain_stderr(stderr_server_name, stderr).await;
        });
        *server.stderr_handle.lock().await = Some(stderr_handle);

        // Run the initialize handshake + send the initialized
        // notification. Failures tear the server down.
        // Cancellation stops the handshake, not its owner: the generation
        // task awaits shutdown_server, including child wait and reader joins.
        let initialized = tokio::select! {
            biased;
            _ = cancel => Err(McpClientError::Transport("[STALE_PROVIDER] MCP construction cancelled".into())),
            result = initialize_server(&server, &self.channel_entries) => result,
        };
        if let Err(err) = initialized {
            shutdown_server(Arc::clone(&server)).await;
            return Err(err);
        }

        self.disconnected.lock().await.remove(&config.name);
        self.servers.lock().await.insert(config.name, server);
        Ok(())
    }

    /// Remove a server and tear its child down.
    pub async fn remove_server(&self, name: &str) -> bool {
        self.disconnected.lock().await.remove(name);
        let server = self.servers.lock().await.remove(name);
        match server {
            Some(server) => {
                shutdown_server(server).await;
                true
            }
            None => false,
        }
    }

    /// Shut down every registered server.
    pub async fn shutdown(&self) {
        self.disconnected.lock().await.clear();
        let map = std::mem::take(&mut *self.servers.lock().await);
        for (_, server) in map {
            shutdown_server(server).await;
        }
        self.drain_channel_notifications();
    }

    /// [`Self::shutdown`] under a deadline, cancelling rather than abandoning.
    ///
    /// Every server is cancelled first and all of them are joined afterwards,
    /// which is the whole point of the split. Cancelling in a loop that the
    /// budget could interrupt would be the old bug in a new place: with N
    /// servers, the first slow one used up the caller's whole budget and every
    /// server behind it kept its child process. Here the budget can only cut
    /// the join short, and by then there is nothing left alive to leak.
    pub async fn close_within(&self, budget: std::time::Duration) -> McpShutdownReport {
        self.disconnected.lock().await.clear();
        let map = std::mem::take(&mut *self.servers.lock().await);
        let mut joins = Vec::with_capacity(map.len());
        for (name, server) in map {
            joins.push((name, cancel_server(server).await));
        }
        self.drain_channel_notifications();
        rebon_tool::join_closed_servers(
            joins
                .into_iter()
                .map(|(name, cancelled)| (name, cancelled.join()))
                .collect(),
            budget,
        )
        .await
    }

    /// Number of live server connections.
    pub async fn server_count(&self) -> usize {
        self.servers.lock().await.len()
    }

    async fn get_server(&self, name: &str) -> Option<Arc<StdioServer>> {
        self.servers.lock().await.get(name).cloned()
    }
}

#[async_trait]
impl McpClient for StdioMcpClient {
    async fn call_tool(&self, call: McpToolCall) -> Result<McpToolResult, McpClientError> {
        let server = self
            .get_server(&call.server)
            .await
            .ok_or_else(|| McpClientError::UnknownServer(call.server.clone()))?;

        let response = send_request(
            &server,
            "tools/call",
            json!({
                "name": call.name,
                "arguments": call.arguments,
            }),
        )
        .await?;

        if let Some(err) = response.error {
            return Err(McpClientError::CallFailed(format!(
                "{} ({})",
                err.message, err.code
            )));
        }
        let result = response
            .result
            .ok_or_else(|| McpClientError::Transport("empty result".into()))?;
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let content = result.get("content").cloned().unwrap_or(result);
        Ok(McpToolResult {
            server: call.server,
            name: call.name,
            content,
            is_error,
        })
    }

    async fn list_tools(&self, server: &str) -> Option<Vec<String>> {
        self.list_tool_definitions(server)
            .await
            .map(|tools| tools.into_iter().map(|tool| tool.name).collect())
    }

    async fn list_tool_definitions(&self, server: &str) -> Option<Vec<McpToolDefinition>> {
        let state = self.get_server(server).await?;
        if let Some(cached) = state.tools_cache.lock().await.clone() {
            return Some(cached);
        }
        let response = send_request(&state, "tools/list", json!({})).await.ok()?;
        let result = response.result?;
        let tools = parse_mcp_tool_definitions(&result);
        *state.tools_cache.lock().await = Some(tools.clone());
        Some(tools)
    }

    fn cached_tool_definitions(&self, server: &str) -> Option<Vec<McpToolDefinition>> {
        let state = if let Ok(guard) = self.servers.try_lock() {
            guard.get(server).cloned()
        } else {
            None
        }?;
        let cached = if let Ok(guard) = state.tools_cache.try_lock() {
            guard.clone()
        } else {
            None
        };
        cached
    }

    fn server_names(&self) -> Vec<String> {
        let mut names = if let Ok(guard) = self.servers.try_lock() {
            guard.keys().cloned().collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        names.sort();
        names
    }

    fn drain_channel_notifications(&self) -> Vec<String> {
        let Ok(mut guard) = self.channel_queue.lock() else {
            tracing::warn!("stdio mcp channel queue lock poisoned while draining notifications");
            return Vec::new();
        };
        guard.drain(..).collect()
    }

    async fn shutdown_transport(&self) {
        self.shutdown().await;
    }

    async fn close_with_timeout(&self, budget: std::time::Duration) -> McpShutdownReport {
        self.close_within(budget).await
    }

    async fn disconnect_server(&self, name: &str) -> bool {
        // Parks the config rather than delegating to `remove_server`: the
        // receipt the front ends print says `/mcp reconnect` brings the server
        // back, and a reconnect needs the config to do that.
        let server = self.servers.lock().await.remove(name);
        match server {
            Some(server) => {
                self.disconnected
                    .lock()
                    .await
                    .insert(name.to_string(), server.config.clone());
                shutdown_server(server).await;
                true
            }
            None => false,
        }
    }

    /// Takes the old connection down first, so a reconnect never leaves two of
    /// the same server running. If bringing it back fails, the name stays
    /// parked and the error says why, and another `/mcp reconnect` may retry —
    /// better than a half-attached server that answers some calls and not
    /// others.
    async fn reconnect_server(&self, name: &str) -> Result<bool, McpClientError> {
        let live = self
            .servers
            .lock()
            .await
            .get(name)
            .map(|server| server.config.clone());
        let config = match live {
            Some(config) => config,
            None => match self.disconnected.lock().await.remove(name) {
                Some(config) => config,
                None => return Ok(false),
            },
        };
        self.remove_server(name).await;
        if let Err(error) = self.add_server(config.clone()).await {
            self.disconnected
                .lock()
                .await
                .insert(name.to_string(), config);
            return Err(error);
        }
        Ok(true)
    }
}

async fn initialize_server(
    server: &Arc<StdioServer>,
    channel_entries: &[ChannelEntry],
) -> Result<(), McpClientError> {
    let init_params = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {},
        },
        "clientInfo": {
            "name": "rebon",
            "version": env!("CARGO_PKG_VERSION"),
        }
    });
    let response = send_request(server, "initialize", init_params).await?;
    if let Some(err) = response.error {
        return Err(McpClientError::Transport(format!(
            "initialize failed: {} ({})",
            err.message, err.code
        )));
    }
    let capabilities = ChannelCapabilities {
        claude_channel: response
            .result
            .as_ref()
            .is_some_and(initialize_result_supports_channel),
    };
    let gate_ctx = ChannelGateContext {
        runtime_enabled: !channel_entries.is_empty(),
        // The stdio runtime does not have OAuth state; MCP tools are local-process
        // integrations, so the minimal runtime treats the local session as authenticated
        // and lets the explicit session/dev allow gate remain authoritative.
        has_oauth: true,
        subscription: SubscriptionType::Individual,
        org_channels_enabled: None,
        org_allowlist: None,
        ledger_allowlist: Vec::new(),
        session_entries: channel_entries.to_vec(),
    };
    match crate::runtime::gate_channel_server(&server.config.name, &capabilities, None, &gate_ctx) {
        ChannelGateResult::Register => {
            server.channel_registered.store(true, Ordering::Release);
            tracing::debug!(
                server = %server.config.name,
                "stdio mcp registered claude/channel notifications"
            );
        }
        ChannelGateResult::Skip { kind, reason } => {
            tracing::debug!(
                server = %server.config.name,
                kind = ?kind,
                reason = %reason,
                "stdio mcp skipped claude/channel notification registration"
            );
        }
    }

    // Send the `initialized` notification (no id, no response).
    send_notification(server, "notifications/initialized", json!({})).await?;
    Ok(())
}

fn initialize_result_supports_channel(result: &Value) -> bool {
    let Some(value) = result
        .get("capabilities")
        .and_then(|capabilities| capabilities.get("experimental"))
        .and_then(|experimental| experimental.get(crate::runtime::CHANNEL_CAPABILITY))
    else {
        return false;
    };
    match value {
        Value::Bool(enabled) => *enabled,
        Value::Null => false,
        _ => true,
    }
}

async fn send_request(
    server: &Arc<StdioServer>,
    method: &str,
    params: Value,
) -> Result<JsonRpcResponse, McpClientError> {
    let id = server.next_id.fetch_add(1, Ordering::Relaxed);
    let (response_tx, response_rx) = oneshot::channel();
    server.pending.lock().await.insert(id, response_tx);

    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    if let Err(err) = write_frame(server, &frame).await {
        server.pending.lock().await.remove(&id);
        return Err(err);
    }

    let timeout = server.config.effective_timeout();
    match tokio::time::timeout(timeout, response_rx).await {
        Ok(Ok(Ok(response))) => Ok(response),
        Ok(Ok(Err(err))) => Err(err),
        Ok(Err(_)) => Err(McpClientError::Transport("response channel dropped".into())),
        Err(_) => {
            server.pending.lock().await.remove(&id);
            Err(McpClientError::Transport(format!(
                "request `{method}` timed out"
            )))
        }
    }
}

async fn send_notification(
    server: &Arc<StdioServer>,
    method: &str,
    params: Value,
) -> Result<(), McpClientError> {
    let frame = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    write_frame(server, &frame).await
}

async fn write_frame(server: &Arc<StdioServer>, frame: &Value) -> Result<(), McpClientError> {
    let mut text = serde_json::to_string(frame)
        .map_err(|err| McpClientError::Transport(format!("serialize: {err}")))?;
    text.push('\n');
    let mut stdin = server.stdin.lock().await;
    stdin
        .write_all(text.as_bytes())
        .await
        .map_err(|err| McpClientError::Transport(format!("stdin write: {err}")))?;
    stdin
        .flush()
        .await
        .map_err(|err| McpClientError::Transport(format!("stdin flush: {err}")))?;
    Ok(())
}

async fn run_reader(
    server: Weak<StdioServer>,
    server_name: String,
    stdout: tokio::process::ChildStdout,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(value): Result<Value, _> = serde_json::from_str(&line) else {
                    tracing::debug!(
                        server = %server_name,
                        "stdio mcp reader dropping non-json line: {line}"
                    );
                    continue;
                };
                // Upgrade per message and drop the strong reference before
                // the next blocking read, so the reader never keeps the
                // server (and thus the child process) alive on its own.
                let Some(server) = server.upgrade() else {
                    tracing::debug!(
                        server = %server_name,
                        "stdio mcp reader exiting: server dropped"
                    );
                    break;
                };
                dispatch_reader_message(&server, value).await;
            }
            Ok(None) => {
                if let Some(server) = server.upgrade() {
                    fail_pending_requests(
                        &server,
                        McpClientError::Transport("stdio mcp reader reached EOF".into()),
                    )
                    .await;
                }
                tracing::debug!(
                    server = %server_name,
                    "stdio mcp reader exiting on EOF"
                );
                break;
            }
            Err(err) => {
                let message = format!("stdio mcp reader read error: {err}");
                tracing::debug!(
                    server = %server_name,
                    error = %err,
                    "stdio mcp reader exiting on read error"
                );
                if let Some(server) = server.upgrade() {
                    fail_pending_requests(&server, McpClientError::Transport(message)).await;
                }
                break;
            }
        }
    }
}

async fn dispatch_reader_message(server: &Arc<StdioServer>, value: Value) {
    let has_method = value.get("method").is_some();
    let id_value = value.get("id").cloned();

    if has_method {
        if let Some(id) = id_value {
            tracing::debug!(
                server = %server.config.name,
                method = value.get("method").and_then(|m| m.as_str()).unwrap_or("<non-string>"),
                "stdio mcp reader rejecting unsupported server request"
            );
            if let Err(err) =
                send_jsonrpc_error_response(server, id, -32601, "Method not found").await
            {
                tracing::debug!(
                    server = %server.config.name,
                    error = ?err,
                    "stdio mcp reader failed to send unsupported-request response"
                );
            }
        } else {
            let method = value
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("<non-string>");
            if method == crate::runtime::CHANNEL_NOTIFICATION_METHOD {
                handle_channel_notification(server, value.get("params"));
            } else {
                tracing::debug!(
                    server = %server.config.name,
                    method = method,
                    "stdio mcp reader ignoring server notification"
                );
            }
        }
        return;
    }

    if let Some(id) = value.get("id").and_then(|v| v.as_i64()) {
        let response = JsonRpcResponse {
            id: Some(id),
            result: value.get("result").cloned(),
            error: value.get("error").map(|e| JsonRpcError {
                code: e.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                message: e
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string(),
            }),
        };
        let mut pending = server.pending.lock().await;
        if let Some(tx) = pending.remove(&id) {
            let _ = tx.send(Ok(response));
        }
    }
}

fn handle_channel_notification(server: &Arc<StdioServer>, params: Option<&Value>) {
    if !server.channel_registered.load(Ordering::Acquire) {
        tracing::debug!(
            server = %server.config.name,
            "stdio mcp reader ignoring unregistered claude/channel notification"
        );
        return;
    }

    let Some(params) = params else {
        tracing::warn!(
            server = %server.config.name,
            "stdio mcp reader ignoring malformed claude/channel notification without params"
        );
        return;
    };
    let Ok(message) = crate::runtime::ChannelMessage::from_params(params) else {
        tracing::warn!(
            server = %server.config.name,
            "stdio mcp reader ignoring malformed claude/channel notification without string content"
        );
        return;
    };

    let meta = message
        .meta
        .into_iter()
        .collect::<HashMap<String, String>>();
    let wrapped =
        crate::runtime::wrap_channel_message(&server.config.name, &message.content, Some(&meta));
    let Ok(mut queue) = server.channel_queue.lock() else {
        tracing::warn!(
            server = %server.config.name,
            "stdio mcp channel queue lock poisoned while enqueuing notification"
        );
        return;
    };
    queue.push_back(wrapped);
}

async fn send_jsonrpc_error_response(
    server: &Arc<StdioServer>,
    id: Value,
    code: i64,
    message: &str,
) -> Result<(), McpClientError> {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        },
    });
    write_frame(server, &frame).await
}

async fn fail_pending_requests(server: &Arc<StdioServer>, err: McpClientError) {
    let pending = std::mem::take(&mut *server.pending.lock().await);
    for (_, tx) in pending {
        let _ = tx.send(Err(err.clone()));
    }
}

async fn drain_stderr(server_name: String, stderr: tokio::process::ChildStderr) {
    const MAX_LOGGED_LINES: usize = 64;
    const MAX_LOGGED_CHARS_PER_LINE: usize = 2048;

    let mut lines = BufReader::new(stderr).lines();
    let mut logged = 0usize;
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if logged < MAX_LOGGED_LINES {
                    let mut truncated = line
                        .chars()
                        .take(MAX_LOGGED_CHARS_PER_LINE)
                        .collect::<String>();
                    if line.chars().count() > MAX_LOGGED_CHARS_PER_LINE {
                        truncated.push_str("...[truncated]");
                    }
                    tracing::debug!(
                        server = %server_name,
                        stderr = %truncated,
                        "stdio mcp server stderr"
                    );
                    logged += 1;
                    if logged == MAX_LOGGED_LINES {
                        tracing::debug!(
                            server = %server_name,
                            "stdio mcp server stderr log limit reached; continuing to drain silently"
                        );
                    }
                }
            }
            Ok(None) => break,
            Err(err) => {
                tracing::debug!(
                    server = %server_name,
                    error = %err,
                    "stdio mcp stderr drain exiting on read error"
                );
                break;
            }
        }
    }
}

/// What a cancelled server has left to fall.
///
/// Reader tasks that have been aborted and a child that has been killed:
/// waiting on these is a formality, and a caller out of budget may stop
/// waiting without leaking anything that runs.
struct CancelledServer {
    reader: Option<tokio::task::JoinHandle<()>>,
    stderr: Option<tokio::task::JoinHandle<()>>,
    child: Option<tokio::process::Child>,
}

/// Cancel one server: release every waiter, stop its readers, kill its child
/// and reap the helpers that child spawned.
///
/// This is the half of a shutdown that must not be skipped, so nothing in it
/// waits on the server itself — every step is either immediate or a lock held
/// for a few instructions. A deadline can therefore never catch this function
/// half-done, which is what lets the caller give up on the join it returns.
async fn cancel_server(server: Arc<StdioServer>) -> CancelledServer {
    fail_pending_requests(
        &server,
        McpClientError::Transport("stdio mcp server shutting down".into()),
    )
    .await;

    let reader = server.reader_handle.lock().await.take();
    if let Some(handle) = reader.as_ref() {
        handle.abort();
    }
    let stderr = server.stderr_handle.lock().await.take();
    if let Some(handle) = stderr.as_ref() {
        handle.abort();
    }
    let mut child = server.child.lock().await.take();
    if let Some(child) = child.as_mut() {
        let _ = child.kill().await;
    }
    // `Child::kill` only reaches the direct child; reap any helpers it
    // spawned (rust-analyzer behind the rust_lsp server, npx-launched
    // servers' node children, …) so they cannot outlive the session.
    let tree = server
        .process_tree
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(mut tree) = tree {
        if let Err(err) = tree.terminate() {
            tracing::debug!(
                server = %server.config.name,
                error = %err,
                "stdio mcp: could not terminate the server process tree"
            );
        }
    }

    CancelledServer {
        reader,
        stderr,
        child,
    }
}

impl CancelledServer {
    /// Wait for the aborted tasks to unwind and the killed child to be reaped.
    async fn join(self) {
        if let Some(handle) = self.reader {
            let _ = handle.await;
        }
        if let Some(handle) = self.stderr {
            let _ = handle.await;
        }
        if let Some(mut child) = self.child {
            let _ = child.wait().await;
        }
    }
}

async fn shutdown_server(server: Arc<StdioServer>) {
    cancel_server(server).await.join().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Absolute path to a Python interpreter, resolved once.
    ///
    /// The interpreter name recorded here is spawned again later in the
    /// test, and other tests in this binary blank `PATH` process-wide — a
    /// bare `python` can therefore stop resolving between the check and the
    /// spawn. An absolute path cannot.
    fn python_available() -> Option<String> {
        static PYTHON: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        PYTHON
            .get_or_init(|| {
                for candidate in ["python3", "python", "py"] {
                    let Ok(output) = std::process::Command::new(candidate)
                        .args(["-c", "import sys; print(sys.executable)"])
                        .stderr(Stdio::null())
                        .output()
                    else {
                        continue;
                    };
                    if !output.status.success() {
                        continue;
                    }
                    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !path.is_empty() && std::path::Path::new(&path).is_file() {
                        return Some(path);
                    }
                }
                None
            })
            .clone()
    }

    /// Minimal Python MCP-ish mock that echoes the tool name +
    /// arguments back as a result. Lets us exercise the stdio
    /// transport end-to-end without a real MCP implementation.
    fn echo_mcp_script() -> &'static str {
        r#"
import json
import sys

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "echo", "version": "0.1"},
            },
        })
    elif method == "notifications/initialized":
        # no response
        pass
    elif method == "tools/list":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "tools": [
                    {"name": "echo", "description": "echo input back"},
                    {"name": "fail", "description": "force an error"},
                ]
            },
        })
    elif method == "tools/call":
        name = msg.get("params", {}).get("name", "")
        args = msg.get("params", {}).get("arguments", {})
        if name == "fail":
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "error": {"code": -32000, "message": "fail invoked"},
            })
        else:
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "content": [
                        {"type": "text", "text": json.dumps({"name": name, "args": args})}
                    ],
                    "isError": False,
                },
            })
"#
    }

    fn write_script(tag: &str) -> tempfile::NamedTempFile {
        write_script_content(tag, echo_mcp_script())
    }

    fn write_script_content(tag: &str, contents: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::Builder::new()
            .prefix(&format!("rebon-mcp-stdio-{tag}-"))
            .suffix(".py")
            .tempfile()
            .unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdio_client_initializes_and_dispatches_tools_call() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP test: python not installed");
            return;
        };
        let script = write_script("call");
        let client = StdioMcpClient::new();
        client
            .add_server(StdioServerConfig::new(
                "echo",
                python,
                vec![script.path().to_string_lossy().into_owned()],
            ))
            .await
            .expect("add_server failed");

        let result = client
            .call_tool(McpToolCall {
                server: "echo".into(),
                name: "echo".into(),
                arguments: json!({ "hello": "world" }),
            })
            .await
            .unwrap();
        assert_eq!(result.server, "echo");
        assert_eq!(result.name, "echo");
        assert!(!result.is_error);
        // Content is the echo script's text array.
        assert!(result.content.is_array());
        let first = &result.content[0];
        assert_eq!(first["type"], "text");
        let text = first["text"].as_str().unwrap();
        assert!(text.contains("\"name\": \"echo\""));
        assert!(text.contains("\"hello\": \"world\""));

        let tools = client.list_tools("echo").await.unwrap();
        assert!(tools.contains(&"echo".to_string()));
        assert!(tools.contains(&"fail".to_string()));

        client.shutdown().await;
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdio_client_initializes_windows_cmd_server_from_path_with_spaces() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP test: python not installed");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let server_dir = temp.path().join("batch server");
        std::fs::create_dir_all(&server_dir).unwrap();
        let script_path = server_dir.join("server.py");
        std::fs::write(&script_path, echo_mcp_script()).unwrap();
        let command_path = server_dir.join("server.cmd");
        std::fs::write(
            &command_path,
            format!("@echo off\r\n{python} \"%~dp0server.py\"\r\n"),
        )
        .unwrap();

        let client = StdioMcpClient::new();
        client
            .add_server(StdioServerConfig::new(
                "batch",
                command_path.to_string_lossy().into_owned(),
                Vec::new(),
            ))
            .await
            .expect("add_server failed for .cmd MCP server");

        assert_eq!(
            client.list_tools("batch").await.unwrap(),
            vec!["echo", "fail"]
        );
        client.shutdown().await;
    }

    /// Completes the handshake, then answers nothing. Stands in for a server
    /// that accepted a call and never came back — an lsp-mcp daemon behind a
    /// wedged rust-analyzer, a plugin host that stopped reading.
    fn silent_after_initialize_script() -> &'static str {
        r#"
import json
import sys

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    if msg.get("method") == "initialize":
        sys.stdout.write(json.dumps({
            "jsonrpc": "2.0",
            "id": msg.get("id"),
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "silent", "version": "0.1"},
            },
        }) + "\n")
        sys.stdout.flush()
"#
    }

    /// The cancel half of a bounded close must land inside the budget, and
    /// releasing waiters is the part of it a caller can observe: whoever was
    /// blocked on a call the server never answered has to learn the transport
    /// is gone, rather than sitting on a oneshot until the request timeout
    /// thirty seconds later.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_bounded_close_releases_a_call_the_server_never_answered() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP test: python not installed");
            return;
        };
        let script = write_script_content("silent-close", silent_after_initialize_script());
        let client = StdioMcpClient::new();
        client
            .add_server(StdioServerConfig::new(
                "silent",
                python,
                vec![script.path().to_string_lossy().into_owned()],
            ))
            .await
            .expect("add_server failed");

        let calling = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .call_tool(McpToolCall {
                        server: "silent".into(),
                        name: "never-answers".into(),
                        arguments: json!({}),
                    })
                    .await
            })
        };

        // Wait until the call is actually outstanding, so the release is what
        // ends it rather than the call never having started.
        let server = client
            .get_server("silent")
            .await
            .expect("server registered");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while server.pending.lock().await.is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !server.pending.lock().await.is_empty(),
            "the call never reached the server, so this proves nothing"
        );
        drop(server);

        let started = std::time::Instant::now();
        let report = client.close_within(Duration::from_secs(20)).await;
        let closing = started.elapsed();

        let error = calling
            .await
            .expect("the calling task panicked")
            .expect_err("a server that never answered cannot have returned a result");
        assert!(
            matches!(error, McpClientError::Transport(_)),
            "expected a transport error, got {error:?}"
        );
        assert_eq!(report.closed, ["silent"]);
        assert!(report.is_complete());
        // Nowhere near the budget, and nowhere near the 30s request timeout
        // that would have ended the call if nothing had released it.
        assert!(
            closing < Duration::from_secs(10),
            "close took {closing:?}, which means it waited rather than cancelled"
        );
        assert_eq!(client.server_count().await, 0);
    }

    /// The test that separates cancelling from abandoning.
    ///
    /// A budget of zero cannot wait for anything, so everything a caller
    /// wrapping the unbounded shutdown in `tokio::time::timeout` would have
    /// done is nothing at all: the servers stay registered, the child keeps
    /// running, and the caller blocked on a call is never woken. The paired
    /// close has to do the opposite — cancel completely, join not at all —
    /// and report honestly that it did not observe the close finish.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_zero_budget_still_cancels_even_though_it_can_join_nothing() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP test: python not installed");
            return;
        };
        let script = write_script_content("zero-budget", silent_after_initialize_script());
        let client = StdioMcpClient::new();
        client
            .add_server(StdioServerConfig::new(
                "silent",
                python,
                vec![script.path().to_string_lossy().into_owned()],
            ))
            .await
            .expect("add_server failed");

        let calling = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .call_tool(McpToolCall {
                        server: "silent".into(),
                        name: "never-answers".into(),
                        arguments: json!({}),
                    })
                    .await
            })
        };
        let server = client
            .get_server("silent")
            .await
            .expect("server registered");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while server.pending.lock().await.is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!server.pending.lock().await.is_empty());
        drop(server);

        let report = client.close_within(Duration::ZERO).await;

        // Which list the server lands in is not asserted, and deliberately:
        // a join that is already finished completes on its first poll, so a
        // zero budget may still observe the close, and saying so is honest.
        // What must hold either way is that the server is accounted for once.
        let mut named = report.closed.clone();
        named.extend(report.timed_out.clone());
        assert_eq!(named, ["silent"]);
        // The cancel half, on the other hand, happened in full: the waiter is
        // released and the server deregistered. That is the whole difference
        // from wrapping the unbounded shutdown in a timeout, which at a zero
        // budget would have left the call blocked and the child running.
        let error = calling
            .await
            .expect("the calling task panicked")
            .expect_err("a server that never answered cannot have returned a result");
        assert!(
            matches!(error, McpClientError::Transport(_)),
            "expected a transport error, got {error:?}"
        );
        assert_eq!(client.server_count().await, 0);
    }

    /// A budget must not be spent on the first server in the map. Every server
    /// is cancelled before any is joined, so a slow one cannot leave the
    /// others attached — which is what the old sequential loop did whenever a
    /// caller wrapped it in a timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_bounded_close_takes_down_every_server_not_only_the_first() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP test: python not installed");
            return;
        };
        let silent = write_script_content("silent-many", silent_after_initialize_script());
        let echo = write_script("echo-many");
        let client = StdioMcpClient::new();
        for (name, script) in [("silent", &silent), ("echo", &echo)] {
            client
                .add_server(StdioServerConfig::new(
                    name,
                    python.clone(),
                    vec![script.path().to_string_lossy().into_owned()],
                ))
                .await
                .expect("add_server failed");
        }

        let live: Vec<_> = {
            let mut handles = Vec::new();
            for name in ["silent", "echo"] {
                let server = client.get_server(name).await.expect("server registered");
                handles.push(Arc::downgrade(&server));
            }
            handles
        };

        let report = client.close_within(Duration::from_secs(20)).await;

        assert_eq!(report.closed, ["echo", "silent"]);
        assert!(report.is_complete());
        assert_eq!(client.server_count().await, 0);
        for server in live {
            assert!(
                server.upgrade().is_none(),
                "a server outlived the close, so its child process did too"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_client_releases_server_without_explicit_shutdown() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP test: python not installed");
            return;
        };
        let script = write_script("drop-release");
        let client = StdioMcpClient::new();
        client
            .add_server(StdioServerConfig::new(
                "echo",
                python,
                vec![script.path().to_string_lossy().into_owned()],
            ))
            .await
            .expect("add_server failed");

        let server = client.get_server("echo").await.unwrap();
        let weak = Arc::downgrade(&server);
        drop(server);
        // Regression guard: the reader task must not hold a strong Arc.
        // With a strong reference the child's stdin stays open, the child
        // never exits, the reader never sees EOF, and the whole chain
        // (server + child process) outlives the client forever.
        drop(client);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while weak.upgrade().is_some() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            weak.upgrade().is_none(),
            "StdioServer leaked after client drop: reader task kept the child process chain alive"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdio_client_surfaces_server_errors_from_tools_call() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP test: python not installed");
            return;
        };
        let script = write_script("err");
        let client = StdioMcpClient::new();
        client
            .add_server(StdioServerConfig::new(
                "echo",
                python,
                vec![script.path().to_string_lossy().into_owned()],
            ))
            .await
            .unwrap();

        let err = client
            .call_tool(McpToolCall {
                server: "echo".into(),
                name: "fail".into(),
                arguments: json!({}),
            })
            .await
            .unwrap_err();
        match err {
            McpClientError::CallFailed(msg) => assert!(msg.contains("fail invoked")),
            other => panic!("unexpected error: {other:?}"),
        }
        client.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdio_client_remove_server_detaches_cleanly() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP test: python not installed");
            return;
        };
        let script = write_script("remove");
        let client = StdioMcpClient::new();
        client
            .add_server(StdioServerConfig::new(
                "echo",
                python,
                vec![script.path().to_string_lossy().into_owned()],
            ))
            .await
            .unwrap();
        assert_eq!(client.server_count().await, 1);
        assert!(client.remove_server("echo").await);
        assert_eq!(client.server_count().await, 0);
    }

    /// The whole reason the lifecycle moved onto the trait: every consumer
    /// holds an `Arc<dyn McpClient>`, and `remove_server` was only ever on the
    /// concrete type — so nothing outside this file could take a server down.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_server_can_be_disconnected_through_the_trait_object() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP test: python not installed");
            return;
        };
        let script = write_script("trait-disconnect");
        let client = StdioMcpClient::new();
        client
            .add_server(StdioServerConfig::new(
                "echo",
                python,
                vec![script.path().to_string_lossy().into_owned()],
            ))
            .await
            .unwrap();

        let erased: Arc<dyn McpClient> = Arc::new(client);
        assert_eq!(erased.server_names(), vec!["echo".to_string()]);
        assert!(erased.disconnect_server("echo").await);
        assert!(erased.server_names().is_empty());
        assert!(
            !erased.disconnect_server("echo").await,
            "a name that is gone is not this client's to disconnect"
        );
    }

    /// A reconnect replaces the connection and keeps the name — the server is
    /// dialed again with the configuration it was started with, so the caller
    /// does not have to hold one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reconnect_replaces_the_connection_and_keeps_the_name() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP test: python not installed");
            return;
        };
        let script = write_script("trait-reconnect");
        let client = StdioMcpClient::new();
        client
            .add_server(StdioServerConfig::new(
                "echo",
                python,
                vec![script.path().to_string_lossy().into_owned()],
            ))
            .await
            .unwrap();

        let erased: Arc<dyn McpClient> = Arc::new(client);
        assert!(erased.reconnect_server("echo").await.unwrap());
        assert_eq!(erased.server_names(), vec!["echo".to_string()]);

        // And it still works afterwards — a reconnect that left a dead handle
        // behind would pass every check above.
        let answer = erased
            .call_tool(McpToolCall {
                server: "echo".into(),
                name: "echo".into(),
                arguments: json!({"value": "after"}),
            })
            .await
            .expect("the reconnected server answers");
        assert!(format!("{answer:?}").contains("after"), "{answer:?}");

        assert!(
            !erased.reconnect_server("ghost").await.unwrap(),
            "a name this client does not have is not its to reconnect"
        );
        erased.shutdown_transport().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdio_client_call_to_unknown_server_returns_error() {
        let client = StdioMcpClient::new();
        let err = client
            .call_tool(McpToolCall {
                server: "ghost".into(),
                name: "x".into(),
                arguments: json!({}),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, McpClientError::UnknownServer(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdio_client_initialize_failure_cleans_unregistered_server() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP test: python not installed");
            return;
        };
        let script = write_script_content(
            "init-fail",
            r#"
import json
import sys
import time

for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("method") == "initialize":
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":msg.get("id"),"error":{"code":-32000,"message":"init failed"}}) + "\n")
        sys.stdout.flush()
        time.sleep(30)
        break
"#,
        );
        let client = StdioMcpClient::new();
        let err = client
            .add_server(StdioServerConfig::new(
                "bad-init",
                python,
                vec![script.path().to_string_lossy().into_owned()],
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, McpClientError::Transport(_)));
        assert_eq!(client.server_count().await, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdio_client_drains_large_stderr_during_initialize() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP test: python not installed");
            return;
        };
        let script = write_script_content(
            "stderr-drain",
            r#"
import json
import sys

sys.stderr.write("x" * (1024 * 1024 * 2) + "\n")
sys.stderr.flush()

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("method") == "initialize":
        send({"jsonrpc":"2.0","id":msg.get("id"),"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"stderr","version":"0.1"}}})
    elif msg.get("method") == "notifications/initialized":
        pass
"#,
        );
        let client = StdioMcpClient::new();
        let mut cfg = StdioServerConfig::new(
            "stderr",
            python,
            vec![script.path().to_string_lossy().into_owned()],
        );
        // Generous on purpose: what this test guards against is a deadlock
        // (initialize never completing while the child blocks on a full
        // stderr pipe), and a deadlock fails at any timeout. A tight one
        // just fails on a loaded machine, where spawning python and
        // pushing two megabytes through a pipe can take several seconds.
        cfg.request_timeout = Some(Duration::from_secs(30));
        tokio::time::timeout(Duration::from_secs(60), client.add_server(cfg))
            .await
            .expect("add_server hung while child wrote stderr")
            .expect("add_server failed");
        assert_eq!(client.server_count().await, 1);
        client.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdio_client_eof_fails_inflight_request_without_waiting_for_timeout() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP test: python not installed");
            return;
        };
        let script = write_script_content(
            "eof-inflight",
            r#"
import json
import sys

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":msg.get("id"),"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"eof","version":"0.1"}}}) + "\n")
        sys.stdout.flush()
    elif method == "notifications/initialized":
        pass
    elif method == "tools/call":
        sys.exit(0)
"#,
        );
        let client = StdioMcpClient::new();
        let mut cfg = StdioServerConfig::new(
            "eof",
            python,
            vec![script.path().to_string_lossy().into_owned()],
        );
        cfg.request_timeout = Some(Duration::from_secs(30));
        client.add_server(cfg).await.unwrap();

        let started = std::time::Instant::now();
        let err = client
            .call_tool(McpToolCall {
                server: "eof".into(),
                name: "anything".into(),
                arguments: json!({}),
            })
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        match err {
            McpClientError::Transport(msg) => {
                assert!(msg.contains("EOF") || msg.contains("dropped"))
            }
            other => panic!("unexpected error: {other:?}"),
        }
        client.shutdown().await;
    }

    fn channel_mcp_script(capability: &str, notification: &str) -> String {
        format!(
            r#"
import json
import sys

CAPABILITY = json.loads(r'''{capability}''')
NOTIFICATION = json.loads(r'''{notification}''')

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        send({{
            "jsonrpc": "2.0",
            "id": msg.get("id"),
            "result": {{
                "protocolVersion": "2024-11-05",
                "capabilities": CAPABILITY,
                "serverInfo": {{"name": "channel", "version": "0.1"}},
            }},
        }})
    elif method == "notifications/initialized":
        if NOTIFICATION is not None:
            send(NOTIFICATION)
"#,
            capability = capability,
            notification = notification,
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdio_client_enqueues_registered_channel_notifications_and_drains_once() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP channel test: python not installed");
            return;
        };
        let script = write_script_content(
            "channel-ok",
            &channel_mcp_script(
                r##"{"experimental":{"claude/channel":true},"tools":{}}"##,
                r##"{"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":"/exit stays model text","meta":{"topic":"alerts","count":3,"bad-key":"ignored"}}}"##,
            ),
        );
        let client = StdioMcpClient::with_channel_entries(vec![ChannelEntry::Server {
            name: "channel".into(),
            dev: true,
        }]);
        client
            .add_server(StdioServerConfig::new(
                "channel",
                python,
                vec![script.path().to_string_lossy().into_owned()],
            ))
            .await
            .expect("add_server failed");

        let drained = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let drained = client.drain_channel_notifications();
                if !drained.is_empty() {
                    break drained;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for channel notification");
        assert_eq!(drained.len(), 1);
        assert_eq!(
            drained[0],
            "<channel source=\"channel\" topic=\"alerts\">\n/exit stays model text\n</channel>"
        );
        assert!(client.drain_channel_notifications().is_empty());
        client.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdio_client_skips_channel_notifications_without_capability_or_dev_allow() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP channel test: python not installed");
            return;
        };

        let missing_cap_script = write_script_content(
            "channel-missing-cap",
            &channel_mcp_script(
                r##"{"tools":{}}"##,
                r##"{"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":"should not enqueue"}}"##,
            ),
        );
        let client = StdioMcpClient::with_channel_entries(vec![ChannelEntry::Server {
            name: "no-cap".into(),
            dev: true,
        }]);
        client
            .add_server(StdioServerConfig::new(
                "no-cap",
                python.clone(),
                vec![missing_cap_script.path().to_string_lossy().into_owned()],
            ))
            .await
            .expect("add_server failed");
        assert!(client.drain_channel_notifications().is_empty());
        client.shutdown().await;

        let non_dev_script = write_script_content(
            "channel-non-dev",
            &channel_mcp_script(
                r##"{"experimental":{"claude/channel":{}},"tools":{}}"##,
                r##"{"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":"should not enqueue"}}"##,
            ),
        );
        let client = StdioMcpClient::with_channel_entries(vec![ChannelEntry::Server {
            name: "non-dev".into(),
            dev: false,
        }]);
        client
            .add_server(StdioServerConfig::new(
                "non-dev",
                python,
                vec![non_dev_script.path().to_string_lossy().into_owned()],
            ))
            .await
            .expect("add_server failed");
        assert!(client.drain_channel_notifications().is_empty());
        client.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdio_client_ignores_malformed_channel_notification_and_keeps_reader_alive() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP channel test: python not installed");
            return;
        };
        let script = write_script_content(
            "channel-malformed",
            &channel_mcp_script(
                r##"{"experimental":{"claude/channel":true},"tools":{}}"##,
                r##"{"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":123}}"##,
            ),
        );
        let client = StdioMcpClient::with_channel_entries(vec![ChannelEntry::Server {
            name: "malformed".into(),
            dev: true,
        }]);
        client
            .add_server(StdioServerConfig::new(
                "malformed",
                python,
                vec![script.path().to_string_lossy().into_owned()],
            ))
            .await
            .expect("add_server failed");
        assert!(client.drain_channel_notifications().is_empty());
        assert_eq!(client.server_count().await, 1);
        client.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdio_client_server_request_with_colliding_id_does_not_satisfy_pending_response() {
        let Some(python) = python_available() else {
            eprintln!("skipping stdio MCP test: python not installed");
            return;
        };
        let script = write_script_content(
            "request-collision",
            r#"
import json
import sys

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"collision","version":"0.1"}}}) + "\n")
        sys.stdout.flush()
    elif method == "notifications/initialized":
        pass
    elif method == "tools/call":
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":mid,"method":"sampling/createMessage","params":{}}) + "\n")
        sys.stdout.flush()
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"real response"}],"isError":False}}) + "\n")
        sys.stdout.flush()
"#,
        );
        let client = StdioMcpClient::new();
        client
            .add_server(StdioServerConfig::new(
                "collision",
                python,
                vec![script.path().to_string_lossy().into_owned()],
            ))
            .await
            .unwrap();

        let result = client
            .call_tool(McpToolCall {
                server: "collision".into(),
                name: "x".into(),
                arguments: json!({}),
            })
            .await
            .unwrap();
        assert_eq!(result.content[0]["text"], "real response");
        client.shutdown().await;
    }
    #[test]
    fn stdio_server_config_minimal_constructor_defaults_timeout_to_30s() {
        let cfg = StdioServerConfig::new("x", "python", vec!["script.py".into()]);
        assert_eq!(cfg.effective_timeout(), Duration::from_secs(30));
    }
}
