//! `cron`: the feature plugin that puts the three scheduled-prompt tools on
//! the process tool seat.
//!
//! `CronCreate` / `CronList` / `CronDelete` are the model's face on the
//! scheduler. The arithmetic and the on-disk task list they operate over
//! (`rebon_tool::cron`) live in `rebon-tool`, because `rebon-core`'s tick
//! loop and the desktop app read the same store. Only the tools are this
//! plugin's.
//!
//! Turning the plugin off (`plugins.cron.enabled = false`, or the legacy
//! `REBON_DISABLE_CRON`) disposes this context, which takes the three tools
//! off the seat: the model stops seeing them at the next turn. Nothing else
//! about the scheduler changes — an already-scheduled task still fires.

use std::sync::Arc;

use rebon_core::tool_seat::{Priority, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};

pub mod cron_create;
pub mod cron_delete;
pub mod cron_list;

pub use cron_create::{CronCreateTool, CRON_CREATE_TOOL_NAME};
pub use cron_delete::{CronDeleteTool, CRON_DELETE_TOOL_NAME};
pub use cron_list::{CronListTool, CRON_LIST_TOOL_NAME};

/// Stable id: the config key `plugins.cron.enabled` and the name in
/// `/kernel plugins`.
pub const PLUGIN_ID: &str = "cron";

/// The provider id the three tools sit under on the seat.
const PROVIDER_ID: &str = "cron";

/// The three tools, in registration order.
///
/// Public so a test that needs the whole builtin catalogue on a bare
/// [`rebon_core::Engine`] can register them without standing up a kernel.
pub fn tools() -> Vec<Arc<dyn rebon_tool::Tool>> {
    vec![
        Arc::new(CronCreateTool) as Arc<dyn rebon_tool::Tool>,
        Arc::new(CronListTool),
        Arc::new(CronDeleteTool),
    ]
}

pub struct CronPlugin;

impl Plugin for CronPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).inject(&[TOOL_SEAT_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = ctx.require::<ToolSeatService>()?;
        seat.register_tools(ctx, PROVIDER_ID, Priority::Feature, tools())
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(CronPlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Scheduled prompts (CronCreate, CronList, CronDelete)",
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

    /// Stands in for `core-tools`, which cannot be depended on from here
    /// (it depends on this crate). All the cron plugin needs from it is the
    /// seat on the kernel root.
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

    fn boot() -> (Arc<Kernel>, Arc<PluginRegistry>) {
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
        let report = registry.reconcile(&DesiredSet::new());
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        (kernel, registry)
    }

    fn seat(kernel: &Kernel) -> Arc<ToolSeat> {
        kernel
            .context()
            .get::<ToolSeatService>()
            .expect("the seat is on the root")
    }

    /// The switch is the whole contract: enabled means the model can resolve
    /// `CronCreate`, disabled means it cannot, and flipping back restores it.
    #[test]
    fn the_switch_takes_the_cron_tools_off_the_seat_and_puts_them_back() {
        let (kernel, registry) = boot();
        let seat = seat(&kernel);
        for name in [
            CRON_CREATE_TOOL_NAME,
            CRON_LIST_TOOL_NAME,
            CRON_DELETE_TOOL_NAME,
        ] {
            assert!(
                seat.resolve(name, None).unwrap().is_some(),
                "{name} resolves while cron is loaded"
            );
        }

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("cron is a feature plugin");
        assert!(
            seat.resolve(CRON_CREATE_TOOL_NAME, None).unwrap().is_none(),
            "disabling the plugin takes its tools off the seat"
        );

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(seat.resolve(CRON_CREATE_TOOL_NAME, None).unwrap().is_some());
    }
}
