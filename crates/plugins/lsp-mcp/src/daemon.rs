//! Per-workspace shared rust-analyzer daemon.
//!
//! Without sharing, every session that touches a `rust_lsp` tool pays
//! for its own rust-analyzer — multiple gigabytes of duplicated index
//! per concurrent session in the same workspace. This module lets the
//! stdio bridge (`rebon-lsp-mcp rust`) delegate its backend to a
//! single daemon process keyed by the canonical workspace root:
//!
//! * **Election** — the daemon holds an exclusive advisory file lock
//!   for its workspace key; losing candidates exit immediately, so
//!   bridges can blindly spawn a candidate and then poll for the winner.
//! * **Discovery** — the winning daemon writes `daemon.json` (pid,
//!   loopback port, auth token, workspace) next to the lock file.
//! * **Transport** — NDJSON request/response over 127.0.0.1: one auth
//!   line, then `{op, file_path, line?, character?}` per call. The
//!   bridge's [`RemoteBackend`] implements [`LspBackend`] over it, so
//!   the MCP surface is unchanged.
//! * **Lifecycle** — the backend inside the daemon is still spawned
//!   lazily on the first call; the daemon exits after `idle_ttl` with
//!   no connected clients (keeping the index warm across quick
//!   session churn) and reaps rust-analyzer through the existing
//!   process-tree guard.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify};

use rebon_types::constant_time_eq;

use crate::{BackendFactory, LspBackend, RustAnalyzerBackend, RustServerOptions};

/// This plugin's binary half, as declared by its own `[[bin]]`. The daemon
/// is a subcommand of *that* executable, not of whatever is running.
const DAEMON_BINARY_NAME: &str = "rebon-lsp-mcp";

pub(crate) const DAEMON_SCHEMA: u32 = 1;
const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound for one forwarded call. Diagnostics can legitimately
/// take its full 10s window, and calls from other sessions queue
/// behind it on the shared backend.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const SPAWN_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(200);

const INFO_FILE: &str = "daemon.json";
const LOCK_FILE: &str = "daemon.lock";
const LOG_FILE: &str = "daemon.log";

/// `CREATE_BREAKAWAY_FROM_JOB` — the bridge lives inside the session's
/// kill-on-close Job Object; the daemon must escape it to outlive the
/// session that happened to spawn it.
#[cfg(windows)]
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

