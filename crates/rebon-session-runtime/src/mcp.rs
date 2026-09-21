//! The MCP stack a session's tools run against.
//!
//! A session is handed one seam — [`DelayedTuiMcpClient`] — at build time and
//! keeps it for its life; what sits behind the seam arrives later, when the
//! servers finish coming up, and can be handed on to the next session a
//! worker builds. [`SessionMcp`] is that stack as one value, and
//! [`McpLoadRequest`] is what it was asked for, so a held stack is only lent
//! to a session that would have asked for the same thing.
//!
//! Startup is best-effort throughout: a server that fails to connect adds a
//! warning and the session goes on without it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use rebon_plugin_mcp::{AggregateMcpClient, HttpMcpClient, SseMcpClient, StdioMcpClient};
use rebon_tool::{
    McpClient, McpClientError, McpShutdownReport, McpToolCall, McpToolDefinition, McpToolResult,
};

/// Outcome of [`build_default_mcp_client_for_cwd`].
///
/// MCP startup is **best-effort**: a server that fails to connect — an
/// unreachable URL, a refused handshake, a transport error — is skipped
/// rather than aborting the whole session. Each skipped server adds a
/// human-readable line to [`warnings`], which the TUI surfaces as a
/// startup notice and the ACP path logs. A genuinely malformed config
/// (bad JSON, duplicate name, unsupported field) is still a hard error
/// returned from the function itself.
///
/// [`warnings`]: McpClientBuild::warnings
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub struct McpClientBuild {
    /// Aggregate client over the servers that actually came up, or
    /// `None` when nothing was configured or every server was skipped.
    pub client: Option<Arc<dyn McpClient>>,
    /// One line per server that failed to start, in config order.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiMcpLoadStatus {
    Loading,
    Ready { warnings: Vec<String> },
    Failed { error: String },
    NotConfigured,
}

pub enum TuiMcpLoadEvent {
    Ready(McpClientBuild),
    NotConfigured,
    Failed(String),
}

#[derive(Clone, Default)]
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub struct DelayedTuiMcpClient {
    delegate: Arc<RwLock<Option<Arc<dyn McpClient>>>>,
}

impl DelayedTuiMcpClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_delegate(&self, client: Arc<dyn McpClient>) {
        {
            let mut guard = self
                .delegate
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some(client);
        }
    }

    pub fn is_ready(&self) -> bool {
        self.delegate
            .read()
            .expect("MCP delegate poisoned")
            .is_some()
    }

    fn delegate(&self) -> Option<Arc<dyn McpClient>> {
        self.delegate.read().expect("MCP delegate poisoned").clone()
    }
}

#[async_trait::async_trait]
impl McpClient for DelayedTuiMcpClient {
    async fn call_tool(&self, call: McpToolCall) -> Result<McpToolResult, McpClientError> {
        let Some(delegate) = self.delegate() else {
            return Err(McpClientError::UnknownServer(call.server));
        };
        delegate.call_tool(call).await
    }

    async fn list_tools(&self, server: &str) -> Option<Vec<String>> {
        let delegate = self.delegate()?;
        delegate.list_tools(server).await
    }

    async fn list_tool_definitions(&self, server: &str) -> Option<Vec<McpToolDefinition>> {
        let delegate = self.delegate()?;
        delegate.list_tool_definitions(server).await
    }

    fn cached_tool_definitions(&self, server: &str) -> Option<Vec<McpToolDefinition>> {
        let delegate = self.delegate()?;
        delegate.cached_tool_definitions(server)
    }

    fn server_names(&self) -> Vec<String> {
        self.delegate()
            .map(|delegate| delegate.server_names())
            .unwrap_or_default()
    }

    fn drain_channel_notifications(&self) -> Vec<String> {
        self.delegate()
            .map(|delegate| delegate.drain_channel_notifications())
            .unwrap_or_default()
    }

    async fn shutdown_transport(&self) {
        // Detach first so no new calls can reach a half-torn-down
        // delegate, then tear the transports down.
        let delegate = self.delegate.write().expect("MCP delegate poisoned").take();
        if let Some(delegate) = delegate {
            delegate.shutdown_transport().await;
        }
    }

