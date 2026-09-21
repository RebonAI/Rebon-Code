//! `sandbox`: confining a shell command with the operating system.
//!
//! Several crates' worth of code, split where the reasons to change differ:
//!
//! * **[`view`]** — the pure decision helpers `/sandbox` and `/doctor`
//!   render: the platform enum, dependency classification, configuration
//!   summaries, override effects, violation formatting. No IO, so the
//!   security-sensitive decision tree stays auditable byte for byte.
//! * **[`runtime`]** — turning a command into one the OS will refuse to let
//!   out of its box: `bwrap` on Linux, `sandbox-exec` on macOS, the
//!   `sandbox-win.exe` helper on Windows, behind one wrap seam.
//! * **[`proxy`]** — the loopback HTTP and SOCKS listener that decides a
//!   confined command's outbound connections by hostname. Kept apart from
//!   the runtime because that half has no async runtime and stays that way:
//!   the runtime describes *where* the proxy listens, this is the listener.
//! * **[`exec`]** — whether a given command is wrapped at all, and the
//!   [`rebon_tool::CommandSandbox`] the tool layer holds.
//! * **[`session`]** — settings on disk to a compiled session sandbox.
//! * **[`doctor`]** — what `/doctor` shows about this machine.
//! * **[`panel`]** — the `/sandbox` panel: the three tabs the [`view`]
//!   models describe, filled in from the settings chain, the machine probe
//!   and [`violations`].
//! * **[`violations`]** — the denials this process has seen, so the panel
//!   has something to show.
//!
//! # The switch
//!
//! `plugins.sandbox.enabled = false` disposes this context, and with it the
//! `session-sandbox` seat, the `/sandbox` command and the panel behind it.
//! Nothing then answers when the harness asks for a session's sandbox, and
//! `Bash` and `PowerShell` spawn the argv they built — which is the same
//! thing they do on a machine that never configured one.
//!
//! With one exception, and it is the whole reason the seat is resolved rather
//! than the plugin called: a workspace whose `settings.json` says
//! `sandbox.enabled = true` while this plugin is off is asking for two
//! contradictory things. Running its commands unconfined is the one outcome
//! the setting exists to prevent, and it would be invisible — every command
//! would succeed. The harness answers that combination with a
//! [`rebon_tool::RefusingSandbox`] instead, and `/doctor` says why.

use std::path::Path;
use std::sync::Arc;

use rebon_command_seat::{
    CommandHandler, CommandKind, CommandSeatService, CommandSpec, Surfaces, COMMAND_SEAT_SERVICE,
};
use rebon_kernel::{
    Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta, Service,
};
use rebon_tool::command_sandbox::{
    CommandSandbox, SandboxDoctor, SessionSandboxService, SessionSandboxSource,
};
use rebon_ui_seat::{DialogDef, UiSeatService};

pub mod doctor;
pub mod exec;
pub mod panel;
pub mod proxy;
pub mod runtime;
pub mod session;
pub mod view;
pub mod violations;

pub use exec::{SandboxPolicy, STRICT_MODE_REFUSAL};

/// Stable id: the config key `plugins.sandbox.enabled` and the name in
/// `/kernel plugins`.
pub const PLUGIN_ID: &str = "sandbox";

/// The provider on the `session-sandbox` seat.
///
/// Stateless: a session's sandbox is compiled per `cwd` because the rules
/// come from that workspace's own settings files, and two sessions in two
/// projects must not share a compiled rule set — or, on Linux, a proxy.
struct SandboxSource;

impl SessionSandboxSource for SandboxSource {
    fn for_session(&self, cwd: &Path) -> Option<Arc<dyn CommandSandbox>> {
        session::resolve_sandbox_policy(cwd)
            .map(|policy| Arc::new(policy) as Arc<dyn CommandSandbox>)
    }

    fn doctor(&self, cwd: &Path, settings_overrides: &[String]) -> SandboxDoctor {
        doctor::doctor_report(cwd, settings_overrides)
    }
}

/// `/sandbox` — the panel over this plugin's own view models.
pub fn command_spec() -> CommandSpec {
    CommandSpec::new("sandbox", "Show the sandbox configuration and overrides")
        .zh_aliases(["沙箱"])
        .surfaces(Surfaces::TUI_ONLY)
        .kind(CommandKind::Panel)
}

/// The `/sandbox` panel, built for the working directory it is asked about.
/// One value: the cwd, because the settings chain is that workspace's.
fn dialog_def() -> DialogDef {
    DialogDef::new(panel::DIALOG_ID, |args| {
        let cwd = match args.value_at(0) {
            "" => std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            value => std::path::PathBuf::from(value),
        };
        Some(Box::new(panel::SandboxPanelState::open(cwd)))
    })
}

