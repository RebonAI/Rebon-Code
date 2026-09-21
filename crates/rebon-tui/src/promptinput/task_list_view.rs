//! Task-list layout, truncation and sorting.
//!
//! This module holds the pure layout/truncation/sorting logic of the task
//! list:
//!
//! * The [`max_display_rows`] calculation from terminal rows
//! * Priority-based sorting (recently completed > in-progress > pending > older completed)
//! * Visible vs hidden task partitioning
//! * Hidden summary text generation (` … +X in progress, Y pending, Z completed`)
//! * Task icon/status mapping
//! * Task display metrics (owner width, subject width, activity width)
//!
//! The rendering is out of scope. The caller
//! supplies terminal dimensions and task data; this module returns a
//! plain-data layout plan.

use std::cmp;
use std::collections::HashSet;

/// The task-list row and its status vocabulary, shared with every other
/// surface that renders the list.
pub use rebon_types::{ListTask, TaskListStatus};

/// Icon and color resolved for a task status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskIcon {
    /// Unicode icon character.
    pub icon: &'static str,
    /// Theme color key, or `None` for default.
    pub color: Option<&'static str>,
}

/// Returns the icon and color for a task status.
pub fn task_icon(status: TaskListStatus) -> TaskIcon {
    match status {
        TaskListStatus::Completed => TaskIcon {
            icon: "\u{2714}", // ✔ HEAVY CHECK MARK
            color: Some("success"),
        },
        TaskListStatus::InProgress => TaskIcon {
            icon: "\u{25A0}", // ■ (BLACK SQUARE)
            color: Some("rebon"),
        },
        TaskListStatus::Pending => TaskIcon {
            icon: "\u{25A1}", // □ (WHITE SQUARE)
            color: None,
        },
    }
}

/// TTL for recently-completed tasks that stay visible during truncation.
pub const RECENT_COMPLETED_TTL_MS: u64 = 30_000;

/// Compute the maximum number of task rows to display: 0 at 10 terminal
/// rows or fewer, otherwise `rows - 14` clamped into `3..=10`.
pub fn max_display_rows(terminal_rows: u16) -> usize {
    if terminal_rows <= 10 {
        0
    } else {
        cmp::min(10, cmp::max(3, (terminal_rows as usize).saturating_sub(14)))
    }
}

/// One entry in the completion-timestamp map, supplied by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionTimestamp {
    /// Task id.
    pub id: String,
    /// Unix-epoch milliseconds when the task transitioned to completed.
    pub completed_at_ms: u64,
}

/// Input bag for the task-list layout resolver.
#[derive(Debug, Clone)]
pub struct TaskListLayoutInput<'a> {
    /// All tasks to consider.
    pub tasks: &'a [ListTask],
    /// Terminal rows (used for [`max_display_rows`]).
    pub terminal_rows: u16,
    /// Terminal columns (used for display metrics).
    pub terminal_columns: u16,
    /// Current time in milliseconds (used for recent-completion TTL).
    pub now_ms: u64,
    /// Completion timestamps tracked by the caller across renders.
    pub completion_timestamps: &'a [CompletionTimestamp],
}

/// Counts of tasks by status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TaskCounts {
    /// Number of completed tasks.
    pub completed: usize,
    /// Number of in-progress tasks.
    pub in_progress: usize,
    /// Number of pending tasks.
    pub pending: usize,
    /// Total task count.
    pub total: usize,
}

/// Display metrics for a single task item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskItemLayout {
    /// The task this layout is for.
    pub task_id: String,
    /// Resolved icon.
    pub icon: TaskIcon,
    /// Whether the task is completed.
    pub is_completed: bool,
    /// Whether the task is in-progress.
    pub is_in_progress: bool,
    /// Whether the task has open blockers.
    pub is_blocked: bool,
    /// Open (unresolved) blocker IDs for this task.
    pub open_blockers: Vec<String>,
    /// Maximum width available for the subject text.
    pub max_subject_width: usize,
    /// Maximum width available for the activity line.
    pub max_activity_width: usize,
}

/// The hidden-tasks summary line (e.g. ` … +2 in progress, 3 pending`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HiddenSummary {
    /// Full text, empty if nothing is hidden.
    pub text: String,
    /// Number of hidden in-progress tasks.
    pub hidden_in_progress: usize,
    /// Number of hidden pending tasks.
    pub hidden_pending: usize,
    /// Number of hidden completed tasks.
    pub hidden_completed: usize,
}

