//! `memory`: durable memory, whole.
//!
//! Everything the feature is now lives here, on five kernel seats and two
//! seams:
//!
//! * the `SaveMemory` tool, on the **tool** seat;
//! * the auto-`MEMORY.md` system-prompt section, on the **prompt** seat at
//!   [`Rung::Memory`](rebon_core::prompt_seat::Rung::Memory), together with
//!   the entrypoints it quotes so the turn can seed its read-state cache
//!   ([`prompt_section`]);
//! * the `nested_memory` reader, on the **attachment** seat
//!   ([`attachments`]);
//! * the `Memory updated in … · /memory to edit` line, on the **turn-hook**
//!   seat ([`update_hook`]);
//! * the document list `/memory` and `/context` print, and the per-agent
//!   memory a sub-agent spawn appends, on the two `rebon-instructions` seams
//!   ([`services`]);
//! * the `/memory` command on the **command** seat ([`command_spec`]), the
//!   browser it opens on the UI seat ([`dialog`]), and the store all of them
//!   read and write ([`memory`]).
//!
//! Switching the plugin off takes all six away and leaves the project
//! instruction documents — `REBON.md` and its `@` includes — exactly as they
//! were: those are not memory, they are what the user wrote, and
//! `rebon-core` renders them from its own table whatever plugins are on.
//!
//! `autoMemoryEnabled` is a second, narrower switch and is deliberately not
//! wired to this one: it is a per-project setting about whether to *load* a
//! project's memory, while this is a process-wide switch about the whole
//! feature.

use std::sync::Arc;

