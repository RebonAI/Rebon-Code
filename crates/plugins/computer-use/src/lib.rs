//! `computer-use`: the feature plugin that owns desktop control end to end.
//!
//! Three things live here and nowhere else: the `ComputerUse` tool
//! ([`computer_use`]), the native desktop runtime it drives ([`runtime`]), and
//! the cell that says whether the active provider may be handed the local
//! desktop at all.
//!
//! The runtime is hosted, not linked into a session: `rebon computer-use
//! serve` and the desktop app run it and publish an endpoint, and the tool is
//! one of its clients. Both hosts are terminals and depend on this crate
//! directly, which is the allowed direction; nothing under the engine reaches
//! back the other way.
//!
//! # Where the capability comes from
//!
//! `ComputerUse` is only offered to the one provider allowed to receive the
//! local desktop, and that judgment belongs to whoever resolved the provider.
//! The step that resolved the provider derives it and publishes it through
//! [`set_provider_capability`]; the tool reads the same cell at call time. A
//! plugin on the *process* tool seat has no session of its own to ask, which
//! is why writer and reader meet at a process-wide cell rather than at a
//! handle passed in at construction.
//!
//! Two gates therefore hold: the tool hides itself unless the capability is on
//! *and* the local Computer Use service is running, and every action but
//! `observe` asks the user first.
//!
//! Turning the plugin off takes `ComputerUse` off the seat outright, whatever
//! the provider says. A desktop session already locked to a window belongs to
//! the service, not the tool, and keeps running.

use std::sync::Arc;

use rebon_core::tool_seat::{Priority, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};

pub mod computer_use;
// The native backends predate this crate and target APIs that Apple and
// Microsoft have since deprecated; the allow travelled with the code rather
// than being widened to the tool beside it.
#[allow(deprecated, unexpected_cfgs)]
pub mod runtime;

pub use computer_use::{set_provider_capability, ComputerUseTool, COMPUTER_USE_TOOL_NAME};
// The runtime's whole surface, flat at the crate root: the two hosts that
// serve it (`rebon computer-use serve`, the desktop app) and the tool beside
// it all name one path, and the wire types stay the contract those three
// share.
pub use runtime::*;

/// The one lock every test in this crate that mutates process-global state
/// takes: the endpoint environment variables, the config home behind the
/// endpoint record, and the provider-capability cell.
///
/// It is `rebon-tool`'s, not a local mutex, because the tool tests already
/// joined that discipline and a second mutex over the same variables
/// serialises nothing. Poison is recovered on purpose — the guards restore
/// what they changed on unwind, so a panicking test leaves the environment
/// sound and only its own assertion failed.
#[cfg(test)]
pub(crate) fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
    rebon_tool::env_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Stable id: the config key `plugins.computer-use.enabled`.
pub const PLUGIN_ID: &str = "computer-use";

const PROVIDER_ID: &str = "computer-use";

/// The one tool, wired to the same provider-capability cell the plugin uses.
///
/// Public so a test that needs the whole builtin catalogue on a bare
/// [`rebon_core::Engine`] can register it without standing up a kernel.
pub fn tools() -> Vec<Arc<dyn rebon_tool::Tool>> {
    let tool = ComputerUseTool::with_provider_capability(computer_use::provider_capability_cell());
    vec![Arc::new(tool) as Arc<dyn rebon_tool::Tool>]
}

pub struct ComputerUsePlugin;

impl Plugin for ComputerUsePlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).inject(&[TOOL_SEAT_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = ctx.require::<ToolSeatService>()?;
        seat.register_tools(ctx, PROVIDER_ID, Priority::Feature, tools())
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(ComputerUsePlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Desktop control (ComputerUse)",
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

    /// Stands in for the real `core-tools` seat plugin, which this crate does
    /// not depend on. All this plugin needs is the root seat.
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

    fn boot() -> (Arc<Kernel>, Arc<PluginRegistry>, Arc<ToolSeat>) {
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
        (kernel, registry, seat)
    }

    /// Without a provider that may receive the desktop, the tool is on the
    /// seat but hides itself, so nothing resolves it. This is the half of
    /// the contract that does not need a running service.
    #[test]
    fn a_provider_without_the_capability_cannot_resolve_the_tool() {
        let _guard = env_test_lock();
        set_provider_capability(false);
        let (_kernel, _registry, seat) = boot();
        assert!(seat
            .resolve(COMPUTER_USE_TOOL_NAME, None)
            .unwrap()
            .is_none());
    }

    /// The switch is the whole contract: with the capability on and the local
    /// service reachable, the model can resolve `ComputerUse`; turning the
    /// plugin off takes it away regardless, and turning it back on restores
    /// it. Unix-only because the fixture fakes the service with a socket.
    #[cfg(unix)]
    #[test]
    fn the_switch_takes_computer_use_off_the_seat_and_puts_it_back() {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::net::UnixListener;

        let _guard = env_test_lock();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("computer-use.sock");
        let active = dir.path().join("active");
        let _listener = UnixListener::bind(&socket).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&active)
            .unwrap();
        std::env::set_var("REBON_COMPUTER_USE_SOCKET", &socket);
        std::env::set_var("REBON_COMPUTER_USE_TOKEN", "token");
        std::env::set_var("REBON_COMPUTER_USE_ACTIVE_PATH", &active);
        set_provider_capability(true);

        let (_kernel, registry, seat) = boot();
        assert!(seat
            .resolve(COMPUTER_USE_TOOL_NAME, None)
            .unwrap()
            .is_some());

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("computer-use is a feature plugin");
        assert!(seat
            .resolve(COMPUTER_USE_TOOL_NAME, None)
            .unwrap()
            .is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(seat
            .resolve(COMPUTER_USE_TOOL_NAME, None)
            .unwrap()
            .is_some());

        set_provider_capability(false);
        std::env::remove_var("REBON_COMPUTER_USE_SOCKET");
        std::env::remove_var("REBON_COMPUTER_USE_TOKEN");
        std::env::remove_var("REBON_COMPUTER_USE_ACTIVE_PATH");
    }
}