/// Result of the task-list layout computation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskListLayout {
    /// Aggregate task counts.
    pub counts: TaskCounts,
    /// Layout for each visible task, in display order.
    pub visible: Vec<TaskItemLayout>,
    /// Summary of hidden tasks (empty if all fit).
    pub hidden_summary: HiddenSummary,
    /// Whether the list is currently truncated.
    pub is_truncated: bool,
    /// The max-display value used.
    pub max_display: usize,
}

/// Compute the full task-list layout plan.
///
/// This is the state-machine core of the task list. Given tasks, terminal
/// dimensions, and completion timestamps, it returns which tasks to show,
/// in what order, and the hidden summary text.
pub fn resolve_task_list_layout(input: &TaskListLayoutInput<'_>) -> TaskListLayout {
    let tasks = input.tasks;
    let max_display = max_display_rows(input.terminal_rows);

    // Task counts
    let completed_count = tasks
        .iter()
        .filter(|t| t.status == TaskListStatus::Completed)
        .count();
    let pending_count = tasks
        .iter()
        .filter(|t| t.status == TaskListStatus::Pending)
        .count();
    let in_progress_count = tasks.len() - completed_count - pending_count;
    let counts = TaskCounts {
        completed: completed_count,
        in_progress: in_progress_count,
        pending: pending_count,
        total: tasks.len(),
    };

    // Unresolved task IDs (non-completed)
    let unresolved_ids: HashSet<&str> = tasks
        .iter()
        .filter(|t| t.status != TaskListStatus::Completed)
        .map(|t| t.id.as_str())
        .collect();

    // Completion timestamp lookup
    let completion_map: std::collections::HashMap<&str, u64> = input
        .completion_timestamps
        .iter()
        .map(|ct| (ct.id.as_str(), ct.completed_at_ms))
        .collect();

    // A single task should stay visible even when tiny inline viewports
    // clamp the max display rows to 0: the task row costs the same height
    // as the hidden summary.
    let needs_truncation = tasks.len() > max_display && (max_display > 0 || tasks.len() > 1);

    let visible_tasks: Vec<&ListTask>;
    let hidden_tasks: Vec<&ListTask>;

    if needs_truncation {
        // Priority sort: recently completed > in-progress > pending (unblocked first) > older completed
        let mut recent_completed: Vec<&ListTask> = Vec::new();
        let mut older_completed: Vec<&ListTask> = Vec::new();
        for task in tasks
            .iter()
            .filter(|t| t.status == TaskListStatus::Completed)
        {
            if let Some(&ts) = completion_map.get(task.id.as_str()) {
                if input.now_ms.saturating_sub(ts) < RECENT_COMPLETED_TTL_MS {
                    recent_completed.push(task);
                    continue;
                }
            }
            older_completed.push(task);
        }
        recent_completed.sort_by(|a, b| by_id_asc(a, b));
        older_completed.sort_by(|a, b| by_id_asc(a, b));

        let mut in_progress: Vec<&ListTask> = tasks
            .iter()
            .filter(|t| t.status == TaskListStatus::InProgress)
            .collect();
        in_progress.sort_by(|a, b| by_id_asc(a, b));

        let mut pending: Vec<&ListTask> = tasks
            .iter()
            .filter(|t| t.status == TaskListStatus::Pending)
            .collect();
        pending.sort_by(|a, b| {
            let a_blocked = a
                .blocked_by
                .iter()
                .any(|id| unresolved_ids.contains(id.as_str()));
            let b_blocked = b
                .blocked_by
                .iter()
                .any(|id| unresolved_ids.contains(id.as_str()));
            if a_blocked != b_blocked {
                return if a_blocked {
                    cmp::Ordering::Greater
                } else {
                    cmp::Ordering::Less
                };
            }
            by_id_asc(a, b)
        });

        let mut prioritized: Vec<&ListTask> = Vec::with_capacity(tasks.len());
        prioritized.extend(recent_completed);
        prioritized.extend(in_progress);
        prioritized.extend(pending);
        prioritized.extend(older_completed);

        visible_tasks = prioritized[..cmp::min(max_display, prioritized.len())].to_vec();
        hidden_tasks = if max_display < prioritized.len() {
            prioritized[max_display..].to_vec()
        } else {
            Vec::new()
        };
    } else {
        let mut sorted: Vec<&ListTask> = tasks.iter().collect();
        sorted.sort_by(|a, b| by_id_asc(a, b));
        visible_tasks = sorted;
        hidden_tasks = Vec::new();
    };

    // Build hidden summary
    let hidden_summary = build_hidden_summary(&hidden_tasks);

    // Build item layouts
    let columns = input.terminal_columns as usize;
    let visible = visible_tasks
        .iter()
        .map(|task| {
            let open_blockers: Vec<String> = task
                .blocked_by
                .iter()
                .filter(|id| unresolved_ids.contains(id.as_str()))
                .cloned()
                .collect();
            let is_blocked = !open_blockers.is_empty();

            // Owner width is 0 here because we don't have the owner-active
            // flag in this pure-data layer. The caller resolves owner display
            // separately (it depends on live teammate state).
            let max_subject_width = cmp::max(15, columns.saturating_sub(15));
            let max_activity_width = cmp::max(15, columns.saturating_sub(15));

            TaskItemLayout {
                task_id: task.id.clone(),
                icon: task_icon(task.status),
                is_completed: task.status == TaskListStatus::Completed,
                is_in_progress: task.status == TaskListStatus::InProgress,
                is_blocked,
                open_blockers,
                max_subject_width,
                max_activity_width,
            }
        })
        .collect();

    TaskListLayout {
        counts,
        visible,
        hidden_summary,
        is_truncated: needs_truncation,
        max_display,
    }
}

