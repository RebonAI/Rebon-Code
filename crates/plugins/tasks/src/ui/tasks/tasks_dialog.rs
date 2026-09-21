//! Background tasks dialog reducer.
//!
//! Side-effecting work — killing a task, pausing one, entering a
//! teammate view — is left to the runtime. What lives here is the pure
//! part:
//!
//! 1. The list/detail mode dispatcher ([`DialogMode`], `selected_index`).
//! 2. The grouping reducer ([`TasksDialogLayout::bash`],
//!    [`TasksDialogLayout::remote`], …) plus the
//!    [`TasksDialogLayout::all_selectable`] ordering.
//! 3. The "skip list on mount" rule (open in detail when there's
//!    exactly one task or an explicit initial detail task id).
//! 4. The keyboard reducer (`x`, `f`, `←`, navigation, enter).
//! 5. The subtitle / actions builders.
//!
//! The side effects are exposed as [`TasksDialogAction`] variants for
//! the consumer to perform.

use crate::ui::tasks::common::{TaskKind, TaskStatus};

/// Pre-built input shape for one selectable item in the dialog.
#[derive(Debug, Clone)]
pub struct DialogItemInput {
    /// The task id.
    pub id: String,
    /// The task kind.
    pub kind: TaskKind,
    /// The task's status.
    pub status: TaskStatus,
    /// Task start time in milliseconds, used for the per-bucket sort.
    pub start_time_ms: u64,
    /// User-facing label string, already computed for this task's kind.
    pub label: String,
}

/// Pre-built input for [`build_dialog_layout`].
#[derive(Debug, Clone)]
pub struct TasksDialogInput {
    /// All tasks that are background tasks. The reducer will then drop
    /// the foregrounded local-agent task and apply
    /// any spinner-tree filtering.
    pub tasks: Vec<DialogItemInput>,
    /// The foregrounded task's id, or `None` if none is foregrounded.
    pub foregrounded_task_id: Option<String>,
    /// Whether the teammate spinner tree is expanded.
    pub show_spinner_tree: bool,
}

/// One section of the dialog. Sections render in a fixed order and
/// empty ones are skipped; the consumer iterates over these to build
/// the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogSection {
    /// Title displayed before the list of items (e.g. `"Agents"`).
    pub title: Option<String>,
    /// Items in the section.
    pub items: Vec<DialogItem>,
}

/// One item in a section. Includes the synthetic `leader` entry that
/// Inserted above teammate tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogItem {
    /// Stable id (`__leader__` for the synthetic leader entry).
    pub id: String,
    /// Task kind discriminant — `None` for the leader entry.
    pub kind: Option<TaskKind>,
    /// Status string (`"running"`, etc.).
    pub status: TaskStatus,
    /// Display label.
    pub label: String,
}

/// Layout result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TasksDialogLayout {
    /// Teammates (with synthetic leader prepended when teammates are
    /// non-empty). Empty when `show_spinner_tree` is true.
    pub teammates: Vec<DialogItem>,
    /// Local-bash items.
    pub bash: Vec<DialogItem>,
    /// MCP monitors.
    pub monitor_mcp: Vec<DialogItem>,
    /// Remote agents.
    pub remote: Vec<DialogItem>,
    /// Local agents (foregrounded one excluded).
    pub local_agent: Vec<DialogItem>,
    /// Local workflows.
    pub workflows: Vec<DialogItem>,
    /// Dream tasks.
    pub dreams: Vec<DialogItem>,
    /// Flat list in selection order (teammates → bash → monitor MCP →
    /// remote → agent → workflows → dream).
    pub all_selectable: Vec<DialogItem>,
}

/// Synthetic id for the leader row.
pub const LEADER_ID: &str = "__leader__";

