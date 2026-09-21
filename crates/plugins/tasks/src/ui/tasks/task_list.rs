//! Task list reducer + per-item projection.
//!
//! Three logical pieces:
//!
//! 1. The recent-completion tracker (task id → completion timestamp)
//!    + the 30s TTL window logic.
//! 2. The prioritization sort: when the list overflows the visible
//!    window, partition into `(recent_completed, in_progress, pending,
//!    older_completed)` then concatenate.
//! 3. The per-item rendering helper that picks the icon, color,
//!    blocked-by suffix, and owner display.
//!
//! All three are pure once you accept a `now_ms` parameter and a
//! pre-built activity-snapshot map: the caller passes in the per-task
//! activity it already has rather than this module looking one up.

use std::collections::BTreeMap;

/// The task-list row and its status vocabulary, shared with every other
/// surface that renders the list.
///
/// Deliberately *not* [`crate::ui::tasks::common::TaskStatus`]: that one is the
/// background-job lifecycle (`running`, `failed`, `killed`), while a list row
/// is only ever pending, in progress, or completed.
pub use rebon_types::{ListTask, TaskListStatus};

const RECENT_COMPLETED_TTL_MS: u64 = 30_000;

/// Recent-completion tracker: task id → the millisecond timestamp the
/// task completed at.
///
/// Pure: the consumer threads `now_ms` in. The sweep drops every id
/// whose 30s window has closed.
#[derive(Debug, Clone, Default)]
pub struct CompletionTracker {
    completion_ts: BTreeMap<String, u64>,
}

impl CompletionTracker {
    /// Empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the tracker against the current task list.
    ///
    /// 1. For each currently-completed task, if we don't already have a
    ///    timestamp, record `now_ms`.
    /// 2. Drop any tracked id that's no longer in the completed set.
    pub fn observe(&mut self, tasks: &[ListTask], now_ms: u64) {
        use std::collections::HashSet;
        let current: HashSet<&str> = tasks
            .iter()
            .filter(|t| t.status == TaskListStatus::Completed)
            .map(|t| t.id.as_str())
            .collect();
        for id in &current {
            if !self.completion_ts.contains_key(*id) {
                self.completion_ts.insert((*id).to_owned(), now_ms);
            }
        }
        let stale: Vec<String> = self
            .completion_ts
            .keys()
            .filter(|id| !current.contains(id.as_str()))
            .cloned()
            .collect();
        for id in stale {
            self.completion_ts.remove(&id);
        }
    }

    /// True when the given id was completed within the last 30s.
    pub fn is_recent(&self, id: &str, now_ms: u64) -> bool {
        match self.completion_ts.get(id) {
            Some(ts) => now_ms.saturating_sub(*ts) < RECENT_COMPLETED_TTL_MS,
            None => false,
        }
    }

    /// Override the completion timestamp for an id. Test helper used
    /// to pin the recent / older split without
    /// emulating wall-clock advance.
    #[doc(hidden)]
    pub fn debug_set_completion_ts(&mut self, id: &str, ts_ms: u64) {
        self.completion_ts.insert(id.to_owned(), ts_ms);
    }

    /// Earliest expiry time across all tracked ids, used to schedule
    /// the next forced re-render: the earliest expiry still ahead of
    /// `now_ms`.
    pub fn next_expiry(&self, now_ms: u64) -> Option<u64> {
        let mut earliest: Option<u64> = None;
        for ts in self.completion_ts.values() {
            let expiry = ts + RECENT_COMPLETED_TTL_MS;
            if expiry > now_ms {
                earliest = Some(match earliest {
                    None => expiry,
                    Some(prev) => prev.min(expiry),
                });
            }
        }
        earliest
    }
}

/// Compute the maximum number of visible task rows for a given
/// terminal height.
pub fn compute_max_display(rows: u16) -> usize {
    if rows <= 10 {
        return 0;
    }
    let candidate = rows.saturating_sub(14).max(3) as usize;
    candidate.min(10)
}

