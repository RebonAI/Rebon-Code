//! Rebon's own file tools, injected into an ACP agent's session.
//!
//! Advertising `fs/write_text_file` is not enough: nothing in ACP
//! forces an agent to use it, and real agents (codex) write to disk
//! themselves, leaving `/rewind` nothing to restore. This module is
//! the countermeasure — an MCP server named [`FS_MCP_SERVER_NAME`]
//! that Rebon injects into `session/new`, offering `write_file` and
//! `edit_file` tools whose descriptions tell the agent that every
//! file modification MUST go through them. Writes made through these
//! tools take the same road as `fs/write_text_file`: confined to the
//! session's roots, snapshotted before landing, rewindable after.
//!
//! # Shape
//!
//! ```text
//! agent ──spawns──▶ rebon __acp-fs-mcp        (stdio MCP bridge)
//!                        │  env: REBON_ACP_FS_{PORT,TOKEN,SESSION}
//!                        ▼  TCP 127.0.0.1, token-authenticated
//! host session ◀── HostFsService ──▶ SnapshotHostFs (confine → snapshot → write)
//! ```
//!
//! The bridge is deliberately dumb: it speaks MCP on stdio and
//! forwards tool calls over a local TCP connection. Everything that
//! matters — root confinement, the pre-write snapshot, the edit
//! matching — happens in the host process, next to the session that
//! owns the file history. The bridge process is spawned by the agent
//! and dies with it (stdin EOF ends the serve loop), so there is no
//! orphan to clean up.
//!
//! The service is one per session, not per agent: snapshots are keyed
//! by whichever turn is currently armed, and arming is driven by each
//! agent's journal, so the service itself is agent-agnostic. Two
//! agents each running their own bridge against the same service is a
//! normal state.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rebon_types::constant_time_eq;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use rebon_proto::{
    FramingMode, JsonRpcError, JsonRpcMessage, JsonRpcRequest, JsonRpcVersion, StdioReader,
    StdioWriter,
};

use crate::host_fs::{DirectHostFs, HostFileHistory, HostFs, SnapshotHostFs};

/// Name the injected MCP server is declared under. Agents typically
/// surface the tools as `mcp__rebon-fs__write_file` and friends.
pub const FS_MCP_SERVER_NAME: &str = "rebon-fs";

/// Environment the host sets on the bridge via the injected
/// `McpServerConfig`. Env rather than argv: argv is visible to every
/// user on the machine, env only to the same user.
pub const FS_PORT_ENV: &str = "REBON_ACP_FS_PORT";
pub const FS_TOKEN_ENV: &str = "REBON_ACP_FS_TOKEN";
pub const FS_SESSION_ENV: &str = "REBON_ACP_FS_SESSION";

/// Bridge argv: `rebon __acp-fs-mcp`.
pub const FS_BRIDGE_SUBCOMMAND: &str = "__acp-fs-mcp";

const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on a single write's content. Enforced after parsing — a
/// pathological line still costs transient memory, but nothing lands
/// on disk past this. The peer is the user's own agent, not a
/// stranger; this is an accident guard, not a security boundary.
const MAX_CONTENT_BYTES: usize = 32 * 1024 * 1024;

// ── wire protocol (bridge ⇄ host, NDJSON over local TCP) ──────────

/// First line of every connection.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct AuthRequest {
    token: String,
    session: String,
}

/// One tool invocation, as the bridge forwards it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct WireRequest {
    op: String,
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    old_string: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    new_string: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replace_all: Option<bool>,
}

/// The host's answer: exactly one of `ok` (a human-readable summary
/// the bridge hands to the model) or `err`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct WireResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ok: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    err: Option<String>,
}

impl WireResponse {
    fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: Some(message.into()),
            err: None,
        }
    }

    fn err(message: impl Into<String>) -> Self {
        Self {
            ok: None,
            err: Some(message.into()),
        }
    }
}

// ── host side ─────────────────────────────────────────────────────

struct ServiceState {
    token: String,
    session_id: String,
    fs: SnapshotHostFs,
}

/// The host half of the injected fs tools, alive as long as the
/// session that started it.
pub struct HostFsService;

/// Keeps the service running; dropping it stops the accept loop and
/// closes the port. Held by the session, so the service dies with it.
pub struct HostFsServiceHandle {
    port: u16,
    token: String,
    join: tokio::task::JoinHandle<()>,
}

impl HostFsServiceHandle {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// The environment the injected `McpServerConfig` gives the
    /// bridge, so host and bridge cannot disagree on variable names.
    pub fn bridge_env(&self, session_id: &str) -> std::collections::BTreeMap<String, String> {
        let mut env = std::collections::BTreeMap::from([
            (FS_PORT_ENV.to_string(), self.port.to_string()),
            (FS_TOKEN_ENV.to_string(), self.token.clone()),
            (FS_SESSION_ENV.to_string(), session_id.to_string()),
        ]);
        // Diagnostic passthrough: agents spawn MCP servers with a
        // clean environment, so the debug hook only reaches the bridge
        // by riding the injected config.
        if let Ok(log) = std::env::var("REBON_ACP_FS_DEBUG_LOG") {
            if !log.trim().is_empty() {
                env.insert("REBON_ACP_FS_DEBUG_LOG".to_string(), log);
            }
        }
        env
    }
}