/// Sort by task id: numerically when both ids parse as `u64`,
/// lexicographically otherwise.
fn by_id_asc(a: &ListTask, b: &ListTask) -> cmp::Ordering {
    let a_num = a.id.parse::<u64>();
    let b_num = b.id.parse::<u64>();
    match (a_num, b_num) {
        (Ok(an), Ok(bn)) => an.cmp(&bn),
        _ => a.id.cmp(&b.id),
    }
}

/// Builds the hidden summary text from hidden tasks.
fn build_hidden_summary(hidden: &[&ListTask]) -> HiddenSummary {
    if hidden.is_empty() {
        return HiddenSummary::default();
    }

    let hidden_pending = hidden
        .iter()
        .filter(|t| t.status == TaskListStatus::Pending)
        .count();
    let hidden_in_progress = hidden
        .iter()
        .filter(|t| t.status == TaskListStatus::InProgress)
        .count();
    let hidden_completed = hidden
        .iter()
        .filter(|t| t.status == TaskListStatus::Completed)
        .count();

    let mut parts: Vec<String> = Vec::new();
    if hidden_in_progress > 0 {
        parts.push(format!("{hidden_in_progress} in progress"));
    }
    if hidden_pending > 0 {
        parts.push(format!("{hidden_pending} pending"));
    }
    if hidden_completed > 0 {
        parts.push(format!("{hidden_completed} completed"));
    }

    HiddenSummary {
        text: format!(" \u{2026} +{}", parts.join(", ")),
        hidden_in_progress,
        hidden_pending,
        hidden_completed,
    }
}

/// Resolves whether a task owner should be displayed.
///
/// The owner is shown from 60 columns up, and only when one is set and
/// that owner is active.
pub fn should_show_owner(columns: u16, owner: Option<&str>, owner_active: bool) -> bool {
    columns >= 60 && owner.is_some() && owner_active
}

/// Computes the owner display width adjustment.
///
/// Width of the ` (@owner)` suffix: `4 + owner.len()`. The byte length is
/// the display width, which is exact for ASCII owners; callers needing
/// full Unicode support should supply their own width function.
pub fn owner_display_width(owner: &str) -> usize {
    // " (@" + owner + ")" = 4 + owner.len()
    4 + owner.len()
}