/// Stable id-ascending comparator. Numeric ids sort
/// numerically; otherwise the
/// comparison falls back to string ordering.
pub fn cmp_by_id(a: &str, b: &str) -> std::cmp::Ordering {
    let a_num = a.parse::<i64>();
    let b_num = b.parse::<i64>();
    match (a_num, b_num) {
        (Ok(an), Ok(bn)) => an.cmp(&bn),
        _ => a.cmp(b),
    }
}

/// Result of [`prioritize_tasks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrioritizedTasks {
    /// Tasks fitting the visible window.
    pub visible: Vec<ListTask>,
    /// Tasks beyond the cutoff.
    pub hidden: Vec<ListTask>,
}

/// Prioritize tasks for display:
///
/// * Recently-completed (within 30s) → first
/// * In-progress → next, sorted by id
/// * Pending → next, blocked tasks at the bottom
/// * Older completed → last
///
/// When the list fits the window, the split is skipped and everything
/// is sorted by id.
pub fn prioritize_tasks(
    tasks: &[ListTask],
    tracker: &CompletionTracker,
    now_ms: u64,
    max_display: usize,
) -> PrioritizedTasks {
    if tasks.len() <= max_display {
        let mut sorted: Vec<ListTask> = tasks.to_vec();
        sorted.sort_by(|a, b| cmp_by_id(&a.id, &b.id));
        return PrioritizedTasks {
            visible: sorted,
            hidden: Vec::new(),
        };
    }

    let mut recent_completed: Vec<ListTask> = Vec::new();
    let mut older_completed: Vec<ListTask> = Vec::new();
    for t in tasks {
        if t.status != TaskListStatus::Completed {
            continue;
        }
        if tracker.is_recent(&t.id, now_ms) {
            recent_completed.push(t.clone());
        } else {
            older_completed.push(t.clone());
        }
    }
    recent_completed.sort_by(|a, b| cmp_by_id(&a.id, &b.id));
    older_completed.sort_by(|a, b| cmp_by_id(&a.id, &b.id));

    let mut in_progress: Vec<ListTask> = tasks
        .iter()
        .filter(|t| t.status == TaskListStatus::InProgress)
        .cloned()
        .collect();
    in_progress.sort_by(|a, b| cmp_by_id(&a.id, &b.id));

    // Pending tasks: blocked ones get pushed to the bottom (within
    // pending), then sort by id within each blocked-ness bucket.
    let unresolved_ids: std::collections::HashSet<&str> = tasks
        .iter()
        .filter(|t| t.status != TaskListStatus::Completed)
        .map(|t| t.id.as_str())
        .collect();
    let mut pending: Vec<ListTask> = tasks
        .iter()
        .filter(|t| t.status == TaskListStatus::Pending)
        .cloned()
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
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            };
        }
        cmp_by_id(&a.id, &b.id)
    });

    let mut prioritized: Vec<ListTask> = Vec::with_capacity(tasks.len());
    prioritized.extend(recent_completed);
    prioritized.extend(in_progress);
    prioritized.extend(pending);
    prioritized.extend(older_completed);

    let visible: Vec<ListTask> = prioritized.iter().take(max_display).cloned().collect();
    let hidden: Vec<ListTask> = prioritized.into_iter().skip(max_display).collect();
    PrioritizedTasks { visible, hidden }
}

/// Build a hidden-task summary.
pub fn build_hidden_summary(hidden: &[ListTask]) -> String {
    if hidden.is_empty() {
        return String::new();
    }
    let in_progress = hidden
        .iter()
        .filter(|t| t.status == TaskListStatus::InProgress)
        .count();
    let pending = hidden
        .iter()
        .filter(|t| t.status == TaskListStatus::Pending)
        .count();
    let completed = hidden
        .iter()
        .filter(|t| t.status == TaskListStatus::Completed)
        .count();
    let mut parts: Vec<String> = Vec::new();
    if in_progress > 0 {
        parts.push(format!("{in_progress} in progress"));
    }
    if pending > 0 {
        parts.push(format!("{pending} pending"));
    }
    if completed > 0 {
        parts.push(format!("{completed} completed"));
    }
    format!(" … +{}", parts.join(", "))
}

