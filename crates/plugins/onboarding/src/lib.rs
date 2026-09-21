//! `onboarding`: the first thing a fresh install shows, and the login behind it.
//!
//! The decision rules — PKCE, the authorize URL, the pasted-code grammar, the
//! token response, the credential file, the error classes — have two consumers
//! only: the wizard and the OAuth driver. The three IO phases of the Codex
//! login (prepare, collect, exchange), and the wizard's own state machine plus
//! the blocking driver that walks it through those phases, are the same
//! material. None of it is terminal work, and all of it changes together.
//!
//! So it is one plugin, split where the reasons to change actually differ:
//!
//! * **[`onboarding`]** — pure decisions, no IO. Every module still pins its
//!   behaviour with a table of cases, and the security bar written at the top
//!   of that module still applies.
//! * **[`oauth`]** — the three IO phases, frontend-agnostic: open a browser at
//!   a PKCE-protected URL, catch the callback on the loopback port (or take a
//!   pasted code), exchange it, persist the credential.
//! * **[`dialog`]** — the wizard: which step the user is on, and what the host
//!   is being asked to do about it.
//! * **[`drive`]** — the blocking driver that turns one
//!   [`dialog::OnboardingDialogOutcome::StartOpenAIOAuth`] into the whole
//!   login, reporting each phase back into the wizard as it goes. It asks a
//!   host-supplied [`drive::OAuthHost`] what the user did, so the phase order
//!   is shared and the keystrokes stay with the front end.
//! * **[`apply`]** — turning one of those answers into a config write, and
//!   telling the wizard what the write did. One copy, so the two event loops
//!   that reach the wizard cannot drift apart on disk.
//! * **[`store`]** — the config reads the wizard opens with, in one place
//!   rather than at each of its six entry points.
//! * **[`migrate`]** — the import step's library: what is discoverable under
//!   `~/.claude` and `~/.codex`, and where each kind has to land for rebon's
//!   loaders to read it back. A plain recursive copy lands it somewhere they
//!   ignore, which is why the terminal and the desktop app share this one.
//!
//! # The switch
//!
//! `plugins.onboarding.enabled = false` disposes this context, and with it:
//! all four commands leave the command seat — `/onboarding`, `/migrate`,
//! `/login` and `/logout` — so the terminal's `/` picker no longer offers
//! them and the line goes to the model as text; and [`first_run_gate`]
//! answers `false`, so a config home that has never completed setup opens
//! straight into a session instead of the wizard.
//!
//! [`migrate`] as a *library* is the one part the switch does not reach. The
//! desktop app's import panel and the terminal wizard call it directly rather
//! than through a seat, so importing from those surfaces keeps working with
//! the plugin off; only the `/migrate` command goes.
//!
//! # What is deliberately not here
//!
//! Drawing, and the keystrokes that drive it. The terminal keeps the ratatui
//! frame, the pane layout, the text fields' cursors, the key handlers that
//! reach for modifier bits this side has no vocabulary for, and the blocking
//! startup event loop that owns the terminal before a session exists.
//! Everything this crate returns is a value.

use std::sync::Arc;

use rebon_command_seat::{CommandHandler, CommandSeatService, CommandSpec, COMMAND_SEAT_SERVICE};
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};

pub mod apply;
pub mod dialog;
pub mod drive;
pub mod migrate;
pub mod oauth;
pub mod onboarding;
pub mod store;

pub use apply::{
    apply_add_provider, apply_add_provider_model, apply_run_migration, apply_update_provider,
};
pub use store::{has_any_provider, load_provider_snapshot, onboarding_open_inputs};

pub use dialog::{
    ExistingSetupChoice, OAuthView, OnboardingDialogOutcome, OnboardingDialogState,
    OnboardingOpenInputs, OnboardingStepTransition, PanelStatus, ProviderFormState,
    ProviderPresetSelection, ProviderSnapshot, PROVIDER_FORMATS,
};

/// Stable id: the config key `plugins.onboarding.enabled` and the name in
/// `/kernel plugins`.
pub const PLUGIN_ID: &str = "onboarding";