impl Drop for HostFsServiceHandle {
    fn drop(&mut self) {
        self.join.abort();
    }
}

impl std::fmt::Debug for HostFsServiceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostFsServiceHandle")
            .field("port", &self.port)
            .finish()
    }
}

impl HostFsService {
    /// Bind a loopback port and start serving. Must be called inside a
    /// tokio runtime; the port is bound synchronously so the caller
    /// can put it into the injected server config before any agent
    /// starts.
    ///
    /// The service builds its own [`SnapshotHostFs`] over the shared
    /// file history rather than borrowing an agent's: same roots, same
    /// snapshot-or-refuse behaviour, no ownership tangle.
    pub fn start(
        file_history: Arc<dyn HostFileHistory>,
        write_roots: Vec<PathBuf>,
        session_id: impl Into<String>,
    ) -> anyhow::Result<HostFsServiceHandle> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let token = rebon_types::secure_random_hex_token()
            .map_err(|err| anyhow::anyhow!("could not generate an auth token: {err}"))?;
        let state = Arc::new(ServiceState {
            token: token.clone(),
            session_id: session_id.into(),
            fs: SnapshotHostFs::new(file_history, write_roots),
        });
        let join = tokio::spawn(accept_loop(listener, state));
        Ok(HostFsServiceHandle { port, token, join })
    }
}

async fn accept_loop(listener: std::net::TcpListener, state: Arc<ServiceState>) {
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(err) => {
            tracing::warn!(error = %err, "acp-fs: could not enter the async listener");
            return;
        }
    };
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                // Concurrent on purpose: each agent runs its own
                // bridge, and one slow write must not stall another
                // agent's connection.
                tokio::spawn(handle_connection(stream, state.clone()));
            }
            Err(err) => {
                // Transient accept errors (fd pressure) should not
                // kill the service; back off briefly instead of
                // spinning.
                tracing::warn!(error = %err, "acp-fs: accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn handle_connection(stream: TcpStream, state: Arc<ServiceState>) {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    // First line authenticates or the connection dies. The timeout is
    // what stops a port-scanning localhost process from pinning a
    // connection open without ever speaking.
    let auth = match tokio::time::timeout(AUTH_TIMEOUT, lines.next_line()).await {
        Ok(Ok(Some(line))) => line,
        _ => return,
    };
    let authorized = serde_json::from_str::<AuthRequest>(&auth)
        .ok()
        .is_some_and(|auth| {
            constant_time_eq(&auth.token, &state.token) && auth.session == state.session_id
        });
    if !authorized {
        tracing::warn!("acp-fs: rejecting an unauthenticated connection");
        let _ = write_response(&mut write_half, &WireResponse::err("unauthorized")).await;
        return;
    }
    if write_response(&mut write_half, &WireResponse::ok("authenticated"))
        .await
        .is_err()
    {
        return;
    }

    // Requests are serial within a connection — the bridge sends one
    // tool call at a time, and an edit racing its own read would be
    // worse than queueing.
    while let Ok(Some(line)) = lines.next_line().await {
        let response = match serde_json::from_str::<WireRequest>(&line) {
            Ok(request) => handle_request(&state, request).await,
            Err(err) => WireResponse::err(format!("malformed request: {err}")),
        };
        if write_response(&mut write_half, &response).await.is_err() {
            return;
        }
    }
}

async fn write_response(
    stream: &mut tokio::net::tcp::OwnedWriteHalf,
    response: &WireResponse,
) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(response)?;
    line.push(b'\n');
    stream.write_all(&line).await
}

async fn handle_request(state: &ServiceState, request: WireRequest) -> WireResponse {
    let path = PathBuf::from(&request.path);
    if !path.is_absolute() {
        return WireResponse::err(format!("`path` must be absolute, got `{}`", request.path));
    }
    let result = match request.op.as_str() {
        "write_file" => {
            let Some(content) = request.content else {
                return WireResponse::err("`content` is required for write_file");
            };
            if content.len() > MAX_CONTENT_BYTES {
                return WireResponse::err(format!(
                    "content is {} bytes; the limit is {MAX_CONTENT_BYTES}",
                    content.len()
                ));
            }
            state
                .fs
                .write_text_file(&state.session_id, &path, &content)
                .await
                .map(|()| format!("Wrote {} bytes to {}", content.len(), path.display()))
        }
        "edit_file" => edit_file(state, &path, &request).await,
        other => return WireResponse::err(format!("unknown op `{other}`")),
    };

    match result {
        Ok(mut message) => {
            // A write outside an armed turn is tolerated — refusing it
            // would push the agent back to writing disk itself — but
            // it is a write `/rewind` will not cover, and both the log
            // and the model should know.
            if state.fs.history().is_armed() == Some(false) {
                tracing::warn!(
                    session_id = %state.session_id,
                    path = %path.display(),
                    "acp-fs: write landed outside a turn; /rewind will not cover it"
                );
                message.push_str(" (written outside a turn; not covered by /rewind)");
            }
            WireResponse::ok(message)
        }
        Err(err) => WireResponse::err(err.to_string()),
    }
}