/// Build the dialog layout from a list of background tasks.
///
/// Sort rule: running tasks first, then by descending start time.
/// Within each bucket the relative order is preserved.
pub fn build_dialog_layout(input: &TasksDialogInput, leader_label: &str) -> TasksDialogLayout {
    let mut sorted: Vec<DialogItem> = input
        .tasks
        .iter()
        .map(|t| DialogItem {
            id: t.id.clone(),
            kind: Some(t.kind),
            status: t.status,
            label: t.label.clone(),
        })
        .collect();
    let start_times: std::collections::HashMap<&str, u64> = input
        .tasks
        .iter()
        .map(|t| (t.id.as_str(), t.start_time_ms))
        .collect();
    sorted.sort_by(|a, b| {
        let a_run = a.status == TaskStatus::Running;
        let b_run = b.status == TaskStatus::Running;
        if a_run && !b_run {
            return std::cmp::Ordering::Less;
        }
        if !a_run && b_run {
            return std::cmp::Ordering::Greater;
        }
        let a_t = start_times.get(a.id.as_str()).copied().unwrap_or(0);
        let b_t = start_times.get(b.id.as_str()).copied().unwrap_or(0);
        b_t.cmp(&a_t)
    });

    let by_kind = |k: TaskKind| -> Vec<DialogItem> {
        sorted
            .iter()
            .filter(|i| i.kind == Some(k))
            .cloned()
            .collect()
    };

    // A monitor is a shell command with a description in front of it, and
    // the runtime splits the two only so it can spawn them differently. One
    // bash bucket holds both, which is what the reader sees.
    let bash: Vec<DialogItem> = sorted
        .iter()
        .filter(|i| matches!(i.kind, Some(TaskKind::LocalShell) | Some(TaskKind::Monitor)))
        .cloned()
        .collect();
    let remote = by_kind(TaskKind::RemoteAgent);
    let local_agent: Vec<DialogItem> = sorted
        .iter()
        .filter(|i| {
            i.kind == Some(TaskKind::LocalAgent)
                && Some(&i.id) != input.foregrounded_task_id.as_ref()
        })
        .cloned()
        .collect();
    let workflows = by_kind(TaskKind::LocalWorkflow);
    let monitor_mcp = by_kind(TaskKind::MonitorMcp);
    let dreams = by_kind(TaskKind::Dream);

    let teammates_only: Vec<DialogItem> = if input.show_spinner_tree {
        Vec::new()
    } else {
        by_kind(TaskKind::InProcessTeammate)
    };

    // Synthetic leader prepended when teammates are non-empty.
    let leader_item = if !teammates_only.is_empty() {
        Some(DialogItem {
            id: LEADER_ID.to_owned(),
            kind: None,
            status: TaskStatus::Running,
            label: format!("@{leader_label}"),
        })
    } else {
        None
    };

    let mut teammates: Vec<DialogItem> = Vec::new();
    if let Some(l) = leader_item.clone() {
        teammates.push(l);
    }
    teammates.extend(teammates_only.iter().cloned());

    // Build the flat all_selectable list. Order: teammates → bash →
    // monitor MCP → remote → agent → workflows → dream.
    let mut all_selectable: Vec<DialogItem> = Vec::new();
    all_selectable.extend(teammates.iter().cloned());
    all_selectable.extend(bash.iter().cloned());
    all_selectable.extend(monitor_mcp.iter().cloned());
    all_selectable.extend(remote.iter().cloned());
    all_selectable.extend(local_agent.iter().cloned());
    all_selectable.extend(workflows.iter().cloned());
    all_selectable.extend(dreams.iter().cloned());

    TasksDialogLayout {
        teammates,
        bash,
        monitor_mcp,
        remote,
        local_agent,
        workflows,
        dreams,
        all_selectable,
    }
}

/// Dialog mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogMode {
    /// List mode — selecting from [`TasksDialogLayout::all_selectable`].
    List,
    /// Detail mode — viewing the task with the given id.
    Detail {
        /// The id of the task being viewed.
        item_id: String,
    },
}

/// Compute the initial dialog mode.
///
/// Returns `(mode, skipped_list_on_mount)`. The flag is used by
/// [`should_close_after_back`] to decide whether to fall through to closing the
/// dialog.
pub fn initial_dialog_mode(
    initial_detail_task_id: Option<&str>,
    selectable: &[DialogItem],
) -> (DialogMode, bool) {
    if let Some(id) = initial_detail_task_id {
        return (
            DialogMode::Detail {
                item_id: id.to_owned(),
            },
            true,
        );
    }
    if selectable.len() == 1 {
        return (
            DialogMode::Detail {
                item_id: selectable[0].id.clone(),
            },
            true,
        );
    }
    (DialogMode::List, false)
}

