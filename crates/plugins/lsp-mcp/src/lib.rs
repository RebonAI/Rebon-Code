use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use rebon_tools_core::ProcessTreeGuard;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex, Notify};
use tokio::task::JoinHandle;

mod daemon;

pub use daemon::{default_daemon_idle_ttl, run_rust_daemon};

pub const RUST_LSP_SERVER_NAME: &str = "rust_lsp";

/// `CREATE_NO_WINDOW` — prevents a visible console on Windows.
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const RUST_ANALYZER_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a diagnostics call waits for rust-analyzer to publish.
/// Non-empty diagnostics return immediately; the full window is only
/// burned to confirm a clean file, and it must outlast a check-on-save
/// (`cargo check`) round trip or first calls always come back empty.
const DEFAULT_DIAGNOSTICS_TIMEOUT: Duration = Duration::from_secs(10);
/// rust-analyzer answers `-32801 content modified` while a change is
/// still being analyzed; the request is safe to retry.
const CONTENT_MODIFIED_RETRIES: usize = 4;
const CONTENT_MODIFIED_RETRY_DELAY: Duration = Duration::from_millis(500);
const LSP_CONTENT_MODIFIED_CODE: i64 = -32801;
const DEFAULT_MAX_TOOL_OUTPUT_CHARS: usize = 16_000;
const MAX_COMMAND_OUTPUT_CHARS: usize = 2048;
const MAX_LSP_HEADER_BYTES: usize = 8 * 1024;
const MAX_LSP_CONTENT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct RustServerOptions {
    pub workspace: PathBuf,
    pub rust_analyzer: String,
    pub rust_analyzer_args: Vec<String>,
    pub request_timeout: Duration,
    pub diagnostics_timeout: Duration,
    pub max_tool_output_chars: usize,
}

impl RustServerOptions {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            rust_analyzer: "rust-analyzer".to_string(),
            rust_analyzer_args: Vec::new(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            diagnostics_timeout: DEFAULT_DIAGNOSTICS_TIMEOUT,
            max_tool_output_chars: DEFAULT_MAX_TOOL_OUTPUT_CHARS,
        }
    }

    pub fn for_current_dir() -> anyhow::Result<Self> {
        Ok(Self::new(
            std::env::current_dir().context("failed to read current directory")?,
        ))
    }

    pub(crate) fn canonicalized(mut self) -> anyhow::Result<Self> {
        self.workspace = self
            .workspace
            .canonicalize()
            .with_context(|| format!("failed to resolve workspace {}", self.workspace.display()))?;
        Ok(self)
    }
}

pub async fn run_rust_server(options: RustServerOptions) -> anyhow::Result<()> {
    let options = options.canonicalized()?;
    let max_output_chars = options.max_tool_output_chars;
    let factory: BackendFactory = Box::new(move || {
        let options = options.clone();
        Box::pin(async move { build_bridge_backend(options).await })
    });
    run_mcp_server(
        tokio::io::stdin(),
        tokio::io::stdout(),
        factory,
        max_output_chars,
    )
    .await
}

/// Resolve the backend for one bridge: the per-workspace shared daemon
/// when possible, a session-local rust-analyzer otherwise.
async fn build_bridge_backend(options: RustServerOptions) -> anyhow::Result<Box<dyn LspBackend>> {
    if !daemon::sharing_disabled() {
        match daemon::connect_or_spawn_daemon(&options).await {
            Ok(remote) => return Ok(Box::new(remote) as Box<dyn LspBackend>),
            Err(err) => {
                tracing::warn!(
                    error = %format!("{err:#}"),
                    "shared rust-analyzer daemon unavailable; falling back to a per-session rust-analyzer"
                );
            }
        }
    }
    let backend = RustAnalyzerBackend::spawn(options).await?;
    Ok(Box::new(backend) as Box<dyn LspBackend>)
}

pub(crate) type BackendFuture =
    Pin<Box<dyn Future<Output = anyhow::Result<Box<dyn LspBackend>>> + Send>>;
pub(crate) type BackendFactory = Box<dyn Fn() -> BackendFuture + Send>;

#[async_trait]
pub(crate) trait LspBackend: Send {
    async fn diagnostics(&mut self, file_path: &str) -> anyhow::Result<Value>;
    async fn hover(&mut self, file_path: &str, line: u64, character: u64) -> anyhow::Result<Value>;
    async fn shutdown(&mut self);
}

struct McpServer {
    backend: Option<Box<dyn LspBackend>>,
    backend_factory: BackendFactory,
    max_output_chars: usize,
}

impl McpServer {
    fn new(backend_factory: BackendFactory, max_output_chars: usize) -> Self {
        Self {
            backend: None,
            backend_factory,
            max_output_chars,
        }
    }

    async fn handle_message(&mut self, message: Value) -> Option<Value> {
        let id = message.get("id").cloned();
        let method = match message.get("method").and_then(Value::as_str) {
            Some(method) => method,
            None => {
                return id.map(|id| jsonrpc_error(id, -32600, "invalid JSON-RPC request"));
            }
        };

        match method {
            "initialize" => match id {
                Some(id) => Some(self.handle_initialize(id).await),
                None => None,
            },
            "notifications/initialized" => None,
            "tools/list" => id.map(|id| self.handle_tools_list(id)),
            "tools/call" => match id {
                Some(id) => Some(
                    self.handle_tools_call(id, message.get("params").cloned())
                        .await,
                ),
                None => None,
            },
            _ => id.map(|id| jsonrpc_error(id, -32601, format!("unknown method `{method}`"))),
        }
    }

    async fn handle_initialize(&mut self, id: Value) -> Value {
        // Deliberately does NOT create the backend: spawning rust-analyzer
        // costs gigabytes of resident memory per workspace index, and most
        // sessions never call a Rust LSP tool. The backend is created on the
        // first tools/call instead (see `ensure_backend`).
        jsonrpc_result(id, mcp_initialize_result())
    }