async fn edit_file(
    state: &ServiceState,
    path: &Path,
    request: &WireRequest,
) -> anyhow::Result<String> {
    let (Some(old_string), Some(new_string)) = (&request.old_string, &request.new_string) else {
        anyhow::bail!("`old_string` and `new_string` are required for edit_file");
    };
    if new_string.len() > MAX_CONTENT_BYTES {
        anyhow::bail!(
            "new_string is {} bytes; the limit is {MAX_CONTENT_BYTES}",
            new_string.len()
        );
    }
    if !path.exists() {
        anyhow::bail!(
            "{} does not exist — use write_file to create a new file",
            path.display()
        );
    }
    let content = DirectHostFs
        .read_text_file(&state.session_id, path, None, None)
        .await?;
    let (updated, replacements) = apply_edit(
        &content,
        old_string,
        new_string,
        request.replace_all.unwrap_or(false),
    )
    .map_err(|err| anyhow::anyhow!("{err} in {}", path.display()))?;
    state
        .fs
        .write_text_file(&state.session_id, path, &updated)
        .await?;
    Ok(format!(
        "Replaced {replacements} occurrence{} in {}",
        if replacements == 1 { "" } else { "s" },
        path.display()
    ))
}

/// The edit itself, free of any I/O: apply `old_string` → `new_string`
/// to `content` and say how many replacements were made.
///
/// Line endings are normalised to the file's own style first — models
/// almost always emit bare `\n`, and on Windows a CRLF file would
/// otherwise never match anything, which would make the whole injected
/// tool useless exactly where it matters most. Matching semantics and
/// error wording follow the local engine's Edit tool, so an agent
/// bounced between the local engine and an ACP one sees one behaviour.
pub fn apply_edit(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<(String, usize), String> {
    let to_crlf = content.contains("\r\n");
    let old = normalize_line_endings(old_string, to_crlf);
    let new = normalize_line_endings(new_string, to_crlf);
    if old.is_empty() {
        return Err(
            "`old_string` must not be empty — use write_file to create or overwrite a file"
                .to_string(),
        );
    }
    if old == new {
        return Err("`old_string` and `new_string` are identical".to_string());
    }
    let occurrences = content.matches(old.as_ref()).count();
    if occurrences == 0 {
        return Err("`old_string` was not found".to_string());
    }
    if occurrences > 1 && !replace_all {
        return Err(format!(
            "`old_string` matched {occurrences} times; set `replace_all` to true \
             to replace every occurrence, or provide a longer, unique `old_string`"
        ));
    }
    if replace_all {
        Ok((content.replace(old.as_ref(), new.as_ref()), occurrences))
    } else {
        Ok((content.replacen(old.as_ref(), new.as_ref(), 1), 1))
    }
}

/// Adapt a model-supplied string to the file's line-ending style.
/// Mirrors the local engine Edit tool's line-ending normalization.
fn normalize_line_endings(s: &str, to_crlf: bool) -> std::borrow::Cow<'_, str> {
    if to_crlf {
        let clean = s.replace("\r\n", "\n");
        std::borrow::Cow::Owned(clean.replace('\n', "\r\n"))
    } else if s.contains("\r\n") {
        std::borrow::Cow::Owned(s.replace("\r\n", "\n"))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

// ── bridge side (runs inside `rebon __acp-fs-mcp`) ────────────────

/// Where the bridge finds its host, read from the environment the
/// injected server config set.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub port: u16,
    pub token: String,
    pub session: String,
}

impl BridgeConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let read = |name: &str| {
            std::env::var(name).map_err(|_| {
                anyhow::anyhow!(
                    "{name} is not set — this command is spawned by an \
                                              ACP agent from Rebon's injected MCP config, not run \
                                              by hand"
                )
            })
        };
        Ok(Self {
            port: read(FS_PORT_ENV)?
                .parse()
                .map_err(|err| anyhow::anyhow!("{FS_PORT_ENV} is not a port number: {err}"))?,
            token: read(FS_TOKEN_ENV)?,
            session: read(FS_SESSION_ENV)?,
        })
    }
}

/// One authenticated connection to the host service.
struct HostConnection {
    lines: tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    write: tokio::net::tcp::OwnedWriteHalf,
}

impl HostConnection {
    async fn connect(config: &BridgeConfig) -> anyhow::Result<Self> {
        let stream = TcpStream::connect(("127.0.0.1", config.port)).await?;
        let (read_half, write_half) = stream.into_split();
        let mut connection = Self {
            lines: BufReader::new(read_half).lines(),
            write: write_half,
        };
        let auth = serde_json::to_string(&AuthRequest {
            token: config.token.clone(),
            session: config.session.clone(),
        })?;
        connection.send_line(&auth).await?;
        let response = connection.read_response().await?;
        if response.ok.is_none() {
            anyhow::bail!(
                "host refused the connection: {}",
                response.err.unwrap_or_else(|| "no reason given".into())
            );
        }
        Ok(connection)
    }

