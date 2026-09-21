//! `monitor`: the feature plugin that puts the `Monitor` tool on the process
//! tool seat.
//!
//! The implementation and its input/network-policy tests live in this plugin.
//! `rebon-tool` retains the shared registry, ownership, cancellation and
//! WebSocket transport runtime held by `ToolContext` and `Engine`.
//!
//! Turning the plugin off takes `Monitor` off the seat. Monitors already
//! running belong to the registry and keep streaming until they finish or
//! `TaskStop` ends them.

use std::sync::Arc;

use rebon_core::tool_seat::{Priority, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};
mod monitor;
pub use monitor::{MonitorTool, MONITOR_TOOL_NAME};

/// Stable id: the config key `plugins.monitor.enabled`.
pub const PLUGIN_ID: &str = "monitor";

const PROVIDER_ID: &str = "monitor";

/// The one tool.
///
/// Public so a test that needs the whole builtin catalogue on a bare
/// [`rebon_core::Engine`] can register it without standing up a kernel.
pub fn tools() -> Vec<Arc<dyn rebon_tool::Tool>> {
    vec![Arc::new(MonitorTool) as Arc<dyn rebon_tool::Tool>]
}

pub struct MonitorPlugin;

impl Plugin for MonitorPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).inject(&[TOOL_SEAT_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = ctx.require::<ToolSeatService>()?;
        seat.register_tools(ctx, PROVIDER_ID, Priority::Feature, tools())
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(MonitorPlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Event monitors (Monitor)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_core::tool_seat::ToolSeat;
    use rebon_core::Engine as RebonEngine;
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};
    use rebon_tool::{PermissionBroker, Tool, ToolContext, ToolResolver};
    use rebon_tools_core::{PermissionBehavior, PermissionDecision, ToolError, ToolResult};
    use serde_json::{json, Value};

    /// Stands in for `core-tools`, which cannot be depended on from here.
    /// Register its real primitives so discovery and deferred invocation use
    /// the same seat as the monitor plugin.
    struct SeatPlugin;

    impl Plugin for SeatPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("test-seat").provides(&[TOOL_SEAT_SERVICE])
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            let seat = ToolSeat::new();
            ctx.provide::<ToolSeatService>(seat.clone())?;
            seat.register_tools(
                ctx,
                "test-core-tools",
                Priority::Core,
                rebon_tool::core_tool_set(rebon_tool::BashTool::new()),
            )
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

    fn plugin_engine() -> (Arc<Kernel>, Arc<PluginRegistry>, RebonEngine) {
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
        let report = registry.reconcile(&DesiredSet::new());
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        let engine = RebonEngine::new().with_permission_broker(Arc::new(ApproveAskBroker));
        assert!(engine.attach_upstream_tool_context(kernel.context().clone()));
        (kernel, registry, engine)
    }

    // Approval is the only substitute: the engine still validates, resolves
    // and executes the real tool through its deferred gateway and broker.
    struct ApproveAskBroker;

    #[async_trait::async_trait]
    impl PermissionBroker for ApproveAskBroker {
        async fn resolve(
            &self,
            tool: &dyn Tool,
            input: Value,
            context: &ToolContext,
            decision: PermissionDecision,
        ) -> ToolResult<Value> {
            match decision.behavior {
                PermissionBehavior::Allow | PermissionBehavior::Ask => {
                    let effective_input = decision.updated_input.unwrap_or(input);
                    tool.call(effective_input, context).await
                }
                other => panic!("expected allow/ask decision, got {other:?}"),
            }
        }
    }

    async fn discover_monitor(engine: &RebonEngine) -> ToolContext {
        let index = Arc::new(engine.build_tool_search_index());
        assert!(index.contains_name("Monitor"));
        let context = ToolContext::new()
            .with_session_id("monitor-deferred-session")
            .with_tool_search_index(index);

        let found = engine
            .invoke_tool("ToolSearch", json!({ "query": "select:Monitor" }), &context)
            .await
            .unwrap();
        assert!(found["result"]
            .as_str()
            .unwrap()
            .contains("\"name\":\"Monitor\""));
        assert!(context.is_deferred_tool_discovered("Monitor"));
        context
    }

    // Migrated from rebon-core::tests: its full-catalogue projection
    // fixtures deliberately panic on call; execution belongs to this plugin.
    #[tokio::test(flavor = "multi_thread")]
    async fn monitor_is_discoverable_and_invokable_as_a_deferred_builtin() {
        let (kernel, _registry, engine) = plugin_engine();
        let context = discover_monitor(&engine).await;

        let error = engine
            .invoke_tool(
                "InvokeDeferredTool",
                json!({
                    "tool_name": "Monitor",
                    "arguments": {
                        "command": "echo ready",
                        "description": "readiness events"
                    }
                }),
                &context,
            )
            .await
            .unwrap_err();
        match error {
            ToolError::Execution { tool, source } => {
                assert_eq!(tool.as_str(), "Monitor");
                assert!(source
                    .to_string()
                    .contains("task runtime controller is unavailable"));
            }
            other => panic!("expected Monitor execution error, got {other:?}"),
        }
        assert!(kernel.unload(PLUGIN_ID));
        assert!(kernel.unload("test-seat"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn discovered_monitor_cannot_be_invoked_after_plugin_unload() {
        let (kernel, _registry, engine) = plugin_engine();
        let context = discover_monitor(&engine).await;
        assert!(kernel.unload(PLUGIN_ID));
        assert!(!engine.build_tool_search_index().contains_name("Monitor"));
        // Keep the old index and discovery state: a previously advertised
        // schema must not keep an unloaded tool executable.
        assert!(context
            .tool_search_index()
            .unwrap()
            .contains_name("Monitor"));
        let error = engine
            .invoke_tool(
                "InvokeDeferredTool",
                json!({
                    "tool_name": "Monitor",
                    "arguments": {
                        "command": "echo ready",
                        "description": "readiness events"
                    }
                }),
                &context,
            )
            .await
            .unwrap_err();
        match error {
            ToolError::UnknownTool { tool } => assert_eq!(tool.as_str(), "Monitor"),
            other => panic!("expected unknown Monitor after unload, got {other:?}"),
        }
        assert!(kernel.unload("test-seat"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deferred_monitor_preserves_schema_and_semantic_validation() {
        let (kernel, _registry, engine) = plugin_engine();
        let context = discover_monitor(&engine).await;
        // Parser cases are covered in monitor::tests; here pin the engine's
        // two validation stages and target error attribution across the bridge.
        for (arguments, expected_reason) in [
            (json!({"command": "echo ready"}), "description"),
            (
                json!({
                    "command": "echo ready",
                    "ws": "ws://localhost/events",
                    "description": "readiness events"
                }),
                "exactly one",
            ),
        ] {
            let error = engine
                .invoke_tool(
                    "InvokeDeferredTool",
                    json!({"tool_name": "Monitor", "arguments": arguments}),
                    &context,
                )
                .await
                .unwrap_err();
            match error {
                ToolError::InvalidInput {
                    tool,
                    reason,
                    error_code,
                } => {
                    assert_eq!(tool.as_str(), "Monitor");
                    assert!(reason.contains(expected_reason), "{reason}");
                    assert_eq!(error_code, Some(400));
                }
                other => panic!("expected invalid Monitor input, got {other:?}"),
            }
        }
        assert!(kernel.unload(PLUGIN_ID));
        assert!(kernel.unload("test-seat"));
    }

    #[test]
    fn missing_seat_is_an_error() {
        assert!(MonitorPlugin.apply(&Kernel::new().context()).is_err());
    }

    #[test]
    fn disabled_startup_reconcile_and_unload_are_clean() {
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
        assert!(registry
            .reconcile(&DesiredSet::new().with(PLUGIN_ID, false))
            .failed
            .is_empty());
        let seat = kernel.context().get::<ToolSeatService>().unwrap();
        assert!(seat.resolve(MONITOR_TOOL_NAME, None).unwrap().is_none());
        registry.set_enabled(PLUGIN_ID, true).unwrap();
        let first = seat.resolve(MONITOR_TOOL_NAME, None).unwrap().unwrap();
        let report = registry.reconcile(&DesiredSet::new());
        assert!(report.failed.is_empty());
        assert!(report.loaded.is_empty() && report.unloaded.is_empty());
        let again = seat.resolve(MONITOR_TOOL_NAME, None).unwrap().unwrap();
        assert_eq!(first.id(), again.id());
        assert_eq!(first.input_schema(), again.input_schema());
        assert!(kernel.unload(PLUGIN_ID));
        assert!(seat.resolve(MONITOR_TOOL_NAME, None).unwrap().is_none());
        assert!(!kernel.unload(PLUGIN_ID));
    }

    #[test]
    fn the_switch_takes_monitor_off_the_seat_and_puts_it_back() {
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
        assert!(seat.resolve(MONITOR_TOOL_NAME, None).unwrap().is_some());

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("monitor is a feature plugin");
        assert!(seat.resolve(MONITOR_TOOL_NAME, None).unwrap().is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(seat.resolve(MONITOR_TOOL_NAME, None).unwrap().is_some());
    }
}