    fn handle_tools_list(&self, id: Value) -> Value {
        // Tool definitions are static — answering tools/list must not boot
        // the backend either, since clients list tools on every session.
        jsonrpc_result(id, rust_tool_definitions())
    }

    async fn ensure_backend(&mut self) -> anyhow::Result<&mut dyn LspBackend> {
        if self.backend.is_none() {
            // The factory stays reusable so a failed spawn (e.g.
            // rust-analyzer not installed yet) can be retried on a later
            // call instead of poisoning the whole session.
            self.backend = Some((self.backend_factory)().await?);
        }
        Ok(self
            .backend
            .as_mut()
            .expect("backend was just initialized")
            .as_mut())
    }

    async fn handle_tools_call(&mut self, id: Value, params: Option<Value>) -> Value {
        let params = params.unwrap_or_else(|| json!({}));
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return jsonrpc_error(id, -32602, "tools/call requires string `name`");
        };
        if !matches!(name, "diagnostics" | "hover") {
            return jsonrpc_error(id, -32602, format!("unknown Rust LSP tool `{name}`"));
        }
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !arguments.is_object() {
            return jsonrpc_error(id, -32602, "tools/call `arguments` must be an object");
        }

        let max_output_chars = self.max_output_chars;
        let backend = match self.ensure_backend().await {
            Ok(backend) => backend,
            Err(err) => {
                return jsonrpc_result(
                    id,
                    tool_text_result(
                        json!({ "error": format!("Rust LSP backend failed to start: {err:#}") }),
                        true,
                        max_output_chars,
                    ),
                );
            }
        };

        let result = match name {
            "diagnostics" => match serde_json::from_value::<DiagnosticsArgs>(arguments) {
                Ok(args) => backend.diagnostics(&args.file_path).await,
                Err(err) => Err(anyhow!("invalid diagnostics arguments: {err}")),
            },
            "hover" => match serde_json::from_value::<HoverArgs>(arguments) {
                Ok(args) => {
                    backend
                        .hover(&args.file_path, args.line, args.character)
                        .await
                }
                Err(err) => Err(anyhow!("invalid hover arguments: {err}")),
            },
            _ => unreachable!("tool name validated above"),
        };

        match result {
            Ok(value) => jsonrpc_result(id, tool_text_result(value, false, self.max_output_chars)),
            Err(err) => jsonrpc_result(
                id,
                tool_text_result(
                    json!({ "error": err.to_string() }),
                    true,
                    self.max_output_chars,
                ),
            ),
        }
    }

    async fn shutdown(&mut self) {
        if let Some(backend) = self.backend.as_mut() {
            backend.shutdown().await;
        }
        self.backend = None;
    }
}

async fn run_mcp_server<R, W>(
    input: R,
    output: W,
    backend_factory: BackendFactory,
    max_output_chars: usize,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut server = McpServer::new(backend_factory, max_output_chars);
    let output = Arc::new(Mutex::new(output));
    let mut lines = BufReader::new(input).lines();

    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let response = match serde_json::from_str::<Value>(trimmed) {
                    Ok(message) => server.handle_message(message).await,
                    Err(err) => Some(jsonrpc_error(
                        Value::Null,
                        -32700,
                        format!("parse error: {err}"),
                    )),
                };
                if let Some(response) = response {
                    write_ndjson(&output, &response).await?;
                }
            }
            Ok(None) => break,
            Err(err) => return Err(err).context("failed to read MCP request"),
        }
    }

    server.shutdown().await;
    Ok(())
}

async fn write_ndjson<W>(output: &Arc<Mutex<W>>, message: &Value) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let mut output = output.lock().await;
    output.write_all(&bytes).await?;
    output.flush().await
}

fn jsonrpc_result(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn jsonrpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
        },
    })
}

fn mcp_initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {
            "tools": {
                "listChanged": false,
            },
        },
        "serverInfo": {
            "name": RUST_LSP_SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

fn rust_tool_definitions() -> Value {
    json!({
        "tools": [
            {
                "name": "diagnostics",
                "description": "Return rust-analyzer diagnostics for a Rust file inside the workspace.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "Path to a Rust file under the current workspace. Relative paths resolve from the workspace root."
                        }
                    },
                    "required": ["file_path"],
                    "additionalProperties": false
                },
                "annotations": {
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "openWorldHint": false
                },
                "_meta": {
                    "anthropic/alwaysLoad": true,
                    "anthropic/searchHint": "rust diagnostics lsp rust-analyzer"
                }
            },
            {
                "name": "hover",
                "description": "Return rust-analyzer hover information for a position in a Rust file inside the workspace.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "Path to a Rust file under the current workspace. Relative paths resolve from the workspace root."
                        },
                        "line": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Zero-based line number."
                        },
                        "character": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Zero-based UTF-16 character offset."
                        }
                    },
                    "required": ["file_path", "line", "character"],
                    "additionalProperties": false
                },
                "annotations": {
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "openWorldHint": false
                },
                "_meta": {
                    "anthropic/alwaysLoad": true,
                    "anthropic/searchHint": "rust hover lsp rust-analyzer type signature"
                }
            }
        ]
    })
}

fn tool_text_result(value: Value, is_error: bool, max_chars: usize) -> Value {
    let text = serde_json::to_string_pretty(&value).expect("serde_json::Value always serializes");
    json!({
        "content": [{
            "type": "text",
            "text": truncate_chars(&text, max_chars),
        }],
        "isError": is_error,
    })
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(32);
    let mut truncated = text.chars().take(keep).collect::<String>();
    truncated.push_str("\n...[truncated]");
    truncated
}

#[derive(Debug, Deserialize)]
struct DiagnosticsArgs {
    file_path: String,
}

#[derive(Debug, Deserialize)]
struct HoverArgs {
    file_path: String,
    line: u64,
    character: u64,
}

