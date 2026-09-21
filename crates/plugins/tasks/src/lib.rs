//! `tasks`: the feature plugin that puts the task, team and queue tools on
//! the process tool seat.
//!
//! Thirteen tools in three groups that share one idea — work this session
//! tracks that outlives a single turn:
//!
//! - the task list (`TaskCreate`, `TaskGet`, `TaskList`, `TaskUpdate`,
//!   `TaskStop`) and its pre-Task predecessor `TodoWrite`;
//! - the team (`TeamCreate`, `TeamDelete`, `SendMessage`);
//! - the Agent Queue a coordinator drives (`QueuePlan`, `QueueVerdict`,
//!   `QueueDispatch`, `QueueBlock`).
//!
//! The periodic nudge to use them came with the tools — see [`attachments`] —
//! and reaches a turn through the kernel's `attachment-producers` seat rather
//! than through the engine's own poller.
//!
//! **What moved and what did not.** The tools and their in-memory execution
//! runtime live here. [`runtime`] owns task snapshots, lifecycle transitions,
//! cancellation and live-event journals; coordinator workers publish into it
//! directly. The persistence stores remain in `rebon-tool`, because
//! `ToolContext` hands them out and readers outside this plugin use them:
//!
//! | stayed in `rebon-tool` | why |
//! |---|---|
//! | `tasks` (the task files, `TaskContext`) | `ToolContext` carries the list id; [`runtime::TaskRegistry`] mirrors execution into those files |
//! | `team_files` / `team_mailbox` / `team_manager` (`TeamContext`) | the spawner, the mailbox poller and the TUI read them without this plugin |
//! | `todo_write` (the todo store) | the TUI's task pane renders it directly |
//! | `queue` (`QueueContext`) | `ToolContext` hands out the controller the coordinator sets |
//!
//! Turning the plugin off takes all thirteen tools and three attachment
//! producers off their process seats. Task runtime state and stores already
//! created by the session are untouched: the model just loses the plugin
//! surfaces that read and write them.
//!
//! **Session scopes.** The tools remain process-wide registrations, while one
//! [`runtime::TaskRegistry`] is bound to each exact session [`Context`] through
//! the typed [`TASK_REGISTRY_SERVICE`] seat before `SessionOpened`. Rust
//! consumers resolve that seat; the small `TaskRuntimeController` callback in
//! `rebon-tool` is only the kernel-free process boundary for detached shell and
//! monitor notifications and resolves the same seat by session id.

use std::sync::Arc;

use rebon_command_seat::{CommandHandler, CommandSeatService, COMMAND_SEAT_SERVICE};
use rebon_core::attachment_seat::{AttachmentSeatService, Order, ATTACHMENT_SEAT_SERVICE};
use rebon_core::tool_seat::{Priority, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};
use rebon_ui_seat::{DialogDef, UiSeatService};

pub mod attachments;
pub mod mailbox;
pub mod queue_dispatch;
pub mod queue_plan;
pub mod queue_verdict;
pub mod roster;
pub mod runtime;
pub mod send_message;
pub mod task_create;
pub mod task_get;
pub mod task_list;
pub mod task_registry;
pub mod task_stop;
pub mod task_update;
pub mod team_create;
pub mod team_delete;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod todo_write;
pub mod ui;
pub mod workflow_progress;