    async fn send_line(&mut self, line: &str) -> anyhow::Result<()> {
        self.write.write_all(line.as_bytes()).await?;
        self.write.write_all(b"\n").await?;
        Ok(())
    }

    async fn read_response(&mut self) -> anyhow::Result<WireResponse> {
        match self.lines.next_line().await? {
            Some(line) => Ok(serde_json::from_str(&line)?),
            None => anyhow::bail!("the host closed the connection"),
        }
    }

    async fn call(&mut self, request: &WireRequest) -> anyhow::Result<WireResponse> {
        let line = serde_json::to_string(request)?;
        tokio::time::timeout(CALL_TIMEOUT, async {
            self.send_line(&line).await?;
            self.read_response().await
        })
        .await
        .map_err(|_| anyhow::anyhow!("the host did not answer within {CALL_TIMEOUT:?}"))?
    }
}

/// Append one line to the file named by `REBON_ACP_FS_DEBUG_LOG`,
/// when set. The bridge runs as a child of the *agent*, whose stderr
/// handling varies; a file is the one place its lifecycle can be
/// observed regardless of who spawned it.
fn debug_log(message: &str) {
    let Ok(path) = std::env::var("REBON_ACP_FS_DEBUG_LOG") else {
        return;
    };
    if path.trim().is_empty() {
        return;
    }
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "[{}] {message}", std::process::id());
    }
}

/// Run the bridge over stdio. Returns on stdin EOF — the agent
/// closing our stdin is the agent going away, and exiting with it is
/// what keeps orphans from accumulating.
pub async fn run_bridge() -> anyhow::Result<()> {
    debug_log("bridge starting");
    let config = match BridgeConfig::from_env() {
        Ok(config) => config,
        Err(err) => {
            debug_log(&format!("bridge env incomplete: {err}"));
            return Err(err);
        }
    };
    let result = serve_bridge(tokio::io::stdin(), tokio::io::stdout(), config).await;
    debug_log(&format!("bridge exiting: {result:?}"));
    result
}

/// The MCP serve loop, generic over its streams so tests can drive it
/// through a duplex pipe.
pub async fn serve_bridge<R, W>(input: R, output: W, config: BridgeConfig) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = StdioReader::new(input);
    let mut writer = StdioWriter::new(output);
    // Lazy: an agent that lists the tools but never calls them should
    // never touch the host. Dropped on failure so the next call
    // reconnects.
    let mut connection: Option<HostConnection> = None;

    while let Some(body) = reader.read_message().await? {
        let framing = reader.framing();
        let message = match JsonRpcMessage::from_bytes(&body) {
            Ok(message) => message,
            Err(err) => {
                let error = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": JsonRpcError::parse_error(err.to_string()),
                });
                write_framed(&mut writer, framing, &error).await?;
                continue;
            }
        };
        let request = match message {
            JsonRpcMessage::Request(request) => request,
            // notifications/initialized and any other notification
            // need no answer; responses we never see (we make no
            // outbound requests).
            JsonRpcMessage::Notification(_) | JsonRpcMessage::Response(_) => continue,
        };

        debug_log(&format!("request: {}", request.method));
        let response = match request.method.as_str() {
            "initialize" => rpc_result(&request, initialize_result()),
            "ping" => rpc_result(&request, serde_json::json!({})),
            "tools/list" => rpc_result(&request, tool_list_result()),
            "tools/call" => {
                let outcome = handle_tools_call(&request, &config, &mut connection).await;
                debug_log(&format!("tools/call outcome: {outcome}"));
                rpc_result(&request, outcome)
            }
            other => serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.id,
                "error": JsonRpcError::method_not_found(other),
            }),
        };
        write_framed(&mut writer, framing, &response).await?;
    }
    Ok(())
}

async fn write_framed<W: AsyncWrite + Unpin>(
    writer: &mut StdioWriter<W>,
    framing: FramingMode,
    message: &serde_json::Value,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(message)?;
    // Answer in the framing the client spoke; MCP clients are NDJSON
    // in practice, but the detection is free.
    match framing {
        FramingMode::ContentLength => writer.write_content_length(&body).await,
        FramingMode::Ndjson | FramingMode::Auto => writer.write_ndjson(&body).await,
    }
}

fn rpc_result(request: &JsonRpcRequest, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": JsonRpcVersion,
        "id": request.id,
        "result": result,
    })
}