/// Whether a front end should open the setup wizard for a config home that has
/// never finished it.
///
/// Two questions in one, because both answers have to agree: the plugin has to
/// be loaded (otherwise there is no wizard to open), and setup has to be
/// unfinished (otherwise there is nothing to ask). A front end that asked only
/// the second would open a wizard the user switched off; one that asked only
/// the first would re-run setup on every start.
///
/// `ctx` is the kernel context the front end already holds. `None` — no kernel
/// at all — is the same answer as a disabled plugin: nothing decided that the
/// wizard should run, so it does not.
pub fn first_run_gate(ctx: Option<&Context>) -> bool {
    wizard_available(ctx) && !rebon_config::has_completed_onboarding()
}

/// Whether there is a wizard to open at all.
///
/// The other half of [`first_run_gate`], asked on its own by the startup path
/// that reaches the wizard for a different reason: no model credentials at
/// all. That is not a first run — a user can delete a provider — but it is
/// still this plugin's surface, and with the plugin off it has to end in the
/// same guidance a user gets when they close the wizard without configuring
/// one.
pub fn wizard_available(ctx: Option<&Context>) -> bool {
    ctx.is_some_and(|ctx| ctx.get::<OnboardingService>().is_some())
}

/// The marker this plugin provides so [`first_run_gate`] can tell "loaded"
/// from "switched off". It carries nothing: presence is the whole message,
/// exactly as `/onboarding`'s presence on the command seat is.
pub struct OnboardingService;

impl rebon_kernel::Service for OnboardingService {
    type Interface = OnboardingService;
    const NAME: &'static str = "onboarding/loaded";
}

/// `/onboarding` as the command seat sees it.
///
/// The four fields are the ones the built-in table declared, carried over
/// unchanged: the same name, the same one-line description, the same two
/// Chinese aliases, and the same `Panel` kind, which is what tells a front end
/// this command opens a surface rather than sending text. The built-in row
/// never set `surfaces`, so this one does not either — both take the
/// `Surfaces::LOCAL` default.
pub fn command_spec() -> CommandSpec {
    CommandSpec::new("onboarding", "Re-run the setup wizard")
        .zh_aliases(["向导", "引导"])
        .kind(rebon_command_seat::CommandKind::Panel)
}

/// `/migrate` as the command seat sees it.
///
/// The import it runs is [`migrate`] — this crate's, and the wizard's own
/// step — so the command belongs to the same switch. The four fields are the
/// ones the built-in table declared, `surfaces` unset there and here, which
/// takes the `Surfaces::LOCAL` default: both local front ends, not over the
/// wire.
pub fn migrate_command_spec() -> CommandSpec {
    CommandSpec::new(
        "migrate",
        "Import Claude Code / Codex skills, agents, and commands",
    )
    .zh_aliases(["导入", "迁移"])
    .kind(rebon_command_seat::CommandKind::Panel)
}

/// Every command this plugin owns, in registration order.
pub fn command_specs() -> Vec<CommandSpec> {
    vec![command_spec(), migrate_command_spec()]
}

pub struct OnboardingPlugin;