pub use attachments::{TaskAttachmentPoller, TaskAttachmentProducer};
pub use mailbox::{
    drain_teammate_mailbox_for, MailboxAttachmentPoller, MailboxAttachmentProducer,
    TeammateMailboxMessage, TeammateMailboxPoller,
};
pub use queue_dispatch::{
    QueueBlockTool, QueueDispatchTool, QUEUE_BLOCK_TOOL_NAME, QUEUE_DISPATCH_TOOL_NAME,
};
pub use queue_plan::{QueuePlanTool, QUEUE_PLAN_TOOL_NAME};
pub use queue_verdict::{QueueVerdictTool, QUEUE_VERDICT_TOOL_NAME};
pub use roster::{
    render_teammate_roster_context, RosterAttachmentPoller, RosterAttachmentProducer,
};
pub use send_message::{SendMessageTool, SEND_MESSAGE_TOOL_NAME};
pub use task_create::{TaskCreateTool, TASK_CREATE_TOOL_NAME};
pub use task_get::{TaskGetTool, TASK_GET_TOOL_NAME};
pub use task_list::{TaskListTool, TASK_LIST_TOOL_NAME};
pub use task_registry::{
    provide_task_registry, require_task_registry, TaskRegistryResolver, TaskRegistrySeat,
    TaskRegistryService, TASK_REGISTRY_SERVICE,
};
pub use task_stop::{TaskStopTool, TASK_STOP_TOOL_NAME};
pub use task_update::{TaskUpdateTool, TASK_UPDATE_TOOL_NAME};
pub use team_create::{TeamCreateTool, TEAM_CREATE_TOOL_NAME};
pub use team_delete::{TeamDeleteTool, TEAM_DELETE_TOOL_NAME};
pub use todo_write::{TodoWriteTool, TODO_WRITE_TOOL_NAME};

/// Stable id: the config key `plugins.tasks.enabled`.
pub const PLUGIN_ID: &str = "tasks";

const PROVIDER_ID: &str = "tasks";

/// Attachment-seat provider id for the teammate mailbox.
const MAILBOX_PROVIDER_ID: &str = "tasks/mailbox";

/// Attachment-seat provider id for the teammate roster.
const ROSTER_PROVIDER_ID: &str = "tasks/roster";

/// Attachment-seat provider id for the periodic task nudge.
const REMINDER_PROVIDER_ID: &str = "tasks/reminder";

/// The thirteen tools, in registration order.
///
/// Public so a test that needs the whole builtin catalogue on a bare
/// [`rebon_core::Engine`] can register them without standing up a kernel.
pub fn tools() -> Vec<Arc<dyn rebon_tool::Tool>> {
    vec![
        Arc::new(TaskCreateTool) as Arc<dyn rebon_tool::Tool>,
        Arc::new(TaskGetTool),
        Arc::new(TaskListTool),
        Arc::new(TaskUpdateTool),
        Arc::new(TaskStopTool),
        Arc::new(TodoWriteTool),
        Arc::new(TeamCreateTool),
        Arc::new(TeamDeleteTool),
        Arc::new(SendMessageTool),
        Arc::new(QueuePlanTool),
        Arc::new(QueueVerdictTool),
        Arc::new(QueueDispatchTool),
        Arc::new(QueueBlockTool),
    ]
}

/// Every tool name this plugin puts on the seat, canonical spelling.
pub const TOOL_NAMES: &[&str] = &[
    TASK_CREATE_TOOL_NAME,
    TASK_GET_TOOL_NAME,
    TASK_LIST_TOOL_NAME,
    TASK_UPDATE_TOOL_NAME,
    TASK_STOP_TOOL_NAME,
    TODO_WRITE_TOOL_NAME,
    TEAM_CREATE_TOOL_NAME,
    TEAM_DELETE_TOOL_NAME,
    SEND_MESSAGE_TOOL_NAME,
    QUEUE_PLAN_TOOL_NAME,
    QUEUE_VERDICT_TOOL_NAME,
    QUEUE_DISPATCH_TOOL_NAME,
    QUEUE_BLOCK_TOOL_NAME,
];

#[derive(Default)]
pub struct TasksPlugin;

