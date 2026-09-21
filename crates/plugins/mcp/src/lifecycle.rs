//! Revocable MCP client handles. Unload invalidates calls and cached listings
//! synchronously; transport shutdown can then run without holding a registry lock.

use async_trait::async_trait;
use rebon_tool::{
    McpClient, McpClientError, McpShutdownReport, McpToolCall, McpToolDefinition, McpToolResult,
};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

pub(super) struct ManagedClient {
    inner: Mutex<Option<Arc<dyn McpClient>>>,
    revoked: Notify,
    // A merged session/global view must not fall through to the host when its
    // session generation is replaced, even if the caller kept the old view.
    parent: Option<Arc<ManagedClient>>,
}

impl ManagedClient {
    pub(super) fn new(inner: Arc<dyn McpClient>) -> Arc<Self> {
        Self::with_parent(inner, None)
    }

    pub(super) fn with_parent(
        inner: Arc<dyn McpClient>,
        parent: Option<Arc<ManagedClient>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Some(inner)),
            revoked: Notify::new(),
            parent,
        })
    }

    fn client(&self) -> Result<Arc<dyn McpClient>, McpClientError> {
        if let Some(parent) = &self.parent {
            parent.client()?;
        }
        self.inner
            .lock()
            .expect("MCP client handle poisoned")
            .clone()
            .ok_or_else(|| {
                McpClientError::Transport("[STALE_PROVIDER] MCP runtime is closed".into())
            })
    }

    pub(super) fn revoke(&self) -> Option<Arc<dyn McpClient>> {
        let client = self
            .inner
            .lock()
            .expect("MCP client handle poisoned")
            .take();
        self.revoked.notify_waiters();
        client
    }

    async fn live<T>(
        &self,
        work: impl std::future::Future<Output = T>,
    ) -> Result<T, McpClientError> {
        // Register before testing the handle so unload cannot be lost between
        // checking liveness and entering select.
        let revoked = self.revoked.notified();
        tokio::pin!(revoked);
        revoked.as_mut().enable();
        let parent_revoked = self.parent.as_ref().map(|parent| parent.revoked.notified());
        tokio::pin!(parent_revoked);
        if let Some(notified) = parent_revoked.as_mut().as_pin_mut() {
            notified.enable();
        }
        self.client()?;
        tokio::select! {
            biased;
            _ = &mut revoked => Err(McpClientError::Transport("[STALE_PROVIDER] MCP runtime is closed".into())),
            _ = async {
                match parent_revoked.as_mut().as_pin_mut() {
                    Some(notified) => notified.await,
                    None => std::future::pending::<()>().await,
                }
            } => Err(McpClientError::Transport("[STALE_PROVIDER] MCP session was replaced".into())),
            result = work => Ok(result),
        }
    }
}

#[async_trait]
impl McpClient for ManagedClient {
    async fn call_tool(&self, call: McpToolCall) -> Result<McpToolResult, McpClientError> {
        let client = self.client()?;
        self.live(client.call_tool(call)).await?
    }
    async fn list_tools(&self, server: &str) -> Option<Vec<String>> {
        let client = self.client().ok()?;
        self.live(client.list_tools(server)).await.ok().flatten()
    }
    async fn list_tool_definitions(&self, server: &str) -> Option<Vec<McpToolDefinition>> {
        let client = self.client().ok()?;
        self.live(client.list_tool_definitions(server))
            .await
            .ok()
            .flatten()
    }
    fn cached_tool_definitions(&self, server: &str) -> Option<Vec<McpToolDefinition>> {
        self.client().ok()?.cached_tool_definitions(server)
    }
    fn server_names(&self) -> Vec<String> {
        self.client()
            .map(|client| client.server_names())
            .unwrap_or_default()
    }
    fn drain_channel_notifications(&self) -> Vec<String> {
        self.client()
            .map(|client| client.drain_channel_notifications())
            .unwrap_or_default()
    }
    async fn shutdown_transport(&self) {
        if let Some(client) = self.revoke() {
            client.shutdown_transport().await;
        }
    }

    async fn close_with_timeout(&self, budget: std::time::Duration) -> McpShutdownReport {
        match self.revoke() {
            Some(client) => client.close_with_timeout(budget).await,
            // Already revoked: there is no transport left to close, and
            // nothing outstanding to report.
            None => McpShutdownReport::default(),
        }
    }
    async fn disconnect_server(&self, name: &str) -> bool {
        let Ok(client) = self.client() else {
            return false;
        };
        self.live(client.disconnect_server(name))
            .await
            .unwrap_or(false)
    }
    async fn reconnect_server(&self, name: &str) -> Result<bool, McpClientError> {
        let client = self.client()?;
        self.live(client.reconnect_server(name)).await?
    }
}

/// Tasks remain owned even if an async shutdown waiter is cancelled. A sync
/// disposer only revokes/schedules; the host must call the async join boundary.
#[derive(Default)]
pub(super) struct ShutdownTasks {
    tasks: Mutex<Vec<futures_util::future::Shared<futures_util::future::BoxFuture<'static, ()>>>>,
}

impl ShutdownTasks {
    pub(super) fn spawn(&self, work: impl std::future::Future<Output = ()> + Send + 'static) {
        use futures_util::FutureExt;
        let work = work.boxed();
        let completion = match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                let task = runtime.spawn(work);
                async move {
                    task.await
                        .expect("MCP lifecycle task failed before shutdown completed")
                }
                .boxed()
            }
            // Disposal may run on a sync host thread. Keep the future, not a
            // detached thread; the host's async shutdown will drive it.
            Err(_) => work,
        }
        .shared();
        self.tasks
            .lock()
            .expect("MCP shutdown tasks poisoned")
            .push(completion);
    }

    pub(super) async fn join(&self) {
        loop {
            let tasks = self
                .tasks
                .lock()
                .expect("MCP shutdown tasks poisoned")
                .clone();
            if tasks.is_empty() {
                return;
            }
            futures_util::future::join_all(tasks).await;
            self.tasks
                .lock()
                .expect("MCP shutdown tasks poisoned")
                .retain(|task| task.peek().is_none());
        }
    }
}

/// Keeps disposed plugin generations alive until the process host joins them.
/// Transport state still has exactly one owner: each SessionRuntime.
#[derive(Default)]
pub(super) struct RuntimeOwner {
    runtimes: Mutex<(bool, Vec<Arc<crate::session::SessionRuntime>>)>,
}

impl RuntimeOwner {
    pub(super) fn register(&self, runtime: Arc<crate::session::SessionRuntime>) {
        let mut state = self.runtimes.lock().expect("MCP runtime owners poisoned");
        if state.0 {
            runtime.unload();
        }
        state.1.push(runtime);
    }

    pub(super) async fn shutdown(&self) {
        loop {
            let runtimes = {
                let mut state = self.runtimes.lock().expect("MCP runtime owners poisoned");
                state.0 = true;
                state.1.clone()
            };
            if runtimes.is_empty() {
                return;
            }
            // Revoke all generations before waiting for any slow transport.
            for runtime in &runtimes {
                runtime.unload();
            }
            for runtime in &runtimes {
                runtime.shutdown().await;
            }
            self.runtimes
                .lock()
                .expect("MCP runtime owners poisoned")
                .1
                .retain(|runtime| !runtimes.iter().any(|joined| Arc::ptr_eq(runtime, joined)));
        }
    }
}

pub(super) fn process_owner() -> &'static RuntimeOwner {
    static OWNER: std::sync::OnceLock<RuntimeOwner> = std::sync::OnceLock::new();
    OWNER.get_or_init(RuntimeOwner::default)
}
