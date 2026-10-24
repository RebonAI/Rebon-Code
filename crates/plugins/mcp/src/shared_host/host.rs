//! The host process: one per config home, running every shared stdio server.
//!
//! The host is each server's only MCP client. It runs the `initialize`
//! handshake once, answers each session's own `initialize` from that result,
//! and from then on relays: a session's request goes to the server under an
//! id of the host's choosing and its response comes back under the session's
//! own; a server notification goes to every session attached; a server
//! request is refused here, as the stdio client refuses it.
//!
//! A server with no session left is kept for [`HostTimings::server_grace`],
//! so a session that ends a turn and starts the next does not pay the
//! server's startup again. The host leaves once it has had no server and no
//! connection for [`HostTimings::idle_exit`].

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use rebon_tools_core::ProcessTreeGuard;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::protocol::{
    host_dir, load_host_info, same_dir, store_host_info, Hello, HostInfo, Reply, Request,
    ServerSpec, SharedServerInfo, StopReason, HOST_SCHEMA, INFO_FILE, LOCK_FILE, STOPPED_METHOD,
};

/// A connection that never says who it is must not keep the host alive.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub(crate) struct HostTimings {
    pub idle_exit: Duration,
    pub server_grace: Duration,
    pub tick: Duration,
}

impl HostTimings {
    fn from_env() -> Self {
        let seconds = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map_or(Duration::from_secs(default), Duration::from_secs)
        };
        Self {
            idle_exit: seconds("REBON_MCP_HOST_IDLE_SECONDS", 30),
            server_grace: seconds("REBON_MCP_SERVER_GRACE_SECONDS", 60),
            tick: Duration::from_secs(1),
        }
    }
}

/// Run the host for this config home, or return at once when another
/// process already is it.
pub async fn run_host() -> anyhow::Result<()> {
    let dir = host_dir()?;
    run_host_in(&dir, HostTimings::from_env()).await
}

pub(crate) async fn run_host_in(dir: &Path, timings: HostTimings) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(LOCK_FILE))?;
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        // Another host won; the session that started this one will find it.
        return Ok(());
    }
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("failed to bind the MCP host listener")?;
    let token = rebon_types::secure_random_hex_token()
        .map_err(|error| anyhow!("failed to make the MCP host token: {error}"))?;
    store_host_info(
        dir,
        &HostInfo {
            schema: HOST_SCHEMA,
            pid: std::process::id(),
            port: listener.local_addr()?.port(),
            token: token.clone(),
        },
    )?;
    let result = serve(listener, token, timings).await;
    // Only our own address: a newer host may already have published its.
    if load_host_info(dir).is_some_and(|info| info.pid == std::process::id()) {
        let _ = std::fs::remove_file(dir.join(INFO_FILE));
    }
    let _ = fs2::FileExt::unlock(&lock);
    result
}

pub(crate) async fn serve(
    listener: TcpListener,
    token: String,
    timings: HostTimings,
) -> anyhow::Result<()> {
    let host = Arc::new(Host {
        token,
        timings,
        servers: StdMutex::new(HashMap::new()),
        starting: tokio::sync::Mutex::new(HashMap::new()),
        connections: AtomicUsize::new(0),
        next_session: AtomicU64::new(1),
        shutdown: Notify::new(),
    });
    let mut tick = tokio::time::interval(timings.tick);
    let mut idle_since: Option<Instant> = None;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("MCP host accept failed")?;
                tokio::spawn(handle_connection(host.clone(), stream));
            }
            _ = host.shutdown.notified() => break,
            _ = tick.tick() => {
                host.retire_idle_servers().await;
                if host.is_idle() {
                    if idle_since.get_or_insert_with(Instant::now).elapsed() >= timings.idle_exit {
                        break;
                    }
                } else {
                    idle_since = None;
                }
            }
        }
    }
    host.stop_all().await;
    Ok(())
}

struct Host {
    token: String,
    timings: HostTimings,
    servers: StdMutex<HashMap<String, Arc<Server>>>,
    /// One start at a time per key, so two sessions that attach together get
    /// one process.
    starting: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    connections: AtomicUsize,
    next_session: AtomicU64,
    shutdown: Notify,
}