/// Adjusts `max_subject_width` when an owner is shown.
pub fn adjusted_subject_width(columns: u16, owner_width: usize) -> usize {
    cmp::max(15, (columns as usize).saturating_sub(15 + owner_width))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: TaskListStatus) -> ListTask {
        ListTask {
            id: id.into(),
            subject: format!("Task {id}"),
            status,
            owner: None,
            blocked_by: Vec::new(),
        }
    }

    fn task_with_blockers(id: &str, status: TaskListStatus, blocked_by: &[&str]) -> ListTask {
        ListTask {
            id: id.into(),
            subject: format!("Task {id}"),
            status,
            owner: None,
            blocked_by: blocked_by.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    // ---------------------------------------------------------------
    // max_display_rows
    // ---------------------------------------------------------------

    #[test]
    fn max_display_rows_returns_zero_for_tiny_terminals() {
        assert_eq!(max_display_rows(5), 0);
        assert_eq!(max_display_rows(10), 0);
    }

    #[test]
    fn max_display_rows_returns_min_three_for_small_terminals() {
        assert_eq!(max_display_rows(11), 3);
        assert_eq!(max_display_rows(16), 3);
    }

    #[test]
    fn max_display_rows_scales_with_terminal_height() {
        assert_eq!(max_display_rows(20), 6);
        assert_eq!(max_display_rows(24), 10);
        assert_eq!(max_display_rows(30), 10);
    }

    #[test]
    fn max_display_rows_caps_at_ten() {
        assert_eq!(max_display_rows(100), 10);
    }

    // ---------------------------------------------------------------
    // task_icon
    // ---------------------------------------------------------------

    #[test]
    fn task_icon_maps_status_to_correct_icon_and_color() {
        let completed = task_icon(TaskListStatus::Completed);
        assert_eq!(completed.color, Some("success"));

        let in_progress = task_icon(TaskListStatus::InProgress);
        assert_eq!(in_progress.color, Some("rebon"));

        let pending = task_icon(TaskListStatus::Pending);
        assert_eq!(pending.color, None);
    }

    // ---------------------------------------------------------------
    // by_id_asc
    // ---------------------------------------------------------------

    #[test]
    fn by_id_asc_sorts_numeric_ids_numerically() {
        let a = task("2", TaskListStatus::Pending);
        let b = task("10", TaskListStatus::Pending);
        assert_eq!(by_id_asc(&a, &b), cmp::Ordering::Less);
    }

    #[test]
    fn by_id_asc_falls_back_to_lexicographic_for_non_numeric() {
        let a = task("alpha", TaskListStatus::Pending);
        let b = task("beta", TaskListStatus::Pending);
        assert_eq!(by_id_asc(&a, &b), cmp::Ordering::Less);
    }

    // ---------------------------------------------------------------
    // resolve_task_list_layout - no truncation
    // ---------------------------------------------------------------

    #[test]
    fn layout_no_truncation_orders_by_id() {
        let tasks = vec![
            task("3", TaskListStatus::InProgress),
            task("1", TaskListStatus::Pending),
            task("2", TaskListStatus::Completed),
        ];
        let layout = resolve_task_list_layout(&TaskListLayoutInput {
            tasks: &tasks,
            terminal_rows: 30,
            terminal_columns: 80,
            now_ms: 100_000,
            completion_timestamps: &[],
        });

        assert!(!layout.is_truncated);
        assert_eq!(layout.counts.total, 3);
        assert_eq!(layout.counts.completed, 1);
        assert_eq!(layout.counts.in_progress, 1);
        assert_eq!(layout.counts.pending, 1);
        assert_eq!(
            layout
                .visible
                .iter()
                .map(|v| v.task_id.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "2", "3"]
        );
        assert!(layout.hidden_summary.text.is_empty());
    }

    #[test]
    fn layout_keeps_single_task_visible_on_tiny_terminals() {
        let tasks = vec![task("1", TaskListStatus::InProgress)];
        let layout = resolve_task_list_layout(&TaskListLayoutInput {
            tasks: &tasks,
            terminal_rows: 10,
            terminal_columns: 80,
            now_ms: 100_000,
            completion_timestamps: &[],
        });

        assert_eq!(layout.max_display, 0);
        assert!(!layout.is_truncated);
        assert_eq!(layout.visible.len(), 1);
        assert!(layout.hidden_summary.text.is_empty());
    }

    // ---------------------------------------------------------------
    // resolve_task_list_layout - truncation
    // ---------------------------------------------------------------

    #[test]
    fn layout_truncation_prioritizes_in_progress_over_old_completed() {
        // max_display for 17 rows = min(10, max(3, 17-14)) = 3
        let tasks = vec![
            task("1", TaskListStatus::Completed),
            task("2", TaskListStatus::Completed),
            task("3", TaskListStatus::InProgress),
            task("4", TaskListStatus::Pending),
            task("5", TaskListStatus::Pending),
        ];
        let layout = resolve_task_list_layout(&TaskListLayoutInput {
            tasks: &tasks,
            terminal_rows: 17,
            terminal_columns: 80,
            now_ms: 100_000,
            completion_timestamps: &[],
        });

        assert!(layout.is_truncated);
        assert_eq!(layout.max_display, 3);
        // Visible: in_progress(3), pending(4), pending(5)
        // Hidden: completed(1), completed(2)
        assert_eq!(
            layout
                .visible
                .iter()
                .map(|v| v.task_id.as_str())
                .collect::<Vec<_>>(),
            vec!["3", "4", "5"]
        );
        assert_eq!(layout.hidden_summary.hidden_completed, 2);
        assert!(layout.hidden_summary.text.contains("2 completed"));
    }

    #[test]
    fn layout_truncation_keeps_recently_completed_visible() {
        // max_display for 17 rows = 3
        let tasks = vec![
            task("1", TaskListStatus::Completed), // recently completed
            task("2", TaskListStatus::Completed), // old completed
            task("3", TaskListStatus::InProgress),
            task("4", TaskListStatus::Pending),
            task("5", TaskListStatus::Pending),
        ];
        let now_ms = 100_000;
        let completion_timestamps = vec![
            CompletionTimestamp {
                id: "1".into(),
                completed_at_ms: now_ms - 5_000, // 5s ago — within TTL
            },
            CompletionTimestamp {
                id: "2".into(),
                completed_at_ms: now_ms - 60_000, // 60s ago — expired
            },
        ];
        let layout = resolve_task_list_layout(&TaskListLayoutInput {
            tasks: &tasks,
            terminal_rows: 17,
            terminal_columns: 80,
            now_ms,
            completion_timestamps: &completion_timestamps,
        });

        // Visible: recent_completed(1), in_progress(3), pending(4)
        assert_eq!(
            layout
                .visible
                .iter()
                .map(|v| v.task_id.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "3", "4"]
        );
        // Hidden: pending(5), old_completed(2)
        assert_eq!(layout.hidden_summary.hidden_pending, 1);
        assert_eq!(layout.hidden_summary.hidden_completed, 1);
    }

    #[test]
    fn layout_truncation_sorts_pending_unblocked_first() {
        // max_display for 17 rows = 3
        let tasks = vec![
            task("1", TaskListStatus::InProgress),
            task_with_blockers("2", TaskListStatus::Pending, &["1"]), // blocked
            task("3", TaskListStatus::Pending),                       // unblocked
            task("4", TaskListStatus::Pending),                       // unblocked
            task("5", TaskListStatus::Completed),
        ];
        let layout = resolve_task_list_layout(&TaskListLayoutInput {
            tasks: &tasks,
            terminal_rows: 17,
            terminal_columns: 80,
            now_ms: 100_000,
            completion_timestamps: &[],
        });

        // Visible: in_progress(1), pending-unblocked(3), pending-unblocked(4)
        // Hidden: pending-blocked(2), old_completed(5)
        assert_eq!(
            layout
                .visible
                .iter()
                .map(|v| v.task_id.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "3", "4"]
        );
        assert_eq!(layout.hidden_summary.hidden_pending, 1);
        assert_eq!(layout.hidden_summary.hidden_completed, 1);
    }

    // ---------------------------------------------------------------
    // hidden summary text
    // ---------------------------------------------------------------

    #[test]
    fn hidden_summary_format_is_expected() {
        let t1 = task("1", TaskListStatus::InProgress);
        let t2 = task("2", TaskListStatus::Pending);
        let t3 = task("3", TaskListStatus::Pending);
        let t4 = task("4", TaskListStatus::Completed);
        let hidden = vec![&t1, &t2, &t3, &t4];
        let summary = build_hidden_summary(&hidden);
        assert_eq!(
            summary.text,
            " \u{2026} +1 in progress, 2 pending, 1 completed"
        );
        assert_eq!(summary.hidden_in_progress, 1);
        assert_eq!(summary.hidden_pending, 2);
        assert_eq!(summary.hidden_completed, 1);
    }

    #[test]
    fn hidden_summary_empty_for_no_hidden_tasks() {
        let summary = build_hidden_summary(&[]);
        assert!(summary.text.is_empty());
    }

    // ---------------------------------------------------------------
    // open blocker detection
    // ---------------------------------------------------------------

    #[test]
    fn layout_detects_open_blockers() {
        let tasks = vec![
            task("1", TaskListStatus::InProgress),
            task_with_blockers("2", TaskListStatus::Pending, &["1", "99"]),
        ];
        let layout = resolve_task_list_layout(&TaskListLayoutInput {
            tasks: &tasks,
            terminal_rows: 30,
            terminal_columns: 80,
            now_ms: 100_000,
            completion_timestamps: &[],
        });

        let task2_layout = &layout.visible[1];
        assert!(task2_layout.is_blocked);
        // "99" is not in the task list so not unresolved — only "1" is an open blocker
        assert_eq!(task2_layout.open_blockers, vec!["1".to_string()]);
    }

    #[test]
    fn layout_completed_blocker_is_not_open() {
        let tasks = vec![
            task("1", TaskListStatus::Completed),
            task_with_blockers("2", TaskListStatus::Pending, &["1"]),
        ];
        let layout = resolve_task_list_layout(&TaskListLayoutInput {
            tasks: &tasks,
            terminal_rows: 30,
            terminal_columns: 80,
            now_ms: 100_000,
            completion_timestamps: &[],
        });

        let task2_layout = &layout.visible[1];
        assert!(!task2_layout.is_blocked);
        assert!(task2_layout.open_blockers.is_empty());
    }

    // ---------------------------------------------------------------
    // display metrics
    // ---------------------------------------------------------------

    #[test]
    fn layout_computes_display_widths_from_columns() {
        let tasks = vec![task("1", TaskListStatus::Pending)];
        let layout = resolve_task_list_layout(&TaskListLayoutInput {
            tasks: &tasks,
            terminal_rows: 30,
            terminal_columns: 100,
            now_ms: 100_000,
            completion_timestamps: &[],
        });

        assert_eq!(layout.visible[0].max_subject_width, 85); // max(15, 100-15)
        assert_eq!(layout.visible[0].max_activity_width, 85);
    }

    #[test]
    fn layout_enforces_minimum_subject_width() {
        let tasks = vec![task("1", TaskListStatus::Pending)];
        let layout = resolve_task_list_layout(&TaskListLayoutInput {
            tasks: &tasks,
            terminal_rows: 30,
            terminal_columns: 20,
            now_ms: 100_000,
            completion_timestamps: &[],
        });

        assert_eq!(layout.visible[0].max_subject_width, 15); // min floor
    }

    // ---------------------------------------------------------------
    // owner helpers
    // ---------------------------------------------------------------

    #[test]
    fn should_show_owner_requires_wide_terminal_and_active_owner() {
        assert!(should_show_owner(80, Some("alice"), true));
        assert!(!should_show_owner(59, Some("alice"), true));
        assert!(!should_show_owner(80, None, true));
        assert!(!should_show_owner(80, Some("alice"), false));
    }

    #[test]
    fn owner_display_width_accounts_for_brackets() {
        assert_eq!(owner_display_width("alice"), 9); // " (@alice)" = 4 + 5
    }

    #[test]
    fn adjusted_subject_width_subtracts_owner_width() {
        assert_eq!(adjusted_subject_width(80, 9), 56); // max(15, 80-15-9)
                                                       // Very narrow terminal: floor at 15
        assert_eq!(adjusted_subject_width(20, 9), 15);
    }

    // ---------------------------------------------------------------
    // edge cases
    // ---------------------------------------------------------------

    #[test]
    fn layout_handles_empty_task_list() {
        let layout = resolve_task_list_layout(&TaskListLayoutInput {
            tasks: &[],
            terminal_rows: 30,
            terminal_columns: 80,
            now_ms: 100_000,
            completion_timestamps: &[],
        });
        assert_eq!(layout.counts.total, 0);
        assert!(layout.visible.is_empty());
        assert!(!layout.is_truncated);
    }

    #[test]
    fn layout_zero_max_display_hides_multiple_tasks() {
        let tasks = vec![
            task("1", TaskListStatus::Pending),
            task("2", TaskListStatus::Pending),
        ];
        let layout = resolve_task_list_layout(&TaskListLayoutInput {
            tasks: &tasks,
            terminal_rows: 8, // <= 10 → max_display = 0
            terminal_columns: 80,
            now_ms: 100_000,
            completion_timestamps: &[],
        });
        assert_eq!(layout.max_display, 0);
        assert!(layout.visible.is_empty());
        assert!(layout.is_truncated);
        assert_eq!(layout.hidden_summary.hidden_pending, 2);
    }

    #[test]
    fn layout_exact_fit_is_not_truncated() {
        // max_display for 17 rows = 3
        let tasks = vec![
            task("1", TaskListStatus::Pending),
            task("2", TaskListStatus::InProgress),
            task("3", TaskListStatus::Completed),
        ];
        let layout = resolve_task_list_layout(&TaskListLayoutInput {
            tasks: &tasks,
            terminal_rows: 17,
            terminal_columns: 80,
            now_ms: 100_000,
            completion_timestamps: &[],
        });
        assert!(!layout.is_truncated);
        assert_eq!(layout.visible.len(), 3);
    }
}