pub(crate) struct RustAnalyzerBackend {
    workspace: PathBuf,
    client: Arc<LspClient<ChildStdin>>,
    child: Option<Child>,
    /// Kills every descendant of the spawned command when dropped.
    /// `rust-analyzer` on PATH is usually a rustup proxy that re-execs
    /// the real toolchain binary as a grandchild, so killing only the
    /// direct child orphans a multi-gigabyte indexer.
    process_tree: Option<ProcessTreeGuard>,
    reader_handle: Option<JoinHandle<()>>,
    stderr_handle: Option<JoinHandle<()>>,
    diagnostics_timeout: Duration,
}

/// Guard the rust-analyzer process tree.
///
/// The guard itself lives in `rebon-tools-core`; what stays here is how
/// this caller reaches the child's raw identifier, and what it calls a
/// child that has none. No breakaway: nothing rust-analyzer starts may
/// opt out of being reaped with it.
#[cfg(windows)]
fn guard_process_tree(child: &Child) -> io::Result<ProcessTreeGuard> {
    let handle = child
        .raw_handle()
        .ok_or_else(|| io::Error::other("child process has no process handle"))?;
    ProcessTreeGuard::for_raw_handle(handle, false)
}

#[cfg(unix)]
fn guard_process_tree(child: &Child) -> io::Result<ProcessTreeGuard> {
    let process_id = child
        .id()
        .ok_or_else(|| io::Error::other("child process has no pid"))?;
    ProcessTreeGuard::for_process_group(process_id)
}

/// Make the spawned command its own process-group leader so the whole
/// tree can be signalled at once. Windows needs no spawn-time setup —
/// job membership is inherited automatically.
#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
fn configure_process_group(_command: &mut Command) {}

impl RustAnalyzerBackend {
    pub(crate) async fn spawn(options: RustServerOptions) -> anyhow::Result<Self> {
        let workspace = options.workspace;
        ensure_rust_analyzer_available(&options.rust_analyzer, &workspace).await?;

        let mut command = Command::new(&options.rust_analyzer);
        command
            .args(&options.rust_analyzer_args)
            .current_dir(&workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        configure_process_group(&mut command);

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn `{}`", options.rust_analyzer))?;
        let process_tree = match guard_process_tree(&child) {
            Ok(guard) => Some(guard),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "rust-lsp: failed to guard rust-analyzer process tree; descendants may outlive this server"
                );
                None
            }
        };
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("rust-analyzer child has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("rust-analyzer child has no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("rust-analyzer child has no stderr"))?;

        let (client, reader_handle) = LspClient::spawn(stdout, stdin, options.request_timeout);
        let stderr_handle = tokio::spawn(drain_stderr(stderr));
        let mut backend = Self {
            workspace,
            client,
            child: Some(child),
            process_tree,
            reader_handle: Some(reader_handle),
            stderr_handle: Some(stderr_handle),
            diagnostics_timeout: options.diagnostics_timeout,
        };

        if let Err(err) = backend.client.initialize(&backend.workspace).await {
            backend.shutdown().await;
            return Err(err).context("rust-analyzer initialize failed");
        }

        Ok(backend)
    }

    fn resolve_workspace_file(&self, file_path: &str) -> anyhow::Result<PathBuf> {
        if file_path.trim().is_empty() {
            bail!("file_path must not be empty");
        }
        let raw = PathBuf::from(file_path);
        let candidate = if raw.is_absolute() {
            raw
        } else {
            self.workspace.join(raw)
        };
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("failed to resolve file_path {}", candidate.display()))?;
        if !canonical.starts_with(&self.workspace) {
            bail!(
                "file_path {} is outside workspace {}",
                canonical.display(),
                self.workspace.display()
            );
        }
        Ok(canonical)
    }
}

async fn ensure_rust_analyzer_available(command: &str, workspace: &Path) -> anyhow::Result<()> {
    let mut child = Command::new(command);
    child
        .arg("--version")
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    child.creation_flags(CREATE_NO_WINDOW);

    let child = child
        .spawn()
        .with_context(|| format_rust_analyzer_spawn_failure(command))?;
    let output = tokio::time::timeout(RUST_ANALYZER_CHECK_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| anyhow!(format_rust_analyzer_check_timeout(command)))?
        .with_context(|| format!("failed to read `{command} --version` output"))?;
    if !output.status.success() {
        bail!(
            "{}",
            format_rust_analyzer_unavailable(
                command,
                &output.status.to_string(),
                &output.stdout,
                &output.stderr,
            )
        );
    }
    Ok(())
}

fn format_rust_analyzer_spawn_failure(command: &str) -> String {
    format!(
        "rust-analyzer is unavailable: failed to spawn `{command}`; install it with `rustup component add rust-analyzer` or put a working rust-analyzer on PATH"
    )
}

fn format_rust_analyzer_check_timeout(command: &str) -> String {
    format!(
        "rust-analyzer is unavailable: `{command} --version` timed out after {}s; install it with `rustup component add rust-analyzer` or put a working rust-analyzer on PATH",
        RUST_ANALYZER_CHECK_TIMEOUT.as_secs()
    )
}

fn format_rust_analyzer_unavailable(
    command: &str,
    status: &str,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    let output = process_output_summary(stdout, stderr);
    if output.is_empty() {
        return format!(
            "rust-analyzer is unavailable: `{command} --version` exited with {status}; install it with `rustup component add rust-analyzer` or put a working rust-analyzer on PATH"
        );
    }
    format!(
        "rust-analyzer is unavailable: `{command} --version` exited with {status}; {output}; install it with `rustup component add rust-analyzer` or put a working rust-analyzer on PATH"
    )
}

fn process_output_summary(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = compact_process_output(stdout);
    let stderr = compact_process_output(stderr);
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("stdout: {stdout}"),
        (true, false) => format!("stderr: {stderr}"),
        (false, false) => format!("stdout: {stdout}; stderr: {stderr}"),
    }
}

fn compact_process_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut compact = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if compact.chars().count() > MAX_COMMAND_OUTPUT_CHARS {
        compact = compact
            .chars()
            .take(MAX_COMMAND_OUTPUT_CHARS.saturating_sub(16))
            .collect::<String>();
        compact.push_str("...[truncated]");
    }
    compact
}