enum Outbound {
    Frame(Value),
    Close,
}

struct Server {
    key: String,
    spec: ServerSpec,
    pid: Option<u32>,
    started_at_ms: u64,
    init_result: Value,
    stdin: tokio::sync::Mutex<ChildStdin>,
    child: tokio::sync::Mutex<Option<Child>>,
    tree: StdMutex<Option<ProcessTreeGuard>>,
    next_id: AtomicI64,
    /// Host id → (session, the session's own id).
    pending: StdMutex<HashMap<i64, (u64, Value)>>,
    sessions: StdMutex<Sessions>,
    reader: StdMutex<Option<JoinHandle<()>>>,
    stderr: StdMutex<Option<JoinHandle<()>>>,
}

#[derive(Default)]
struct Sessions {
    attached: HashMap<u64, UnboundedSender<Outbound>>,
    idle_since: Option<Instant>,
    /// Set once, under this lock, so an attach can never land on a server
    /// that is on its way out.
    retired: bool,
}

struct ConnectionCount(Arc<Host>);

impl Drop for ConnectionCount {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl Host {
    fn is_idle(&self) -> bool {
        self.connections.load(Ordering::SeqCst) == 0 && lock(&self.servers).is_empty()
    }

    fn live(&self, key: &str) -> Option<Arc<Server>> {
        lock(&self.servers).get(key).cloned()
    }

    async fn server_for(self: &Arc<Self>, spec: ServerSpec) -> Result<Arc<Server>, String> {
        let key = spec.key();
        if let Some(server) = self.live(&key) {
            return Ok(server);
        }
        let gate = self
            .starting
            .lock()
            .await
            .entry(key.clone())
            .or_default()
            .clone();
        let _turn = gate.lock().await;
        if let Some(server) = self.live(&key) {
            return Ok(server);
        }
        let server = start_server(Arc::downgrade(self), spec, key.clone()).await?;
        lock(&self.servers).insert(key, server.clone());
        Ok(server)
    }

    fn list(&self) -> Vec<SharedServerInfo> {
        let mut servers: Vec<SharedServerInfo> = lock(&self.servers)
            .values()
            .map(|server| SharedServerInfo {
                name: server.spec.name.clone(),
                command: server.spec.command.clone(),
                args: server.spec.args.clone(),
                cwd: server.spec.cwd.clone(),
                pid: server.pid,
                sessions: lock(&server.sessions).attached.len(),
                started_at_ms: server.started_at_ms,
            })
            .collect();
        servers.sort_by(|a, b| a.name.cmp(&b.name).then(a.cwd.cmp(&b.cwd)));
        servers
    }

    /// Stop the servers called `name`; with `cwd`, only the ones running
    /// there.
    async fn stop_named(&self, name: &str, reason: StopReason, cwd: Option<&str>) -> usize {
        let named: Vec<Arc<Server>> = lock(&self.servers)
            .values()
            .filter(|server| {
                server.spec.name == name && cwd.is_none_or(|cwd| same_dir(&server.spec.cwd, cwd))
            })
            .cloned()
            .collect();
        let mut stopped = 0;
        for server in named {
            if self.retire(&server, Some(reason), false).await {
                stopped += 1;
            }
        }
        stopped
    }

    async fn stop_all(&self) {
        let all: Vec<Arc<Server>> = lock(&self.servers).values().cloned().collect();
        for server in all {
            self.retire(&server, None, false).await;
        }
    }

    async fn retire_idle_servers(&self) {
        let grace = self.timings.server_grace;
        let idle: Vec<Arc<Server>> = lock(&self.servers)
            .values()
            .filter(|server| {
                let sessions = lock(&server.sessions);
                sessions.attached.is_empty()
                    && sessions
                        .idle_since
                        .is_some_and(|since| since.elapsed() >= grace)
            })
            .cloned()
            .collect();
        for server in idle {
            // Checked again under the lock: a session may have attached since.
            let still_idle = {
                let mut sessions = lock(&server.sessions);
                let idle = sessions.attached.is_empty() && !sessions.retired;
                if idle {
                    sessions.retired = true;
                }
                idle
            };
            if still_idle {
                self.finish_retire(&server, None, false).await;
            }
        }
    }