/// The tool call itself. Never fails the RPC: a host that is down or
/// a write that is refused comes back as an `isError` tool result the
/// model can read and react to, not a protocol error that some agents
/// treat as fatal.
async fn handle_tools_call(
    request: &JsonRpcRequest,
    config: &BridgeConfig,
    connection: &mut Option<HostConnection>,
) -> serde_json::Value {
    let params = request.params.clone().unwrap_or_default();
    let name = params
        .get("name")
        .and_then(|name| name.as_str())
        .unwrap_or_default();
    let arguments = params.get("arguments").cloned().unwrap_or_default();

    let wire = match wire_request_for(name, &arguments) {
        Ok(wire) => wire,
        Err(message) => return tool_text_result(&message, true),
    };

    // One reconnect: the host restarting between calls is ordinary
    // (the connection is as old as the first call). A retry after a
    // half-delivered edit is safe in the way that matters — if the
    // first attempt actually applied, the retry fails with
    // "old_string was not found" and the model re-reads the file.
    let mut last_error = None;
    for _ in 0..2 {
        let established = match connection.take() {
            Some(established) => established,
            None => match HostConnection::connect(config).await {
                Ok(established) => established,
                Err(err) => {
                    last_error = Some(err);
                    continue;
                }
            },
        };
        let mut established = established;
        match established.call(&wire).await {
            Ok(response) => {
                *connection = Some(established);
                return match (response.ok, response.err) {
                    (Some(message), _) => tool_text_result(&message, false),
                    (None, Some(message)) => tool_text_result(&message, true),
                    (None, None) => tool_text_result("the host sent an empty response", true),
                };
            }
            Err(err) => {
                last_error = Some(err);
            }
        }
    }
    tool_text_result(
        &format!(
            "could not reach the Rebon host that owns this session: {}",
            last_error
                .map(|err| err.to_string())
                .unwrap_or_else(|| "unknown error".into())
        ),
        true,
    )
}

fn wire_request_for(name: &str, arguments: &serde_json::Value) -> Result<WireRequest, String> {
    let string_arg = |key: &str| -> Result<String, String> {
        arguments
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .ok_or_else(|| format!("`{key}` is required and must be a string"))
    };
    match name {
        "write_file" => Ok(WireRequest {
            op: "write_file".into(),
            path: string_arg("path")?,
            content: Some(string_arg("content")?),
            old_string: None,
            new_string: None,
            replace_all: None,
        }),
        "edit_file" => Ok(WireRequest {
            op: "edit_file".into(),
            path: string_arg("path")?,
            content: None,
            old_string: Some(string_arg("old_string")?),
            new_string: Some(string_arg("new_string")?),
            replace_all: arguments.get("replace_all").and_then(|v| v.as_bool()),
        }),
        other => Err(format!(
            "unknown tool `{other}`; this server offers write_file and edit_file"
        )),
    }
}

