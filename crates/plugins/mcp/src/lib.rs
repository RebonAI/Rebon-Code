//! MCP tools, transports, server management and session runtime.
//! Disabling this plugin removes both the generic tool and turn proxies,
//! revokes previously issued clients, clears caches and closes transports.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use rebon_core::mcp_runtime::{
    McpRuntimeService, McpTurnToolsService, MCP_RUNTIME_SERVICE, MCP_TURN_TOOLS_SERVICE,
};
use rebon_core::tool_seat::{Priority, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_kernel::{
    Context, Disposer, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta,
};

mod lifecycle;
pub mod mcp;
pub mod mcp_http;
pub mod mcp_sse;
pub mod mcp_stdio;
pub mod runtime;
mod session;
mod turn_tools;

pub use mcp::{AggregateMcpClient, InMemoryMcpClient, McpProxyTool, McpTool};
pub use mcp_http::{HttpMcpClient, HttpServerConfig};
pub use mcp_sse::{SseMcpClient, SseServerConfig};
pub use mcp_stdio::{StdioMcpClient, StdioServerConfig};

/// Stable id: the config key `plugins.mcp.enabled`.
pub const PLUGIN_ID: &str = "mcp";

const PROVIDER_ID: &str = "mcp-tool";

/// The one tool.
///
/// Public so a test that needs the whole builtin catalogue on a bare
/// [`rebon_core::Engine`] can register it without standing up a kernel.
pub fn tools() -> Vec<Arc<dyn rebon_tool::Tool>> {
    vec![Arc::new(McpTool) as Arc<dyn rebon_tool::Tool>]
}

pub struct McpPlugin;

impl Plugin for McpPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID)
            .inject(&[TOOL_SEAT_SERVICE])
            .provides(&[MCP_RUNTIME_SERVICE, MCP_TURN_TOOLS_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        self.apply_with_owner(ctx, lifecycle::process_owner())
    }
}

impl McpPlugin {
    fn apply_with_owner(
        &self,
        ctx: &Context,
        owner: &lifecycle::RuntimeOwner,
    ) -> Result<(), KernelError> {
        let seat = ctx.require::<ToolSeatService>()?;
        let runtime = session::SessionRuntime::new();
        owner.register(runtime.clone());
        let owned = runtime.clone();
        ctx.effect_labeled("MCP runtime ownership", move || {
            Disposer::new(move || owned.unload())
        });
        let active = runtime.active.clone();
        ctx.provide::<McpRuntimeService>(Arc::new(move |request| {
            let runtime = runtime.clone();
            Box::pin(async move { runtime.client_for_session(request).await })
        }))?;
        ctx.provide::<McpTurnToolsService>(Arc::new(move |definitions| {
            active.load(Ordering::Acquire).then(|| {
                Arc::new(turn_tools::McpToolProvider {
                    definitions,
                    active: active.clone(),
                }) as Arc<dyn rebon_core::tool_seat::SeatToolProvider>
            })
        }))?;
        seat.register_tools(ctx, PROVIDER_ID, Priority::Feature, tools())
    }
}