pub struct SandboxPlugin;

impl Plugin for SandboxPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID)
            .inject(&[COMMAND_SEAT_SERVICE])
            .optional_inject(&[UiSeatService::NAME])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        // The seat goes on this plugin's own context, so disabling the plugin
        // takes it out of the registry.
        ctx.provide::<SessionSandboxService>(Arc::new(SandboxSource))?;

        // `/sandbox` describes what this plugin confines, so it comes and
        // goes with the same switch: a session that turned the sandbox off is
        // not offered a panel about a sandbox nothing will build. The handler
        // is `Native` — opening a panel needs the dialog stack, which only a
        // front end holds.
        let commands = ctx.require::<CommandSeatService>()?;
        let spec = command_spec();
        let handler = CommandHandler::Native(spec.name.clone());
        commands.register(ctx, spec, handler)?;

        // The panel is optional: a kernel booted without the UI seat — a
        // headless harness, `rebon exec` — still confines its commands, it
        // just has no screen to describe them on.
        if let Ok(ui) = ctx.require::<UiSeatService>() {
            ui.register_dialog(ctx, dialog_def())?;
        }
        Ok(())
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(SandboxPlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "OS-level command sandbox (bubblewrap / seatbelt / sandbox-win, /sandbox)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_command_seat::{CommandSeat, Surface};
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};
    use rebon_ui_seat::UiSeat;

    /// Stands in for `core-commands` and `core-ui`, which live in
    /// `rebon-harness` — it depends on this crate, so it cannot be depended
    /// on from here. All this plugin needs of them is the two seats on the
    /// kernel root.
    struct SeatPlugin;

    impl Plugin for SeatPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("test-seat").provides(&[COMMAND_SEAT_SERVICE, UiSeatService::NAME])
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.provide::<CommandSeatService>(CommandSeat::new())?;
            ctx.provide::<UiSeatService>(UiSeat::new())
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

    fn booted() -> (Arc<Kernel>, Arc<PluginRegistry>) {
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

    /// A kernel without this plugin answers nothing, and that is not an
    /// error: it is the machine the sandbox was never configured on.
    #[test]
    fn a_kernel_without_the_plugin_offers_no_session_sandbox() {
        let kernel = Kernel::new();
        assert!(kernel.context().get::<SessionSandboxService>().is_none());
    }

    /// And a provider answers until its own scope goes away — which is
    /// exactly what `plugins.sandbox.enabled = false` does.
    #[test]
    fn the_seat_is_gone_once_the_plugin_scope_is_disposed() {
        let (kernel, registry) = booted();
        assert!(kernel.context().get::<SessionSandboxService>().is_some());

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("sandbox is a feature plugin");
        assert!(kernel.context().get::<SessionSandboxService>().is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(kernel.context().get::<SessionSandboxService>().is_some());
    }

    /// `/sandbox` and its panel are this plugin's, so the switch takes them
    /// with the confinement they describe — a panel that stayed while the
    /// seat went would describe a sandbox nothing was going to build.
    #[test]
    fn the_switch_takes_the_command_and_the_panel_with_the_seat() {
        let (kernel, registry) = booted();
        let commands: Arc<rebon_command_seat::CommandSeat> = kernel
            .context()
            .get::<CommandSeatService>()
            .expect("the command seat is on the root");
        let ui: Arc<UiSeat> = kernel
            .context()
            .get::<UiSeatService>()
            .expect("the ui seat is on the root");

        let registered = commands.find("sandbox").expect("registered while loaded");
        assert_eq!(registered.owner, PLUGIN_ID);
        assert_eq!(registered.handler.native_id(), Some("sandbox"));
        assert!(registered.spec.available_on(Surface::Tui));
        assert!(ui.has(panel::DIALOG_ID));

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("sandbox is a feature plugin");
        assert!(commands.find("sandbox").is_none());
        assert!(!ui.has(panel::DIALOG_ID));

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(commands.find("sandbox").is_some());
        assert!(ui.has(panel::DIALOG_ID));
    }

    /// The panel is optional. A headless kernel — `rebon exec`, a worker —
    /// has no UI seat and must still get the sandbox rather than fail to
    /// load the plugin over a screen it will never draw.
    #[test]
    fn the_plugin_loads_without_a_ui_seat() {
        let kernel = Kernel::new();
        let ctx = kernel.context().fork(PLUGIN_ID);
        ctx.provide::<CommandSeatService>(CommandSeat::new())
            .expect("the command seat is required");
        SandboxPlugin
            .apply(&ctx)
            .expect("no panel, still a sandbox");
        assert!(kernel.context().get::<SessionSandboxService>().is_some());
    }
}
