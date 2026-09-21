//! `web`: the feature plugin that puts `WebFetch` and `WebSearch` on the
//! process tool seat.
//!
//! Both tools and the local search engines they fall back to
//! ([`web_search_local`]) live here. The contract the rest of the tree
//! needs whether or not this plugin is loaded stays in `rebon-tool`: the
//! two canonical names and `WebSearchDelegate`, which the engine builds
//! for the Codex OAuth route.
//!
//! Turning the plugin off takes both tools off the seat. A provider's own
//! server-side web search is unaffected — that is the model's, not ours.

use std::sync::Arc;

use rebon_core::tool_seat::{Priority, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};

pub mod web_fetch;
pub mod web_search;
pub mod web_search_local;

pub use rebon_tool::web::{WebSearchDelegate, WEB_FETCH_TOOL_NAME, WEB_SEARCH_TOOL_NAME};
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;

/// Stable id: the config key `plugins.web.enabled`.
pub const PLUGIN_ID: &str = "web";

const PROVIDER_ID: &str = "web";

/// The two tools, in registration order.
///
/// Public so a test that needs the whole builtin catalogue on a bare
/// [`rebon_core::Engine`] can register them without standing up a kernel.
pub fn tools() -> Vec<Arc<dyn rebon_tool::Tool>> {
    vec![
        Arc::new(WebFetchTool) as Arc<dyn rebon_tool::Tool>,
        Arc::new(WebSearchTool),
    ]
}

pub struct WebPlugin;

impl Plugin for WebPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).inject(&[TOOL_SEAT_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = ctx.require::<ToolSeatService>()?;
        seat.register_tools(ctx, PROVIDER_ID, Priority::Feature, tools())
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(WebPlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Web access (WebFetch, WebSearch)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_core::tool_seat::ToolSeat;
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};
    use rebon_tool::{Tool, ToolResolver};

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

    #[test]
    fn the_switch_takes_the_web_tools_off_the_seat_and_puts_them_back() {
        let (kernel, registry) = boot();
        let seat = seat(&kernel);
        for name in [WEB_FETCH_TOOL_NAME, WEB_SEARCH_TOOL_NAME] {
            assert!(
                seat.resolve(name, None).unwrap().is_some(),
                "{name} resolves while web is loaded"
            );
        }

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("web is a feature plugin");
        assert!(seat.resolve(WEB_SEARCH_TOOL_NAME, None).unwrap().is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(seat.resolve(WEB_SEARCH_TOOL_NAME, None).unwrap().is_some());
    }

    /// `rebon-tool` pins the kinded builtins it owns against the shared
    /// facts table; `WebFetch` and `WebSearch` live here, so this crate
    /// pins them. Without it, a name or kind could drift on one side
    /// unnoticed.
    #[test]
    fn the_web_tools_match_the_shared_facts_table() {
        for tool in [
            Arc::new(WebFetchTool) as Arc<dyn Tool>,
            Arc::new(WebSearchTool),
        ] {
            let name = tool.id().as_str().to_string();
            let shared = rebon_tools_core::BUILTIN_TOOL_FACTS
                .iter()
                .find(|entry| entry.name == name)
                .unwrap_or_else(|| panic!("{name} is missing from the shared facts table"));
            assert_eq!(tool.aliases(), shared.aliases, "{name}");
            assert_eq!(tool.kind(), shared.kind, "{name}");
            assert_eq!(tool.file_target_field(), shared.file_target_field, "{name}");
        }
    }
}