/// Terminal process-host boundary. Revokes live generations and joins all MCP
/// build/close tasks, including generations already removed by sync disposal.
/// Call before dropping the Tokio runtime; disposal alone is not completion.
pub async fn shutdown_process_runtimes() {
    lifecycle::process_owner().shutdown().await;
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(McpPlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "MCP servers (Mcp)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod execution_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_core::tool_seat::ToolSeat;
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};
    use rebon_tool::{mcp::MCP_TOOL_NAME, ToolResolver};

    /// Stands in for the `core-tools` plugin, which cannot be depended on
    /// from here. All this plugin needs is the root seat.
    struct SeatPlugin;

    impl Plugin for SeatPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("test-seat").provides(&[TOOL_SEAT_SERVICE])
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.provide::<ToolSeatService>(ToolSeat::new())
        }
    }

    fn make_seat(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
        Ok(Box::new(SeatPlugin))
    }

    static DEFS: &[PluginDef] = &[
        PluginDef {
            id: "test-seat",
            title: "Test seat",
            kind: PluginKind::Core,
            default_enabled: true,
            factory: make_seat,
        },
        PLUGIN,
    ];

    /// The switch is the whole contract: enabled means the model can resolve
    /// `Mcp`, disabled means it cannot, and flipping back restores it.
    #[test]
    fn the_switch_takes_mcp_off_the_seat_and_puts_it_back() {
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
        let report = registry.reconcile(&DesiredSet::new());
        assert!(report.failed.is_empty(), "{:?}", report.failed);

        let seat: Arc<ToolSeat> = kernel
            .context()
            .get::<ToolSeatService>()
            .expect("the seat is on the root");
        assert!(seat.resolve(MCP_TOOL_NAME, None).unwrap().is_some());

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("mcp is a feature plugin");
        assert!(seat.resolve(MCP_TOOL_NAME, None).unwrap().is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(seat.resolve(MCP_TOOL_NAME, None).unwrap().is_some());
    }

    struct ProbeClient {
        inner: InMemoryMcpClient,
        pending: std::sync::atomic::AtomicBool,
        started: tokio::sync::Notify,
        closed: tokio::sync::Notify,
        shutdowns: std::sync::atomic::AtomicUsize,
    }
    impl ProbeClient {
        fn new() -> Arc<Self> {
            let inner = InMemoryMcpClient::new();
            inner.register_tool("server", "ping", serde_json::json!({"ok":true}));
            Arc::new(Self {
                inner,
                pending: false.into(),
                started: Default::default(),
                closed: Default::default(),
                shutdowns: 0.into(),
            })
        }
    }
    #[async_trait::async_trait]
    impl rebon_tool::McpClient for ProbeClient {
        async fn call_tool(
            &self,
            call: rebon_tool::McpToolCall,
        ) -> Result<rebon_tool::McpToolResult, rebon_tool::McpClientError> {
            self.started.notify_one();
            if self.pending.load(Ordering::Acquire) {
                std::future::pending::<()>().await;
            }
            self.inner.call_tool(call).await
        }
        fn server_names(&self) -> Vec<String> {
            self.inner.server_names()
        }
        fn cached_tool_definitions(
            &self,
            server: &str,
        ) -> Option<Vec<rebon_tool::McpToolDefinition>> {
            self.inner.cached_tool_definitions(server)
        }
        async fn list_tool_definitions(
            &self,
            server: &str,
        ) -> Option<Vec<rebon_tool::McpToolDefinition>> {
            self.inner.list_tool_definitions(server).await
        }
        async fn shutdown_transport(&self) {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            self.closed.notify_one();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disable_revokes_retained_runtime_proxy_and_generic_handles_and_closes_once() {
        use rebon_core::mcp_runtime::McpSessionRequest;
        use rebon_tool::{McpToolCall, ToolContext};
        use serde_json::json;
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
        assert!(registry.reconcile(&DesiredSet::new()).failed.is_empty());
        let root = kernel.context();
        let seat = root.require::<ToolSeatService>().unwrap();
        let factory = root.require::<McpRuntimeService>().unwrap();
        let contribute = root.require::<McpTurnToolsService>().unwrap();
        let probe = ProbeClient::new();
        let managed = factory(McpSessionRequest {
            session_id: "test".into(),
            servers: vec![],
            global: Some(probe.clone()),
        })
        .await
        .unwrap()
        .unwrap();
        let definitions = managed.list_tool_definitions("server").await.unwrap();
        let provider = contribute(vec![("server".into(), definitions[0].clone())]).unwrap();
        let proxy = provider.resolve("mcp__server__ping").unwrap();
        let generic = seat.resolve(MCP_TOOL_NAME, None).unwrap().unwrap();
        let context = ToolContext::new().with_mcp_client(managed.clone());
        assert_eq!(
            proxy.call(json!({}), &context).await.unwrap()["content"]["ok"],
            true
        );
        probe.pending.store(true, Ordering::Release);
        // Consume the first successful call's notification before starting a pending call.
        probe.started.notified().await;
        let held = managed.clone();
        let pending = tokio::spawn(async move {
            held.call_tool(McpToolCall {
                server: "server".into(),
                name: "ping".into(),
                arguments: json!({}),
            })
            .await
        });
        probe.started.notified().await;
        registry.set_enabled(PLUGIN_ID, false).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), pending)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(root.get::<McpRuntimeService>().is_none());
        assert!(root.get::<McpTurnToolsService>().is_none());
        assert!(managed.server_names().is_empty());
        assert!(managed.cached_tool_definitions("server").is_none());
        assert!(managed.list_tool_definitions("server").await.is_none());
        assert!(!managed.disconnect_server("server").await);
        assert!(managed.reconnect_server("server").await.is_err());
        assert!(provider.tools().is_empty());
        assert!(provider.resolve("mcp__server__ping").is_none());
        assert!(contribute(vec![]).is_none());
        assert!(factory(McpSessionRequest {
            session_id: "test".into(),
            servers: vec![],
            global: None
        })
        .await
        .is_err());
        for tool in [&generic, &proxy] {
            assert!(!tool.is_enabled());
            assert!(tool.validate_input(&json!({}), &context).await.is_err());
            assert!(tool.check_permissions(&json!({}), &context).await.is_err());
            assert!(tool.call(json!({}), &context).await.is_err());
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), probe.closed.notified())
            .await
            .unwrap();
        managed.shutdown_transport().await;
        assert_eq!(probe.shutdowns.load(Ordering::SeqCst), 1);
        assert_eq!(probe.inner.calls().len(), 1);

        registry.set_enabled(PLUGIN_ID, true).unwrap();
        let next_factory = root.require::<McpRuntimeService>().unwrap();
        assert!(!Arc::ptr_eq(&factory, &next_factory));
        let next_probe = ProbeClient::new();
        let fresh = next_factory(McpSessionRequest {
            session_id: "test".into(),
            servers: vec![],
            global: Some(next_probe.clone()),
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(fresh.server_names(), vec!["server"]);
        assert!(!proxy.is_enabled());
        assert!(provider.tools().is_empty());
        assert!(seat.resolve(MCP_TOOL_NAME, None).unwrap().is_some());
        registry.set_enabled(PLUGIN_ID, false).unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            next_probe.closed.notified(),
        )
        .await
        .unwrap();
        assert_eq!(next_probe.shutdowns.load(Ordering::SeqCst), 1);
    }
}
