//! `updater`: everything Rebon knows about its own updates.
//!
//! The decision logic — channels, semver, the max-version cap, the
//! installation-source discriminant and the per-user scheduler specs — the npm
//! request and the installation-source detection for *this* process, the
//! `/update` command and the `rebon update …` subcommand bodies all change
//! together. None of it is terminal work, so it is one plugin, with the split
//! drawn where the reasons to change actually differ:
//!
//! * **[`updater`]** — pure decisions, no I/O; every module pins its
//!   behaviour with a table of cases.
//! * **[`check`]** — the npm registry request, and only that.
//! * **[`installation`]** — what this process's installation looks like, and
//!   the lines every surface prints about it.
//! * **[`command`]** — `/update`: parse, decide, write the preference,
//!   compose the sentence.
//! * **[`cli`]** — `rebon update …`, plus the supervisor scheduler
//!   registration that shares the same platform code.
//! * **[`seat`]** — who starts the startup check, and how a front end asks
//!   whether it has answered.
//!
//! # The switch
//!
//! `plugins.updater.enabled = false` disposes this context, and with it:
//! `/update` leaves the command seat, so the terminal's `/` picker no longer
//! offers it and the line goes to the model as text; `update-check` leaves the
//! service registry, so the terminal's drain has nothing to poll and nothing
//! starts a check; and `/status` prints no update section, because it asks the
//! same seat.
//!
//! What stays is `rebon update status` and `rebon update service …`. Those are
//! clap subcommands parsed before a kernel exists — see [`cli`] — and a plugin
//! switch cannot be consulted by a process that has not booted one.
//!
//! # What is deliberately not here
//!
//! Drawing. The terminal keeps the notice pinned above its prompt, the
//! transcript it prints feedback into, and the frame-by-frame drain of the
//! startup check. Those need an `AppState` and a ratatui frame; everything
//! this crate returns is a value.

use std::sync::Arc;

use rebon_command_seat::{
    CommandHandler, CommandSeatService, CommandSpec, Surfaces, COMMAND_SEAT_SERVICE,
};
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};

pub mod check;
pub mod cli;
pub mod command;
pub mod installation;
pub mod seat;
pub mod updater;

pub use check::{check_for_update, UpdateCheckResult};
pub use command::{
    parse_update_command, report_check_result, run_update_command,
    save_update_auto_install_setting, update_usage_text, NoticeChange, UpdateCommand,
    UpdateCommandOutcome,
};
pub use installation::{
    detect_current_installation, format_auto_install_status, format_headless_update_status,
    format_update_status, UpdateNoticeState, UpdateStatusInfo,
};
pub use seat::{UpdateCheckPoll, UpdateCheckSeat, UpdateCheckService, UPDATE_CHECK_SERVICE};
pub use updater::*;

/// Stable id: the config key `plugins.updater.enabled` and the name in
/// `/kernel plugins`.
pub const PLUGIN_ID: &str = "updater";

/// This plugin's update-check seat as `ctx` sees it, or `None` when
/// `plugins.updater.enabled` is off.
///
/// The one question a front end asks about updates that is not "what does
/// this say": absence is the answer, and every surface that would otherwise
/// print an update line reads it the same way.
pub fn update_check_seat(ctx: &Context) -> Option<Arc<UpdateCheckSeat>> {
    ctx.get::<UpdateCheckService>()
}

/// `/update` as the command seat sees it.
///
/// The hint is the grammar [`parse_update_command`] accepts, written next to
/// the parser rather than in a table someone editing the parser would never
/// open. [`Surfaces::LOCAL`] is what the built-in table declared: the desktop
/// app lists the command even though only the terminal runs it, and narrowing
/// that is a product decision, not this crate's.
pub fn command_spec() -> CommandSpec {
    CommandSpec::new("update", "Manage local Rebon update checks")
        .zh_aliases(["更新"])
        .hint("status|check|skip|channel <latest|stable>|auto <on|off|status>")
        .surfaces(Surfaces::LOCAL)
}

pub struct UpdaterPlugin;