    /// Take a server out of service. `false` when it already was.
    async fn retire(
        &self,
        server: &Arc<Server>,
        reason: Option<StopReason>,
        from_reader: bool,
    ) -> bool {
        {
            let mut sessions = lock(&server.sessions);
            if sessions.retired {
                return false;
            }
            sessions.retired = true;
        }
        self.finish_retire(server, reason, from_reader).await;
        true
    }

    async fn finish_retire(
        &self,
        server: &Arc<Server>,
        reason: Option<StopReason>,
        from_reader: bool,
    ) {
        {
            let mut servers = lock(&self.servers);
            if servers
                .get(&server.key)
                .is_some_and(|current| Arc::ptr_eq(current, server))
            {
                servers.remove(&server.key);
            }
        }
        let attached = std::mem::take(&mut lock(&server.sessions).attached);
        for (_, session) in attached {
            if reason == Some(StopReason::Disabled) {
                let _ = session.send(Outbound::Frame(json!({
                    "jsonrpc": "2.0",
                    "method": STOPPED_METHOD,
                    "params": { "reason": "disabled" },
                })));
            }
            let _ = session.send(Outbound::Close);
        }
        lock(&server.pending).clear();
        // The reader retiring its own server is already ending; aborting it
        // here would cancel this very function at its next await.
        if !from_reader {
            if let Some(reader) = lock(&server.reader).take() {
                reader.abort();
            }
        }
        if let Some(stderr) = lock(&server.stderr).take() {
            stderr.abort();
        }
        if let Some(mut child) = server.child.lock().await.take() {
            let _ = child.kill().await;
        }
        if let Some(mut tree) = lock(&server.tree).take() {
            let _ = tree.terminate();
        }
    }
}

impl Server {
    /// `false` once the server is retired: the caller starts a fresh one.
    fn add_session(&self, id: u64, session: UnboundedSender<Outbound>) -> bool {
        let mut sessions = lock(&self.sessions);
        if sessions.retired {
            return false;
        }
        sessions.attached.insert(id, session);
        sessions.idle_since = None;
        true
    }

    fn remove_session(&self, id: u64) {
        lock(&self.pending).retain(|_, (session, _)| *session != id);
        let mut sessions = lock(&self.sessions);
        sessions.attached.remove(&id);
        if sessions.attached.is_empty() {
            sessions.idle_since = Some(Instant::now());
        }
    }

    fn send_to(&self, id: u64, frame: Value) {
        if let Some(session) = lock(&self.sessions).attached.get(&id) {
            let _ = session.send(Outbound::Frame(frame));
        }
    }

    fn broadcast(&self, frame: &Value) {
        for session in lock(&self.sessions).attached.values() {
            let _ = session.send(Outbound::Frame(frame.clone()));
        }
    }

    async fn write(&self, frame: &Value) -> std::io::Result<()> {
        write_line(&mut *self.stdin.lock().await, frame).await
    }
}

async fn write_line<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Value,
) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(frame).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

async fn start_server(
    host: Weak<Host>,
    spec: ServerSpec,
    key: String,
) -> Result<Arc<Server>, String> {
    let mut command = Command::new(&spec.command);
    command
        .args(&spec.args)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(crate::mcp_stdio::CREATE_NO_WINDOW);
    crate::mcp_stdio::configure_process_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn failed: {error}"))?;
    let tree = crate::mcp_stdio::guard_process_tree(&child).ok();
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err("the server's stdio was not piped".into());
    };
    let stderr_task = tokio::spawn(drain_stderr(spec.name.clone(), stderr));
    let mut lines = BufReader::new(stdout).lines();
    let timeout = Duration::from_millis(spec.init_timeout_ms.max(1));
    let init_result = match tokio::time::timeout(timeout, handshake(&mut stdin, &mut lines)).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            stderr_task.abort();
            let _ = child.kill().await;
            return Err(error);
        }
        Err(_) => {
            stderr_task.abort();
            let _ = child.kill().await;
            return Err("request `initialize` timed out".into());
        }
    };
    let server = Arc::new(Server {
        key,
        pid: child.id(),
        started_at_ms: now_ms(),
        init_result,
        stdin: tokio::sync::Mutex::new(stdin),
        child: tokio::sync::Mutex::new(Some(child)),
        tree: StdMutex::new(tree),
        next_id: AtomicI64::new(1),
        pending: StdMutex::new(HashMap::new()),
        sessions: StdMutex::new(Sessions {
            idle_since: Some(Instant::now()),
            ..Sessions::default()
        }),
        reader: StdMutex::new(None),
        stderr: StdMutex::new(Some(stderr_task)),
        spec,
    });
    let reader = tokio::spawn(pump_server(host, Arc::downgrade(&server), lines));
    *lock(&server.reader) = Some(reader);
    Ok(server)
}

