//! `skill`: skills, whole — the model, the loader, the index, the tool, the
//! listing, the discovery subscriber and the selector.
//!
//! A skill is a `SKILL.md` file with frontmatter. This crate owns every part
//! of what happens to one:
//!
//! | module | what it owns |
//! |---|---|
//! | [`skills`] | the pure model: frontmatter, naming, the command shape, conditional matching, the compiled-in bundled definitions |
//! | [`loader`] | reading them off disk at startup and while a session runs |
//! | [`bundled_cache`] | writing a bundled skill out so the model can `Read` it |
//! | [`skill`] | [`SkillRegistry`], the loaded index, and the [`SkillTool`] that expands one |
//! | [`catalog`] | what this session can invoke, for the listing and for `/<name>` |
//! | [`attachments`] | the `skill_listing` reminder, as a delta |
//! | [`discovery_hook`] | registering what a tool round just brought into view |
//! | [`dialog`] | the `/skills` selector |
//!
//! It was four places: the pure half was a crate of its own, the tool
//! and the index were `rebon-tool`'s, the loader and the bundled cache were
//! `rebon-core`'s, and this crate was a registration shell. The engine
//! carried a Cargo dependency on skills to build a system prompt, and the tool
//! could not travel because `ToolContext` held the registry by name. Both are
//! gone: the registry rides in `ToolContext`'s generic extension bag as
//! [`SkillContext`], and the engine consumes seats instead of a subsystem.
//!
//! **Seats.** The tool on `tool-registry`; the listing on
//! `attachment-producers` at [`Order::Listing`], the rung it held as the
//! second of the engine's fixed producers behind the date roll; the discovery
//! subscriber on `turn-hooks` at `Order::NORMAL`, the rung it held among the
//! engine's builtin subscribers; the `/skills` command on `command-registry`
//! and the selector it opens on `ui-registry` when a kernel has one.
//!
//! **What the switch does.** Turning the plugin off takes `Skill` off the
//! tool seat, the listing off the attachment seat, the discovery subscriber
//! off the hook seat and `/skills` off the command seat: the model can no
//! longer invoke a skill by name, is no longer told which ones exist, no
//! longer picks up new ones, and the command that manages them is gone from
//! `/help` and the `/` picker. Skills stay loaded and `/`-commands still
//! expand, because those are the front end's — it calls
//! [`loader::load_startup_skills`] itself and reads the index it gets back.
//! Turning the plugin back on makes them invocable again.

use std::sync::Arc;