/// Result of a navigation event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavigationOutcome {
    /// Updated selected index, clamped to `[0, total)`.
    pub selected_index: usize,
}

/// Move the selection up. Clamps at 0.
pub fn nav_previous(selected_index: usize) -> NavigationOutcome {
    NavigationOutcome {
        selected_index: selected_index.saturating_sub(1),
    }
}

/// Move the selection down. Clamps at `total - 1` (or 0 if total == 0).
pub fn nav_next(selected_index: usize, total: usize) -> NavigationOutcome {
    if total == 0 {
        return NavigationOutcome { selected_index: 0 };
    }
    NavigationOutcome {
        selected_index: (selected_index + 1).min(total - 1),
    }
}

/// Keyboard event accepted by the dialog reducer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TasksDialogEvent {
    /// `←` — close the dialog.
    Left,
    /// `x` — pause/preserve the selected running task.
    XKey,
    /// `f` — foreground (teammate) or back-to-leader.
    FKey,
    /// `enter` — view the selected task.
    Enter,
}

/// Action emitted by the reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TasksDialogAction {
    /// Dismiss the dialog and report the result.
    Dismiss(String),
    /// Switch to detail mode for the given item.
    OpenDetail(String),
    /// Issue a pause/preserve request against the given task.
    PausePreserve {
        /// Task id to pause/preserve.
        id: String,
        /// Task kind for routing.
        kind: TaskKind,
    },
    /// Issue an enter-teammate-view side effect.
    EnterTeammateView(String),
    /// Issue an exit-teammate-view side effect (the leader entry).
    ExitTeammateView,
    /// Event ignored — the gating flag was off.
    Ignore,
}

/// Reducer for the background tasks dialog's keyboard events.
pub fn handle_tasks_dialog_event(
    mode: &DialogMode,
    selection: Option<&DialogItem>,
    event: TasksDialogEvent,
) -> TasksDialogAction {
    if !matches!(mode, DialogMode::List) {
        return TasksDialogAction::Ignore;
    }
    match event {
        TasksDialogEvent::Left => {
            TasksDialogAction::Dismiss("Background tasks dialog dismissed".into())
        }
        TasksDialogEvent::Enter => match selection {
            None => TasksDialogAction::Ignore,
            Some(item) => match item.kind {
                None => {
                    // Synthetic leader → exit teammate view + dismiss
                    TasksDialogAction::ExitTeammateView
                }
                Some(_) => TasksDialogAction::OpenDetail(item.id.clone()),
            },
        },
        TasksDialogEvent::XKey => match selection {
            None => TasksDialogAction::Ignore,
            Some(item) => {
                if item.status != TaskStatus::Running {
                    return TasksDialogAction::Ignore;
                }
                let Some(kind) = item.kind else {
                    return TasksDialogAction::Ignore;
                };
                if matches!(
                    kind,
                    TaskKind::LocalShell
                        | TaskKind::Monitor
                        | TaskKind::LocalAgent
                        | TaskKind::InProcessTeammate
                        | TaskKind::LocalWorkflow
                        | TaskKind::MonitorMcp
                        | TaskKind::Dream
                        | TaskKind::RemoteAgent
                ) {
                    TasksDialogAction::PausePreserve {
                        id: item.id.clone(),
                        kind,
                    }
                } else {
                    TasksDialogAction::Ignore
                }
            }
        },
        TasksDialogEvent::FKey => match selection {
            None => TasksDialogAction::Ignore,
            Some(item) => match item.kind {
                Some(TaskKind::InProcessTeammate) if item.status == TaskStatus::Running => {
                    TasksDialogAction::EnterTeammateView(item.id.clone())
                }
                None => TasksDialogAction::ExitTeammateView,
                _ => TasksDialogAction::Ignore,
            },
        },
    }
}

/// Decision returned by [`should_close_after_back`]. The actual state
/// mutation lives in the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackToListOutcome {
    /// Drop back into list mode.
    GoToList,
    /// Dismiss the dialog entirely.
    Close,
}

/// Decide what to do when the user hits `←` from a detail view.
pub fn should_close_after_back(
    skipped_list_on_mount: bool,
    selectable_count: usize,
) -> BackToListOutcome {
    if skipped_list_on_mount && selectable_count <= 1 {
        BackToListOutcome::Close
    } else {
        BackToListOutcome::GoToList
    }
}

