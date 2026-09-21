//! `structured-output`: the feature plugin that puts `StructuredOutput` on
//! the process tool seat.
//!
//! The tool implementation and its tests live here. The shared
//! `StructuredOutputChannel` and schema validator remain in `rebon-tool`:
//! `ToolContext` and the coordinator need them even with this plugin off.
//!
//! Turning the plugin off takes the tool off the seat. A workflow agent that
//! was given a schema then has no way to return one, and the coordinator
//! reports the missing structured output as it already does for a worker
//! that never called the tool.

use std::sync::Arc;

use rebon_core::tool_seat::{Priority, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};
mod structured_output;
pub use structured_output::{StructuredOutputTool, STRUCTURED_OUTPUT_TOOL_NAME};

/// Stable id: the config key `plugins.structured-output.enabled`.
pub const PLUGIN_ID: &str = "structured-output";

const PROVIDER_ID: &str = "structured-output";

/// The one tool.
pub fn tools() -> Vec<Arc<dyn rebon_tool::Tool>> {
    vec![Arc::new(StructuredOutputTool) as Arc<dyn rebon_tool::Tool>]
}

pub struct StructuredOutputPlugin;

impl Plugin for StructuredOutputPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).inject(&[TOOL_SEAT_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = ctx.require::<ToolSeatService>()?;
        seat.register_tools(ctx, PROVIDER_ID, Priority::Feature, tools())
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(StructuredOutputPlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Schema-checked agent results (StructuredOutput)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_core::tool_seat::ToolSeat;
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};
    use rebon_tool::ToolResolver;

    /// Stands in for `core-tools`, which cannot
    /// be depended on from here. All this plugin needs is the root seat.
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

    #[test]
    fn missing_seat_is_an_error() {
        assert!(StructuredOutputPlugin
            .apply(&Kernel::new().context())
            .is_err());
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
        assert!(seat
            .resolve(STRUCTURED_OUTPUT_TOOL_NAME, None)
            .unwrap()
            .is_none());
        registry.set_enabled(PLUGIN_ID, true).unwrap();
        let first = seat
            .resolve(STRUCTURED_OUTPUT_TOOL_NAME, None)
            .unwrap()
            .unwrap();
        let report = registry.reconcile(&DesiredSet::new());
        assert!(report.failed.is_empty());
        assert!(report.loaded.is_empty() && report.unloaded.is_empty());
        let again = seat
            .resolve(STRUCTURED_OUTPUT_TOOL_NAME, None)
            .unwrap()
            .unwrap();
        assert_eq!(first.id(), again.id());
        assert_eq!(first.input_schema(), again.input_schema());
        assert!(kernel.unload(PLUGIN_ID));
        assert!(seat
            .resolve(STRUCTURED_OUTPUT_TOOL_NAME, None)
            .unwrap()
            .is_none());
        assert!(!kernel.unload(PLUGIN_ID));
    }

    #[test]
    fn the_switch_takes_structured_output_off_the_seat_and_puts_it_back() {
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
        assert!(seat
            .resolve(STRUCTURED_OUTPUT_TOOL_NAME, None)
            .unwrap()
            .is_some());

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("structured-output is a feature plugin");
        assert!(seat
            .resolve(STRUCTURED_OUTPUT_TOOL_NAME, None)
            .unwrap()
            .is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(seat
            .resolve(STRUCTURED_OUTPUT_TOOL_NAME, None)
            .unwrap()
            .is_some());
    }
}