pub fn default_daemon_idle_ttl() -> Duration {
    std::env::var("REBON_RUST_LSP_DAEMON_IDLE_SECONDS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_IDLE_TTL)
}

pub(crate) fn sharing_disabled() -> bool {
    std::env::var("REBON_RUST_LSP_NO_SHARE")
        .map(|value| {
            let value = value.trim();
            !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Stable directory key for a canonical workspace path: a readable
/// basename prefix plus an FNV-1a hash of the full path.
fn workspace_key(workspace: &Path) -> String {
    let full = workspace.to_string_lossy();
    let base: String = workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string())
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        .take(24)
        .collect();
    let base = if base.is_empty() {
        "workspace".to_string()
    } else {
        base
    };
    format!("{base}-{:016x}", fnv1a64(full.as_bytes()))
}

fn daemon_root_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("REBON_LSP_DAEMON_DIR") {
        return Ok(PathBuf::from(dir));
    }
    // `$REBON_CONFIG_DIR`, then `~/.rebon` — the same resolution the rest of
    // the process uses, so a relocated config home takes its daemons along.
    let config_home = rebon_session::config_home_with_env(|name| std::env::var_os(name))
        .ok_or_else(|| {
            anyhow!("cannot locate the rebon config home: none of REBON_CONFIG_DIR, USERPROFILE or HOME is set")
        })?;
    Ok(config_home.join("lsp-daemons"))
}

pub(crate) fn daemon_dir_for_workspace(workspace: &Path) -> anyhow::Result<PathBuf> {
    Ok(daemon_root_dir()?.join(workspace_key(workspace)))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DaemonInfo {
    pub schema: u32,
    pub pid: u32,
    pub port: u16,
    pub token: String,
    pub workspace: String,
}

fn load_daemon_info(dir: &Path) -> Option<DaemonInfo> {
    let text = std::fs::read_to_string(dir.join(INFO_FILE)).ok()?;
    let info: DaemonInfo = serde_json::from_str(&text).ok()?;
    if info.schema != DAEMON_SCHEMA {
        return None;
    }
    Some(info)
}

fn store_daemon_info(dir: &Path, info: &DaemonInfo) -> anyhow::Result<()> {
    let text = serde_json::to_string_pretty(info)?;
    rebon_session::write_file_atomically(&dir.join(INFO_FILE), text.as_bytes())
        .with_context(|| format!("failed to publish {INFO_FILE} in {}", dir.display()))
}

// ---------------------------------------------------------------------------
// Wire protocol
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct AuthRequest {
    token: String,
    workspace: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireRequest {
    op: String,
    file_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    line: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    character: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ok: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    err: Option<String>,
}

async fn write_json_line<W>(writer: &mut W, value: &Value) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}

// ---------------------------------------------------------------------------
// Daemon (server side)
// ---------------------------------------------------------------------------

/// Run the shared daemon for `options.workspace`, exiting quietly if
/// another daemon already owns the workspace lock.
pub async fn run_rust_daemon(options: RustServerOptions, idle_ttl: Duration) -> anyhow::Result<()> {
    let options = options.canonicalized()?;
    let dir = daemon_dir_for_workspace(&options.workspace)?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create daemon dir {}", dir.display()))?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(LOCK_FILE))?;
    if fs2::FileExt::try_lock_exclusive(&lock_file).is_err() {
        // Lost the election — the winner serves this workspace.
        return Ok(());
    }

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("failed to bind daemon listener")?;
    let port = listener.local_addr()?.port();
    let token = rebon_types::secure_random_hex_token()
        .map_err(|err| anyhow!("failed to generate daemon auth token: {err}"))?;
    let workspace_string = options.workspace.to_string_lossy().into_owned();
    store_daemon_info(
        &dir,
        &DaemonInfo {
            schema: DAEMON_SCHEMA,
            pid: std::process::id(),
            port,
            token: token.clone(),
            workspace: workspace_string.clone(),
        },
    )?;

    let factory: BackendFactory = Box::new(move || {
        let options = options.clone();
        Box::pin(async move {
            let backend = RustAnalyzerBackend::spawn(options).await?;
            Ok(Box::new(backend) as Box<dyn LspBackend>)
        })
    });
    let result = serve_daemon(listener, factory, token, workspace_string, idle_ttl).await;
    // Only unpublish our own info: a replacement daemon may already
    // have overwritten the file while we were shutting down.
    if load_daemon_info(&dir).is_some_and(|info| info.pid == std::process::id()) {
        let _ = std::fs::remove_file(dir.join(INFO_FILE));
    }
    let _ = fs2::FileExt::unlock(&lock_file);
    result
}

struct SharedBackend {
    backend: Option<Box<dyn LspBackend>>,
    factory: BackendFactory,
}

impl SharedBackend {
    async fn ensure(&mut self) -> anyhow::Result<&mut dyn LspBackend> {
        if self.backend.is_none() {
            // Factory failures stay retryable, matching the bridge's
            // local-backend semantics.
            self.backend = Some((self.factory)().await?);
        }
        Ok(self
            .backend
            .as_mut()
            .expect("backend was just initialized")
            .as_mut())
    }
}

/// Serve daemon connections until the listener errors or the daemon
/// has had zero clients for `idle_ttl`.
pub(crate) async fn serve_daemon(
    listener: TcpListener,
    factory: BackendFactory,
    token: String,
    workspace: String,
    idle_ttl: Duration,
) -> anyhow::Result<()> {
    let shared = Arc::new(Mutex::new(SharedBackend {
        backend: None,
        factory,
    }));
    let clients = Arc::new(AtomicUsize::new(0));
    let activity = Arc::new(Notify::new());

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("daemon accept failed")?;
                clients.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(handle_connection(
                    stream,
                    Arc::clone(&shared),
                    token.clone(),
                    workspace.clone(),
                    Arc::clone(&clients),
                    Arc::clone(&activity),
                ));
            }
            _ = activity.notified() => {
                // A client disconnected; loop so the idle arm re-arms
                // against the new client count.
            }
            _ = tokio::time::sleep(idle_ttl), if clients.load(Ordering::SeqCst) == 0 => {
                break;
            }
        }
    }

    let mut guard = shared.lock().await;
    if let Some(mut backend) = guard.backend.take() {
        backend.shutdown().await;
    }
    Ok(())
}