/// `initialize` and `initialized`, as the stdio client sends them.
async fn handshake(
    stdin: &mut ChildStdin,
    lines: &mut Lines<BufReader<ChildStdout>>,
) -> Result<Value, String> {
    write_line(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "clientInfo": { "name": "rebon", "version": env!("CARGO_PKG_VERSION") },
            },
        }),
    )
    .await
    .map_err(|error| format!("stdin write: {error}"))?;
    loop {
        let line = lines
            .next_line()
            .await
            .map_err(|error| format!("stdout read: {error}"))?
            .ok_or_else(|| "the server exited during initialize".to_string())?;
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if value.get("method").is_some() {
            if let Some(id) = value.get("id").cloned() {
                let _ = write_line(stdin, &error_response(id, -32601, "Method not found")).await;
            }
            continue;
        }
        if value.get("id").and_then(Value::as_i64) != Some(0) {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(format!(
                "initialize failed: {} ({})",
                error.get("message").and_then(Value::as_str).unwrap_or(""),
                error.get("code").and_then(Value::as_i64).unwrap_or(0)
            ));
        }
        let result = value.get("result").cloned().unwrap_or(Value::Null);
        write_line(
            stdin,
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} }),
        )
        .await
        .map_err(|error| format!("stdin write: {error}"))?;
        return Ok(result);
    }
}

async fn pump_server(
    host: Weak<Host>,
    server: Weak<Server>,
    mut lines: Lines<BufReader<ChildStdout>>,
) {
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(mut value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let Some(server) = server.upgrade() else {
            return;
        };
        if value.get("method").is_some() {
            match value.get("id").cloned() {
                // Server requests are refused, as the stdio client refuses them.
                Some(id) => {
                    let _ = server
                        .write(&error_response(id, -32601, "Method not found"))
                        .await;
                }
                None => server.broadcast(&value),
            }
            continue;
        }
        let Some(id) = value.get("id").and_then(Value::as_i64) else {
            continue;
        };
        let Some((session, original)) = lock(&server.pending).remove(&id) else {
            continue;
        };
        value["id"] = original;
        server.send_to(session, value);
    }
    if let (Some(host), Some(server)) = (host.upgrade(), server.upgrade()) {
        host.retire(&server, None, true).await;
    }
}

async fn drain_stderr(name: String, stderr: tokio::process::ChildStderr) {
    const MAX_LINES: usize = 64;
    let mut lines = BufReader::new(stderr).lines();
    let mut logged = 0;
    while let Ok(Some(line)) = lines.next_line().await {
        if logged < MAX_LINES {
            let line: String = line.chars().take(2048).collect();
            eprintln!("[{name}] {line}");
            logged += 1;
        }
    }
}

