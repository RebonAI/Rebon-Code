//! `notebook`: the feature plugin that puts `NotebookEdit` on the process
//! tool seat.
//!
//! The implementation and its tests live here. The read-before-edit check,
//! path-scope gate, mutation ledger and history tracker remain shared with
//! the core file tools in `rebon-tool`.
//!
//! `NotebookEdit` stays in `rebon_tools_core::BUILTIN_TOOL_FACTS` with kind
//! `FileEdit` whether or not this plugin is loaded: the table records what a
//! builtin tool is, not who registers it. That is what keeps an `Edit(...)`
//! permission rule and `acceptEdits` covering notebooks.

use std::sync::Arc;

use rebon_core::tool_seat::{Priority, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};
mod notebook_edit;
pub use notebook_edit::{NotebookEditTool, NOTEBOOK_EDIT_TOOL_NAME};

/// Stable id: the config key `plugins.notebook.enabled`.
pub const PLUGIN_ID: &str = "notebook";

const PROVIDER_ID: &str = "notebook";

/// The one tool.
///
/// Public so a test that needs the whole builtin catalogue on a bare
/// [`rebon_core::Engine`] can register it without standing up a kernel.
pub fn tools() -> Vec<Arc<dyn rebon_tool::Tool>> {
    vec![Arc::new(NotebookEditTool) as Arc<dyn rebon_tool::Tool>]
}

pub struct NotebookPlugin;

impl Plugin for NotebookPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).inject(&[TOOL_SEAT_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = ctx.require::<ToolSeatService>()?;
        seat.register_tools(ctx, PROVIDER_ID, Priority::Feature, tools())
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(NotebookPlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Jupyter notebooks (NotebookEdit)",
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

    /// Stands in for `core-tools`, which cannot be depended on from here.
    /// All this plugin needs is the root seat.
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
        assert!(NotebookPlugin.apply(&Kernel::new().context()).is_err());
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
            .resolve(NOTEBOOK_EDIT_TOOL_NAME, None)
            .unwrap()
            .is_none());
        registry.set_enabled(PLUGIN_ID, true).unwrap();
        let first = seat
            .resolve(NOTEBOOK_EDIT_TOOL_NAME, None)
            .unwrap()
            .unwrap();
        let report = registry.reconcile(&DesiredSet::new());
        assert!(report.failed.is_empty());
        assert!(report.loaded.is_empty() && report.unloaded.is_empty());
        let again = seat
            .resolve(NOTEBOOK_EDIT_TOOL_NAME, None)
            .unwrap()
            .unwrap();
        assert_eq!(first.id(), again.id());
        assert_eq!(first.input_schema(), again.input_schema());
        assert!(kernel.unload(PLUGIN_ID));
        assert!(seat
            .resolve(NOTEBOOK_EDIT_TOOL_NAME, None)
            .unwrap()
            .is_none());
        assert!(!kernel.unload(PLUGIN_ID));
    }

    #[test]
    fn the_switch_takes_notebook_edit_off_the_seat_and_puts_it_back() {
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
            .resolve(NOTEBOOK_EDIT_TOOL_NAME, None)
            .unwrap()
            .is_some());

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("notebook is a feature plugin");
        assert!(seat
            .resolve(NOTEBOOK_EDIT_TOOL_NAME, None)
            .unwrap()
            .is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(seat
            .resolve(NOTEBOOK_EDIT_TOOL_NAME, None)
            .unwrap()
            .is_some());
    }

    /// The file-edit class must still cover notebooks with the tool behind a
    /// switch: `acceptEdits` and `Edit(...)` rules read the shared table.
    #[test]
    fn notebook_edit_is_still_a_file_edit_in_the_shared_table() {
        use rebon_tool::Tool;
        let tool = NotebookEditTool;
        assert_eq!(tool.aliases(), &["NotebookEditTool"]);
        assert_eq!(tool.kind(), rebon_tools_core::ToolKind::FileEdit);
        assert_eq!(tool.file_target_field(), Some("notebook_path"));
        assert!(tool.should_defer());
        for name in [NOTEBOOK_EDIT_TOOL_NAME, "NotebookEditTool"] {
            assert_eq!(
                rebon_tool::file_target_field_for_name(name),
                Some("notebook_path")
            );
        }
        assert_eq!(
            rebon_tools_core::tool_kind_for_name(NOTEBOOK_EDIT_TOOL_NAME),
            rebon_tools_core::ToolKind::FileEdit
        );
    }
}