#[async_trait]
impl LspBackend for RustAnalyzerBackend {
    async fn diagnostics(&mut self, file_path: &str) -> anyhow::Result<Value> {
        let file = self.resolve_workspace_file(file_path)?;
        let diagnostics = self
            .client
            .diagnostics_for_file(&file, self.diagnostics_timeout)
            .await?;
        Ok(json!({
            "file_path": file.display().to_string(),
            "diagnostics": diagnostics,
        }))
    }

    async fn hover(&mut self, file_path: &str, line: u64, character: u64) -> anyhow::Result<Value> {
        let file = self.resolve_workspace_file(file_path)?;
        let hover = self.client.hover(&file, line, character).await?;
        Ok(json!({
            "file_path": file.display().to_string(),
            "line": line,
            "character": character,
            "hover": hover,
        }))
    }

    async fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        // `Child::kill` only reaches the direct child (often a rustup
        // proxy); reap the real rust-analyzer and any other
        // descendants too.
        if let Some(mut tree) = self.process_tree.take() {
            if let Err(err) = tree.terminate() {
                tracing::debug!(error = %err, "rust-analyzer: could not terminate the process tree");
            }
        }
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
            let _ = handle.await;
        }
        if let Some(handle) = self.stderr_handle.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

async fn drain_stderr(stderr: tokio::process::ChildStderr) {
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
                    tracing::debug!(stderr = %truncated, "rust-analyzer stderr");
                    logged += 1;
                }
            }
            Ok(None) => break,
            Err(err) => {
                tracing::debug!(error = %err, "rust-analyzer stderr drain failed");
                break;
            }
        }
    }
}

#[derive(Debug)]
struct LspClient<W> {
    writer: Mutex<W>,
    pending: Mutex<HashMap<i64, oneshot::Sender<LspResponse>>>,
    opened_versions: Mutex<HashMap<String, i32>>,
    diagnostics: Mutex<HashMap<String, DiagnosticsEntry>>,
    diagnostics_notify: Notify,
    diagnostics_epoch: AtomicU64,
    next_id: AtomicI64,
    request_timeout: Duration,
}

#[derive(Debug, Clone)]
struct DiagnosticsEntry {
    diagnostics: Vec<Value>,
    epoch: u64,
}

#[derive(Debug)]
struct LspResponse {
    result: Option<Value>,
    error: Option<Value>,
}

impl<W> LspClient<W>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    fn spawn<R>(reader: R, writer: W, request_timeout: Duration) -> (Arc<Self>, JoinHandle<()>)
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let client = Arc::new(Self {
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            opened_versions: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(HashMap::new()),
            diagnostics_notify: Notify::new(),
            diagnostics_epoch: AtomicU64::new(0),
            next_id: AtomicI64::new(1),
            request_timeout,
        });
        let reader_client = Arc::clone(&client);
        let handle = tokio::spawn(async move {
            run_lsp_reader(reader_client, reader).await;
        });
        (client, handle)
    }

    async fn initialize(&self, workspace: &Path) -> anyhow::Result<()> {
        let root_uri = path_to_file_uri(workspace);
        let name = workspace
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace");
        self.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "capabilities": {},
                "workspaceFolders": [{
                    "uri": root_uri,
                    "name": name,
                }],
            }),
        )
        .await?;
        self.notification("initialized", json!({})).await?;
        Ok(())
    }

    async fn diagnostics_for_file(
        &self,
        file: &Path,
        diagnostics_timeout: Duration,
    ) -> anyhow::Result<Vec<Value>> {
        let uri = path_to_file_uri(file);
        let before = self.diagnostics_epoch.load(Ordering::Acquire);
        self.open_or_refresh_file(file, &uri).await?;
        // Compiler diagnostics (source "rustc") only refresh through
        // check-on-save; the text sent above was read from disk, so the
        // save notification is truthful.
        self.notification(
            "textDocument/didSave",
            json!({ "textDocument": { "uri": uri } }),
        )
        .await?;
        let deadline = tokio::time::Instant::now() + diagnostics_timeout;

        loop {
            if let Some(entry) = self.diagnostics.lock().await.get(&uri).cloned() {
                // A fresh non-empty publish is definitive. A fresh EMPTY one
                // is not — rust-analyzer publishes its (often empty) native
                // diagnostics well before the check-on-save results arrive,
                // so returning on the first publish would miss every
                // compiler error. Empty answers wait out the full window.
                if entry.epoch > before && !entry.diagnostics.is_empty() {
                    return Ok(entry.diagnostics);
                }
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            if tokio::time::timeout(remaining, self.diagnostics_notify.notified())
                .await
                .is_err()
            {
                break;
            }
        }

        Ok(self
            .diagnostics
            .lock()
            .await
            .get(&uri)
            .map(|entry| entry.diagnostics.clone())
            .unwrap_or_default())
    }

    async fn hover(&self, file: &Path, line: u64, character: u64) -> anyhow::Result<Value> {
        let uri = path_to_file_uri(file);
        self.open_or_refresh_file(file, &uri).await?;
        let params = json!({
            "textDocument": { "uri": uri },
            "position": {
                "line": line,
                "character": character,
            },
        });
        let mut last_err = None;
        for attempt in 0..CONTENT_MODIFIED_RETRIES {
            match self.request("textDocument/hover", params.clone()).await {
                Ok(value) => return Ok(value),
                Err(err) if is_content_modified_error(&err) => {
                    last_err = Some(err);
                    if attempt + 1 < CONTENT_MODIFIED_RETRIES {
                        tokio::time::sleep(CONTENT_MODIFIED_RETRY_DELAY).await;
                    }
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_err.expect("retry loop ran at least once"))
    }

    async fn open_or_refresh_file(&self, file: &Path, uri: &str) -> anyhow::Result<()> {
        let text = tokio::fs::read_to_string(file)
            .await
            .with_context(|| format!("failed to read {}", file.display()))?;
        let (method, params) = {
            let mut opened = self.opened_versions.lock().await;
            match opened.get_mut(uri) {
                Some(version) => {
                    *version += 1;
                    (
                        "textDocument/didChange",
                        json!({
                            "textDocument": {
                                "uri": uri,
                                "version": *version,
                            },
                            "contentChanges": [{ "text": text }],
                        }),
                    )
                }
                None => {
                    opened.insert(uri.to_string(), 1);
                    (
                        "textDocument/didOpen",
                        json!({
                            "textDocument": {
                                "uri": uri,
                                "languageId": "rust",
                                "version": 1,
                                "text": text,
                            },
                        }),
                    )
                }
            }
        };
        self.notification(method, params).await
    }

    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        if let Err(err) = self.write_lsp_json(&frame).await {
            self.pending.lock().await.remove(&id);
            return Err(err).with_context(|| format!("failed to write LSP request `{method}`"));
        }

        let response = match tokio::time::timeout(self.request_timeout, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => bail!("LSP response channel dropped for `{method}`"),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("LSP request `{method}` timed out");
            }
        };
        if let Some(error) = response.error {
            bail!(
                "LSP request `{method}` failed: {}",
                lsp_error_message(&error)
            );
        }
        Ok(response.result.unwrap_or(Value::Null))
    }

    async fn notification(&self, method: &str, params: Value) -> anyhow::Result<()> {
        let frame = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_lsp_json(&frame)
            .await
            .with_context(|| format!("failed to write LSP notification `{method}`"))
    }

    async fn write_lsp_json(&self, value: &Value) -> io::Result<()> {
        let mut writer = self.writer.lock().await;
        write_lsp_json(&mut *writer, value).await
    }
}