/// One projected task row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskItemView {
    /// Glyph used as the icon — one of the [`TaskListIcon`] variants.
    pub icon: TaskListIcon,
    /// Theme color name (e.g. `"success"`, `"rebon"`) — `None` falls
    /// back to the default text color.
    pub color: Option<String>,
    /// `true` when the task should be rendered bold.
    pub bold: bool,
    /// `true` when the subject should be struck through.
    pub strikethrough: bool,
    /// `true` when the row should be rendered dim.
    pub dim: bool,
    /// Subject text, already truncated.
    pub display_subject: String,
    /// `(@owner)` suffix when the owner is shown. `None` skips the
    /// suffix entirely.
    pub owner_suffix: Option<TaskListOwner>,
    /// Blocked-by line, e.g. ` → blocked by #1, #2`. `None` when not
    /// blocked.
    pub blocked_by: Option<String>,
    /// Optional activity row underneath the main row (with the
    /// trailing ellipsis).
    pub activity_row: Option<String>,
}

/// Discrete icon variant for [`TaskItemView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskListIcon {
    /// Completed-task checkmark.
    Tick,
    /// In-progress filled square.
    SquareSmallFilled,
    /// Pending hollow square.
    SquareSmall,
}

/// Owner row — the `(@name)` marker shown next to an owned task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskListOwner {
    /// Plain `@{name}` string.
    pub label: String,
    /// Theme color (e.g. `"green"`) — when `None` the consumer falls
    /// back to plain text.
    pub color: Option<String>,
}

/// Inputs for [`build_task_item`]. The several pieces of
/// state the row needs — owner color, activity, owner-active — arrive
/// as pre-built fields rather than being recomputed here.
#[derive(Debug, Clone)]
pub struct TaskItemInput {
    /// The task itself.
    pub task: ListTask,
    /// Theme color name for the owner pill (when one is mapped).
    pub owner_color: Option<String>,
    /// Open blockers — only the ids that are still unresolved.
    pub open_blockers: Vec<String>,
    /// Optional activity description, already summarised by the caller.
    pub activity: Option<String>,
    /// `true` when the owner is one of the active teammates.
    pub owner_active: bool,
    /// Terminal width.
    pub columns: u16,
}

const ICON_RESERVED_COLS: u16 = 15;

/// String width approximation by character count rather than
/// display-width math; the tests pin the char-count behavior.
fn approx_string_width(s: &str) -> usize {
    s.chars().count()
}

fn truncate_to_width(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let head: String = s.chars().take(max - 1).collect();
    format!("{head}…")
}

