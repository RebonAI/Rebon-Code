//! `profile`: the feature plugin that owns a named working mode end to end —
//! the `/profile` command, the two tools that propose one, and everything
//! about a profile that is not drawing it.
//!
//! A profile bundles provider, model, agent backend, permission mode and tool
//! surface under a name. `ProfileSwitch` asks the user to move this session
//! onto a saved one; `ProfileSave` drafts a new one for them to accept.
//! Neither tool applies anything — both carry a proposal, and the front end
//! resolves it against what the session is really running, shows the diff and
//! writes only after the user says yes.
//!
//! [`rebon_config::profile_store`] owns the files and the checking, and says of
//! itself that it never applies anything because applying reaches a live
//! session. That line is right; what it left unsaid is that *reaching a live
//! session* is four handles, and the several hundred lines around them —
//! parsing the command, editing one field, deciding which rows a prompt shows,
//! writing the sentence the user reads — are not front-end work at all. They
//! belong to the feature, not to whichever surface happens to show it — and
//! keeping them anywhere else puts half of one feature below the plugin
//! holding the other half.
//!
//! So the split is:
//!
//! * **`rebon-config`** — the profile on disk, its validation, the bypass gate.
//! * **this crate** — the command, the edit, the proposal, the prose, the two
//!   tools, and what the permission layer must not decide without the user.
//! * **the front end** — [`ProfileSession`], six methods over handles it
//!   already holds, plus whatever it does to draw the result.
//!
//! # Why a trait and not a struct of values
//!
//! Applying a profile *mutates*: it switches the agent backend, moves the
//! permission mode, narrows the tool filter. A snapshot could describe the
//! session but not move it, so the apply order — which is load-bearing, since
//! a failed step has to report which earlier ones already landed — would have
//! gone back to the front end and been written once per front end. Six small
//! methods keep the order here.
//!
//! # What is deliberately not here
//!
//! Re-resolving the runtime after provider/model move. That needs a live model
//! client and a tokio handle, and the front ends differ in where those are, so
//! [`RuntimeRefresh`] is *returned* rather than performed — the same shape the
//! terminal already used internally for the permission-callback case.
//!
//! # The switch
//!
//! Turning the plugin off (`plugins.profile.enabled = false`) disposes this
//! context, which takes both tools off the tool seat, `/profile` off the
//! command seat and the profile carve-out off the permission seat. Profiles
//! already on disk are untouched; nothing in the process offers them.

use std::sync::Arc;

use rebon_command_seat::{
    CommandHandler, CommandSeatService, CommandSpec, Surfaces, COMMAND_SEAT_SERVICE,
};
use rebon_core::permission_seat::{PermissionRuleSeatService, PERMISSION_RULE_SEAT_SERVICE};
use rebon_core::tool_seat::{Priority, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};

pub mod apply;
pub mod command;
pub mod field;
pub mod permission_rule;
pub mod profile_save;
pub mod profile_switch;
pub mod proposal;
pub mod render;
pub mod surface;
#[cfg(test)]
mod test_session;

pub use apply::{apply_to_session, reset_tool_surface, save_current_session, ProfileSession};
pub use command::{handle_profile_command, parse_profile_args, switches_agent, ProfileCommand};
pub use field::{is_clear_word, parse_tools_value, set_profile_field};
pub use permission_rule::ProfilePermissionRule;
pub use profile_save::ProfileSaveTool;
pub use profile_switch::ProfileSwitchTool;
pub use proposal::{
    applied_input, apply_approved_proposal, resolve_proposal_action, save_proposal,
    switch_proposal, ApprovedOutcome, ProfileProposal, ProfileProposalAction, ProfileProposalRow,
};
pub use render::{
    declared_lines, render_list, render_profile, set_usage_text, unapplied_notes, unchanged_note,
    usage_text,
};
pub use surface::{ProfileApplyOutcome, ProfileCommandResult, RuntimeRefresh};