impl Plugin for OnboardingPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).inject(&[COMMAND_SEAT_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        // The marker goes on this plugin's own context, so disabling the
        // plugin takes it out of the registry and `first_run_gate` stops
        // answering yes. Nothing else to arm: the wizard has no background
        // work of its own, it runs when a front end opens it.
        ctx.provide::<OnboardingService>(Arc::new(OnboardingService))?;

        // The four commands are this plugin's, so they come and go with the
        // same switch. Every handler is `Native`: opening the wizard, running
        // the import and driving a sign-in all need the dialog stack and a
        // terminal, which only a front end holds.
        let commands = ctx.require::<CommandSeatService>()?;
        for spec in command_specs() {
            let handler = CommandHandler::Native(spec.name.clone());
            commands.register(ctx, spec, handler)?;
        }

        Ok(())
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(OnboardingPlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Setup wizard and Codex login (/onboarding)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_command_seat::Surfaces;
    use rebon_command_seat::{CommandKind, CommandSeat, Surface};
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

    /// `/onboarding` is this plugin's command, and it carries the four fields
    /// the built-in table declared, so moving it off that table is not a
    /// change to what the `/` picker shows.
    #[test]
    fn the_switch_takes_the_command_off_the_seat_and_puts_it_back() {
        let (kernel, registry) = booted();
        let seat: Arc<CommandSeat> = kernel
            .context()
            .get::<CommandSeatService>()
            .expect("the seat is on the root");

        let registered = seat.find("onboarding").expect("registered while loaded");
        assert_eq!(registered.owner, PLUGIN_ID);
        assert_eq!(registered.handler.native_id(), Some("onboarding"));
        assert_eq!(registered.spec.description, "Re-run the setup wizard");
        assert_eq!(registered.spec.kind, CommandKind::Panel);
        assert!(registered.spec.available_on(Surface::Tui));

        // Both Chinese spellings the built-in row carried are still on the
        // spec. `find` matches `name` and `aliases` only, so this is what
        // carrying them over means today, and it is the same as before.
        assert_eq!(
            registered.spec.zh_aliases.as_ref(),
            ["向导".to_string(), "引导".to_string()]
        );

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("onboarding is a feature plugin");
        assert!(seat.find("onboarding").is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(seat.find("onboarding").is_some());
    }

    /// `/migrate`, `/login` and `/logout` are this plugin's too: the import
    /// runs [`migrate`] and the sign-in runs [`oauth`], both of which are in
    /// this crate. Each carries the fields its built-in row declared, and all
    /// three leave with the switch.
    #[test]
    fn the_switch_also_takes_migrate_login_and_logout() {
        let (kernel, registry) = booted();
        let seat: Arc<CommandSeat> = kernel
            .context()
            .get::<CommandSeatService>()
            .expect("the seat is on the root");

        let migrate = seat.find("migrate").expect("registered while loaded");
        assert_eq!(migrate.owner, PLUGIN_ID);
        assert_eq!(migrate.handler.native_id(), Some("migrate"));
        assert_eq!(
            migrate.spec.description,
            "Import Claude Code / Codex skills, agents, and commands"
        );
        assert_eq!(migrate.spec.kind, CommandKind::Panel);
        assert_eq!(
            migrate.spec.zh_aliases.as_ref(),
            ["导入".to_string(), "迁移".to_string()]
        );
        // The built-in row set no `surfaces`, so this one takes the same
        // `LOCAL` default: both local front ends, not over the wire.
        assert_eq!(migrate.spec.surfaces, Surfaces::LOCAL);

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("onboarding is a feature plugin");
        for name in ["migrate"] {
            assert!(seat.find(name).is_none(), "/{name} outlived the switch");
        }

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        for name in ["migrate"] {
            assert!(seat.find(name).is_some(), "/{name} did not come back");
        }
    }

    /// The first-run wizard is gated on the same switch as the command: a
    /// user who turned the plugin off is not shown a wizard on next start,
    /// however untouched their config home is.
    #[test]
    fn the_switch_closes_the_first_run_gate_and_opens_it_again() {
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("onboarding-gate");
        let (kernel, registry) = booted();

        assert!(
            first_run_gate(Some(kernel.context())),
            "a fresh config home with the plugin loaded is a first run"
        );
        assert!(wizard_available(Some(kernel.context())));

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("onboarding is a feature plugin");
        assert!(
            !first_run_gate(Some(kernel.context())),
            "with the plugin off the same config home opens straight into a session"
        );
        assert!(!wizard_available(Some(kernel.context())));

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(first_run_gate(Some(kernel.context())));
    }

    /// Finished setup closes the gate while leaving the wizard reachable —
    /// that is the difference between the two questions, and it is what makes
    /// `/onboarding` still open something on a configured machine.
    #[test]
    fn finishing_setup_closes_the_gate_but_leaves_the_wizard_reachable() {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("onboarding-done");
        let (kernel, _registry) = booted();
        assert!(first_run_gate(Some(kernel.context())));

        std::fs::write(
            home.path().join("config.json"),
            r#"{"hasCompletedOnboarding":true}"#,
        )
        .expect("the test owns this directory");

        assert!(!first_run_gate(Some(kernel.context())));
        assert!(wizard_available(Some(kernel.context())));
    }

    /// No kernel at all is the same answer as a disabled plugin. A surface
    /// that cannot ask has not been told to open a wizard.
    #[test]
    fn without_a_kernel_neither_question_says_yes() {
        assert!(!first_run_gate(None));
        assert!(!wizard_available(None));
    }
}