async fn handle_connection(
    stream: TcpStream,
    shared: Arc<Mutex<SharedBackend>>,
    token: String,
    workspace: String,
    clients: Arc<AtomicUsize>,
    activity: Arc<Notify>,
) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    // A connection that never authenticates must not pin the daemon
    // alive, so the auth exchange is bounded.
    let auth_ok = match tokio::time::timeout(AUTH_TIMEOUT, lines.next_line()).await {
        Ok(Ok(Some(line))) => match serde_json::from_str::<AuthRequest>(&line) {
            Ok(auth) => constant_time_eq(&auth.token, &token) && auth.workspace == workspace,
            Err(_) => false,
        },
        _ => false,
    };

    if !auth_ok {
        let _ = write_json_line(&mut write, &json!({ "err": "unauthorized" })).await;
    } else {
        let _ = write_json_line(&mut write, &json!({ "ok": true })).await;
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let response = match serde_json::from_str::<WireRequest>(&line) {
                Err(err) => json!({ "err": format!("invalid daemon request: {err}") }),
                Ok(request) => {
                    let mut guard = shared.lock().await;
                    match guard.ensure().await {
                        Err(err) => json!({
                            "err": format!("Rust LSP backend failed to start: {err:#}"),
                        }),
                        Ok(backend) => {
                            let result = match request.op.as_str() {
                                "diagnostics" => backend.diagnostics(&request.file_path).await,
                                "hover" => match (request.line, request.character) {
                                    (Some(line), Some(character)) => {
                                        backend.hover(&request.file_path, line, character).await
                                    }
                                    _ => Err(anyhow!("hover requires line and character")),
                                },
                                other => Err(anyhow!("unknown daemon op `{other}`")),
                            };
                            match result {
                                Ok(value) => json!({ "ok": value }),
                                Err(err) => json!({ "err": format!("{err:#}") }),
                            }
                        }
                    }
                }
            };
            if write_json_line(&mut write, &response).await.is_err() {
                break;
            }
        }
    }

    clients.fetch_sub(1, Ordering::SeqCst);
    activity.notify_waiters();
}

// ---------------------------------------------------------------------------
// Bridge (client side)
// ---------------------------------------------------------------------------

/// [`LspBackend`] that forwards calls to the shared daemon.
#[derive(Debug)]
pub(crate) struct RemoteBackend {
    lines: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
}

impl RemoteBackend {
    pub(crate) async fn connect(dir: &Path, workspace: &Path) -> anyhow::Result<Self> {
        let info =
            load_daemon_info(dir).ok_or_else(|| anyhow!("no daemon info at {}", dir.display()))?;
        Self::connect_with(&info, workspace).await
    }

    pub(crate) async fn connect_with(info: &DaemonInfo, workspace: &Path) -> anyhow::Result<Self> {
        let stream = tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect(("127.0.0.1", info.port)),
        )
        .await
        .map_err(|_| anyhow!("daemon connect timed out"))?
        .context("daemon connect failed")?;
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();

        let auth = serde_json::to_value(AuthRequest {
            token: info.token.clone(),
            workspace: workspace.to_string_lossy().into_owned(),
        })?;
        write_json_line(&mut write, &auth)
            .await
            .context("failed to send daemon auth")?;
        let reply = tokio::time::timeout(AUTH_TIMEOUT, lines.next_line())
            .await
            .map_err(|_| anyhow!("daemon auth timed out"))?
            .context("failed to read daemon auth reply")?
            .ok_or_else(|| anyhow!("daemon closed the connection during auth"))?;
        let reply: WireResponse = serde_json::from_str(&reply)
            .with_context(|| format!("invalid daemon auth reply: {reply}"))?;
        if let Some(err) = reply.err {
            bail!("daemon rejected connection: {err}");
        }
        Ok(Self { lines, write })
    }

    async fn call(&mut self, request: WireRequest) -> anyhow::Result<Value> {
        let frame = serde_json::to_value(&request)?;
        write_json_line(&mut self.write, &frame)
            .await
            .context("failed to send daemon request")?;
        let line = tokio::time::timeout(CALL_TIMEOUT, self.lines.next_line())
            .await
            .map_err(|_| anyhow!("daemon call `{}` timed out", request.op))?
            .context("failed to read daemon response")?
            .ok_or_else(|| anyhow!("daemon closed the connection"))?;
        let response: WireResponse = serde_json::from_str(&line)
            .with_context(|| format!("invalid daemon response: {line}"))?;
        if let Some(err) = response.err {
            bail!("{err}");
        }
        response
            .ok
            .ok_or_else(|| anyhow!("daemon response carried neither ok nor err"))
    }
}