/// The `ProfileSwitch` tool's canonical name.
pub const PROFILE_SWITCH_TOOL_NAME: &str = "ProfileSwitch";

/// Key the approving front end injects into the `ProfileSwitch` input to
/// report what it actually did. Its absence tells the tool that approval came
/// from a surface that cannot apply profiles.
pub const PROFILE_SWITCH_APPLIED_KEY: &str = "applied";

/// The `ProfileSave` tool's canonical name.
pub const PROFILE_SAVE_TOOL_NAME: &str = "ProfileSave";

/// Key the approving front end injects to report what it wrote. Absent means
/// nothing was written.
pub const PROFILE_SAVE_WRITTEN_KEY: &str = "written";

/// Stable id: the config key `plugins.profile.enabled` and the name in
/// `/kernel plugins`.
pub const PLUGIN_ID: &str = "profile";

/// The provider id the two tools sit under on the seat.
const PROVIDER_ID: &str = "profile";

/// The two tools, in registration order.
///
/// Public so a test that needs the whole builtin catalogue on a bare
/// [`rebon_core::Engine`] can register them without standing up a kernel.
pub fn tools() -> Vec<Arc<dyn rebon_tool::Tool>> {
    vec![
        Arc::new(ProfileSwitchTool) as Arc<dyn rebon_tool::Tool>,
        Arc::new(ProfileSaveTool),
    ]
}

/// `/profile` as the command seat sees it.
///
/// The hint is the whole grammar [`parse_profile_args`] accepts, which is why
/// it is written next to the parser's crate rather than in a table someone
/// editing the parser would never open.
///
/// Terminal only: applying a profile moves a live session through
/// [`ProfileSession`], and the desktop settings window has no profile surface
/// to move. Offering the command where nothing runs it is exactly the drift
/// `Surfaces::TUI_ONLY` exists to prevent.
pub fn command_spec() -> CommandSpec {
    CommandSpec::new(
        "profile",
        "Switch provider, model, agent, permissions and tools as one bundle",
    )
    .zh_aliases(["侧写", "配置组"])
    .hint("[name]|list|show <name>|save <name>|set <name> <field> <value>|remove <name>|reset")
    .surfaces(Surfaces::TUI_ONLY)
}

pub struct ProfilePlugin;