/// Project a task to a [`TaskItemView`].
pub fn build_task_item(input: &TaskItemInput) -> TaskItemView {
    let is_completed = input.task.status == TaskListStatus::Completed;
    let is_in_progress = input.task.status == TaskListStatus::InProgress;
    let is_blocked = !input.open_blockers.is_empty();

    let (icon, color) = match input.task.status {
        TaskListStatus::Completed => (TaskListIcon::Tick, Some("success".to_owned())),
        TaskListStatus::InProgress => (TaskListIcon::SquareSmallFilled, Some("rebon".to_owned())),
        TaskListStatus::Pending => (TaskListIcon::SquareSmall, None),
    };

    let show_activity = is_in_progress && !is_blocked && input.activity.is_some();
    let show_owner = input.columns >= 60 && input.task.owner.is_some() && input.owner_active;

    let owner_width: usize = if show_owner {
        // ` (@owner)` length
        let owner = input.task.owner.as_deref().unwrap_or("");
        approx_string_width(&format!(" (@{owner})"))
    } else {
        0
    };

    let max_subject_width =
        ((input.columns as i64) - ICON_RESERVED_COLS as i64 - owner_width as i64).max(15) as usize;
    let display_subject = truncate_to_width(&input.task.subject, max_subject_width);

    let max_activity_width = ((input.columns as i64) - ICON_RESERVED_COLS as i64).max(15) as usize;

    let activity_row: Option<String> = if show_activity {
        let act = input.activity.as_deref().unwrap_or("");
        if act.is_empty() {
            None
        } else {
            let trunc = truncate_to_width(act, max_activity_width);
            Some(format!("  {trunc}…"))
        }
    } else {
        None
    };

    let owner_suffix: Option<TaskListOwner> = if show_owner {
        let owner = input.task.owner.as_deref().unwrap_or("");
        Some(TaskListOwner {
            label: format!("@{owner}"),
            color: input.owner_color.clone(),
        })
    } else {
        None
    };

    let blocked_by: Option<String> = if is_blocked {
        let mut sorted = input.open_blockers.clone();
        sorted.sort_by(|a, b| {
            let an = a.parse::<i64>().unwrap_or(i64::MAX);
            let bn = b.parse::<i64>().unwrap_or(i64::MAX);
            an.cmp(&bn)
        });
        let formatted: Vec<String> = sorted.into_iter().map(|id| format!("#{id}")).collect();
        Some(format!(" › blocked by {}", formatted.join(", ")))
    } else {
        None
    };

    TaskItemView {
        icon,
        color,
        bold: is_in_progress,
        strikethrough: is_completed,
        dim: is_completed || is_blocked,
        display_subject,
        owner_suffix,
        blocked_by,
        activity_row,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, subject: &str, status: TaskListStatus) -> ListTask {
        ListTask {
            id: id.into(),
            subject: subject.into(),
            status,
            owner: None,
            blocked_by: vec![],
        }
    }

    #[test]
    fn cmp_by_id_numeric_then_string() {
        assert_eq!(cmp_by_id("1", "2"), std::cmp::Ordering::Less);
        assert_eq!(cmp_by_id("10", "2"), std::cmp::Ordering::Greater);
        assert_eq!(cmp_by_id("a", "b"), std::cmp::Ordering::Less);
        // Mixed numeric / non-numeric: the parse fails on one side, so
        // the comparison falls back to string ordering, which sorts
        // letters after digits — '1' (0x31) < 'a' (0x61).
        assert_eq!(cmp_by_id("a", "1"), std::cmp::Ordering::Greater);
    }

    #[test]
    fn max_display_table() {
        assert_eq!(compute_max_display(10), 0);
        assert_eq!(compute_max_display(8), 0);
        // rows=15 → max(3, 1)=3, min(10, 3)=3
        assert_eq!(compute_max_display(15), 3);
        // rows=20 → max(3, 6)=6, min(10, 6)=6
        assert_eq!(compute_max_display(20), 6);
        // rows=30 → max(3, 16)=16, min(10, 16)=10
        assert_eq!(compute_max_display(30), 10);
    }

    #[test]
    fn tracker_observes_completion_then_drops_uncompleted() {
        let mut tracker = CompletionTracker::new();
        let tasks = vec![
            task("1", "first", TaskListStatus::Completed),
            task("2", "second", TaskListStatus::InProgress),
        ];
        tracker.observe(&tasks, 1000);
        assert!(tracker.is_recent("1", 1000));
        assert!(!tracker.is_recent("2", 1000));

        // Now task 1 disappears (uncompleted) and task 2 becomes
        // completed
        let tasks2 = vec![task("2", "second", TaskListStatus::Completed)];
        tracker.observe(&tasks2, 5000);
        assert!(!tracker.is_recent("1", 5000));
        assert!(tracker.is_recent("2", 5000));
    }

    #[test]
    fn tracker_recent_window_expires_at_30s() {
        let mut tracker = CompletionTracker::new();
        let tasks = vec![task("1", "x", TaskListStatus::Completed)];
        tracker.observe(&tasks, 0);
        assert!(tracker.is_recent("1", 29_999));
        assert!(!tracker.is_recent("1", 30_000));
    }

    #[test]
    fn tracker_next_expiry_returns_earliest() {
        let mut tracker = CompletionTracker::new();
        let tasks_a = vec![task("1", "a", TaskListStatus::Completed)];
        tracker.observe(&tasks_a, 0);
        let tasks_b = vec![
            task("1", "a", TaskListStatus::Completed),
            task("2", "b", TaskListStatus::Completed),
        ];
        tracker.observe(&tasks_b, 5000);
        // expiries: 30000, 35000 → next is 30000
        assert_eq!(tracker.next_expiry(0), Some(30_000));
        // After 30000 expires
        assert_eq!(tracker.next_expiry(31_000), Some(35_000));
        // After both expire
        assert_eq!(tracker.next_expiry(40_000), None);
    }

    #[test]
    fn prioritize_below_max_just_sorts() {
        let tasks = vec![
            task("3", "c", TaskListStatus::Pending),
            task("1", "a", TaskListStatus::Completed),
            task("2", "b", TaskListStatus::InProgress),
        ];
        let tracker = CompletionTracker::new();
        let p = prioritize_tasks(&tasks, &tracker, 0, 10);
        let ids: Vec<&str> = p.visible.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["1", "2", "3"]);
        assert!(p.hidden.is_empty());
    }

    #[test]
    fn prioritize_overflow_partitions_correctly() {
        let mut tracker = CompletionTracker::new();
        let tasks = vec![
            task("1", "a", TaskListStatus::Completed),
            task("2", "b", TaskListStatus::InProgress),
            task("3", "c", TaskListStatus::Pending),
            task("4", "d", TaskListStatus::Completed),
            task("5", "e", TaskListStatus::Pending),
        ];
        // Observe task 1 fresh at t=0; task 4 at t=0 too but we'll
        // probe it after the recent window expires.
        tracker.observe(&tasks, 0);
        // Manually push task 4's timestamp into the past so the
        // recent-window check classifies it as older.
        tracker.debug_set_completion_ts("4", 0);
        tracker.debug_set_completion_ts("1", 60_000); // still recent at now=70000
        let now = 70_000;
        let p = prioritize_tasks(&tasks, &tracker, now, 3);
        let ids: Vec<&str> = p.visible.iter().map(|t| t.id.as_str()).collect();
        // recent_completed [1], in_progress [2], pending [3, 5],
        // older_completed [4] → window of 3 picks [1, 2, 3]
        assert_eq!(ids, vec!["1", "2", "3"]);
        let hidden_ids: Vec<&str> = p.hidden.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(hidden_ids, vec!["5", "4"]);
    }

    #[test]
    fn prioritize_blocked_pending_at_bottom() {
        let mut a = task("1", "a", TaskListStatus::Pending);
        a.blocked_by = vec!["2".into()];
        let b = task("2", "b", TaskListStatus::Pending);
        let c = task("3", "c", TaskListStatus::Pending);
        let d = task("4", "d", TaskListStatus::Pending);
        let tasks = vec![a, b, c, d];
        let tracker = CompletionTracker::new();
        let p = prioritize_tasks(&tasks, &tracker, 0, 3);
        let ids: Vec<&str> = p.visible.iter().map(|t| t.id.as_str()).collect();
        // Pending non-blocked first by id (2, 3, 4) → window of 3 picks [2, 3, 4]
        // Blocked task #1 falls into hidden.
        assert_eq!(ids, vec!["2", "3", "4"]);
    }

    #[test]
    fn hidden_summary_empty() {
        assert_eq!(build_hidden_summary(&[]), "");
    }

    #[test]
    fn hidden_summary_mixed() {
        let hidden = vec![
            task("1", "a", TaskListStatus::Pending),
            task("2", "b", TaskListStatus::Pending),
            task("3", "c", TaskListStatus::InProgress),
            task("4", "d", TaskListStatus::Completed),
        ];
        assert_eq!(
            build_hidden_summary(&hidden),
            " … +1 in progress, 2 pending, 1 completed"
        );
    }

    #[test]
    fn hidden_summary_only_pending() {
        let hidden = vec![task("1", "a", TaskListStatus::Pending)];
        assert_eq!(build_hidden_summary(&hidden), " … +1 pending");
    }

    fn item_input(task: ListTask, columns: u16) -> TaskItemInput {
        TaskItemInput {
            task,
            owner_color: None,
            open_blockers: vec![],
            activity: None,
            owner_active: false,
            columns,
        }
    }

    #[test]
    fn item_completed_strikethrough_dim() {
        let view = build_task_item(&item_input(
            task("1", "done thing", TaskListStatus::Completed),
            80,
        ));
        assert!(view.strikethrough);
        assert!(view.dim);
        assert_eq!(view.icon, TaskListIcon::Tick);
        assert_eq!(view.color.as_deref(), Some("success"));
        assert!(!view.bold);
    }

    #[test]
    fn item_in_progress_bold() {
        let view = build_task_item(&item_input(
            task("1", "doing thing", TaskListStatus::InProgress),
            80,
        ));
        assert!(view.bold);
        assert!(!view.dim);
        assert_eq!(view.icon, TaskListIcon::SquareSmallFilled);
        assert_eq!(view.color.as_deref(), Some("rebon"));
    }

    #[test]
    fn item_pending_no_color() {
        let view = build_task_item(&item_input(task("1", "todo", TaskListStatus::Pending), 80));
        assert_eq!(view.icon, TaskListIcon::SquareSmall);
        assert!(view.color.is_none());
        assert!(!view.bold);
        assert!(!view.dim);
    }

    #[test]
    fn item_blocked_by_sort_and_format() {
        let mut t = task("1", "blocked task", TaskListStatus::Pending);
        t.blocked_by = vec!["3".into(), "1".into(), "10".into()];
        let mut input = item_input(t, 80);
        input.open_blockers = vec!["10".into(), "3".into(), "1".into()];
        let view = build_task_item(&input);
        assert_eq!(
            view.blocked_by.as_deref(),
            Some(" › blocked by #1, #3, #10")
        );
        assert!(view.dim);
    }

    #[test]
    fn item_owner_suffix_only_when_60_columns_and_owner_active() {
        let mut t = task("1", "x", TaskListStatus::InProgress);
        t.owner = Some("researcher".into());

        // Columns < 60 → no owner pill
        let mut input = item_input(t.clone(), 50);
        input.owner_active = true;
        let view = build_task_item(&input);
        assert!(view.owner_suffix.is_none());

        // owner_active=false → no pill
        let mut input2 = item_input(t.clone(), 80);
        input2.owner_active = false;
        let view = build_task_item(&input2);
        assert!(view.owner_suffix.is_none());

        // Both conditions → pill present
        let mut input3 = item_input(t, 80);
        input3.owner_active = true;
        input3.owner_color = Some("blue".into());
        let view = build_task_item(&input3);
        let owner = view.owner_suffix.unwrap();
        assert_eq!(owner.label, "@researcher");
        assert_eq!(owner.color.as_deref(), Some("blue"));
    }

    #[test]
    fn item_activity_row_only_when_in_progress_not_blocked() {
        let mut t = task("1", "x", TaskListStatus::InProgress);
        t.owner = Some("a".into());
        let mut input = item_input(t.clone(), 80);
        input.activity = Some("looking up references".into());
        let view = build_task_item(&input);
        assert!(view.activity_row.is_some());
        assert!(view.activity_row.as_ref().unwrap().ends_with('…'));

        // Blocked → no activity row
        input.open_blockers = vec!["2".into()];
        let view = build_task_item(&input);
        assert!(view.activity_row.is_none());

        // Pending → no activity row
        let mut t2 = task("1", "x", TaskListStatus::Pending);
        t2.owner = Some("a".into());
        let mut input2 = item_input(t2, 80);
        input2.activity = Some("a".into());
        let view = build_task_item(&input2);
        assert!(view.activity_row.is_none());
    }

    #[test]
    fn item_subject_truncation() {
        let long = "x".repeat(200);
        let t = task("1", &long, TaskListStatus::Pending);
        let view = build_task_item(&item_input(t, 80));
        // 80 - 15 = 65; truncated to 65 chars
        assert!(view.display_subject.chars().count() <= 65);
        assert!(view.display_subject.ends_with('…'));
    }
}