#[async_trait]
impl LspBackend for RemoteBackend {
    async fn diagnostics(&mut self, file_path: &str) -> anyhow::Result<Value> {
        self.call(WireRequest {
            op: "diagnostics".to_string(),
            file_path: file_path.to_string(),
            line: None,
            character: None,
        })
        .await
    }

    async fn hover(&mut self, file_path: &str, line: u64, character: u64) -> anyhow::Result<Value> {
        self.call(WireRequest {
            op: "hover".to_string(),
            file_path: file_path.to_string(),
            line: Some(line),
            character: Some(character),
        })
        .await
    }

    async fn shutdown(&mut self) {
        // Dropping the socket halves is the disconnect signal; the
        // daemon decrements its client count on EOF.
    }
}

/// Connect to the workspace's shared daemon, electing one first when
/// none is reachable.
pub(crate) async fn connect_or_spawn_daemon(
    options: &RustServerOptions,
) -> anyhow::Result<RemoteBackend> {
    let dir = daemon_dir_for_workspace(&options.workspace)?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create daemon dir {}", dir.display()))?;
    if let Ok(backend) = RemoteBackend::connect(&dir, &options.workspace).await {
        return Ok(backend);
    }
    spawn_daemon_candidate(&options.workspace, &dir)?;
    let deadline = tokio::time::Instant::now() + SPAWN_WAIT_TIMEOUT;
    loop {
        match RemoteBackend::connect(&dir, &options.workspace).await {
            Ok(backend) => return Ok(backend),
            Err(err) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(err).context("shared rust-analyzer daemon did not come up");
                }
                tokio::time::sleep(SPAWN_POLL_INTERVAL).await;
            }
        }
    }
}

/// The executable that carries the `rust --daemon` entrypoint.
///
/// This plugin's binary half is `rebon-lsp-mcp`, and the daemon is that
/// binary's own subcommand — not a subcommand of whatever happens to be
/// running. Resolving it the way every other plugin binary is resolved
/// (beside the running executable, never through `PATH`) keeps the spawn
/// correct whether the bridge was started as `rebon-lsp-mcp rust` or
/// in-process by a host that lives next to it.
///
/// Using `current_exe()` directly is what broke this before: the argv was
/// still written for the era when the bridge was a `rebon` subcommand, so
/// the daemon was spawned with an extra `lsp-mcp` element that clap refused
/// — and the failure surfaced only as a silent fall back to one
/// rust-analyzer per session.
fn daemon_executable() -> anyhow::Result<PathBuf> {
    let running = std::env::current_exe().context("failed to resolve current executable")?;
    rebon_types::sibling_binary::resolve_from_executable(DAEMON_BINARY_NAME, &running)
        .with_context(|| format!("failed to locate the {DAEMON_BINARY_NAME} executable"))
}

/// Launch a detached daemon candidate. Losing candidates exit on their
/// own once they fail the lock election, so this is safe to call even
/// when a daemon is already running.
fn spawn_daemon_candidate(workspace: &Path, dir: &Path) -> anyhow::Result<()> {
    let exe = daemon_executable()?;

    #[cfg(windows)]
    {
        // First choice: break out of the session's kill-on-close job so
        // the daemon survives the spawning session. Fall back to an
        // in-job daemon (dies with this session — same as unshared) if
        // the job does not permit breakaway.
        match build_daemon_command(&exe, workspace, dir, true)?.spawn() {
            Ok(_) => return Ok(()),
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    "daemon breakaway spawn failed; retrying inside the job"
                );
            }
        }
        build_daemon_command(&exe, workspace, dir, false)?
            .spawn()
            .context("failed to spawn shared rust-analyzer daemon")?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        build_daemon_command(&exe, workspace, dir)?
            .spawn()
            .context("failed to spawn shared rust-analyzer daemon")?;
        Ok(())
    }
}

#[cfg(windows)]
fn build_daemon_command(
    exe: &Path,
    workspace: &Path,
    dir: &Path,
    breakaway: bool,
) -> anyhow::Result<std::process::Command> {
    use std::os::windows::process::CommandExt;

    let mut command = daemon_command_base(exe, workspace, dir)?;
    let mut flags = crate::CREATE_NO_WINDOW;
    if breakaway {
        flags |= CREATE_BREAKAWAY_FROM_JOB;
    }
    command.creation_flags(flags);
    Ok(command)
}

