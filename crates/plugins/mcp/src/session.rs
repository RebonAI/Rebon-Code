//! Session MCP construction, reuse and plugin-generation ownership.

use crate::lifecycle::{ManagedClient, ShutdownTasks};
use crate::mcp::AggregateMcpClient;
use crate::mcp_http::{HttpMcpClient, HttpServerConfig};
use crate::mcp_sse::{SseMcpClient, SseServerConfig};
use crate::mcp_stdio::{StdioMcpClient, StdioServerConfig};
use rebon_agent_core::PromptExecutorError;
use rebon_core::mcp_runtime::McpSessionRequest;
use rebon_proto::McpServerConfig;
use rebon_tool::McpClient;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

struct SessionEntry {
    fingerprint: String,
    client: Arc<ManagedClient>,
    cancelled: tokio::sync::watch::Sender<bool>,
    // Failures are cached too. Editing the configuration is the retry boundary.
    ready: tokio::sync::watch::Receiver<Option<Result<(), String>>>,
}

impl SessionEntry {
    fn revoke(&self) {
        let _ = self.client.revoke();
        self.cancelled.send_replace(true);
    }

    async fn client(&self) -> Result<Arc<ManagedClient>, PromptExecutorError> {
        let mut ready = self.ready.clone();
        let mut cancelled = self.cancelled.subscribe();
        loop {
            if *cancelled.borrow() {
                return Err(stale_generation());
            }
            if let Some(result) = ready.borrow().clone() {
                result.map_err(PromptExecutorError::Execution)?;
                return Ok(self.client.clone());
            }
            tokio::select! {
                biased;
                _ = cancelled.changed() => return Err(stale_generation()),
                changed = ready.changed() => {
                    changed.map_err(|_| PromptExecutorError::Execution("MCP construction task ended without a result".into()))?;
                }
            }
        }
    }
}

fn stale_generation() -> PromptExecutorError {
    PromptExecutorError::Execution("[STALE_PROVIDER] MCP session generation was replaced".into())
}