impl Plugin for UpdaterPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).inject(&[COMMAND_SEAT_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        // The seat goes on this plugin's own context, so disabling the plugin
        // takes it out of the registry: a front end that looks it up gets
        // nothing, which is the whole of "no check, no notice".
        //
        // Nothing else to arm. The seat starts its own check on the first
        // poll, and nobody polls until a front end that can draw a notice
        // looks, so `rebon exec`, an ACP session and a background worker
        // reach the registry exactly as often as they show an update banner —
        // never.
        ctx.provide::<UpdateCheckService>(Arc::new(UpdateCheckSeat::new()))?;

        // `/update` is this plugin's command, so it comes and goes with the
        // same switch. The handler is `Native`: running it needs the notice
        // and the transcript, which only a front end holds.
        let commands = ctx.require::<CommandSeatService>()?;
        let spec = command_spec();
        let handler = CommandHandler::Native(spec.name.clone());
        commands.register(ctx, spec, handler)?;

        Ok(())
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(UpdaterPlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Update checks (/update, rebon update)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_command_seat::{CommandSeat, Surface};
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};

    /// Stands in for `core-commands`, which provides the command seat. It
    /// lives in `rebon-harness`, which depends on this crate and so cannot be
    /// depended on from here; all this plugin needs of it is the seat on the
    /// kernel root.
    struct SeatPlugin;

    impl Plugin for SeatPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("test-seat").provides(&[COMMAND_SEAT_SERVICE])
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.provide::<CommandSeatService>(CommandSeat::new())
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

    /// `/update` is this plugin's command: a session that turned update
    /// checks off is not offered a command whose every branch reaches a
    /// feature that is gone.
    #[test]
    fn the_switch_takes_the_command_off_the_seat_and_puts_it_back() {
        let (kernel, registry) = booted();
        let seat: Arc<CommandSeat> = kernel
            .context()
            .get::<CommandSeatService>()
            .expect("the seat is on the root");

        let registered = seat.find("update").expect("registered while loaded");
        assert_eq!(registered.owner, PLUGIN_ID);
        assert_eq!(registered.handler.native_id(), Some("update"));
        assert_eq!(registered.spec.hint, command_spec().hint);
        assert!(registered.spec.available_on(Surface::Tui));
        assert!(registered.spec.available_on(Surface::Desktop));

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("updater is a feature plugin");
        assert!(seat.find("update").is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(seat.find("update").is_some());
    }

    /// And so does the seat the startup check lands on. With the plugin off
    /// there is nothing to resolve, so a front end's drain finds nothing and
    /// `/status` has no update section to print.
    #[test]
    fn the_switch_takes_the_update_check_seat_out_of_the_registry_and_puts_it_back() {
        let (kernel, registry) = booted();
        assert!(kernel.context().get::<UpdateCheckService>().is_some());

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("updater is a feature plugin");
        assert!(kernel.context().get::<UpdateCheckService>().is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(kernel.context().get::<UpdateCheckService>().is_some());
    }

    /// Turning the plugin off is the whole chain: no seat to resolve, so the
    /// front end's drain has nothing to poll, so nothing decides to ask npm.
    /// Loading it merely offers a seat — the request waits for a reader.
    ///
    /// [`update_check_seat`] is the exact call the terminal's status surface and
    /// `/status`
    /// make, so this is the lookup the terminal really performs.
    #[test]
    fn a_disabled_plugin_leaves_nothing_to_poll_and_a_loaded_one_asks_nobody_yet() {
        let (kernel, registry) = booted();

        let seat = update_check_seat(kernel.context()).expect("loaded plugins offer the seat");
        assert!(
            !seat.decided(),
            "loading the plugin does not by itself reach the registry"
        );

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("updater is a feature plugin");
        assert!(
            update_check_seat(kernel.context()).is_none(),
            "with the plugin off there is no seat to poll, so no check can start"
        );

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        let reloaded = update_check_seat(kernel.context()).expect("re-enabling offers a seat");
        assert!(
            !reloaded.decided(),
            "a reload starts from a seat that has asked nobody"
        );
    }
}