    async fn close_with_timeout(&self, budget: std::time::Duration) -> McpShutdownReport {
        // Detaching is the cancel half here, and it happens before anything
        // can block: once the delegate is out of the lock no new call can
        // reach it, whatever the budget does next.
        let delegate = self.delegate.write().expect("MCP delegate poisoned").take();
        match delegate {
            Some(delegate) => delegate.close_with_timeout(budget).await,
            None => McpShutdownReport::default(),
        }
    }

    /// Per-server lifecycle passes through to whatever is behind the delay.
    /// Before the MCP runtime has landed there is nothing to disconnect, and
    /// saying so ("no such server") is the same answer a caller would get for a
    /// name that does not exist.
    async fn disconnect_server(&self, name: &str) -> bool {
        let Some(delegate) = self.delegate() else {
            return false;
        };
        delegate.disconnect_server(name).await
    }

    async fn reconnect_server(&self, name: &str) -> Result<bool, McpClientError> {
        let Some(delegate) = self.delegate() else {
            return Ok(false);
        };
        delegate.reconnect_server(name).await
    }
}

#[cfg(any(test, feature = "test-support"))]
pub type TestTuiMcpBuilder = Arc<
    dyn Fn(
            PathBuf,
            Vec<rebon_plugin_mcp::runtime::ChannelEntry>,
            Vec<String>,
            bool,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<McpClientBuild>> + Send>,
        > + Send
        + Sync,
>;

#[cfg(any(test, feature = "test-support"))]
fn test_tui_mcp_builder_cell() -> &'static std::sync::Mutex<Option<TestTuiMcpBuilder>> {
    static CELL: std::sync::OnceLock<std::sync::Mutex<Option<TestTuiMcpBuilder>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(any(test, feature = "test-support"))]