use rebon_command_seat::{
    CommandHandler, CommandKind, CommandSeatService, CommandSpec, Surfaces, COMMAND_SEAT_SERVICE,
};
use rebon_core::attachment_seat::{AttachmentSeatService, Order, ATTACHMENT_SEAT_SERVICE};
use rebon_core::tool_seat::{Priority, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_core::turn_hook::{TurnHookSeatService, TURN_HOOK_SEAT_SERVICE};
use rebon_kernel::{
    Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta, Service,
};
use rebon_ui_seat::input::{decode, SkillsDialogInput};
use rebon_ui_seat::{DialogDef, UiSeatService};

pub mod attachments;
pub mod bundled_cache;
pub mod catalog;
pub mod dialog;
pub mod discovery_hook;
pub mod loader;
pub mod skill;
pub mod skill_bundles;
#[deny(missing_docs)]
pub mod skills;

pub use attachments::{SkillAttachmentPoller, SkillAttachmentProducer};
pub use catalog::RegistrySkillCatalog;
pub use discovery_hook::{ProgressiveSkillDiscoveryHook, PROGRESSIVE_SKILL_DISCOVERY_HOOK_ID};
pub use loader::{load_startup_skills, SkillLoaderConfig, SkillState};
pub use skill::{Skill, SkillContext, SkillRegistry, SkillSource, SkillTool, SKILL_TOOL_NAME};
pub use skills::skill_command::parse_user_skill_invocation;

/// Stable id: the config key `plugins.skill.enabled`.
pub const PLUGIN_ID: &str = "skill";

const PROVIDER_ID: &str = "skill";

/// The one tool.
pub fn tools() -> Vec<Arc<dyn rebon_tool::Tool>> {
    vec![Arc::new(SkillTool) as Arc<dyn rebon_tool::Tool>]
}

/// `/skills` as the command seat sees it.
///
/// The fields, each of them load-bearing: the one-line description the `/`
/// picker shows, the Chinese alias, the `Panel` kind, and the two surfaces —
/// both local front ends and the `rebon serve` page.
///
/// The command opens [`dialog::SkillsDialogState`], which this plugin also
/// registers, so the two arrive and leave together: with the plugin off there
/// is no selector for `/skills` to open.
pub fn command_spec() -> CommandSpec {
    CommandSpec::new("skills", "Manage available skills")
        .zh_aliases(["技能"])
        .surfaces(Surfaces::LOCAL.with(Surfaces::WEB))
        .kind(CommandKind::Panel)
}

pub struct SkillPlugin;

impl Plugin for SkillPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID)
            .inject(&[
                TOOL_SEAT_SERVICE,
                ATTACHMENT_SEAT_SERVICE,
                COMMAND_SEAT_SERVICE,
            ])
            .optional_inject(&[UiSeatService::NAME, TURN_HOOK_SEAT_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = ctx.require::<ToolSeatService>()?;
        seat.register_tools(ctx, PROVIDER_ID, Priority::Feature, tools())?;

        // The listing. [`Order::Listing`] is the rung it held inside the
        // engine's fixed producer order: second, right behind the date roll,
        // which took [`Order::DayRoll`] on this same seat when this left.
        let attachment_seat = ctx.require::<AttachmentSeatService>()?;
        attachment_seat.register(
            ctx,
            PROVIDER_ID,
            Order::Listing,
            Arc::new(SkillAttachmentProducer),
        )?;

        // `/skills` is this plugin's command, so it comes and goes with the
        // same switch. The handler is `Native`: opening the selector needs
        // the loaded index and the dialog stack, which only a front end
        // holds.
        let commands = ctx.require::<CommandSeatService>()?;
        let spec = command_spec();
        let handler = CommandHandler::Native(spec.name.clone());
        commands.register(ctx, spec, handler)?;

        // Progressive discovery is optional the same way the selector is: a
        // kernel booted without the hook seat still gets the tool and the
        // listing, and the session simply never grows its index mid-turn.
        if let Ok(hooks) = ctx.require::<TurnHookSeatService>() {
            discovery_hook::subscribe(ctx, &hooks)?;
        }

        // The selector is optional: a kernel booted without the UI seat
        // still gets the tool.
        if let Ok(ui) = ctx.require::<UiSeatService>() {
            ui.register_dialog(ctx, dialog_def())?;
        }
        Ok(())
    }
}