impl Plugin for TasksPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID)
            .inject(&[
                TOOL_SEAT_SERVICE,
                ATTACHMENT_SEAT_SERVICE,
                COMMAND_SEAT_SERVICE,
            ])
            .optional_inject(&[
                TASK_REGISTRY_SERVICE,
                <UiSeatService as rebon_kernel::Service>::NAME,
            ])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        // The host binds state before announcing the session. This plugin only
        // opens access to it; unloading closes new access through the
        // PluginStateChanged listener installed with the binding, while held
        // Arcs finish normally and re-enable sees the same state.
        ctx.on::<rebon_kernel::SessionOpened>(|event| {
            if let Some(seat) = event.ctx.get::<TaskRegistryService>() {
                seat.set_available(true);
            }
        });

        let seat = ctx.require::<ToolSeatService>()?;
        seat.register_tools(ctx, PROVIDER_ID, Priority::Feature, tools())?;

        // Three producers, each on the rung it held inside the engine's fixed
        // producer order: the teammate roster with the other catalogues, the
        // teammate mailbox seventh of eight, the task nudge last. Their
        // relative order is the rungs' doing, not this registration order's,
        // and each takes its own provider id so the seat can tell them apart.
        let attachment_seat = ctx.require::<AttachmentSeatService>()?;
        attachment_seat.register(
            ctx,
            ROSTER_PROVIDER_ID,
            Order::Listing,
            Arc::new(RosterAttachmentProducer),
        )?;
        attachment_seat.register(
            ctx,
            MAILBOX_PROVIDER_ID,
            Order::Mailbox,
            Arc::new(MailboxAttachmentProducer),
        )?;
        attachment_seat.register(
            ctx,
            REMINDER_PROVIDER_ID,
            Order::Reminder,
            Arc::new(TaskAttachmentProducer),
        )?;

        // `/tasks`, `/workflows` and `/teams`: the three ways a person asks to
        // see the work these tools created. They belong to the same switch as
        // the tools, so they are registered here rather than listed in the
        // built-in table. The handler is `Native` — opening a panel is a write
        // into the front end's own state, so the front end runs it by name.
        let commands = ctx.require::<CommandSeatService>()?;
        for spec in ui::commands::command_specs() {
            let handler = CommandHandler::Native(spec.name.clone());
            commands.register(ctx, spec, handler)?;
        }

        // The background-task panel those commands open. Optional, like the
        // memory browser's: a kernel booted without a UI seat — a headless
        // harness, a worker — still gets the tools. Disposing this plugin's
        // context takes the registration with it, so a front end that asks
        // the seat for `tasks` while the plugin is off is told there is no
        // such panel rather than opening one over a runtime that is gone.
        //
        // `/teams` is not here, and cannot be until the seat carries a
        // session handle: its reducer kills teammates and cycles their
        // permission modes through the live `TaskRegistry`, which
        // `DialogModel::on_key` has no room for. Holding the registry in the
        // dialog instead was tried and does not work either — the terminal
        // opens the overlay from a footer pill that runs before a session
        // exists, and production `AppState` deliberately keeps only a
        // snapshot projection (RFC A6), so there is no registry to hand it.
        // The `/teams` command still goes off with this plugin.
        if let Ok(ui) = ctx.require::<UiSeatService>() {
            ui.register_dialog(ctx, background_tasks_dialog_def())?;
        }

        Ok(())
    }
}