pub struct TestTuiMcpBuilderGuard;

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestTuiMcpBuilderGuard {
    fn drop(&mut self) {
        {
            let mut guard = test_tui_mcp_builder_cell()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = None;
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn set_test_tui_mcp_builder(builder: TestTuiMcpBuilder) -> TestTuiMcpBuilderGuard {
    *test_tui_mcp_builder_cell()
        .lock()
        .expect("test MCP builder cell poisoned") = Some(builder);
    TestTuiMcpBuilderGuard
}

async fn build_tui_mcp_client_for_loader(
    cwd: PathBuf,
    allowed_channels: Vec<rebon_plugin_mcp::runtime::ChannelEntry>,
    mcp_configs: Vec<String>,
    strict_mcp_config: bool,
    plugin_mcp_configs: Vec<crate::mcp_config::PluginMcpConfig>,
) -> anyhow::Result<McpClientBuild> {
    #[cfg(any(test, feature = "test-support"))]
    {
        let builder = test_tui_mcp_builder_cell()
            .lock()
            .expect("test MCP builder cell poisoned")
            .clone();
        if let Some(builder) = builder {
            return builder(cwd, allowed_channels, mcp_configs, strict_mcp_config).await;
        }
    }

    build_default_mcp_client_for_cwd(
        &cwd,
        allowed_channels,
        &mcp_configs,
        strict_mcp_config,
        &plugin_mcp_configs,
    )
    .await
}

fn spawn_tui_mcp_loader(
    cwd: PathBuf,
    allowed_channels: Vec<rebon_plugin_mcp::runtime::ChannelEntry>,
    mcp_configs: Vec<String>,
    strict_mcp_config: bool,
    plugin_mcp_configs: Vec<crate::mcp_config::PluginMcpConfig>,
) -> UnboundedReceiver<TuiMcpLoadEvent> {
    let (tx, rx) = unbounded_channel();
    tokio::spawn(async move {
        let event = match build_tui_mcp_client_for_loader(
            cwd,
            allowed_channels,
            mcp_configs,
            strict_mcp_config,
            plugin_mcp_configs,
        )
        .await
        {
            Ok(build) => {
                if build.client.is_some() {
                    TuiMcpLoadEvent::Ready(build)
                } else if build.warnings.is_empty() {
                    TuiMcpLoadEvent::NotConfigured
                } else {
                    TuiMcpLoadEvent::Failed(build.warnings.join("\n"))
                }
            }
            Err(err) => TuiMcpLoadEvent::Failed(err.to_string()),
        };
        let _ = tx.send(event);
    });
    rx
}

pub async fn build_default_mcp_client_for_cwd(
    cwd: &Path,
    allowed_channels: Vec<rebon_plugin_mcp::runtime::ChannelEntry>,
    mcp_configs: &[String],
    strict_mcp_config: bool,
    plugin_mcp_configs: &[crate::mcp_config::PluginMcpConfig],
) -> anyhow::Result<McpClientBuild> {
    let configs = crate::mcp_config::collect_default_mcp_configs_with_plugin_overrides(
        cwd,
        mcp_configs,
        strict_mcp_config,
        plugin_mcp_configs,
    )?;
    if configs.is_empty() {
        return Ok(McpClientBuild {
            client: None,
            warnings: Vec::new(),
        });
    }

    let stdio_client = StdioMcpClient::with_channel_entries(allowed_channels);
    let http_client = HttpMcpClient::new();
    let sse_client = SseMcpClient::new();
    let mut has_stdio = false;
    let mut has_http = false;
    let mut has_sse = false;
    let mut warnings: Vec<String> = Vec::new();
    for item in configs {
        let name = item.config.name().to_string();
        let source = item.source.clone();
        // `add_server` only registers a server on a successful handshake,
        // so a failure here leaves the client untouched — we can record
        // the error and move on without corrupting later servers.
        let start_error = match item.config {
            crate::mcp_config::McpServerConfig::Stdio(config) => {
                match stdio_client.add_server(config).await {
                    Ok(()) => {
                        has_stdio = true;
                        None
                    }
                    Err(err) => Some(err),
                }
            }
            crate::mcp_config::McpServerConfig::Http(config) => {
                match http_client.add_server(config).await {
                    Ok(()) => {
                        has_http = true;
                        None
                    }
                    Err(err) => Some(err),
                }
            }
            crate::mcp_config::McpServerConfig::Sse(config) => {
                match sse_client.add_server(config).await {
                    Ok(()) => {
                        has_sse = true;
                        None
                    }
                    Err(err) => Some(err),
                }
            }
        };
        match start_error {
            None => {
                tracing::info!(server = %name, source = %source, "rebon: MCP server attached");
            }
            Some(err) => {
                tracing::warn!(
                    server = %name,
                    source = %source,
                    error = %err,
                    "rebon: MCP server failed to start; skipping and continuing"
                );
                warnings.push(format!(
                    "MCP server `{name}` from {source} is unavailable and was skipped: {err}"
                ));
            }
        }
    }

    let mut clients: Vec<Arc<dyn McpClient>> = Vec::new();
    if has_stdio {
        clients.push(Arc::new(stdio_client) as Arc<dyn McpClient>);
    }
    if has_http {
        clients.push(Arc::new(http_client) as Arc<dyn McpClient>);
    }
    if has_sse {
        clients.push(Arc::new(sse_client) as Arc<dyn McpClient>);
    }
    let client = if clients.len() == 1 {
        clients.pop()
    } else if clients.is_empty() {
        None
    } else {
        Some(Arc::new(AggregateMcpClient::new(clients)) as Arc<dyn McpClient>)
    };
    Ok(McpClientBuild { client, warnings })
}

/// What the MCP loader was asked for. A held stack is only lent to a session
/// that would have asked for the same thing; anything else gets a fresh load.
#[derive(Clone, PartialEq, Eq)]
pub struct McpLoadRequest {
    pub cwd: PathBuf,
    pub allowed_channels: Vec<rebon_plugin_mcp::runtime::ChannelEntry>,
    pub mcp_configs: Vec<String>,
    pub strict_mcp_config: bool,
    pub plugin_mcp_configs: Vec<crate::mcp_config::PluginMcpConfig>,
}

/// The MCP stack a process runs a session's tools against.
///
/// One value rather than four fields because it outlives any one
/// [`TuiEngineSession`] in a worker. A worker builds a session per turn and
/// per idle command, and rebuilding the servers each time cost their startup
/// on every turn — and, since nothing in the worker ever collected the
/// loader's result, never produced a single MCP tool. So the worker takes
/// the stack back when a session goes and lends it to the next build, the
/// way it does with its session lock
/// ([`rebon_session::HeldSessionLock`]).
///
/// `None` on a session says this process does not host the servers: it is a
/// client of somebody else's session, and what it shows about MCP comes from
/// the owner.
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub struct SessionMcp {
    /// The seam the executor and every tool context hold clones of. What is
    /// behind it comes and goes — a load lands, a handover clears it — but
    /// the seam itself is fixed at build time.
    pub delayed: DelayedTuiMcpClient,
    /// The same seam, typed as the client the rest of the harness wants.
    pub client: Arc<dyn McpClient>,
    pub load_status: TuiMcpLoadStatus,
    pub load_rx: UnboundedReceiver<TuiMcpLoadEvent>,
    request: McpLoadRequest,
}

impl McpLoadRequest {
    /// A request for a session that configures nothing.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test() -> Self {
        Self {
            cwd: PathBuf::from("."),
            allowed_channels: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
            plugin_mcp_configs: Vec::new(),
        }
    }
}

impl SessionMcp {
    /// Start loading. The servers come up in the background and land
    /// through [`Self::drain_load_events`]. Needs a tokio runtime to be
    /// current: the loader is a task on it.
    pub fn spawn(request: McpLoadRequest) -> Self {
        let delayed = DelayedTuiMcpClient::new();
        let client: Arc<dyn McpClient> = Arc::new(delayed.clone());
        let load_rx = spawn_tui_mcp_loader(
            request.cwd.clone(),
            request.allowed_channels.clone(),
            request.mcp_configs.clone(),
            request.strict_mcp_config,
            request.plugin_mcp_configs.clone(),
        );
        Self {
            delayed,
            client,
            load_status: TuiMcpLoadStatus::Loading,
            load_rx,
            request,
        }
    }

    /// A held stack if it answers `request`, otherwise a fresh load — with
    /// the held one torn down rather than leaked.
    pub(crate) async fn take_or_spawn(held: Option<Self>, request: McpLoadRequest) -> Self {
        match held {
            Some(held) if held.request == request => held,
            Some(stale) => {
                tracing::info!(
                    "rebon startup: held MCP stack was loaded for another cwd or config; reloading"
                );
                stale.shutdown().await;
                Self::spawn(request)
            }
            None => Self::spawn(request),
        }
    }

    /// Collect whatever the loader has finished. Returns the status when
    /// anything arrived.
    pub fn drain_load_events(&mut self) -> Option<&TuiMcpLoadStatus> {
        let mut changed = false;
        while let Ok(event) = self.load_rx.try_recv() {
            self.apply_load_event(event);
            changed = true;
        }
        changed.then_some(&self.load_status)
    }

    fn apply_load_event(&mut self, event: TuiMcpLoadEvent) {
        self.load_status = match event {
            TuiMcpLoadEvent::Ready(build) => {
                if let Some(client) = build.client {
                    self.delayed.set_delegate(client);
                }
                TuiMcpLoadStatus::Ready {
                    warnings: build.warnings,
                }
            }
            TuiMcpLoadEvent::NotConfigured => TuiMcpLoadStatus::NotConfigured,
            TuiMcpLoadEvent::Failed(error) => TuiMcpLoadStatus::Failed { error },
        };
    }

    /// Wait, up to `timeout`, for a load still in flight.
    ///
    /// A turn lists its tools once, when it starts, so a load that lands a
    /// moment later is a turn without MCP. A person typing at a terminal
    /// rarely loses that race; a worker's first prompt was queued before the
    /// worker existed and would lose it every time. Returns whether the
    /// load has finished.
    /// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
    #[doc(hidden)]
    pub async fn wait_for_load(&mut self, timeout: std::time::Duration) -> bool {
        self.drain_load_events();
        if self.load_status != TuiMcpLoadStatus::Loading {
            return true;
        }
        match tokio::time::timeout(timeout, self.load_rx.recv()).await {
            Ok(Some(event)) => {
                self.apply_load_event(event);
                true
            }
            // The loader went away without answering; nothing more will
            // come, and the status stays what it is.
            Ok(None) => false,
            Err(_) => false,
        }
    }

    /// List every server's tools once, so `cached_tool_definitions` — what
    /// `/mcp` and the owner's status snapshot read — has them before the
    /// first turn lists them for itself. Nothing to do before the load has
    /// landed: there are no servers to ask yet.
    pub(crate) async fn warm_tool_cache(&self) {
        for server in self.client.server_names() {
            let _ = self.client.list_tool_definitions(&server).await;
        }
    }

    /// Tear the servers down. A load that finishes after this is torn down
    /// too, rather than parked in a channel nobody reads.
    pub async fn shutdown(mut self) {
        self.load_rx.close();
        for client in self.drain_late_clients() {
            client.shutdown_transport().await;
        }
        self.delayed.shutdown_transport().await;
    }

    /// [`Self::shutdown`] under a deadline the transports enforce themselves.
    ///
    /// A caller with a budget — the background worker finishing a turn — used
    /// to wrap [`Self::shutdown`] in `tokio::time::timeout`, which abandons
    /// the teardown wherever it had got to and leaves server processes
    /// running past the session. This asks the transports to close within the
    /// budget instead, so cancelling always happens and only the join can be
    /// cut short; the report names whatever was still falling.
    pub(crate) async fn close_within(mut self, budget: std::time::Duration) -> McpShutdownReport {
        self.load_rx.close();
        let mut report = McpShutdownReport::default();
        // Clients that landed after the shutdown began get the same budget:
        // they are separate transports, and one of them being slow is no
        // reason to skip the stack the session actually used.
        let late = self.drain_late_clients();
        let deadline = tokio::time::Instant::now() + budget;
        for client in late {
            report.absorb(client.close_with_timeout(budget).await);
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        report.absorb(self.delayed.close_with_timeout(left).await);
        report
    }

    /// Clients delivered by a load that finished after the shutdown started.
    /// Nobody is reading the channel any more, so they would otherwise be
    /// parked there with their servers still running.
    fn drain_late_clients(&mut self) -> Vec<Arc<dyn McpClient>> {
        let mut clients = Vec::new();
        while let Ok(event) = self.load_rx.try_recv() {
            if let TuiMcpLoadEvent::Ready(build) = event {
                if let Some(client) = build.client {
                    clients.push(client);
                }
            }
        }
        clients
    }

    /// A ready stack whose one transport is the caller's, for a test that
    /// wants to watch the close happen.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn with_probe_client(client: Arc<dyn McpClient>) -> Self {
        let (_tx, load_rx) = tokio::sync::mpsc::unbounded_channel();
        let delayed = DelayedTuiMcpClient::new();
        delayed.set_delegate(client);
        let client: Arc<dyn McpClient> = Arc::new(delayed.clone());
        Self {
            delayed,
            client,
            load_status: TuiMcpLoadStatus::NotConfigured,
            load_rx,
            request: McpLoadRequest::for_test(),
        }
    }

    /// A stack whose loader is the test's channel, so a test can deliver
    /// whatever load outcome it wants to observe.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn with_loader(
        load_rx: UnboundedReceiver<TuiMcpLoadEvent>,
        load_status: TuiMcpLoadStatus,
    ) -> Self {
        let delayed = DelayedTuiMcpClient::new();
        let client: Arc<dyn McpClient> = Arc::new(delayed.clone());
        Self {
            delayed,
            client,
            load_status,
            load_rx,
            request: McpLoadRequest::for_test(),
        }
    }
}