/// Build the dialog subtitle.
///
/// Three counts: `running_teammates` ("agents"), `running_bash`
/// ("active shells"), `running_remote_or_agents` ("active agents").
/// Joined with ` · `, hidden when zero.
pub fn build_subtitle(
    running_teammates: u64,
    running_bash: u64,
    running_remote_or_agents: u64,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if running_teammates > 0 {
        let word = if running_teammates != 1 {
            "agents"
        } else {
            "agent"
        };
        parts.push(format!("{running_teammates} {word}"));
    }
    if running_bash > 0 {
        let word = if running_bash != 1 {
            "active shells"
        } else {
            "active shell"
        };
        parts.push(format!("{running_bash} {word}"));
    }
    if running_remote_or_agents > 0 {
        let word = if running_remote_or_agents != 1 {
            "active agents"
        } else {
            "active agent"
        };
        parts.push(format!("{running_remote_or_agents} {word}"));
    }
    parts.join(" · ")
}

/// Build the actions list shown in the dialog footer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DialogActions {
    /// `↑/↓ select` — always shown.
    pub show_select: bool,
    /// `Enter view` — always shown.
    pub show_view: bool,
    /// `f foreground` — shown when an in-process teammate is
    /// selected and running.
    pub show_foreground: bool,
    /// `x background` — shown when the selection can be preserved in
    /// the background while it keeps running.
    pub show_stop: bool,
    /// `…stop all agents` — shown when any local-agent task is
    /// running. The actual key chord is wired by the consumer.
    pub show_stop_all_agents: bool,
    /// `←/Esc close` — always shown.
    pub show_close: bool,
}

