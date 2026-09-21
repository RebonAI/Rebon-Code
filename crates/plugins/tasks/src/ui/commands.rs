//! The three commands that open this plugin's surfaces.
//!
//! `/tasks`, `/workflows` and `/teams` each open a dialog whose state and
//! reducer live next door in [`crate::ui`], over data only the task runtime
//! has. Turning `plugins.tasks.enabled` off takes the thirteen tools and
//! every one of those surfaces with it, so these three have to go too —
//! which is why they are registered here on the command seat rather than
//! listed in `rebon-slash-commands`'s built-in table.
//!
//! The handler stays `CommandHandler::Native`: opening a dialog means
//! writing into the front end's own state, so the front end runs it, keyed
//! by the command's name. Nothing here reaches a terminal.

use rebon_command_seat::{Category, CommandKind, CommandSpec, Surfaces};

/// Surfaces `/tasks` and `/workflows` answer on: the terminal, the desktop
/// app and the web page all have a background-task panel.
const LOCAL_WEB: Surfaces = Surfaces::LOCAL.with(Surfaces::WEB);

/// `/tasks` — the background-task dialog.
///
/// `/bg` is an alias here rather than a command of its own: it means "show
/// me the list" on the desktop, and a spelling that shows a list in one
/// front end must not move a session in the other.
pub fn tasks_command_spec() -> CommandSpec {
    CommandSpec::new("tasks", "Show background jobs and sub-agents")
        .aliases(["bg"])
        .zh_aliases(["任务", "后台任务"])
        .category(Category::Agent)
        .surfaces(LOCAL_WEB)
        .kind(CommandKind::Panel)
}

/// `/workflows` — the same dialog, filtered to this session's workflow runs.
pub fn workflows_command_spec() -> CommandSpec {
    CommandSpec::new("workflows", "Show workflows launched in this session")
        .zh_aliases(["工作流"])
        .surfaces(LOCAL_WEB)
        .kind(CommandKind::Panel)
}

/// `/teams` — the teammate overlay. Terminal only: it drives tmux panes and
/// the in-process teammate registry, and no other front end has a surface
/// for either.
pub fn teams_command_spec() -> CommandSpec {
    CommandSpec::new("teams", "Show team tasks")
        .category(Category::Agent)
        .surfaces(Surfaces::TUI_ONLY)
        .kind(CommandKind::Panel)
}

/// All three, in registration order.
pub fn command_specs() -> Vec<CommandSpec> {
    vec![
        tasks_command_spec(),
        workflows_command_spec(),
        teams_command_spec(),
    ]
}