async fn handle_connection(host: Arc<Host>, stream: TcpStream) {
    host.connections.fetch_add(1, Ordering::SeqCst);
    let _count = ConnectionCount(host.clone());
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, lines.next_line()).await {
        Ok(Ok(Some(line))) => serde_json::from_str::<Hello>(&line).ok(),
        _ => None,
    };
    let Some(hello) = hello else {
        return;
    };
    if hello.schema != HOST_SCHEMA || !rebon_types::constant_time_eq(&hello.token, &host.token) {
        let _ = reply(&mut write, &Reply::error("not authorised")).await;
        return;
    }
    match hello.request {
        Request::List => {
            let servers = host.list();
            let _ = reply(
                &mut write,
                &Reply {
                    servers: Some(servers),
                    ..Reply::default()
                },
            )
            .await;
        }
        Request::Stop { name, reason, cwd } => {
            let stopped = host.stop_named(&name, reason, cwd.as_deref()).await;
            let _ = reply(
                &mut write,
                &Reply {
                    stopped: Some(stopped),
                    ..Reply::default()
                },
            )
            .await;
        }
        Request::Shutdown => {
            let _ = reply(&mut write, &Reply::default()).await;
            host.shutdown.notify_one();
        }
        Request::Attach { spec } => attach(host, spec, lines, write).await,
    }
}

async fn reply(write: &mut tokio::net::tcp::OwnedWriteHalf, reply: &Reply) -> std::io::Result<()> {
    write_line(
        write,
        &serde_json::to_value(reply).map_err(std::io::Error::other)?,
    )
    .await
}

async fn attach(
    host: Arc<Host>,
    spec: ServerSpec,
    mut lines: Lines<BufReader<OwnedReadHalf>>,
    mut write: tokio::net::tcp::OwnedWriteHalf,
) {
    let session = host.next_session.fetch_add(1, Ordering::Relaxed);
    let (sender, mut outbound) = unbounded_channel();
    // A server retired between being found and being joined is replaced once;
    // twice in a row means something is stopping it on purpose.
    let mut server = None;
    for _ in 0..2 {
        match host.server_for(spec.clone()).await {
            Ok(found) => {
                if found.add_session(session, sender.clone()) {
                    server = Some(found);
                    break;
                }
            }
            Err(error) => {
                let _ = reply(&mut write, &Reply::error(error)).await;
                return;
            }
        }
    }
    let Some(server) = server else {
        let _ = reply(&mut write, &Reply::error("the server was stopped")).await;
        return;
    };
    if reply(
        &mut write,
        &Reply {
            pid: server.pid,
            ..Reply::default()
        },
    )
    .await
    .is_err()
    {
        server.remove_session(session);
        return;
    }
    let writer = tokio::spawn(async move {
        while let Some(message) = outbound.recv().await {
            match message {
                Outbound::Frame(frame) => {
                    if write_line(&mut write, &frame).await.is_err() {
                        break;
                    }
                }
                Outbound::Close => break,
            }
        }
        let _ = write.shutdown().await;
    });
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if !relay_from_session(&server, session, &sender, value).await {
            break;
        }
    }
    server.remove_session(session);
    drop(sender);
    let _ = writer.await;
}

/// One message from a session. `false` when the server can no longer be
/// written to.
async fn relay_from_session(
    server: &Server,
    session: u64,
    sender: &UnboundedSender<Outbound>,
    mut value: Value,
) -> bool {
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let id = value.get("id").cloned();
    match (method.as_deref(), id) {
        (Some("initialize"), Some(id)) => {
            let _ = sender.send(Outbound::Frame(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": server.init_result,
            })));
            true
        }
        (Some("notifications/initialized"), None) => true,
        (Some(_), Some(id)) => {
            let host_id = server.next_id.fetch_add(1, Ordering::Relaxed);
            lock(&server.pending).insert(host_id, (session, id.clone()));
            value["id"] = json!(host_id);
            if server.write(&value).await.is_err() {
                lock(&server.pending).remove(&host_id);
                let _ = sender.send(Outbound::Frame(error_response(
                    id,
                    -32000,
                    "the shared MCP server is not accepting input",
                )));
                return false;
            }
            true
        }
        (Some(method), None) => {
            if method == "notifications/cancelled" {
                let wanted = value.pointer("/params/requestId").cloned();
                let host_id = lock(&server.pending).iter().find_map(|(host_id, entry)| {
                    (entry.0 == session && Some(&entry.1) == wanted.as_ref()).then_some(*host_id)
                });
                let Some(host_id) = host_id else {
                    return true;
                };
                value["params"]["requestId"] = json!(host_id);
            }
            server.write(&value).await.is_ok()
        }
        // A response: the only requests a server makes are answered here.
        (None, _) => true,
    }
}