pub(super) async fn cancelled(mut cancel: tokio::sync::watch::Receiver<bool>) {
    while !*cancel.borrow_and_update() {
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Arc<SessionEntry>>,
    globals: Vec<(Arc<dyn McpClient>, Arc<ManagedClient>)>,
}

pub(super) struct SessionRuntime {
    pub(super) active: Arc<AtomicBool>,
    state: Mutex<State>,
    tasks: ShutdownTasks,
}

impl SessionRuntime {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            active: Arc::new(AtomicBool::new(true)),
            state: Mutex::new(State::default()),
            tasks: ShutdownTasks::default(),
        })
    }

    fn ensure_live(&self) -> Result<(), PromptExecutorError> {
        if self.active.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(PromptExecutorError::Execution(
                "[STALE_PROVIDER] MCP plugin is unloaded".into(),
            ))
        }
    }

    /// Select a generation under a short registry lock. A newer fingerprint
    /// revokes the old generation immediately, including an unfinished build.
    pub(super) async fn client_for_session(
        &self,
        request: McpSessionRequest,
    ) -> Result<Option<Arc<dyn McpClient>>, PromptExecutorError> {
        let fingerprint = serde_json::to_string(&request.servers).map_err(|error| {
            PromptExecutorError::Misconfigured(format!(
                "failed to fingerprint ACP mcpServers: {error}"
            ))
        })?;
        let entry = {
            let mut state = self.state.lock().expect("MCP runtime state poisoned");
            self.ensure_live()?;
            if state
                .sessions
                .get(&request.session_id)
                .is_some_and(|entry| entry.fingerprint != fingerprint)
            {
                state
                    .sessions
                    .remove(&request.session_id)
                    .expect("entry exists")
                    .revoke();
            }
            if request.servers.is_empty() {
                None
            } else {
                Some(
                    state
                        .sessions
                        .entry(request.session_id.clone())
                        .or_insert_with(|| self.start_generation(fingerprint, request.servers))
                        .clone(),
                )
            }
        };
        let session = match &entry {
            Some(entry) => Some(entry.client().await?),
            None => None,
        };
        let global = {
            let mut state = self.state.lock().expect("MCP runtime state poisoned");
            self.ensure_live()?;
            if let Some(entry) = &entry {
                if !state
                    .sessions
                    .get(&request.session_id)
                    .is_some_and(|current| Arc::ptr_eq(current, entry))
                {
                    return Err(stale_generation());
                }
            }
            request.global.map(|global| {
                if let Some((_, managed)) = state
                    .globals
                    .iter()
                    .find(|(original, _)| Arc::ptr_eq(original, &global))
                {
                    return managed.clone() as Arc<dyn McpClient>;
                }
                let managed = ManagedClient::new(global.clone());
                state.globals.push((global, managed.clone()));
                managed as Arc<dyn McpClient>
            })
        };
        Ok(match (session, global) {
            // A session-defined server shadows the host server of the same name.
            (Some(session), Some(global)) => Some(ManagedClient::with_parent(
                Arc::new(AggregateMcpClient::new(vec![session.clone(), global])),
                Some(session),
            ) as Arc<dyn McpClient>),
            (Some(session), None) => Some(session as Arc<dyn McpClient>),
            (None, global) => global,
        })
    }

    fn start_generation(
        &self,
        fingerprint: String,
        configs: Vec<McpServerConfig>,
    ) -> Arc<SessionEntry> {
        let transports = SessionTransports::new();
        let client = ManagedClient::new(transports.aggregate.clone());
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (ready_tx, ready_rx) = tokio::sync::watch::channel(None);
        let entry = Arc::new(SessionEntry {
            fingerprint,
            client,
            cancelled: cancel_tx,
            ready: ready_rx,
        });
        let generation = entry.clone();
        // Registered while holding state: host shutdown cannot miss a task
        // between deciding which generation owns it and saving its JoinHandle.
        self.tasks.spawn(async move {
            let built = build_session_mcp_client(&configs, &transports, cancel_rx.clone()).await;
            let live = built.is_ok() && !*cancel_rx.borrow();
            if !live {
                let _ = generation.client.revoke();
            }
            let _ = ready_tx.send_replace(Some(built.map_err(|error| error.to_string())));
            if live {
                cancelled(cancel_rx).await;
            }
            let _ = generation.client.revoke();
            // Also closes successful earlier transports on failure/cancellation.
            transports.aggregate.shutdown_transport().await;
        });
        entry
    }

    /// Async completion boundary used by the process host, not the sync disposer.
    pub(super) async fn shutdown(&self) {
        self.unload();
        self.tasks.join().await;
    }

    /// Revoke cached and already-issued handles before releasing their owners.
    pub(super) fn unload(&self) {
        let mut state = self.state.lock().expect("MCP runtime state poisoned");
        self.active.store(false, Ordering::Release);
        for (_, entry) in state.sessions.drain() {
            entry.revoke();
        }
        let clients = state
            .globals
            .drain(..)
            .filter_map(|(_, client)| client.revoke())
            .collect::<Vec<_>>();
        if !clients.is_empty() {
            self.tasks.spawn(async move {
                for client in clients {
                    client.shutdown_transport().await;
                }
            });
        }
    }
}

struct SessionTransports {
    stdio: Arc<StdioMcpClient>,
    http: Arc<HttpMcpClient>,
    sse: Arc<SseMcpClient>,
    aggregate: Arc<AggregateMcpClient>,
}

impl SessionTransports {
    fn new() -> Self {
        let stdio = Arc::new(StdioMcpClient::new());
        let http = Arc::new(HttpMcpClient::new());
        let sse = Arc::new(SseMcpClient::new());
        let aggregate = Arc::new(AggregateMcpClient::new(vec![
            stdio.clone(),
            http.clone(),
            sse.clone(),
        ]));
        Self {
            stdio,
            http,
            sse,
            aggregate,
        }
    }
}