/// The `/skills` selector, built from the loaded index the front end
/// collected. One value: a JSON [`SkillsDialogInput`].
fn dialog_def() -> DialogDef {
    DialogDef::new(dialog::DIALOG_ID, |args| {
        let input: SkillsDialogInput = decode(args.value_at(0))?;
        dialog::SkillsDialogState::open(input)
            .map(|dialog| Box::new(dialog) as Box<dyn rebon_dialog::model::DialogModel>)
    })
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(SkillPlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Skills (Skill)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_command_seat::{CommandSeat, Surface};
    use rebon_core::attachment_seat::AttachmentSeat;
    use rebon_core::tool_seat::ToolSeat;
    use rebon_core::turn_hook::TurnHookSeat;
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};
    use rebon_tool::{Tool, ToolResolver};

    /// Stands in for `core-tools` and `core-commands`, which cannot be
    /// depended on from here. All this plugin needs of them is the four seats
    /// on the kernel root.
    struct SeatPlugin;

    impl Plugin for SeatPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("test-seat").provides(&[
                TOOL_SEAT_SERVICE,
                ATTACHMENT_SEAT_SERVICE,
                TURN_HOOK_SEAT_SERVICE,
                COMMAND_SEAT_SERVICE,
            ])
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.provide::<AttachmentSeatService>(AttachmentSeat::new())?;
            ctx.provide::<TurnHookSeatService>(TurnHookSeat::new())?;
            ctx.provide::<CommandSeatService>(CommandSeat::new())?;
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

    fn loaded_kernel() -> (Arc<Kernel>, Arc<PluginRegistry>) {
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
    /// `Skill`, disabled means it cannot, and flipping back restores it.
    #[test]
    fn the_switch_takes_skill_off_the_seat_and_puts_it_back() {
        let (kernel, registry) = loaded_kernel();

        let seat: Arc<ToolSeat> = kernel
            .context()
            .get::<ToolSeatService>()
            .expect("the seat is on the root");
        assert!(seat.resolve(SKILL_TOOL_NAME, None).unwrap().is_some());

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("skill is a feature plugin");
        assert!(seat.resolve(SKILL_TOOL_NAME, None).unwrap().is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(seat.resolve(SKILL_TOOL_NAME, None).unwrap().is_some());
    }

    /// `/skills` is this plugin's command, carrying the fields `/help` and
    /// the `/` picker show.
    #[test]
    fn the_switch_takes_the_command_off_the_seat_and_puts_it_back() {
        let (kernel, registry) = loaded_kernel();

        let seat: Arc<CommandSeat> = kernel
            .context()
            .get::<CommandSeatService>()
            .expect("the seat is on the root");

        let registered = seat.find("skills").expect("/skills is registered");
        assert_eq!(registered.owner, PLUGIN_ID);
        assert_eq!(registered.handler.native_id(), Some("skills"));
        // Spelled out rather than compared against `command_spec()`, which
        // would only compare the function against itself. Losing one of these
        // is invisible until a picker stops finding the command or `/help`
        // files it under the wrong tab.
        let spec = &registered.spec;
        assert_eq!(spec.description.as_ref(), "Manage available skills");
        assert_eq!(spec.zh_aliases, vec!["技能"]);
        assert!(spec.aliases.is_empty());
        assert_eq!(spec.hint, None);
        assert_eq!(spec.kind, CommandKind::Panel);
        assert_eq!(spec.category, rebon_command_seat::Category::Command);
        // Both local front ends and the `rebon serve` page, and nothing else.
        assert_eq!(spec.surfaces, Surfaces::LOCAL.with(Surfaces::WEB));
        assert!(spec.available_on(Surface::Tui));

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("skill is a feature plugin");
        assert!(seat.find("skills").is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(seat.find("skills").is_some());
    }

    /// The listing goes off and on with the tool, which is the point: naming
    /// skills the model can no longer invoke would be worse than silence.
    #[test]
    fn the_switch_takes_the_listing_off_the_attachment_seat_and_puts_it_back() {
        let (kernel, registry) = loaded_kernel();

        let attachment_seat = kernel
            .context()
            .get::<AttachmentSeatService>()
            .expect("the attachment seat is on the root");
        assert_eq!(
            attachment_seat.provider_ids(),
            vec![PROVIDER_ID.to_string()]
        );

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("skill is a feature plugin");
        assert!(attachment_seat.provider_ids().is_empty());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert_eq!(
            attachment_seat.provider_ids(),
            vec![PROVIDER_ID.to_string()]
        );
    }

    /// And so does discovery. A session that keeps growing its index while
    /// the tool that invokes it is switched off would be collecting names
    /// nobody can use.
    #[test]
    fn the_switch_takes_the_discovery_subscriber_off_the_hook_seat_and_puts_it_back() {
        let (kernel, registry) = loaded_kernel();

        let hooks = kernel
            .context()
            .get::<TurnHookSeatService>()
            .expect("the hook seat is on the root");
        assert!(hooks
            .subscriber_ids()
            .contains(&PROGRESSIVE_SKILL_DISCOVERY_HOOK_ID.to_string()));

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("skill is a feature plugin");
        assert!(!hooks
            .subscriber_ids()
            .contains(&PROGRESSIVE_SKILL_DISCOVERY_HOOK_ID.to_string()));

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(hooks
            .subscriber_ids()
            .contains(&PROGRESSIVE_SKILL_DISCOVERY_HOOK_ID.to_string()));
    }

    /// Each crate pins its own tools against the shared facts table, and
    /// `Skill` is this one's. The row is what policy reads to know
    /// `SkillTool` names this tool, and what the engine reads to name the
    /// tool it dispatches a skill invocation through — neither of which can
    /// ask the tool itself.
    #[test]
    fn the_skill_tool_matches_the_shared_facts_table() {
        let tool = SkillTool;
        let shared = rebon_tools_core::BUILTIN_TOOL_FACTS
            .iter()
            .find(|entry| entry.name == tool.id().as_str())
            .expect("Skill is in the shared facts table");
        assert_eq!(shared.name, SKILL_TOOL_NAME);
        assert_eq!(tool.aliases(), shared.aliases);
        assert_eq!(tool.kind(), shared.kind);
        assert_eq!(tool.file_target_field(), shared.file_target_field);
    }
}