/// Build the actions shape from the current selection + the
/// running-agents flag.
pub fn build_actions(
    selection: Option<&DialogItem>,
    any_local_agent_running: bool,
) -> DialogActions {
    let mut a = DialogActions {
        show_select: true,
        show_view: true,
        show_close: true,
        ..DialogActions::default()
    };
    if let Some(item) = selection {
        if item.kind == Some(TaskKind::InProcessTeammate) && item.status == TaskStatus::Running {
            a.show_foreground = true;
        }
        if matches!(
            item.kind,
            Some(TaskKind::LocalShell)
                | Some(TaskKind::Monitor)
                | Some(TaskKind::LocalAgent)
                | Some(TaskKind::InProcessTeammate)
                | Some(TaskKind::LocalWorkflow)
                | Some(TaskKind::MonitorMcp)
                | Some(TaskKind::Dream)
                | Some(TaskKind::RemoteAgent)
        ) && item.status == TaskStatus::Running
        {
            a.show_stop = true;
        }
    }
    if any_local_agent_running {
        a.show_stop_all_agents = true;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, kind: TaskKind, status: TaskStatus, start: u64) -> DialogItemInput {
        DialogItemInput {
            id: id.into(),
            kind,
            status,
            start_time_ms: start,
            label: id.into(),
        }
    }

    #[test]
    fn layout_groups_by_kind() {
        let input = TasksDialogInput {
            tasks: vec![
                item("a", TaskKind::LocalShell, TaskStatus::Running, 1000),
                item("b", TaskKind::RemoteAgent, TaskStatus::Running, 1500),
                item("c", TaskKind::LocalAgent, TaskStatus::Running, 2000),
                item("d", TaskKind::Dream, TaskStatus::Running, 500),
            ],
            foregrounded_task_id: None,
            show_spinner_tree: false,
        };
        let layout = build_dialog_layout(&input, "leader");
        assert_eq!(layout.bash.len(), 1);
        assert_eq!(layout.remote.len(), 1);
        assert_eq!(layout.local_agent.len(), 1);
        assert_eq!(layout.dreams.len(), 1);
    }

    #[test]
    fn layout_running_first_then_descending_start_time() {
        let input = TasksDialogInput {
            tasks: vec![
                item(
                    "old-running",
                    TaskKind::LocalShell,
                    TaskStatus::Running,
                    100,
                ),
                item(
                    "new-running",
                    TaskKind::LocalShell,
                    TaskStatus::Running,
                    500,
                ),
                item(
                    "old-completed",
                    TaskKind::LocalShell,
                    TaskStatus::Completed,
                    50,
                ),
                item(
                    "new-completed",
                    TaskKind::LocalShell,
                    TaskStatus::Completed,
                    1000,
                ),
            ],
            foregrounded_task_id: None,
            show_spinner_tree: false,
        };
        let layout = build_dialog_layout(&input, "leader");
        let ids: Vec<&str> = layout.bash.iter().map(|i| i.id.as_str()).collect();
        // Running first (descending start), then non-running (descending start)
        assert_eq!(
            ids,
            vec![
                "new-running",
                "old-running",
                "new-completed",
                "old-completed"
            ]
        );
    }

    #[test]
    fn layout_drops_foregrounded_local_agent() {
        let input = TasksDialogInput {
            tasks: vec![
                item("fg", TaskKind::LocalAgent, TaskStatus::Running, 0),
                item("bg", TaskKind::LocalAgent, TaskStatus::Running, 0),
            ],
            foregrounded_task_id: Some("fg".into()),
            show_spinner_tree: false,
        };
        let layout = build_dialog_layout(&input, "leader");
        let ids: Vec<&str> = layout.local_agent.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["bg"]);
    }

    #[test]
    fn layout_spinner_tree_hides_teammates() {
        let input = TasksDialogInput {
            tasks: vec![item(
                "t1",
                TaskKind::InProcessTeammate,
                TaskStatus::Running,
                0,
            )],
            foregrounded_task_id: None,
            show_spinner_tree: true,
        };
        let layout = build_dialog_layout(&input, "leader");
        assert!(layout.teammates.is_empty());
    }

    #[test]
    fn layout_leader_inserted_when_teammates_present() {
        let input = TasksDialogInput {
            tasks: vec![item(
                "t1",
                TaskKind::InProcessTeammate,
                TaskStatus::Running,
                0,
            )],
            foregrounded_task_id: None,
            show_spinner_tree: false,
        };
        let layout = build_dialog_layout(&input, "leader");
        assert_eq!(layout.teammates[0].id, LEADER_ID);
        assert_eq!(layout.teammates[0].label, "@leader");
        assert!(layout.teammates[0].kind.is_none());
        assert_eq!(layout.teammates[1].id, "t1");
    }

    #[test]
    fn layout_no_leader_when_no_teammates() {
        let input = TasksDialogInput {
            tasks: vec![item("a", TaskKind::LocalShell, TaskStatus::Running, 0)],
            foregrounded_task_id: None,
            show_spinner_tree: false,
        };
        let layout = build_dialog_layout(&input, "leader");
        assert!(layout.teammates.is_empty());
    }

    #[test]
    fn a_monitor_lands_in_the_bash_bucket_beside_a_shell() {
        // The runtime spawns a monitor differently from a plain shell
        // command, and used to be flattened onto one `LocalBash` before it
        // reached this builder. Now that both halves read one enum, the
        // fold lives here — a monitor that fell out of the bash bucket
        // would vanish from `/tasks` entirely.
        let input = TasksDialogInput {
            tasks: vec![
                item("shell", TaskKind::LocalShell, TaskStatus::Running, 10),
                item("monitor", TaskKind::Monitor, TaskStatus::Running, 20),
            ],
            foregrounded_task_id: None,
            show_spinner_tree: false,
        };
        let layout = build_dialog_layout(&input, "lead");
        let bash: Vec<&str> = layout.bash.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(bash, vec!["monitor", "shell"], "newest running first");
        assert_eq!(layout.all_selectable.len(), 2);
    }

    #[test]
    fn a_running_monitor_can_be_stopped_like_a_shell() {
        let item = list_mode_item(TaskKind::Monitor, TaskStatus::Running);
        assert!(build_actions(Some(&item), false).show_stop);
        assert!(matches!(
            handle_tasks_dialog_event(&DialogMode::List, Some(&item), TasksDialogEvent::XKey),
            TasksDialogAction::PausePreserve { .. }
        ));
    }

    #[test]
    fn layout_all_selectable_order() {
        let input = TasksDialogInput {
            tasks: vec![
                item("bash", TaskKind::LocalShell, TaskStatus::Running, 0),
                item(
                    "teammate",
                    TaskKind::InProcessTeammate,
                    TaskStatus::Running,
                    0,
                ),
                item("dream", TaskKind::Dream, TaskStatus::Running, 0),
            ],
            foregrounded_task_id: None,
            show_spinner_tree: false,
        };
        let layout = build_dialog_layout(&input, "lead");
        let ids: Vec<&str> = layout
            .all_selectable
            .iter()
            .map(|i| i.id.as_str())
            .collect();
        // teammates(__leader__, teammate) → bash → monitor → remote → agent → workflows → dream
        assert_eq!(ids, vec!["__leader__", "teammate", "bash", "dream"]);
    }

    #[test]
    fn initial_mode_with_explicit_id() {
        let (mode, skipped) = initial_dialog_mode(Some("xyz"), &[]);
        assert_eq!(
            mode,
            DialogMode::Detail {
                item_id: "xyz".into()
            }
        );
        assert!(skipped);
    }

    #[test]
    fn initial_mode_one_item_skips_to_detail() {
        let items = vec![DialogItem {
            id: "only".into(),
            kind: Some(TaskKind::LocalShell),
            status: TaskStatus::Running,
            label: "only".into(),
        }];
        let (mode, skipped) = initial_dialog_mode(None, &items);
        assert_eq!(
            mode,
            DialogMode::Detail {
                item_id: "only".into()
            }
        );
        assert!(skipped);
    }

    #[test]
    fn initial_mode_zero_or_many_starts_in_list() {
        let (mode, skipped) = initial_dialog_mode(None, &[]);
        assert_eq!(mode, DialogMode::List);
        assert!(!skipped);

        let items = vec![
            DialogItem {
                id: "a".into(),
                kind: Some(TaskKind::LocalShell),
                status: TaskStatus::Running,
                label: "a".into(),
            },
            DialogItem {
                id: "b".into(),
                kind: Some(TaskKind::LocalShell),
                status: TaskStatus::Running,
                label: "b".into(),
            },
        ];
        let (mode, skipped) = initial_dialog_mode(None, &items);
        assert_eq!(mode, DialogMode::List);
        assert!(!skipped);
    }

    #[test]
    fn nav_clamps_at_zero() {
        assert_eq!(nav_previous(0).selected_index, 0);
        assert_eq!(nav_previous(3).selected_index, 2);
    }

    #[test]
    fn nav_clamps_at_total() {
        assert_eq!(nav_next(2, 5).selected_index, 3);
        assert_eq!(nav_next(4, 5).selected_index, 4);
        assert_eq!(nav_next(0, 0).selected_index, 0);
    }

    fn list_mode_item(kind: TaskKind, status: TaskStatus) -> DialogItem {
        DialogItem {
            id: "t".into(),
            kind: Some(kind),
            status,
            label: "t".into(),
        }
    }

    #[test]
    fn handle_event_left_dismisses_in_list_mode() {
        let action = handle_tasks_dialog_event(&DialogMode::List, None, TasksDialogEvent::Left);
        assert!(matches!(action, TasksDialogAction::Dismiss(_)));
    }

    #[test]
    fn handle_event_left_ignored_in_detail_mode() {
        let action = handle_tasks_dialog_event(
            &DialogMode::Detail {
                item_id: "x".into(),
            },
            None,
            TasksDialogEvent::Left,
        );
        assert_eq!(action, TasksDialogAction::Ignore);
    }

    #[test]
    fn handle_event_x_running_pauses_supported_kinds() {
        for k in [
            TaskKind::LocalShell,
            TaskKind::Monitor,
            TaskKind::LocalAgent,
            TaskKind::InProcessTeammate,
            TaskKind::LocalWorkflow,
            TaskKind::MonitorMcp,
            TaskKind::Dream,
            TaskKind::RemoteAgent,
        ] {
            let item = list_mode_item(k, TaskStatus::Running);
            let action =
                handle_tasks_dialog_event(&DialogMode::List, Some(&item), TasksDialogEvent::XKey);
            assert!(matches!(action, TasksDialogAction::PausePreserve { .. }));
        }
    }

    #[test]
    fn handle_event_x_not_running_ignored() {
        let item = list_mode_item(TaskKind::LocalShell, TaskStatus::Completed);
        let action =
            handle_tasks_dialog_event(&DialogMode::List, Some(&item), TasksDialogEvent::XKey);
        assert_eq!(action, TasksDialogAction::Ignore);
    }

    #[test]
    fn handle_event_f_teammate_running_enters_view() {
        let item = list_mode_item(TaskKind::InProcessTeammate, TaskStatus::Running);
        let action =
            handle_tasks_dialog_event(&DialogMode::List, Some(&item), TasksDialogEvent::FKey);
        assert_eq!(action, TasksDialogAction::EnterTeammateView("t".into()));
    }

    #[test]
    fn handle_event_f_leader_exits_view() {
        let item = DialogItem {
            id: LEADER_ID.into(),
            kind: None,
            status: TaskStatus::Running,
            label: "@leader".into(),
        };
        let action =
            handle_tasks_dialog_event(&DialogMode::List, Some(&item), TasksDialogEvent::FKey);
        assert_eq!(action, TasksDialogAction::ExitTeammateView);
    }

    #[test]
    fn handle_event_enter_opens_detail() {
        let item = list_mode_item(TaskKind::LocalShell, TaskStatus::Running);
        let action =
            handle_tasks_dialog_event(&DialogMode::List, Some(&item), TasksDialogEvent::Enter);
        assert_eq!(action, TasksDialogAction::OpenDetail("t".into()));
    }

    #[test]
    fn handle_event_enter_on_leader_exits_view() {
        let item = DialogItem {
            id: LEADER_ID.into(),
            kind: None,
            status: TaskStatus::Running,
            label: "@leader".into(),
        };
        let action =
            handle_tasks_dialog_event(&DialogMode::List, Some(&item), TasksDialogEvent::Enter);
        assert_eq!(action, TasksDialogAction::ExitTeammateView);
    }

    #[test]
    fn back_to_list_close_when_skipped_and_one_or_zero() {
        assert_eq!(should_close_after_back(true, 0), BackToListOutcome::Close);
        assert_eq!(should_close_after_back(true, 1), BackToListOutcome::Close);
    }

    #[test]
    fn back_to_list_go_to_list_when_skipped_but_more_items() {
        assert_eq!(
            should_close_after_back(true, 2),
            BackToListOutcome::GoToList
        );
    }

    #[test]
    fn back_to_list_go_to_list_when_not_skipped() {
        assert_eq!(
            should_close_after_back(false, 1),
            BackToListOutcome::GoToList
        );
    }

    #[test]
    fn subtitle_singular_plural() {
        assert_eq!(build_subtitle(1, 0, 0), "1 agent");
        assert_eq!(build_subtitle(3, 0, 0), "3 agents");
        assert_eq!(build_subtitle(0, 1, 0), "1 active shell");
        assert_eq!(build_subtitle(0, 5, 0), "5 active shells");
        assert_eq!(build_subtitle(0, 0, 1), "1 active agent");
        assert_eq!(build_subtitle(0, 0, 7), "7 active agents");
    }

    #[test]
    fn subtitle_joins_with_separator() {
        assert_eq!(
            build_subtitle(2, 1, 3),
            "2 agents · 1 active shell · 3 active agents"
        );
    }

    #[test]
    fn subtitle_empty_when_all_zero() {
        assert_eq!(build_subtitle(0, 0, 0), "");
    }

    #[test]
    fn actions_show_foreground_only_for_running_teammate() {
        let mut item = list_mode_item(TaskKind::InProcessTeammate, TaskStatus::Running);
        let actions = build_actions(Some(&item), false);
        assert!(actions.show_foreground);
        assert!(actions.show_stop);

        item.status = TaskStatus::Completed;
        let actions = build_actions(Some(&item), false);
        assert!(!actions.show_foreground);
        assert!(!actions.show_stop);

        let bash = list_mode_item(TaskKind::LocalShell, TaskStatus::Running);
        let actions = build_actions(Some(&bash), false);
        assert!(!actions.show_foreground);
        assert!(actions.show_stop);
    }

    #[test]
    fn actions_show_stop_all_agents_when_running_local_agent_anywhere() {
        let bash = list_mode_item(TaskKind::LocalShell, TaskStatus::Running);
        let actions = build_actions(Some(&bash), true);
        assert!(actions.show_stop_all_agents);
    }

    #[test]
    fn actions_always_have_select_view_close() {
        let actions = build_actions(None, false);
        assert!(actions.show_select);
        assert!(actions.show_view);
        assert!(actions.show_close);
    }
}