use rebon_command_seat::{
    CommandHandler, CommandKind, CommandSeatService, CommandSpec, Surfaces, COMMAND_SEAT_SERVICE,
};
use rebon_core::attachment_seat::{AttachmentSeatService, Order, ATTACHMENT_SEAT_SERVICE};
use rebon_core::prompt_seat::{PromptSeatService, PROMPT_SEAT_SERVICE};
use rebon_core::tool_seat::{Priority, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_core::turn_hook::{Order as HookOrder, TurnHookSeatService, TURN_HOOK_SEAT_SERVICE};
use rebon_kernel::{
    Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta, Service,
};
use rebon_ui_seat::{DialogDef, UiSeatService};

pub mod attachments;
pub mod dialog;
pub mod memory;
pub mod prompt_section;
pub mod save_memory;
pub mod services;
pub mod update_hook;

pub use attachments::{NestedMemoryAttachmentPoller, NestedMemoryProducer};
pub use dialog::{MemoryDialogState, MemoryFileEntry};
pub use prompt_section::MemoryPromptSections;
pub use save_memory::{SaveMemoryTool, SAVE_MEMORY_TOOL_NAME};
pub use services::MemoryAgentPrompts;
pub use update_hook::{MemoryUpdateNotification, MEMORY_NOTIFICATION_FIELD};

/// Stable id: the config key `plugins.memory.enabled`.
pub const PLUGIN_ID: &str = "memory";

const PROVIDER_ID: &str = "memory";

/// The one tool.
///
/// Public so a test that needs the whole builtin catalogue on a bare
/// [`rebon_core::Engine`] can register it without standing up a kernel.
pub fn tools() -> Vec<Arc<dyn rebon_tool::Tool>> {
    vec![Arc::new(SaveMemoryTool) as Arc<dyn rebon_tool::Tool>]
}

/// `/memory` as the command seat sees it.
///
/// Every field is the one the built-in table declared, carried over
/// unchanged: the same name, the same one-line description, the same Chinese
/// alias, the same `Session` kind, and the same wide surface set — every
/// front end, the `rebon serve` page, the mobile app, and the
/// session-control bit that makes a mirror forward it to the process owning
/// the session rather than answering from a stale projection.
///
/// The command lists what [`memory`] loaded and opens the browser this plugin
/// registers on the UI seat, so the two arrive and leave together.
pub fn command_spec() -> CommandSpec {
    CommandSpec::new("memory", "List loaded memory and instruction files")
        .zh_aliases(["记忆"])
        .surfaces(
            Surfaces::ALL
                .with(Surfaces::WEB)
                .with(Surfaces::MOBILE)
                .with(Surfaces::SESSION_CONTROL),
        )
        .kind(CommandKind::Session)
}

pub struct MemoryPlugin;

impl Plugin for MemoryPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID)
            .inject(&[
                TOOL_SEAT_SERVICE,
                ATTACHMENT_SEAT_SERVICE,
                PROMPT_SEAT_SERVICE,
                TURN_HOOK_SEAT_SERVICE,
                COMMAND_SEAT_SERVICE,
            ])
            .optional_inject(&[UiSeatService::NAME])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = ctx.require::<ToolSeatService>()?;
        seat.register_tools(ctx, PROVIDER_ID, Priority::Feature, tools())?;

        // The documents a turn walked into. [`Order::Context`] is the rung it
        // held inside the engine's fixed producer order: fourth, behind the
        // skill listing and ahead of the queued prompts.
        let attachment_seat = ctx.require::<AttachmentSeatService>()?;
        attachment_seat.register(
            ctx,
            PROVIDER_ID,
            Order::Context,
            Arc::new(NestedMemoryProducer),
        )?;

        // The auto-`MEMORY.md` prompt section. `Rung::Memory` is rank 40 of
        // the stable plane — where the engine's own `memory` section stood
        // until this took it over — so a session with the plugin on receives
        // the same bytes it always did, and one with it off loses exactly
        // that block. The project instruction files at rank 30 are not
        // memory and stay with the engine.
        let prompt_seat = ctx.require::<PromptSeatService>()?;
        prompt_seat.register(ctx, PROVIDER_ID, Arc::new(MemoryPromptSections))?;

        // The `Memory updated in … · /memory to edit` line, which used to be
        // three copies of an auto-memory branch inside `Write`, `Edit` and
        // `MultiEdit`. Ordinary order: it only adds a field, and nothing else
        // in the phase reads it.
        let turn_hooks = ctx.require::<TurnHookSeatService>()?;
        turn_hooks.subscribe_scoped(
            ctx,
            update_hook::HOOK_ID,
            HookOrder::NORMAL,
            Arc::new(MemoryUpdateNotification),
        )?;

        // What other people ask this plugin: the document list `/memory` and
        // `/context` print, and the per-agent memory a sub-agent spawn
        // appends. Both seams live in `rebon-instructions`, below the plugin
        // boundary, because their consumers are on the far side of it.
        rebon_instructions::loaded_documents::provide(
            ctx,
            Arc::new(crate::memory::loaded_files::MemoryLoadedDocuments),
        )?;
        ctx.provide::<rebon_instructions::agent_documents::AgentMemoryPromptService>(Arc::new(
            MemoryAgentPrompts,
        ))?;

        // `/memory` is this plugin's command, so it comes and goes with the
        // same switch. The handler is `Native`: printing the list needs the
        // session's working directory and opening the browser needs the
        // dialog stack, and only a front end holds either.
        let commands = ctx.require::<CommandSeatService>()?;
        let spec = command_spec();
        let handler = CommandHandler::Native(spec.name.clone());
        commands.register(ctx, spec, handler)?;

        // The browser is optional: a kernel booted without the UI seat
        // (a headless harness) still gets the tool.
        if let Ok(ui) = ctx.require::<UiSeatService>() {
            ui.register_dialog(ctx, dialog_def())?;
        }
        Ok(())
    }
}