impl Plugin for ProfilePlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).inject(&[
            TOOL_SEAT_SERVICE,
            COMMAND_SEAT_SERVICE,
            PERMISSION_RULE_SEAT_SERVICE,
        ])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = ctx.require::<ToolSeatService>()?;
        seat.register_tools(ctx, PROVIDER_ID, Priority::Feature, tools())?;

        // `/profile` is the user doing for themselves what the two tools ask
        // permission to do, so it belongs to the same switch. The handler is
        // `Native`: the terminal maps the id to `handle_profile_command` with
        // the session handles only it holds.
        let commands = ctx.require::<CommandSeatService>()?;
        let spec = command_spec();
        let handler = CommandHandler::Native(spec.name.clone());
        commands.register(ctx, spec, handler)?;

        // What the permission layer must not decide without the user. The
        // engine used to know these two tools by name; it asks the seat now.
        let rules = ctx.require::<PermissionRuleSeatService>()?;
        rules.register(ctx, PROVIDER_ID, Arc::new(ProfilePermissionRule))?;

        Ok(())
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(ProfilePlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Profiles (ProfileSwitch, ProfileSave)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_command_seat::{CommandSeat, Surface};
    use rebon_core::permission_seat::PermissionRuleSeat;
    use rebon_core::tool_seat::ToolSeat;
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};
    use rebon_tool::{ToolContext, ToolResolver};

    /// Stands in for the Core plugins that provide the three seats this one
    /// registers on — `core-tools`, `core-commands` and whichever provides the
    /// permission seat. Those plugins depend on this crate and so cannot be
    /// depended on from here; all this plugin needs of them is the seats on
    /// the kernel root.
    struct SeatPlugin;

    impl Plugin for SeatPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("test-seat").provides(&[
                TOOL_SEAT_SERVICE,
                COMMAND_SEAT_SERVICE,
                PERMISSION_RULE_SEAT_SERVICE,
            ])
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.provide::<ToolSeatService>(ToolSeat::new())?;
            ctx.provide::<CommandSeatService>(CommandSeat::new())?;
            ctx.provide::<PermissionRuleSeatService>(PermissionRuleSeat::new())
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

    /// The switch is the whole contract: enabled means the model can resolve
    /// both tools, disabled means it cannot, and flipping back restores them.
    #[test]
    fn the_switch_takes_the_profile_tools_off_the_seat_and_puts_them_back() {
        let (kernel, registry) = booted();

        let seat: Arc<ToolSeat> = kernel
            .context()
            .get::<ToolSeatService>()
            .expect("the seat is on the root");
        for name in [PROFILE_SWITCH_TOOL_NAME, PROFILE_SAVE_TOOL_NAME] {
            assert!(
                seat.resolve(name, None).unwrap().is_some(),
                "{name} resolves while profile is loaded"
            );
        }

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("profile is a feature plugin");
        for name in [PROFILE_SWITCH_TOOL_NAME, PROFILE_SAVE_TOOL_NAME] {
            assert!(
                seat.resolve(name, None).unwrap().is_none(),
                "disabling the plugin takes {name} off the seat"
            );
        }

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(seat
            .resolve(PROFILE_SWITCH_TOOL_NAME, None)
            .unwrap()
            .is_some());
        assert!(seat
            .resolve(PROFILE_SAVE_TOOL_NAME, None)
            .unwrap()
            .is_some());
    }

    /// `/profile` is this plugin's command, so it comes and goes with the
    /// same switch — a session that turned profiles off is not offered a
    /// command whose every branch reaches a feature that is gone.
    ///
    /// The handler stays [`CommandHandler::Native`]: running `/profile` needs
    /// the live session handles only a front end holds.
    #[test]
    fn the_switch_takes_the_command_off_the_seat_and_puts_it_back() {
        let (kernel, registry) = booted();
        let seat: Arc<CommandSeat> = kernel
            .context()
            .get::<CommandSeatService>()
            .expect("the seat is on the root");

        let registered = seat.find("profile").expect("registered while loaded");
        assert_eq!(registered.owner, PLUGIN_ID);
        assert_eq!(registered.handler.native_id(), Some("profile"));
        assert_eq!(registered.spec.hint, command_spec().hint);
        assert!(registered.spec.available_on(Surface::Tui));
        assert!(!registered.spec.available_on(Surface::Desktop));

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("profile is a feature plugin");
        assert!(seat.find("profile").is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(seat.find("profile").is_some());
    }

    /// And so does the permission carve-out. A rule claiming calls that can
    /// no longer be made is the seat's own kind of stale state.
    #[test]
    fn the_switch_takes_the_permission_rule_off_the_seat_and_puts_it_back() {
        let (kernel, registry) = booted();
        let seat: Arc<PermissionRuleSeat> = kernel
            .context()
            .get::<PermissionRuleSeatService>()
            .expect("the seat is on the root");
        let context = ToolContext::new();

        assert_eq!(
            seat.rules()
                .requires_user_decision(PROFILE_SWITCH_TOOL_NAME, &context),
            Some(rebon_core::permission_seat::DecisionScope::EvenUnderBypass)
        );

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("profile is a feature plugin");
        assert!(seat.rules().is_empty());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert_eq!(
            seat.rules()
                .requires_user_decision(PROFILE_SAVE_TOOL_NAME, &context),
            Some(rebon_core::permission_seat::DecisionScope::EvenUnderBypass)
        );
    }
}