async fn run_lsp_reader<R, W>(client: Arc<LspClient<W>>, mut reader: R)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    loop {
        match read_lsp_json(&mut reader).await {
            Ok(Some(message)) => handle_lsp_message(&client, message).await,
            Ok(None) => {
                fail_lsp_pending(&client, "LSP reader reached EOF").await;
                break;
            }
            Err(err) => {
                fail_lsp_pending(&client, &format!("LSP reader failed: {err}")).await;
                break;
            }
        }
    }
}

async fn handle_lsp_message<W>(client: &Arc<LspClient<W>>, message: Value)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    if message
        .get("method")
        .and_then(Value::as_str)
        .is_some_and(|method| method == "textDocument/publishDiagnostics")
    {
        if let Some(params) = message.get("params") {
            handle_publish_diagnostics(client, params).await;
        }
        return;
    }

    let Some(id) = message.get("id").and_then(Value::as_i64) else {
        return;
    };
    let response = LspResponse {
        result: message.get("result").cloned(),
        error: message.get("error").cloned(),
    };
    if let Some(tx) = client.pending.lock().await.remove(&id) {
        let _ = tx.send(response);
    }
}

async fn handle_publish_diagnostics<W>(client: &Arc<LspClient<W>>, params: &Value)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let Some(uri) = params.get("uri").and_then(Value::as_str) else {
        return;
    };
    let uri = normalize_file_uri(uri);
    let diagnostics = params
        .get("diagnostics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let epoch = client.diagnostics_epoch.fetch_add(1, Ordering::AcqRel) + 1;
    client
        .diagnostics
        .lock()
        .await
        .insert(uri, DiagnosticsEntry { diagnostics, epoch });
    client.diagnostics_notify.notify_waiters();
}

async fn fail_lsp_pending<W>(client: &Arc<LspClient<W>>, message: &str)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let pending = std::mem::take(&mut *client.pending.lock().await);
    for (_, tx) in pending {
        let _ = tx.send(LspResponse {
            result: None,
            error: Some(json!({ "message": message })),
        });
    }
}

fn lsp_error_message(error: &Value) -> String {
    if let Some(message) = error.get("message").and_then(Value::as_str) {
        if let Some(code) = error.get("code").and_then(Value::as_i64) {
            return format!("{message} ({code})");
        }
        return message.to_string();
    }
    error.to_string()
}

/// Matches the `({code})` suffix that [`lsp_error_message`] renders for
/// `-32801 ContentModified` responses.
fn is_content_modified_error(err: &anyhow::Error) -> bool {
    err.to_string()
        .contains(&format!("({LSP_CONTENT_MODIFIED_CODE})"))
}

async fn read_lsp_json<R>(reader: &mut R) -> io::Result<Option<Value>>
where
    R: AsyncRead + Unpin,
{
    let Some(bytes) = read_lsp_frame(reader).await? else {
        return Ok(None);
    };
    let value = serde_json::from_slice(&bytes)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok(Some(value))
}

async fn read_lsp_frame<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let mut headers = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read_exact(&mut byte).await {
            Ok(_) => {
                headers.push(byte[0]);
                if headers.len() > MAX_LSP_HEADER_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "LSP header is too large",
                    ));
                }
                if headers.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof && headers.is_empty() => {
                return Ok(None);
            }
            Err(err) => return Err(err),
        }
    }

    let header_text = std::str::from_utf8(&headers)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let content_length = header_text
        .split("\r\n")
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("Content-Length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length"))?;
    if content_length > MAX_LSP_CONTENT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LSP frame is too large",
        ));
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).await?;
    Ok(Some(body))
}

fn encode_lsp_frame(value: &Value) -> io::Result<Vec<u8>> {
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    Ok(frame)
}

async fn write_lsp_json<W>(writer: &mut W, value: &Value) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let frame = encode_lsp_frame(value)?;
    writer.write_all(&frame).await?;
    writer.flush().await
}

fn path_to_file_uri(path: &Path) -> String {
    let mut path = path.to_string_lossy().replace('\\', "/");
    // `canonicalize` on Windows yields extended-length paths (`\\?\C:\...`);
    // rust-analyzer rejects those inside file URIs ("url is not a file"),
    // so strip the prefix back to a plain drive path.
    if let Some(stripped) = path.strip_prefix("//?/") {
        path = stripped.to_string();
    }
    lowercase_drive_letter(&mut path);
    if cfg!(windows) && !path.starts_with('/') {
        path.insert(0, '/');
    }
    format!("file://{}", percent_encode_uri_path(&path))
}