/// The generation task owns the transaction and awaits cleanup on every exit.
async fn build_session_mcp_client(
    configs: &[McpServerConfig],
    transports: &SessionTransports,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<(), PromptExecutorError> {
    if configs.is_empty() {
        return Err(PromptExecutorError::Misconfigured(
            "ACP mcpServers was non-empty but no supported MCP transports were configured".into(),
        ));
    }
    let SessionTransports {
        stdio, http, sse, ..
    } = transports;
    for config in configs {
        if *cancel.borrow() {
            return Err(stale_generation());
        }
        let (transport, result) = match config {
            McpServerConfig::Stdio {
                name,
                command,
                args,
                env,
                cwd,
            } => (
                "start ACP stdio",
                stdio
                    .add_server_cancellable(
                        StdioServerConfig {
                            name: name.clone(),
                            command: command.clone(),
                            args: args.clone(),
                            env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                            cwd: cwd.clone(),
                            request_timeout: None,
                        },
                        cancelled(cancel.clone()),
                    )
                    .await,
            ),
            McpServerConfig::Http { name, url, headers } => (
                "connect ACP HTTP",
                tokio::select! {
                    biased;
                    _ = cancelled(cancel.clone()) => return Err(stale_generation()),
                    result = http.add_server(HttpServerConfig {
                        name: name.clone(),
                        url: url.clone(),
                        headers: headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                        request_timeout: None,
                    }) => result,
                },
            ),
            McpServerConfig::Sse { name, url, headers } => (
                "connect ACP SSE",
                sse.add_server_cancellable(
                    SseServerConfig {
                        name: name.clone(),
                        url: url.clone(),
                        headers: headers
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                        request_timeout: None,
                    },
                    cancelled(cancel.clone()),
                )
                .await,
            ),
        };
        if let Err(error) = result {
            return Err(PromptExecutorError::Execution(format!(
                "failed to {transport} MCP server: {error}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use rebon_tool::{McpClientError, McpToolCall};
    use serde_json::{json, Value};
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Notify;
    use tokio::time::{timeout, Duration};

    // A real local HTTP transport; Drop aborts the bounded test server task.
    pub(crate) struct Server {
        url: String,
        pub(crate) initializes: Arc<AtomicUsize>,
        pub(crate) calls: Arc<Mutex<Vec<Value>>>,
        started: Arc<Notify>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl Server {
        pub(crate) async fn start(fail_first: bool, gate: Option<Arc<Notify>>) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let initializes = Arc::new(AtomicUsize::new(0));
            let count = initializes.clone();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let recorded = calls.clone();
            let started = Arc::new(Notify::new());
            let notify = started.clone();
            let task = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    let header_end = loop {
                        let mut byte = [0];
                        stream.read_exact(&mut byte).await.unwrap();
                        bytes.push(byte[0]);
                        if bytes.ends_with(b"\r\n\r\n") {
                            break bytes.len();
                        }
                    };
                    let headers = String::from_utf8_lossy(&bytes);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    bytes.resize(header_end + length, 0);
                    stream.read_exact(&mut bytes[header_end..]).await.unwrap();
                    let request: Value = serde_json::from_slice(&bytes[header_end..]).unwrap();
                    let method = request["method"].as_str().unwrap();
                    let failed = if method == "initialize" {
                        let attempt = count.fetch_add(1, Ordering::SeqCst);
                        notify.notify_one();
                        if let Some(gate) = &gate {
                            gate.notified().await;
                        }
                        fail_first && attempt == 0
                    } else {
                        false
                    };
                    let result = match method {
                        "initialize" => {
                            json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"test","version":"1"}})
                        }
                        "tools/list" => {
                            json!({"tools":[{"name":"ping","description":"Ping","inputSchema":{"type":"object"},"_meta":{"anthropic/alwaysLoad":false}}]})
                        }
                        "tools/call" => {
                            recorded.lock().unwrap().push(request["params"].clone());
                            json!({"content":{"source":"session"},"isError":false})
                        }
                        _ => json!({}),
                    };
                    let body = if failed { json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"first attempt fails"}}) }
                        else { json!({"jsonrpc":"2.0","id":request["id"],"result":result}) }.to_string();
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    // The client may be cancelled by the unload test.
                    let _ = stream.write_all(response.as_bytes()).await;
                }
            });
            Self {
                url,
                initializes,
                calls,
                started,
                task,
            }
        }
        pub(crate) fn config(&self, name: &str) -> McpServerConfig {
            McpServerConfig::Http {
                name: name.into(),
                url: self.url.clone(),
                headers: Default::default(),
            }
        }
    }
    fn request(servers: Vec<McpServerConfig>) -> McpSessionRequest {
        McpSessionRequest {
            session_id: "session".into(),
            servers,
            global: None,
        }
    }
    fn ping() -> McpToolCall {
        McpToolCall {
            server: "server".into(),
            name: "ping".into(),
            arguments: json!({}),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_turns_reuse_connection_and_empty_config_revokes_old_handle() {
        let server = Server::start(false, None).await;
        let runtime = SessionRuntime::new();
        let (a, b) = tokio::join!(
            runtime.client_for_session(request(vec![server.config("server")])),
            runtime.client_for_session(request(vec![server.config("server")]))
        );
        let a = a.unwrap().unwrap();
        let b = b.unwrap().unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(server.initializes.load(Ordering::SeqCst), 1);
        assert_eq!(
            a.call_tool(ping()).await.unwrap().content["source"],
            "session"
        );
        assert_eq!(
            a.list_tool_definitions("server").await.unwrap()[0].name,
            "ping"
        );
        assert_eq!(a.cached_tool_definitions("server").unwrap()[0].name, "ping");
        assert!(runtime
            .client_for_session(request(vec![]))
            .await
            .unwrap()
            .is_none());
        assert!(a.server_names().is_empty());
        assert!(a.cached_tool_definitions("server").is_none());
        assert!(a.list_tool_definitions("server").await.is_none());
        assert!(a.call_tool(ping()).await.is_err());
        assert!(b.reconnect_server("server").await.is_err());
        runtime.unload();
        runtime.shutdown().await;
        assert!(runtime.state.lock().unwrap().sessions.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn construction_failure_is_sticky_until_configuration_changes() {
        let server = Server::start(true, None).await;
        let runtime = SessionRuntime::new();
        let first = runtime
            .client_for_session(request(vec![server.config("server")]))
            .await
            .err()
            .unwrap()
            .to_string();
        let second = runtime
            .client_for_session(request(vec![server.config("server")]))
            .await
            .err()
            .unwrap()
            .to_string();
        assert_eq!(first, second);
        assert!(first.contains("first attempt fails"));
        assert_eq!(server.initializes.load(Ordering::SeqCst), 1);
        let retried = runtime
            .client_for_session(request(vec![server.config("renamed")]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retried.server_names(), vec!["renamed"]);
        assert_eq!(server.initializes.load(Ordering::SeqCst), 2);
        runtime.unload();
        runtime.shutdown().await;
        assert!(retried.server_names().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn merged_view_shadows_host_and_never_falls_through_after_session_replacement() {
        let server = Server::start(false, None).await;
        let host = Arc::new(crate::InMemoryMcpClient::new());
        host.register_tool("server", "ping", json!({"source":"host"}));
        let runtime = SessionRuntime::new();
        let mut configured = request(vec![server.config("server")]);
        configured.global = Some(host.clone());
        let merged = runtime
            .client_for_session(configured)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(merged.server_names(), vec!["server"]);
        assert_eq!(
            merged.call_tool(ping()).await.unwrap().content["source"],
            "session"
        );
        assert!(host.calls().is_empty());
        let mut empty = request(vec![]);
        empty.global = Some(host.clone());
        let host_view = runtime.client_for_session(empty).await.unwrap().unwrap();
        assert!(matches!(
            merged.call_tool(ping()).await,
            Err(McpClientError::Transport(_))
        ));
        assert!(merged.server_names().is_empty());
        assert!(merged.cached_tool_definitions("server").is_none());
        assert!(host.calls().is_empty());
        assert_eq!(
            host_view.call_tool(ping()).await.unwrap().content["source"],
            "host"
        );
        runtime.unload();
        runtime.shutdown().await;
        assert!(host_view.call_tool(ping()).await.is_err());
        assert!(runtime.state.lock().unwrap().globals.is_empty());
    }

    struct ClosingProbe {
        started: Notify,
        release: Notify,
        finished: AtomicBool,
        shutdowns: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl McpClient for ClosingProbe {
        async fn call_tool(
            &self,
            _: McpToolCall,
        ) -> Result<rebon_tool::McpToolResult, McpClientError> {
            unreachable!("shutdown probe never executes tools")
        }
        async fn shutdown_transport(&self) {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
            self.finished.store(true, Ordering::Release);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn host_shutdown_joins_transport_even_after_synchronous_dispose() {
        for off_runtime in [false, true] {
            let kernel = rebon_kernel::Kernel::new();
            kernel
                .context()
                .provide::<rebon_core::tool_seat::ToolSeatService>(
                    rebon_core::tool_seat::ToolSeat::new(),
                )
                .unwrap();
            let plugin = kernel.context().fork("mcp");
            let owner = Arc::new(crate::lifecycle::RuntimeOwner::default());
            crate::McpPlugin.apply_with_owner(&plugin, &owner).unwrap();
            let factory = kernel
                .context()
                .get::<rebon_core::mcp_runtime::McpRuntimeService>()
                .unwrap();
            let probe = Arc::new(ClosingProbe {
                started: Notify::new(),
                release: Notify::new(),
                finished: AtomicBool::new(false),
                shutdowns: AtomicUsize::new(0),
            });
            let mut config = request(vec![]);
            config.global = Some(probe.clone());
            let client = factory(config).await.unwrap().unwrap();
            // No current Tokio handle on this host thread: disposal must retain the
            // close future for the async owner, rather than using a detached thread.
            if off_runtime {
                std::thread::spawn(move || plugin.dispose()).join().unwrap();
            } else {
                plugin.dispose();
            }
            assert!(!probe.finished.load(Ordering::Acquire));
            assert!(client.call_tool(ping()).await.is_err());
            let closing = owner.clone();
            let mut shutdown = tokio::spawn(async move { closing.shutdown().await });
            timeout(Duration::from_secs(2), probe.started.notified())
                .await
                .unwrap();
            assert!(
                timeout(Duration::from_millis(100), &mut shutdown)
                    .await
                    .is_err(),
                "host shutdown returned while transport shutdown was still pending"
            );
            // Cancelling a waiter must not detach/lose its join handles.
            shutdown.abort();
            assert!(shutdown.await.unwrap_err().is_cancelled());
            assert!(!probe.finished.load(Ordering::Acquire));
            probe.release.notify_one();
            timeout(Duration::from_secs(2), owner.shutdown())
                .await
                .unwrap();
            assert!(probe.finished.load(Ordering::Acquire));
            owner.shutdown().await;
            assert_eq!(probe.shutdowns.load(Ordering::SeqCst), 1);
            assert!(factory(request(vec![])).await.is_err());
            let late = SessionRuntime::new();
            owner.register(late.clone());
            assert!(late.client_for_session(request(vec![])).await.is_err());
            owner.shutdown().await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replacement_cancels_pending_generation_without_waiting_for_old_io() {
        for empty in [false, true] {
            let gate = Arc::new(Notify::new());
            let old_server = Server::start(false, Some(gate.clone())).await;
            let new_server = Server::start(false, None).await;
            let runtime = SessionRuntime::new();
            let owner = runtime.clone();
            let old_config = old_server.config("old");
            let pending =
                tokio::spawn(
                    async move { owner.client_for_session(request(vec![old_config])).await },
                );
            timeout(Duration::from_secs(5), old_server.started.notified())
                .await
                .unwrap();
            let configs = if empty {
                vec![]
            } else {
                vec![new_server.config("new")]
            };
            let replacement = timeout(
                Duration::from_secs(2),
                runtime.client_for_session(request(configs.clone())),
            )
            .await
            .expect("new fingerprint must cancel the old build, not queue behind its IO")
            .unwrap();
            assert_eq!(replacement.is_none(), empty);
            let obsolete = timeout(Duration::from_secs(2), pending)
                .await
                .unwrap()
                .unwrap();
            assert!(obsolete
                .err()
                .unwrap()
                .to_string()
                .contains("STALE_PROVIDER"));
            gate.notify_one();
            let again = runtime.client_for_session(request(configs)).await.unwrap();
            if let Some(replacement) = replacement {
                assert!(Arc::ptr_eq(&replacement, &again.unwrap()));
                assert_eq!(replacement.server_names(), vec!["new"]);
                assert_eq!(new_server.initializes.load(Ordering::SeqCst), 1);
            } else {
                assert!(again.is_none());
            }
            runtime.unload();
            runtime.shutdown().await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unload_cancels_pending_construction_and_prevents_cache_resurrection() {
        let gate = Arc::new(Notify::new());
        let server = Server::start(false, Some(gate)).await;
        let runtime = SessionRuntime::new();
        let owner = runtime.clone();
        let config = server.config("server");
        let pending =
            tokio::spawn(async move { owner.client_for_session(request(vec![config])).await });
        timeout(Duration::from_secs(5), server.started.notified())
            .await
            .unwrap();
        runtime.unload();
        runtime.shutdown().await;
        let result = timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap();
        assert!(result.err().unwrap().to_string().contains("STALE_PROVIDER"));
        assert!(runtime.state.lock().unwrap().sessions.is_empty());
        assert!(runtime.client_for_session(request(vec![])).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replacement_and_host_join_close_unpublished_stdio_child() {
        let node = std::env::var("REBON_TEST_NODE")
            .expect("set REBON_TEST_NODE to the absolute node executable");
        assert!(std::path::Path::new(&node).is_absolute());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let script = format!(
            "const s = require('net').connect({}, '127.0.0.1', () => s.write('ready')); process.stdin.resume(); setInterval(() => {{}}, 1000);",
            listener.local_addr().unwrap().port()
        );
        let runtime = SessionRuntime::new();
        let generation = runtime.clone();
        let pending = tokio::spawn(async move {
            generation
                .client_for_session(request(vec![McpServerConfig::Stdio {
                    name: "pending-child".into(),
                    command: node,
                    args: vec!["-e".into(), script],
                    env: Default::default(),
                    cwd: None,
                }]))
                .await
        });
        let (mut child_socket, _) = timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut ready = [0; 5];
        timeout(Duration::from_secs(2), child_socket.read_exact(&mut ready))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&ready, b"ready");
        assert!(timeout(
            Duration::from_secs(2),
            runtime.client_for_session(request(vec![]))
        )
        .await
        .unwrap()
        .unwrap()
        .is_none());
        assert!(timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap()
            .err()
            .unwrap()
            .to_string()
            .contains("STALE_PROVIDER"));
        timeout(Duration::from_secs(2), runtime.shutdown())
            .await
            .expect("host must join child wait and both pipe readers");
        let eof = timeout(Duration::from_secs(2), child_socket.read_u8())
            .await
            .expect("stdio child survived host shutdown");
        assert!(
            eof.is_err(),
            "unpublished child must close its socket before host exit"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unload_cancels_sse_handshake_and_closes_unpublished_reader() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/sse", listener.local_addr().unwrap());
        let started = Arc::new(Notify::new());
        let ready = started.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(stream.read_u8().await.unwrap());
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n: connected\n\n").await.unwrap();
            ready.notify_one();
            let mut byte = [0];
            stream.read(&mut byte).await.unwrap()
        });
        let runtime = SessionRuntime::new();
        let owner = runtime.clone();
        let pending = tokio::spawn(async move {
            owner
                .client_for_session(request(vec![McpServerConfig::Sse {
                    name: "pending-sse".into(),
                    url,
                    headers: Default::default(),
                }]))
                .await
        });
        timeout(Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        runtime.unload();
        runtime.shutdown().await;
        assert!(timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert_eq!(
            timeout(Duration::from_secs(2), server)
                .await
                .expect("SSE reader leaked after unload")
                .unwrap(),
            0
        );
        assert!(runtime.state.lock().unwrap().sessions.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_shutdown_removes_live_and_parked_connection_caches() {
        let server = Server::start(false, None).await;
        let client = HttpMcpClient::new();
        client
            .add_server(HttpServerConfig::new("server", server.url.clone()))
            .await
            .unwrap();
        assert!(client.list_tool_definitions("server").await.is_some());
        assert!(client.cached_tool_definitions("server").is_some());
        let erased: &dyn McpClient = &client;
        erased.shutdown_transport().await;
        assert_eq!(client.server_count().await, 0);
        assert!(erased.cached_tool_definitions("server").is_none());
        assert!(erased.call_tool(ping()).await.is_err());
        client
            .add_server(HttpServerConfig::new("server", server.url.clone()))
            .await
            .unwrap();
        assert!(erased.disconnect_server("server").await);
        erased.shutdown_transport().await;
        assert!(!erased.reconnect_server("server").await.unwrap());
        assert_eq!(server.initializes.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn partial_transport_failure_does_not_publish_a_client() {
        let server = Server::start(false, None).await;
        let runtime = SessionRuntime::new();
        let broken = McpServerConfig::Http {
            name: "broken".into(),
            url: "://invalid".into(),
            headers: Default::default(),
        };
        let result = runtime
            .client_for_session(request(vec![server.config("server"), broken]))
            .await;
        assert!(result.is_err());
        assert_eq!(server.initializes.load(Ordering::SeqCst), 1);
        let entry = runtime.state.lock().unwrap().sessions["session"].clone();
        assert!(entry.client.server_names().is_empty());
        let (_cancel, cancelled) = tokio::sync::watch::channel(false);
        assert!(
            build_session_mcp_client(&[], &SessionTransports::new(), cancelled)
                .await
                .is_err()
        );
        runtime.shutdown().await;
    }
}