/// The `/memory` browser, built from the working directory it is asked
/// about. One value: the cwd.
fn dialog_def() -> DialogDef {
    DialogDef::new(dialog::DIALOG_ID, |args| {
        Some(Box::new(MemoryDialogState::open(dialog::entries_for(
            args.value_at(0),
        ))))
    })
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(MemoryPlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Durable memory (SaveMemory)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_command_seat::{CommandSeat, Surface};
    use rebon_core::attachment_seat::AttachmentSeat;
    use rebon_core::prompt_seat::PromptSeat;
    use rebon_core::tool_seat::ToolSeat;
    use rebon_core::turn_hook::TurnHookSeat;
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};
    use rebon_tool::ToolResolver;

    /// Stands in for `core-tools` and `core-commands`, which this crate
    /// cannot depend on. All this plugin needs of them is the five root
    /// seats.
    struct SeatPlugin;

    impl Plugin for SeatPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("test-seat").provides(&[
                TOOL_SEAT_SERVICE,
                ATTACHMENT_SEAT_SERVICE,
                PROMPT_SEAT_SERVICE,
                TURN_HOOK_SEAT_SERVICE,
                COMMAND_SEAT_SERVICE,
            ])
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.provide::<AttachmentSeatService>(AttachmentSeat::new())?;
            ctx.provide::<PromptSeatService>(PromptSeat::new())?;
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

    #[test]
    fn the_switch_takes_save_memory_off_the_seat_and_puts_it_back() {
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
        assert!(seat.resolve(SAVE_MEMORY_TOOL_NAME, None).unwrap().is_some());

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("memory is a feature plugin");
        assert!(seat.resolve(SAVE_MEMORY_TOOL_NAME, None).unwrap().is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(seat.resolve(SAVE_MEMORY_TOOL_NAME, None).unwrap().is_some());
    }

    /// `/memory` is this plugin's command, and it carries every field the
    /// built-in table declared — the wide surface set most of all, since it is
    /// what puts the command on the mobile app and makes a mirror forward it
    /// instead of answering from a stale projection.
    #[test]
    fn the_switch_takes_the_command_off_the_seat_and_puts_it_back() {
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
        assert!(registry.reconcile(&DesiredSet::new()).failed.is_empty());

        let seat: Arc<CommandSeat> = kernel
            .context()
            .get::<CommandSeatService>()
            .expect("the seat is on the root");

        let registered = seat.find("memory").expect("/memory is registered");
        assert_eq!(registered.owner, PLUGIN_ID);
        assert_eq!(registered.handler.native_id(), Some("memory"));
        // Spelled out rather than compared against `command_spec()`, which
        // would only compare the function with itself. These are the fields the
        // built-in row carried.
        let spec = &registered.spec;
        assert_eq!(
            spec.description.as_ref(),
            "List loaded memory and instruction files"
        );
        assert_eq!(spec.zh_aliases, vec!["记忆"]);
        assert!(spec.aliases.is_empty());
        assert_eq!(spec.hint, None);
        assert_eq!(spec.kind, CommandKind::Session);
        assert_eq!(spec.category, rebon_command_seat::Category::Command);
        for surface in [
            Surface::Tui,
            Surface::Desktop,
            Surface::Acp,
            Surface::Web,
            Surface::Mobile,
            Surface::SessionControl,
        ] {
            assert!(spec.available_on(surface), "{surface:?} was dropped");
        }

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("memory is a feature plugin");
        assert!(seat.find("memory").is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(seat.find("memory").is_some());
    }

    /// The nested-memory reader goes off and on with the writer. Switching
    /// the plugin off stops inlining a `REBON.md` a tool walked into; the
    /// files themselves, and the `MEMORY.md` loading, are untouched.
    #[test]
    fn the_switch_takes_the_document_reader_off_the_attachment_seat_and_puts_it_back() {
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
        assert!(registry.reconcile(&DesiredSet::new()).failed.is_empty());

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
            .expect("memory is a feature plugin");
        assert!(attachment_seat.provider_ids().is_empty());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert_eq!(
            attachment_seat.provider_ids(),
            vec![PROVIDER_ID.to_string()]
        );
    }
}