#[cfg(not(windows))]
fn build_daemon_command(
    exe: &Path,
    workspace: &Path,
    dir: &Path,
) -> anyhow::Result<std::process::Command> {
    use std::os::unix::process::CommandExt;

    let mut command = daemon_command_base(exe, workspace, dir)?;
    // Leave the bridge's process group so the session's group kill
    // does not take the shared daemon down with it.
    command.process_group(0);
    Ok(command)
}

fn daemon_command_base(
    exe: &Path,
    workspace: &Path,
    dir: &Path,
) -> anyhow::Result<std::process::Command> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOG_FILE))?;
    let stderr = log.try_clone()?;
    let mut command = std::process::Command::new(exe);
    // `exe` is this binary (`rebon-lsp-mcp`), whose only subcommand is
    // `rust` — an extra `lsp-mcp` argv element is an unknown subcommand and
    // clap refuses the spawn, which shows up as a silent fall back to a
    // per-session rust-analyzer.
    command
        .arg("rust")
        .arg("--daemon")
        .current_dir(workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(stderr));
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[derive(Clone, Default)]
    struct FakeBackend {
        calls: Arc<StdMutex<Vec<String>>>,
        shutdowns: Arc<StdMutex<usize>>,
    }

    #[async_trait]
    impl LspBackend for FakeBackend {
        async fn diagnostics(&mut self, file_path: &str) -> anyhow::Result<Value> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("diagnostics:{file_path}"));
            if file_path == "boom.rs" {
                return Err(anyhow!("backend exploded"));
            }
            Ok(json!({ "file_path": file_path, "diagnostics": [{ "message": "shared" }] }))
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
            Ok(json!({ "hover": { "contents": "shared hover" } }))
        }

        async fn shutdown(&mut self) {
            *self.shutdowns.lock().unwrap() += 1;
        }
    }

    struct TestDaemon {
        info: DaemonInfo,
        server: tokio::task::JoinHandle<anyhow::Result<()>>,
        spawns: Arc<StdMutex<usize>>,
        calls: Arc<StdMutex<Vec<String>>>,
        shutdowns: Arc<StdMutex<usize>>,
    }

    async fn start_test_daemon(workspace: &str, idle_ttl: Duration) -> TestDaemon {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = "test-token".to_string();
        let spawns = Arc::new(StdMutex::new(0usize));
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let shutdowns = Arc::new(StdMutex::new(0usize));
        let factory: BackendFactory = {
            let spawns = Arc::clone(&spawns);
            let calls = Arc::clone(&calls);
            let shutdowns = Arc::clone(&shutdowns);
            Box::new(move || {
                *spawns.lock().unwrap() += 1;
                let backend = FakeBackend {
                    calls: Arc::clone(&calls),
                    shutdowns: Arc::clone(&shutdowns),
                };
                Box::pin(async move { Ok(Box::new(backend) as Box<dyn LspBackend>) })
            })
        };
        let server = tokio::spawn(serve_daemon(
            listener,
            factory,
            token.clone(),
            workspace.to_string(),
            idle_ttl,
        ));
        TestDaemon {
            info: DaemonInfo {
                schema: DAEMON_SCHEMA,
                pid: std::process::id(),
                port,
                token,
                workspace: workspace.to_string(),
            },
            server,
            spawns,
            calls,
            shutdowns,
        }
    }

    #[test]
    fn workspace_key_is_stable_and_distinct() {
        let a = workspace_key(Path::new("C:/dev/project-a"));
        let b = workspace_key(Path::new("C:/dev/project-b"));
        assert_eq!(a, workspace_key(Path::new("C:/dev/project-a")));
        assert_ne!(a, b);
        assert!(a.starts_with("project-a-"), "key was {a}");
    }

    #[test]
    fn daemon_info_round_trips_and_rejects_other_schemas() {
        let dir = tempfile::tempdir().unwrap();
        let info = DaemonInfo {
            schema: DAEMON_SCHEMA,
            pid: 7,
            port: 12345,
            token: "tok".into(),
            workspace: "C:/ws".into(),
        };
        store_daemon_info(dir.path(), &info).unwrap();
        let loaded = load_daemon_info(dir.path()).unwrap();
        assert_eq!(loaded.port, 12345);
        assert_eq!(loaded.token, "tok");

        let stale = DaemonInfo {
            schema: DAEMON_SCHEMA + 1,
            ..info
        };
        store_daemon_info(dir.path(), &stale).unwrap();
        assert!(load_daemon_info(dir.path()).is_none());
    }

    #[tokio::test]
    async fn two_clients_share_one_backend() {
        let daemon = start_test_daemon("C:/ws", Duration::from_secs(30)).await;
        let workspace = Path::new("C:/ws");

        let mut first = RemoteBackend::connect_with(&daemon.info, workspace)
            .await
            .unwrap();
        let mut second = RemoteBackend::connect_with(&daemon.info, workspace)
            .await
            .unwrap();

        let diagnostics = first.diagnostics("src/lib.rs").await.unwrap();
        assert_eq!(diagnostics["diagnostics"][0]["message"], "shared");
        let hover = second.hover("src/lib.rs", 1, 2).await.unwrap();
        assert_eq!(hover["hover"]["contents"], "shared hover");

        assert_eq!(*daemon.spawns.lock().unwrap(), 1, "backend must be shared");
        assert_eq!(
            *daemon.calls.lock().unwrap(),
            vec![
                "diagnostics:src/lib.rs".to_string(),
                "hover:src/lib.rs:1:2".to_string()
            ]
        );
        daemon.server.abort();
    }

    #[tokio::test]
    async fn backend_errors_propagate_to_the_client() {
        let daemon = start_test_daemon("C:/ws", Duration::from_secs(30)).await;
        let mut client = RemoteBackend::connect_with(&daemon.info, Path::new("C:/ws"))
            .await
            .unwrap();

        let err = client.diagnostics("boom.rs").await.unwrap_err();
        assert!(err.to_string().contains("backend exploded"));
        // The connection stays usable after a failed call.
        let ok = client.diagnostics("src/lib.rs").await.unwrap();
        assert_eq!(ok["diagnostics"][0]["message"], "shared");
        daemon.server.abort();
    }

    #[tokio::test]
    async fn wrong_token_and_wrong_workspace_are_rejected() {
        let daemon = start_test_daemon("C:/ws", Duration::from_secs(30)).await;

        let mut bad_token = daemon.info.clone();
        bad_token.token = "wrong".into();
        let err = RemoteBackend::connect_with(&bad_token, Path::new("C:/ws"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unauthorized"), "err was {err:#}");

        let err = RemoteBackend::connect_with(&daemon.info, Path::new("C:/other"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unauthorized"), "err was {err:#}");
        daemon.server.abort();
    }

    #[tokio::test]
    async fn daemon_exits_after_idle_ttl_and_shuts_backend_down() {
        let daemon = start_test_daemon("C:/ws", Duration::from_millis(300)).await;
        let workspace = Path::new("C:/ws");

        let mut client = RemoteBackend::connect_with(&daemon.info, workspace)
            .await
            .unwrap();
        client.diagnostics("src/lib.rs").await.unwrap();
        drop(client);

        let result = tokio::time::timeout(Duration::from_secs(5), daemon.server)
            .await
            .expect("daemon did not exit after idle TTL")
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(
            *daemon.shutdowns.lock().unwrap(),
            1,
            "shared backend must be shut down on daemon exit"
        );
    }

    #[tokio::test]
    async fn connected_client_keeps_daemon_alive_past_ttl() {
        let daemon = start_test_daemon("C:/ws", Duration::from_millis(200)).await;
        let mut client = RemoteBackend::connect_with(&daemon.info, Path::new("C:/ws"))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            !daemon.server.is_finished(),
            "daemon must not idle out while a client is connected"
        );
        // Still serving after the TTL would have fired.
        let ok = client.diagnostics("src/lib.rs").await.unwrap();
        assert_eq!(ok["diagnostics"][0]["message"], "shared");
        daemon.server.abort();
    }

    #[tokio::test]
    async fn losing_candidate_exits_without_serving() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(LOCK_FILE))
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&lock_file).unwrap();

        // A second exclusive lock on the same file must fail — this is
        // the election a losing daemon candidate observes.
        let candidate = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(LOCK_FILE))
            .unwrap();
        assert!(fs2::FileExt::try_lock_exclusive(&candidate).is_err());
    }
}