/// Lowercase a leading `C:` drive letter. rust-analyzer normalizes drive
/// letters to lowercase in the URIs it publishes (`file:///c:/…`), and the
/// diagnostics map is keyed by URI string — the two sides must agree or
/// every diagnostics lookup silently misses.
fn lowercase_drive_letter(path: &mut String) {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_uppercase() {
        if let Some(head) = path.get_mut(0..1) {
            head.make_ascii_lowercase();
        }
    }
}

/// Normalize a URI received from the server so it compares equal to the
/// URIs produced by [`path_to_file_uri`].
fn normalize_file_uri(uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("file:///") else {
        return uri.to_string();
    };
    let mut rest = rest.to_string();
    lowercase_drive_letter(&mut rest);
    format!("file:///{rest}")
}

fn percent_encode_uri_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(*byte as char)
            }
            byte => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc as StdArc, Mutex as StdMutex};

    #[tokio::test]
    async fn lsp_frame_parser_round_trips_content_length_messages() {
        let value = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": { "message": "hello" }
        });
        let frame = encode_lsp_frame(&value).unwrap();
        assert!(frame.starts_with(b"Content-Length: "));

        let mut cursor = std::io::Cursor::new(frame);
        let parsed = read_lsp_json(&mut cursor).await.unwrap().unwrap();

        assert_eq!(parsed, value);
    }

    #[tokio::test]
    async fn rust_analyzer_preflight_missing_binary_is_actionable() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join(if cfg!(windows) {
            "missing-rust-analyzer.exe"
        } else {
            "missing-rust-analyzer"
        });
        let command = missing.to_string_lossy().to_string();

        let err = ensure_rust_analyzer_available(&command, temp.path())
            .await
            .unwrap_err();
        let message = format!("{err:#}");

        assert!(message.contains("rust-analyzer is unavailable"));
        assert!(message.contains("failed to spawn"));
        assert!(message.contains("rustup component add rust-analyzer"));
    }

    #[cfg(windows)]
    #[test]
    fn file_uri_strips_windows_extended_length_prefix() {
        let uri = path_to_file_uri(Path::new(r"\\?\C:\ws\src\lib.rs"));
        assert_eq!(uri, "file:///c:/ws/src/lib.rs");

        let plain = path_to_file_uri(Path::new(r"C:\ws\src\lib.rs"));
        assert_eq!(plain, "file:///c:/ws/src/lib.rs");
    }

    #[test]
    fn server_uris_normalize_to_client_form() {
        // rust-analyzer publishes lowercase drive letters; ours must match.
        assert_eq!(
            normalize_file_uri("file:///C:/ws/src/lib.rs"),
            "file:///c:/ws/src/lib.rs"
        );
        assert_eq!(
            normalize_file_uri("file:///home/user/lib.rs"),
            "file:///home/user/lib.rs"
        );
    }

    #[test]
    fn rust_analyzer_unavailable_message_includes_process_output() {
        let message = format_rust_analyzer_unavailable(
            "rust-analyzer",
            "exit code: 1",
            b"",
            b"error: Unknown binary 'rust-analyzer.exe' in official toolchain\n",
        );

        assert!(message.contains("`rust-analyzer --version` exited with exit code: 1"));
        assert!(message.contains("stderr: error: Unknown binary 'rust-analyzer.exe'"));
        assert!(message.contains("rustup component add rust-analyzer"));
    }

    #[tokio::test]
    async fn initialize_succeeds_lazily_and_tools_call_preserves_backend_error_chain() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"diagnostics","arguments":{"file_path":"src/lib.rs"}}}"#,
            "\n"
        );
        let mut output = Vec::new();
        let factory: BackendFactory = Box::new(|| {
            Box::pin(async {
                Err::<Box<dyn LspBackend>, _>(
                    anyhow!("inner unavailable").context("outer startup failed"),
                )
            })
        });

        run_mcp_server(
            input.as_bytes(),
            &mut output,
            factory,
            DEFAULT_MAX_TOOL_OUTPUT_CHARS,
        )
        .await
        .unwrap();

        let responses = parse_ndjson(&output);
        // A broken backend must not fail the handshake — the error belongs
        // to the tool call that actually needs rust-analyzer.
        assert_eq!(
            responses[0]["result"]["serverInfo"]["name"],
            RUST_LSP_SERVER_NAME
        );
        assert_eq!(responses[1]["result"]["isError"], true);
        let message = responses[1]["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(message.contains("Rust LSP backend failed to start"));
        assert!(message.contains("outer startup failed"));
        assert!(message.contains("inner unavailable"));
    }

    #[tokio::test]
    async fn backend_spawns_lazily_once_on_first_tools_call() {
        let spawn_count = StdArc::new(StdMutex::new(0usize));
        let calls = StdArc::new(StdMutex::new(Vec::new()));
        let factory: BackendFactory = {
            let spawn_count = StdArc::clone(&spawn_count);
            let calls = StdArc::clone(&calls);
            Box::new(move || {
                *spawn_count.lock().unwrap() += 1;
                let backend = FakeBackend {
                    calls: StdArc::clone(&calls),
                };
                Box::pin(async move { Ok(Box::new(backend) as Box<dyn LspBackend>) })
            })
        };
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"diagnostics","arguments":{"file_path":"src/lib.rs"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"hover","arguments":{"file_path":"src/lib.rs","line":1,"character":2}}}"#,
            "\n"
        );
        let mut output = Vec::new();

        run_mcp_server(
            input.as_bytes(),
            &mut output,
            factory,
            DEFAULT_MAX_TOOL_OUTPUT_CHARS,
        )
        .await
        .unwrap();

        let responses = parse_ndjson(&output);
        // initialize + tools/list answer from static data without booting
        // the backend; the two tools/call requests share one backend.
        assert_eq!(responses[1]["result"]["tools"][0]["name"], "diagnostics");
        assert_eq!(*spawn_count.lock().unwrap(), 1);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "diagnostics:src/lib.rs".to_string(),
                "hover:src/lib.rs:1:2".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn failed_backend_spawn_is_retried_on_next_tools_call() {
        let attempts = StdArc::new(StdMutex::new(0usize));
        let calls = StdArc::new(StdMutex::new(Vec::new()));
        let factory: BackendFactory = {
            let attempts = StdArc::clone(&attempts);
            let calls = StdArc::clone(&calls);
            Box::new(move || {
                let attempt = {
                    let mut attempts = attempts.lock().unwrap();
                    *attempts += 1;
                    *attempts
                };
                let calls = StdArc::clone(&calls);
                Box::pin(async move {
                    if attempt == 1 {
                        Err(anyhow!("rust-analyzer is unavailable"))
                    } else {
                        Ok(Box::new(FakeBackend { calls }) as Box<dyn LspBackend>)
                    }
                })
            })
        };
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"diagnostics","arguments":{"file_path":"src/lib.rs"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"diagnostics","arguments":{"file_path":"src/lib.rs"}}}"#,
            "\n"
        );
        let mut output = Vec::new();

        run_mcp_server(
            input.as_bytes(),
            &mut output,
            factory,
            DEFAULT_MAX_TOOL_OUTPUT_CHARS,
        )
        .await
        .unwrap();

        let responses = parse_ndjson(&output);
        assert_eq!(responses[1]["result"]["isError"], true);
        assert_eq!(responses[2]["result"]["isError"], false);
        assert_eq!(*attempts.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn mcp_initialize_and_tools_list_return_rust_tools() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
            "\n"
        );
        let mut output = Vec::new();

        run_mcp_server(
            input.as_bytes(),
            &mut output,
            fake_factory(FakeBackend::default()),
            DEFAULT_MAX_TOOL_OUTPUT_CHARS,
        )
        .await
        .unwrap();

        let responses = parse_ndjson(&output);
        assert_eq!(
            responses[0]["result"]["serverInfo"]["name"],
            RUST_LSP_SERVER_NAME
        );
        let tools = responses[1]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "diagnostics");
        assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
        assert_eq!(tools[0]["_meta"]["anthropic/alwaysLoad"], true);
        assert_eq!(tools[1]["name"], "hover");
    }

    #[tokio::test]
    async fn mcp_tools_call_routes_diagnostics_and_hover_to_backend() {
        let calls = StdArc::new(StdMutex::new(Vec::new()));
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"diagnostics","arguments":{"file_path":"src/lib.rs"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"hover","arguments":{"file_path":"src/lib.rs","line":2,"character":4}}}"#,
            "\n"
        );
        let mut output = Vec::new();

        run_mcp_server(
            input.as_bytes(),
            &mut output,
            fake_factory(FakeBackend {
                calls: calls.clone(),
            }),
            DEFAULT_MAX_TOOL_OUTPUT_CHARS,
        )
        .await
        .unwrap();

        let responses = parse_ndjson(&output);
        let diagnostics_text = responses[1]["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        let diagnostics: Value = serde_json::from_str(diagnostics_text).unwrap();
        assert_eq!(diagnostics["diagnostics"][0]["message"], "fake diagnostic");

        let hover_text = responses[2]["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        let hover: Value = serde_json::from_str(hover_text).unwrap();
        assert_eq!(hover["hover"]["contents"]["value"], "fake hover");

        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "diagnostics:src/lib.rs".to_string(),
                "hover:src/lib.rs:2:4".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn lsp_client_routes_fake_server_diagnostics_and_hover() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let file = src.join("lib.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        let file = file.canonicalize().unwrap();

        let (client_stream, server_stream) = tokio::io::duplex(16 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_stream);
        let (server_reader, server_writer) = tokio::io::split(server_stream);
        let (client, reader_handle) =
            LspClient::spawn(client_reader, client_writer, Duration::from_secs(2));
        let fake_handle = tokio::spawn(run_fake_lsp_server(server_reader, server_writer));

        client.initialize(&workspace).await.unwrap();
        let diagnostics = client
            .diagnostics_for_file(&file, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(diagnostics[0]["message"], "fake diagnostic");

        let hover = client.hover(&file, 0, 3).await.unwrap();
        assert_eq!(hover["contents"]["value"], "fake hover");

        reader_handle.abort();
        fake_handle.abort();
    }

    #[tokio::test]
    async fn diagnostics_waits_past_empty_native_pass_for_check_results() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let file = src.join("lib.rs");
        std::fs::write(&file, "fn main() { broken }\n").unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        let file = file.canonicalize().unwrap();

        let (client_stream, server_stream) = tokio::io::duplex(16 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_stream);
        let (server_reader, server_writer) = tokio::io::split(server_stream);
        let (client, reader_handle) =
            LspClient::spawn(client_reader, client_writer, Duration::from_secs(2));
        let fake_handle = tokio::spawn(run_check_on_save_fake_server(server_reader, server_writer));

        client.initialize(&workspace).await.unwrap();
        // The empty publish triggered by didOpen must not satisfy the
        // wait — only the non-empty one that follows didSave may.
        let diagnostics = client
            .diagnostics_for_file(&file, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(diagnostics[0]["message"], "check-on-save diagnostic");

        reader_handle.abort();
        fake_handle.abort();
    }

    #[tokio::test]
    async fn hover_retries_content_modified_responses() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let file = src.join("lib.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        let file = file.canonicalize().unwrap();

        let (client_stream, server_stream) = tokio::io::duplex(16 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_stream);
        let (server_reader, server_writer) = tokio::io::split(server_stream);
        let (client, reader_handle) =
            LspClient::spawn(client_reader, client_writer, Duration::from_secs(2));
        let fake_handle = tokio::spawn(run_flaky_hover_fake_server(server_reader, server_writer));

        client.initialize(&workspace).await.unwrap();
        let hover = client.hover(&file, 0, 3).await.unwrap();
        assert_eq!(hover["contents"]["value"], "retried hover");

        reader_handle.abort();
        fake_handle.abort();
    }

    #[derive(Default, Clone)]
    struct FakeBackend {
        calls: StdArc<StdMutex<Vec<String>>>,
    }

    #[async_trait]
    impl LspBackend for FakeBackend {
        async fn diagnostics(&mut self, file_path: &str) -> anyhow::Result<Value> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("diagnostics:{file_path}"));
            Ok(json!({
                "file_path": file_path,
                "diagnostics": [{ "message": "fake diagnostic" }]
            }))
        }

        async fn hover(
            &mut self,
            file_path: &str,
            line: u64,
            character: u64,
        ) -> anyhow::Result<Value> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("hover:{file_path}:{line}:{character}"));
            Ok(json!({
                "file_path": file_path,
                "line": line,
                "character": character,
                "hover": {
                    "contents": { "kind": "markdown", "value": "fake hover" }
                }
            }))
        }

        async fn shutdown(&mut self) {}
    }

    fn fake_factory(backend: FakeBackend) -> BackendFactory {
        Box::new(move || {
            let backend = backend.clone();
            Box::pin(async move { Ok(Box::new(backend) as Box<dyn LspBackend>) })
        })
    }

    fn parse_ndjson(output: &[u8]) -> Vec<Value> {
        String::from_utf8(output.to_vec())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    async fn run_fake_lsp_server<R, W>(mut reader: R, mut writer: W)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        loop {
            let Some(message) = read_lsp_json(&mut reader).await.unwrap() else {
                break;
            };
            match message.get("method").and_then(Value::as_str) {
                Some("initialize") => {
                    write_lsp_json(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": message["id"].clone(),
                            "result": { "capabilities": {} }
                        }),
                    )
                    .await
                    .unwrap();
                }
                Some("initialized") => {}
                Some("textDocument/didOpen") | Some("textDocument/didChange") => {
                    let uri = message["params"]["textDocument"]["uri"]
                        .as_str()
                        .or_else(|| {
                            message["params"]["textDocument"]
                                .get("uri")
                                .and_then(Value::as_str)
                        })
                        .unwrap();
                    write_lsp_json(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/publishDiagnostics",
                            "params": {
                                "uri": uri,
                                "diagnostics": [{
                                    "range": {
                                        "start": { "line": 0, "character": 0 },
                                        "end": { "line": 0, "character": 2 }
                                    },
                                    "severity": 1,
                                    "message": "fake diagnostic"
                                }]
                            }
                        }),
                    )
                    .await
                    .unwrap();
                }
                Some("textDocument/hover") => {
                    write_lsp_json(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": message["id"].clone(),
                            "result": {
                                "contents": { "kind": "markdown", "value": "fake hover" }
                            }
                        }),
                    )
                    .await
                    .unwrap();
                }
                _ => {}
            }
        }
    }

    /// Mimics rust-analyzer's publish ordering: an empty native pass right
    /// after didOpen/didChange, compiler results only after didSave.
    async fn run_check_on_save_fake_server<R, W>(mut reader: R, mut writer: W)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut opened_uri: Option<String> = None;
        loop {
            let Some(message) = read_lsp_json(&mut reader).await.unwrap() else {
                break;
            };
            match message.get("method").and_then(Value::as_str) {
                Some("initialize") => {
                    write_lsp_json(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": message["id"].clone(),
                            "result": { "capabilities": {} }
                        }),
                    )
                    .await
                    .unwrap();
                }
                Some("textDocument/didOpen") | Some("textDocument/didChange") => {
                    let uri = message["params"]["textDocument"]["uri"]
                        .as_str()
                        .unwrap()
                        .to_string();
                    write_lsp_json(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/publishDiagnostics",
                            "params": { "uri": uri, "diagnostics": [] }
                        }),
                    )
                    .await
                    .unwrap();
                    opened_uri = Some(uri);
                }
                Some("textDocument/didSave") => {
                    let uri = opened_uri.clone().expect("didSave before didOpen");
                    write_lsp_json(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/publishDiagnostics",
                            "params": {
                                "uri": uri,
                                "diagnostics": [{
                                    "range": {
                                        "start": { "line": 0, "character": 12 },
                                        "end": { "line": 0, "character": 18 }
                                    },
                                    "severity": 1,
                                    "source": "rustc",
                                    "message": "check-on-save diagnostic"
                                }]
                            }
                        }),
                    )
                    .await
                    .unwrap();
                }
                _ => {}
            }
        }
    }

    /// Answers the first hover with `-32801 content modified` (as
    /// rust-analyzer does mid-analysis) and succeeds on the retry.
    async fn run_flaky_hover_fake_server<R, W>(mut reader: R, mut writer: W)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut hover_calls = 0usize;
        loop {
            let Some(message) = read_lsp_json(&mut reader).await.unwrap() else {
                break;
            };
            match message.get("method").and_then(Value::as_str) {
                Some("initialize") => {
                    write_lsp_json(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": message["id"].clone(),
                            "result": { "capabilities": {} }
                        }),
                    )
                    .await
                    .unwrap();
                }
                Some("textDocument/hover") => {
                    hover_calls += 1;
                    let response = if hover_calls == 1 {
                        json!({
                            "jsonrpc": "2.0",
                            "id": message["id"].clone(),
                            "error": {
                                "code": LSP_CONTENT_MODIFIED_CODE,
                                "message": "content modified"
                            }
                        })
                    } else {
                        json!({
                            "jsonrpc": "2.0",
                            "id": message["id"].clone(),
                            "result": {
                                "contents": { "kind": "markdown", "value": "retried hover" }
                            }
                        })
                    };
                    write_lsp_json(&mut writer, &response).await.unwrap();
                }
                _ => {}
            }
        }
    }
}