fn tool_text_result(text: &str, is_error: bool) -> serde_json::Value {
    serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

fn initialize_result() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": FS_MCP_SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// The two tools, with descriptions written for the model that will
/// read them: the MUST is the entire point, because nothing in ACP
/// can force an agent to route writes through the host — only its own
/// tool-selection can.
fn tool_list_result() -> serde_json::Value {
    serde_json::json!({
        "tools": [
            {
                "name": "write_file",
                "description": "Create or overwrite a file through the Rebon host, which \
                    snapshots the previous contents so the user can rewind the change. You \
                    MUST use this tool (or edit_file) for EVERY file you create or modify in \
                    this workspace. NEVER modify files any other way — not with shell \
                    commands (echo, sed, tee, heredocs, python -c) and not with your own \
                    built-in write or edit tools — those writes silently lose the user's \
                    rewind protection. Reading files does not need this server; use your \
                    normal read tools. Writes outside the session's working directories are \
                    refused.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute path of the file to write."
                        },
                        "content": {
                            "type": "string",
                            "description": "Complete new file contents; the file is fully replaced."
                        }
                    },
                    "required": ["path", "content"],
                    "additionalProperties": false
                },
                "annotations": {
                    "readOnlyHint": false,
                    "destructiveHint": true,
                    "idempotentHint": true,
                    "openWorldHint": false
                }
            },
            {
                "name": "edit_file",
                "description": "Replace an exact string in an existing file through the Rebon \
                    host (snapshotted so the user can rewind). You MUST use this tool (or \
                    write_file) for EVERY file modification; never edit files via shell \
                    commands or your own built-in edit tools, or the user loses rewind \
                    protection. old_string must match the file contents exactly, including \
                    whitespace and indentation, and must be unique in the file unless \
                    replace_all is true. The file must already exist — use write_file to \
                    create new files.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute path of the file to edit."
                        },
                        "old_string": {
                            "type": "string",
                            "description": "Exact text to replace, unique in the file unless replace_all is true."
                        },
                        "new_string": {
                            "type": "string",
                            "description": "Text to replace it with."
                        },
                        "replace_all": {
                            "type": "boolean",
                            "description": "Replace every occurrence instead of requiring a unique match.",
                            "default": false
                        }
                    },
                    "required": ["path", "old_string", "new_string"],
                    "additionalProperties": false
                },
                "annotations": {
                    "readOnlyHint": false,
                    "destructiveHint": true,
                    "idempotentHint": false,
                    "openWorldHint": false
                }
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ── apply_edit, the pure part ─────────────────────────────────

    #[test]
    fn apply_edit_replaces_a_unique_match_once() {
        let (updated, count) = apply_edit("a b c", "b", "x", false).unwrap();
        assert_eq!(updated, "a x c");
        assert_eq!(count, 1);
    }

    #[test]
    fn apply_edit_requires_a_unique_match_unless_replace_all() {
        let err = apply_edit("a a", "a", "x", false).unwrap_err();
        assert!(err.contains("matched 2 times"), "{err}");
        assert!(err.contains("replace_all"), "{err}");

        let (updated, count) = apply_edit("a a", "a", "x", true).unwrap();
        assert_eq!(updated, "x x");
        assert_eq!(count, 2);
    }

    #[test]
    fn apply_edit_names_a_missing_match() {
        let err = apply_edit("abc", "zzz", "x", false).unwrap_err();
        assert!(err.contains("was not found"), "{err}");
    }

    #[test]
    fn apply_edit_rejects_empty_and_identical_strings() {
        assert!(apply_edit("abc", "", "x", false)
            .unwrap_err()
            .contains("write_file"));
        assert!(apply_edit("abc", "b", "b", false)
            .unwrap_err()
            .contains("identical"));
    }

    #[test]
    fn apply_edit_matches_lf_input_against_a_crlf_file() {
        // Models emit bare \n; a CRLF file that never matched would
        // make the injected tool useless exactly where it matters.
        let content = "fn main() {\r\n    old();\r\n}\r\n";
        let (updated, _) = apply_edit(
            content,
            "fn main() {\n    old();",
            "fn main() {\n    new();",
            false,
        )
        .expect("LF old_string must match a CRLF file");
        assert_eq!(updated, "fn main() {\r\n    new();\r\n}\r\n");
    }

    #[test]
    fn apply_edit_strips_stray_crlf_for_an_lf_file() {
        let (updated, _) = apply_edit("one\ntwo\n", "one\r\ntwo", "uno\r\ndos", false).unwrap();
        assert_eq!(updated, "uno\ndos\n");
    }

    // ── the host service over real TCP ────────────────────────────

    /// Records snapshot calls and can report an unarmed pipeline.
    #[derive(Default)]
    struct RecordingHistory {
        calls: Mutex<Vec<String>>,
        armed: Mutex<Option<bool>>,
    }

    impl RecordingHistory {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl HostFileHistory for RecordingHistory {
        fn begin_turn(&self, _session_id: &str, _turn_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn snapshot_before_write(&self, session_id: &str, path: &Path) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("snapshot {session_id} {}", path.display()));
            Ok(())
        }
        fn end_turn(&self, _session_id: &str, _turn_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn is_armed(&self) -> Option<bool> {
            *self.armed.lock().unwrap()
        }
    }

    fn scratch_dir(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("rebon-acp-fs-mcp-{name}-"))
            .tempdir()
            .expect("scratch dir")
    }

    const SESSION: &str = "sess-1";

    fn start_service(root: &Path) -> (Arc<RecordingHistory>, HostFsServiceHandle) {
        let history = Arc::new(RecordingHistory::default());
        let handle = HostFsService::start(history.clone(), vec![root.to_path_buf()], SESSION)
            .expect("service starts");
        (history, handle)
    }

    struct TestClient {
        lines: tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
        write: tokio::net::tcp::OwnedWriteHalf,
    }

    impl TestClient {
        async fn connect(port: u16) -> Self {
            let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let (read_half, write_half) = stream.into_split();
            Self {
                lines: BufReader::new(read_half).lines(),
                write: write_half,
            }
        }

        async fn send(&mut self, value: &serde_json::Value) {
            let mut line = serde_json::to_vec(value).unwrap();
            line.push(b'\n');
            self.write.write_all(&line).await.unwrap();
        }

        async fn recv(&mut self) -> Option<WireResponse> {
            self.lines
                .next_line()
                .await
                .ok()
                .flatten()
                .map(|line| serde_json::from_str(&line).unwrap())
        }

        async fn authed(port: u16, token: &str) -> Self {
            let mut client = Self::connect(port).await;
            client
                .send(&serde_json::json!({ "token": token, "session": SESSION }))
                .await;
            let response = client.recv().await.expect("auth response");
            assert!(response.ok.is_some(), "auth should succeed: {response:?}");
            client
        }
    }

    #[tokio::test]
    async fn a_write_through_the_service_is_snapshotted_first() {
        let root = scratch_dir("write");
        let (history, handle) = start_service(root.path());
        let path = root.path().join("note.txt");

        let mut client = TestClient::authed(handle.port(), handle.token()).await;
        client
            .send(&serde_json::json!({
                "op": "write_file",
                "path": path.to_string_lossy(),
                "content": "hello",
            }))
            .await;
        let response = client.recv().await.unwrap();
        assert!(response.ok.is_some(), "{response:?}");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert_eq!(
            history.calls(),
            vec![format!("snapshot {SESSION} {}", path.display())],
            "the snapshot must precede the write"
        );
    }

    #[tokio::test]
    async fn a_wrong_token_or_session_is_refused() {
        let root = scratch_dir("auth");
        let (history, handle) = start_service(root.path());

        let mut client = TestClient::connect(handle.port()).await;
        client
            .send(&serde_json::json!({ "token": "wrong", "session": SESSION }))
            .await;
        let response = client.recv().await.unwrap();
        assert_eq!(response.err.as_deref(), Some("unauthorized"));

        let mut client = TestClient::connect(handle.port()).await;
        client
            .send(&serde_json::json!({ "token": handle.token(), "session": "other" }))
            .await;
        let response = client.recv().await.unwrap();
        assert_eq!(response.err.as_deref(), Some("unauthorized"));

        assert!(history.calls().is_empty());
    }

    #[tokio::test]
    async fn a_write_outside_the_roots_is_refused_without_a_snapshot() {
        let root = scratch_dir("roots");
        let (history, handle) = start_service(root.path());
        let elsewhere = scratch_dir("escape");
        let outside = elsewhere.path().join("escape.txt");

        let mut client = TestClient::authed(handle.port(), handle.token()).await;
        client
            .send(&serde_json::json!({
                "op": "write_file",
                "path": outside.to_string_lossy(),
                "content": "nope",
            }))
            .await;
        let response = client.recv().await.unwrap();
        assert!(
            response
                .err
                .is_some_and(|err| err.contains("outside the session")),
            "escapes must be refused"
        );
        assert!(history.calls().is_empty());
        assert!(!outside.exists());
    }

    #[tokio::test]
    async fn an_edit_goes_through_the_same_snapshot_pipeline() {
        let root = scratch_dir("edit");
        let (history, handle) = start_service(root.path());
        let path = root.path().join("main.rs");
        std::fs::write(&path, "fn main() { old(); }").unwrap();

        let mut client = TestClient::authed(handle.port(), handle.token()).await;
        client
            .send(&serde_json::json!({
                "op": "edit_file",
                "path": path.to_string_lossy(),
                "old_string": "old()",
                "new_string": "new()",
            }))
            .await;
        let response = client.recv().await.unwrap();
        assert!(
            response
                .ok
                .as_deref()
                .is_some_and(|ok| ok.contains("Replaced 1")),
            "{response:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn main() { new(); }"
        );
        assert_eq!(history.calls().len(), 1);
    }

    #[tokio::test]
    async fn editing_a_missing_file_points_at_write_file() {
        let root = scratch_dir("edit-missing");
        let (_history, handle) = start_service(root.path());

        let mut client = TestClient::authed(handle.port(), handle.token()).await;
        client
            .send(&serde_json::json!({
                "op": "edit_file",
                "path": root.path().join("ghost.txt").to_string_lossy(),
                "old_string": "a",
                "new_string": "b",
            }))
            .await;
        let response = client.recv().await.unwrap();
        assert!(
            response.err.is_some_and(|err| err.contains("write_file")),
            "the error should route the model to the tool that creates files"
        );
    }

    #[tokio::test]
    async fn a_write_outside_a_turn_succeeds_but_says_so() {
        // Refusing would push the agent back to writing disk itself;
        // the honest move is to write, warn, and tell the model.
        let root = scratch_dir("unarmed");
        let (history, handle) = start_service(root.path());
        *history.armed.lock().unwrap() = Some(false);

        let mut client = TestClient::authed(handle.port(), handle.token()).await;
        client
            .send(&serde_json::json!({
                "op": "write_file",
                "path": root.path().join("late.txt").to_string_lossy(),
                "content": "tail write",
            }))
            .await;
        let response = client.recv().await.unwrap();
        assert!(
            response
                .ok
                .as_deref()
                .is_some_and(|ok| ok.contains("not covered by /rewind")),
            "{response:?}"
        );
        assert!(root.path().join("late.txt").exists());
    }

    #[tokio::test]
    async fn the_service_takes_reconnects_and_concurrent_connections() {
        let root = scratch_dir("reconnect");
        let (_history, handle) = start_service(root.path());

        let first = TestClient::authed(handle.port(), handle.token()).await;
        drop(first.write);
        // A second connection after the first died, plus a concurrent
        // pair — an agent restarting its bridge must not be locked out.
        let mut second = TestClient::authed(handle.port(), handle.token()).await;
        let mut third = TestClient::authed(handle.port(), handle.token()).await;
        for (n, client) in [(1u32, &mut second), (2, &mut third)] {
            client
                .send(&serde_json::json!({
                    "op": "write_file",
                    "path": root.path().join(format!("{n}.txt")).to_string_lossy(),
                    "content": "x",
                }))
                .await;
            assert!(client.recv().await.unwrap().ok.is_some());
        }
    }

    #[tokio::test]
    async fn dropping_the_handle_closes_the_port() {
        let root = scratch_dir("drop");
        let (_history, handle) = start_service(root.path());
        let port = handle.port();
        drop(handle);
        // The abort is asynchronous, so wait for the port to actually go
        // away rather than sleeping a fixed 50ms and asserting once. A
        // machine that is merely busy — this suite runs many tests in
        // parallel — can take longer than that to tear the listener down,
        // and the test then reports a leak that is not there. Polling keeps
        // a real leak failing, at the cost of a slow pass.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut closed = false;
        while std::time::Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", port)).await.is_err() {
                closed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            closed,
            "a dropped session must not leave its fs port listening"
        );
    }

    // ── the bridge over a duplex pipe ─────────────────────────────

    async fn drive_bridge(
        config: BridgeConfig,
        requests: Vec<serde_json::Value>,
    ) -> Vec<serde_json::Value> {
        let (mut agent_side, bridge_in) = tokio::io::duplex(64 * 1024);
        let (bridge_out, server_side) = tokio::io::duplex(64 * 1024);
        let bridge = tokio::spawn(serve_bridge(bridge_in, bridge_out, config));

        for request in &requests {
            let mut line = serde_json::to_vec(request).unwrap();
            line.push(b'\n');
            agent_side.write_all(&line).await.unwrap();
        }
        drop(agent_side); // EOF: the bridge must exit cleanly.

        let mut responses = Vec::new();
        let mut lines = BufReader::new(server_side).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            responses.push(serde_json::from_str(&line).unwrap());
        }
        bridge
            .await
            .expect("bridge task")
            .expect("stdin EOF is a clean exit");
        responses
    }

    fn bridge_config(port: u16, token: &str) -> BridgeConfig {
        BridgeConfig {
            port,
            token: token.to_string(),
            session: SESSION.to_string(),
        }
    }

    fn rpc(id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    #[tokio::test]
    async fn the_bridge_answers_initialize_and_tools_list_without_a_host() {
        // Port 1 is closed; listing tools must not need the host.
        let responses = drive_bridge(
            bridge_config(1, "unused"),
            vec![
                rpc(1, "initialize", serde_json::json!({})),
                serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                rpc(2, "tools/list", serde_json::json!({})),
            ],
        )
        .await;

        assert_eq!(responses.len(), 2, "the notification gets no reply");
        assert_eq!(
            responses[0]["result"]["serverInfo"]["name"],
            FS_MCP_SERVER_NAME
        );
        let tools = responses[1]["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["write_file", "edit_file"]);
        for tool in tools {
            let description = tool["description"].as_str().unwrap();
            assert!(
                description.contains("MUST"),
                "the imperative is the whole point: {description}"
            );
        }
    }

    #[tokio::test]
    async fn a_tool_call_reaches_the_host_and_writes_the_file() {
        let root = scratch_dir("bridge-write");
        let (history, handle) = start_service(root.path());
        let path = root.path().join("via-bridge.txt");

        let responses = drive_bridge(
            bridge_config(handle.port(), handle.token()),
            vec![rpc(
                1,
                "tools/call",
                serde_json::json!({
                    "name": "write_file",
                    "arguments": { "path": path.to_string_lossy(), "content": "from the agent" },
                }),
            )],
        )
        .await;

        assert_eq!(responses[0]["result"]["isError"], false, "{responses:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "from the agent");
        assert_eq!(history.calls().len(), 1, "snapshotted like any host write");
    }

    #[tokio::test]
    async fn an_unreachable_host_is_a_tool_error_not_a_crash() {
        let responses = drive_bridge(
            bridge_config(1, "unused"),
            vec![rpc(
                1,
                "tools/call",
                serde_json::json!({
                    "name": "write_file",
                    "arguments": { "path": "/tmp/x", "content": "x" },
                }),
            )],
        )
        .await;

        assert_eq!(responses[0]["result"]["isError"], true);
        let text = responses[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("could not reach"), "{text}");
    }

    #[tokio::test]
    async fn unknown_tools_and_methods_fail_without_touching_the_host() {
        let responses = drive_bridge(
            bridge_config(1, "unused"),
            vec![
                rpc(
                    1,
                    "tools/call",
                    serde_json::json!({ "name": "delete_everything" }),
                ),
                rpc(2, "resources/list", serde_json::json!({})),
                rpc(3, "ping", serde_json::json!({})),
            ],
        )
        .await;

        assert_eq!(responses[0]["result"]["isError"], true);
        assert_eq!(
            responses[1]["error"]["code"],
            rebon_proto::error_code::METHOD_NOT_FOUND
        );
        assert!(responses[2]["result"].is_object());
    }

    #[tokio::test]
    async fn malformed_json_gets_a_parse_error_and_the_loop_survives() {
        let (mut agent_side, bridge_in) = tokio::io::duplex(4096);
        let (bridge_out, server_side) = tokio::io::duplex(4096);
        let bridge = tokio::spawn(serve_bridge(bridge_in, bridge_out, bridge_config(1, "t")));

        agent_side.write_all(b"{not json}\n").await.unwrap();
        let list = serde_json::to_vec(&rpc(1, "tools/list", serde_json::json!({}))).unwrap();
        agent_side.write_all(&list).await.unwrap();
        agent_side.write_all(b"\n").await.unwrap();
        drop(agent_side);

        let mut lines = BufReader::new(server_side).lines();
        let first: serde_json::Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(first["error"]["code"], rebon_proto::error_code::PARSE_ERROR);
        let second: serde_json::Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert!(
            second["result"]["tools"].is_array(),
            "the loop must survive"
        );
        bridge.await.unwrap().unwrap();
    }
}