/// `/tasks` and `/workflows`: the same panel, the second one with a kind
/// filter. Built from a [`ui::background_tasks_dialog::BackgroundTasksDialogOpen`]
/// payload, because a list of snapshots does not fit in positional strings.
fn background_tasks_dialog_def() -> DialogDef {
    use ui::background_tasks_dialog::{BackgroundTasksDialogOpen, BackgroundTasksDialogState};
    DialogDef::new(rebon_ui_seat::ids::dialog::TASKS, |args| {
        let opened = args.payload_as::<BackgroundTasksDialogOpen>()?;
        Some(Box::new(BackgroundTasksDialogState::open_with(opened)))
    })
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(TasksPlugin::default()))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Tasks, teams and the Agent Queue",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_command_seat::CommandSeat;
    use rebon_core::attachment_seat::AttachmentSeat;
    use rebon_core::tool_seat::ToolSeat;
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};
    use rebon_tool::{Tool, ToolResolver};
    use rebon_ui_seat::{DialogArgs, UiSeat};

    /// Stands in for `core-tools` and `core-commands`, which cannot be
    /// depended on here. All this plugin needs is the three root seats.
    struct SeatPlugin;

    impl Plugin for SeatPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("test-seat").provides(&[
                TOOL_SEAT_SERVICE,
                ATTACHMENT_SEAT_SERVICE,
                COMMAND_SEAT_SERVICE,
                <UiSeatService as rebon_kernel::Service>::NAME,
            ])
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.provide::<AttachmentSeatService>(AttachmentSeat::new())?;
            ctx.provide::<CommandSeatService>(CommandSeat::new())?;
            ctx.provide::<UiSeatService>(UiSeat::new())?;
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

    /// `TodoWrite` and the five `Task*` tools are each other's off switch —
    /// `is_todo_v2_enabled()` decides which half a session sees, and the
    /// seat honours `is_enabled()`. So no one environment resolves all
    /// thirteen; the twelve that are live by default are here and
    /// [`the_switch_reaches_todo_write_too`] covers the thirteenth.
    const DEFAULT_ON: &[&str] = &[
        TASK_CREATE_TOOL_NAME,
        TASK_GET_TOOL_NAME,
        TASK_LIST_TOOL_NAME,
        TASK_UPDATE_TOOL_NAME,
        TASK_STOP_TOOL_NAME,
        TEAM_CREATE_TOOL_NAME,
        TEAM_DELETE_TOOL_NAME,
        SEND_MESSAGE_TOOL_NAME,
        QUEUE_PLAN_TOOL_NAME,
        QUEUE_VERDICT_TOOL_NAME,
        QUEUE_DISPATCH_TOOL_NAME,
        QUEUE_BLOCK_TOOL_NAME,
    ];

    /// Sets one variable for as long as it is held, under the same lock
    /// `TestConfigHome` takes, so two tests cannot read each other's
    /// environment.
    struct EnvGuard {
        _home: rebon_tool::tasks::test_support::TestConfigHome,
        name: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let home = rebon_tool::tasks::test_support::TestConfigHome::new("plugin-switch");
            let previous = std::env::var(name).ok();
            std::env::set_var(name, value);
            Self {
                _home: home,
                name,
                previous,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }

    /// The three commands go off with the tools, and come back with them.
    ///
    /// Each one opens a surface over data only this plugin's runtime has, so
    /// a session with the plugin off must not offer them: a `/tasks` that
    /// answers nothing would reach the model as prompt text.
    /// The `/tasks` panel goes off with the plugin too.
    ///
    /// The command and the panel are two doors to the same room: a front
    /// end that still held the command's id and asked the seat to open the
    /// panel would otherwise get one, over a runtime that is gone.
    #[test]
    fn the_switch_takes_the_tasks_panel_off_the_ui_seat_and_puts_it_back() {
        use ui::background_tasks_dialog::BackgroundTasksDialogOpen;

        let (kernel, registry) = boot();
        let ui: Arc<UiSeat> = kernel
            .context()
            .get::<UiSeatService>()
            .expect("the seat is on the root");
        let id = rebon_ui_seat::ids::dialog::TASKS;
        let args = || DialogArgs::payload(BackgroundTasksDialogOpen::default());

        assert!(ui.has(id), "registered while the plugin is loaded");
        assert_eq!(ui.open(id, args()).map(|panel| panel.id()), Some(id));

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("tasks is a feature plugin");
        assert!(!ui.has(id), "disabling the plugin takes the panel with it");
        assert!(ui.open(id, args()).is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(ui.has(id));
        assert!(ui.open(id, args()).is_some());
    }

    /// A payload of the wrong type declines rather than panicking, which is
    /// the contract every seat factory answers to.
    #[test]
    fn the_tasks_panel_declines_a_payload_it_does_not_recognise() {
        let (kernel, _registry) = boot();
        let ui: Arc<UiSeat> = kernel
            .context()
            .get::<UiSeatService>()
            .expect("the seat is on the root");
        assert!(ui
            .open(rebon_ui_seat::ids::dialog::TASKS, DialogArgs::payload(7u32))
            .is_none());
        assert!(ui
            .open(rebon_ui_seat::ids::dialog::TASKS, DialogArgs::none())
            .is_none());
    }

    #[test]
    fn the_switch_takes_the_three_commands_off_the_seat_and_puts_them_back() {
        let (kernel, registry) = boot();
        let commands: Arc<CommandSeat> = kernel
            .context()
            .get::<CommandSeatService>()
            .expect("the seat is on the root");
        let names = ["tasks", "workflows", "teams"];

        for name in names {
            let found = commands.find(name).expect("registered while loaded");
            assert!(
                matches!(&found.handler, CommandHandler::Native(id) if id == name),
                "/{name} is the front end's to run"
            );
        }
        // `/bg` is an alias of `/tasks`, and goes off with it.
        assert!(commands.find("bg").is_some());

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("tasks is a feature plugin");
        for name in names.iter().chain(["bg"].iter()) {
            assert!(
                commands.find(name).is_none(),
                "disabling the plugin takes /{name} off the seat"
            );
        }

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        for name in names {
            assert!(commands.find(name).is_some(), "/{name} is back on the seat");
        }
    }

    #[test]
    fn the_switch_takes_the_tools_off_the_seat_and_puts_them_back() {
        let (kernel, registry) = boot();
        let seat = seat(&kernel);

        assert_eq!(TOOL_NAMES.len(), 13);
        assert_eq!(DEFAULT_ON.len(), 12);
        for name in DEFAULT_ON {
            assert!(
                seat.resolve(name, None).unwrap().is_some(),
                "{name} resolves while tasks is loaded"
            );
        }

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("tasks is a feature plugin");
        for name in TOOL_NAMES {
            assert!(
                seat.resolve(name, None).unwrap().is_none(),
                "disabling the plugin takes {name} off the seat"
            );
        }

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        for name in DEFAULT_ON {
            assert!(
                seat.resolve(name, None).unwrap().is_some(),
                "{name} is back on the seat"
            );
        }
    }

    #[test]
    fn disabling_the_plugin_keeps_its_runtime_state_alive() {
        let (kernel, plugin_registry) = boot();
        let task_registry = runtime::TaskRegistry::new();
        let task_id = runtime::TaskId::new("runtime-survives-switch");
        let mut snapshot = runtime::TaskSnapshot::new_pending(
            task_id.clone(),
            "runtime state".into(),
            runtime::TaskData::MonitorMcp(runtime::MonitorMcpData {
                server_name: "test".into(),
                description: "runtime state".into(),
            }),
        );
        snapshot.status = runtime::TaskStatus::Running;
        task_registry.insert(task_id.clone(), snapshot, rebon_types::PromptCancel::new());

        plugin_registry
            .set_enabled(PLUGIN_ID, false)
            .expect("tasks is a feature plugin");
        assert_eq!(
            task_registry.snapshot(&task_id).unwrap().status,
            runtime::TaskStatus::Running
        );
        assert!(seat(&kernel)
            .resolve(TASK_LIST_TOOL_NAME, None)
            .unwrap()
            .is_none());

        plugin_registry
            .set_enabled(PLUGIN_ID, true)
            .expect("tasks plugin can be restored");
        assert_eq!(
            task_registry.snapshot(&task_id).unwrap().status,
            runtime::TaskStatus::Running
        );
        assert!(seat(&kernel)
            .resolve(TASK_LIST_TOOL_NAME, None)
            .unwrap()
            .is_some());
    }

    #[test]
    fn task_registry_service_follows_plugin_availability_without_losing_state() {
        let (kernel, plugin_registry) = boot();
        let session = kernel.context().fork_scoped("session/test");
        let task_registry = Arc::new(runtime::TaskRegistry::new());
        provide_task_registry(&session, task_registry.clone())
            .expect("the session accepts its task registry");
        kernel.context().emit(&rebon_kernel::SessionOpened {
            session_id: "session-test".into(),
            ctx: session.clone(),
        });

        let resolved = require_task_registry(&session).expect("tasks plugin enables consumption");
        assert!(Arc::ptr_eq(&resolved, &task_registry));
        let task_id = runtime::TaskId::new("runtime-survives-switch");
        let mut snapshot = runtime::TaskSnapshot::new_pending(
            task_id.clone(),
            "runtime state".into(),
            runtime::TaskData::MonitorMcp(runtime::MonitorMcpData {
                server_name: "test".into(),
                description: "runtime state".into(),
            }),
        );
        snapshot.status = runtime::TaskStatus::Running;
        resolved.insert(task_id.clone(), snapshot, rebon_types::PromptCancel::new());

        plugin_registry
            .set_enabled(PLUGIN_ID, false)
            .expect("tasks is a feature plugin");
        assert!(require_task_registry(&session).is_err());
        assert_eq!(
            resolved
                .snapshot(&task_id)
                .expect("held Arc stays legal")
                .status,
            runtime::TaskStatus::Running
        );

        plugin_registry
            .set_enabled(PLUGIN_ID, true)
            .expect("tasks plugin can be restored");
        let restored = require_task_registry(&session).expect("consumption is restored");
        assert!(Arc::ptr_eq(&restored, &task_registry));
        assert_eq!(
            restored
                .snapshot(&task_id)
                .expect("state survives the switch")
                .status,
            runtime::TaskStatus::Running
        );
    }

    /// All three producers go off and on with the tools, which is the point:
    /// a reminder to use tools that left the seat would be worse than
    /// silence, and a mailbox with no `SendMessage` to answer it is a dead
    /// end.
    ///
    /// They come back in **rung** order — roster, mailbox, nudge — whatever
    /// order `apply` registered them in.
    #[test]
    fn the_switch_takes_every_producer_off_the_attachment_seat_and_puts_them_back() {
        let (kernel, registry) = boot();
        let attachment_seat = kernel
            .context()
            .get::<AttachmentSeatService>()
            .expect("the attachment seat is on the root");

        let all = vec![
            ROSTER_PROVIDER_ID.to_string(),
            MAILBOX_PROVIDER_ID.to_string(),
            REMINDER_PROVIDER_ID.to_string(),
        ];
        assert_eq!(attachment_seat.provider_ids(), all);

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("tasks is a feature plugin");
        assert!(attachment_seat.provider_ids().is_empty());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert_eq!(attachment_seat.provider_ids(), all);
    }

    /// The thirteenth tool. In a non-interactive session the halves swap:
    /// `TodoWrite` is the live one and the five `Task*` tools are not. The
    /// plugin switch still governs both halves.
    #[test]
    fn the_switch_reaches_todo_write_too() {
        let _env = EnvGuard::set("REBON_NON_INTERACTIVE", "1");
        let (kernel, registry) = boot();
        let seat = seat(&kernel);

        assert!(
            seat.resolve(TODO_WRITE_TOOL_NAME, None).unwrap().is_some(),
            "TodoWrite is the live half when todo-v2 is off"
        );
        assert!(
            seat.resolve(TASK_CREATE_TOOL_NAME, None).unwrap().is_none(),
            "and the Task tools are the gated half"
        );

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("tasks is a feature plugin");
        assert!(seat.resolve(TODO_WRITE_TOOL_NAME, None).unwrap().is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(seat.resolve(TODO_WRITE_TOOL_NAME, None).unwrap().is_some());
    }

    /// `rebon-tool` pins the kinded builtins it still owns against the shared
    /// facts table; the six `ToolKind::Task` tools moved here, so this crate
    /// pins them. Without it, a name or kind could drift on one side
    /// unnoticed.
    #[test]
    fn the_kinded_tools_match_the_shared_facts_table() {
        let kinded: Vec<Arc<dyn Tool>> = vec![
            Arc::new(TaskCreateTool) as Arc<dyn Tool>,
            Arc::new(TaskGetTool),
            Arc::new(TaskListTool),
            Arc::new(TaskUpdateTool),
            Arc::new(TaskStopTool),
            Arc::new(TodoWriteTool),
        ];
        for tool in kinded {
            assert_eq!(
                tool.kind(),
                rebon_tools_core::ToolKind::Task,
                "{}",
                tool.id().as_str()
            );
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
