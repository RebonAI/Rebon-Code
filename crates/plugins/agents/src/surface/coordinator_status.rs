//! Coordinator agent-status row projector.
//!
//! Groups task statuses, evicts stale completed items, and formats compact
//! background-task rows below the prompt input. The pure pieces are:
//!
//! 1. **[`get_visible_agent_tasks`]** — filter `tasks` down to panel
//! agent tasks whose `evict_after` is not `Some(0)`, sorted by
//! `start_time`.
//! 2. **Row suffix** — `<sep> <elapsed>[ · ↑/↓
//! <tokens> tokens][ · <queued> queued][ · x to stop/clear]`.
//! 3. **Row prefix** — `pointer ` if selected/hover.
//!
//! [`build_coordinator_row`] projects task inputs to a
//! [`CoordinatorRowDisplay`] the consumer renders. Ticking `now_ms`
//! and evicting finished tasks is the consumer's problem.

/// Status of a local agent task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalAgentTaskStatus {
    /// Still running.
    Running,
    /// Paused.
    Paused,
    /// Completed successfully.
    Completed,
    /// Failed.
    Failed,
    /// Cancelled.
    Cancelled,
}

impl LocalAgentTaskStatus {
    /// True if this is a terminal status (completed, failed, or
    /// cancelled).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            LocalAgentTaskStatus::Completed
                | LocalAgentTaskStatus::Failed
                | LocalAgentTaskStatus::Cancelled
        )
    }

    /// True if running (not paused, not terminal).
    pub fn is_running(self) -> bool {
        matches!(self, LocalAgentTaskStatus::Running)
    }
}

/// Snapshot of a single local agent task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAgentTask {
    /// Stable task id.
    pub id: String,
    /// Task description (used as a fallback for the row label).
    pub description: String,
    /// Optional progress summary that supersedes `description`.
    pub progress_summary: Option<String>,
    /// Current status.
    pub status: LocalAgentTaskStatus,
    /// Start time (ms since epoch).
    pub start_time: i64,
    /// End time (ms since epoch). `None` while still running.
    pub end_time: Option<i64>,
    /// Total time spent paused (ms).
    pub total_paused_ms: u64,
    /// Token count (from progress).
    pub token_count: Option<u64>,
    /// True if the most recent activity was a downstream tool call.
    pub last_activity_inbound: bool,
    /// Number of pending messages queued for the agent.
    pub queued_count: usize,
    /// Eviction deadline (ms since epoch). `None` = "running /
    /// retained" — always visible. `Some(0)` = "evict immediately".
    pub evict_after: Option<i64>,
}

/// Filter the task list to those visible in the panel.
///
/// Tasks with no eviction deadline are always kept. Tasks with
/// `Some(0)` are hidden; the rest are sorted by `start_time`.
pub fn get_visible_agent_tasks(tasks: Vec<LocalAgentTask>) -> Vec<LocalAgentTask> {
    let mut out: Vec<LocalAgentTask> = tasks
        .into_iter()
        .filter(|t| t.evict_after != Some(0))
        .collect();
    out.sort_by_key(|t| t.start_time);
    out
}

/// Pre-built display projection for one agent row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatorRowDisplay {
    /// Cursor prefix (`pointer ` or ` `).
    pub prefix: &'static str,
    /// Bullet character (filled or hollow).
    pub bullet: &'static str,
    /// Optional `<name>: ` prefix.
    pub name_part: String,
    /// Description body (caller does the wrap).
    pub description: String,
    /// Suffix string with separator + elapsed + tokens + queued.
    pub suffix: String,
    /// True if the row should be dim-colored.
    pub dim: bool,
}

/// Inputs to [`build_coordinator_row`].
#[derive(Debug, Clone)]
pub struct CoordinatorRowInputs<'a> {
    /// The task to render.
    pub task: &'a LocalAgentTask,
    /// Display name (`None` falls back to no prefix).
    pub name: Option<&'a str>,
    /// True if this row is the highlighted selection.
    pub is_selected: bool,
    /// True if the user is currently viewing this teammate.
    pub is_viewed: bool,
    /// Current "now" in ms since epoch — the consumer ticks this.
    pub now_ms: i64,
}

/// Pinned bullet glyphs.
pub const FILLED_BULLET: &str = "\u{25CF}";
/// Hollow bullet glyph.
pub const HOLLOW_BULLET: &str = "\u{25CB}";
/// Cursor pointer glyph.
pub const POINTER: &str = "\u{276F} ";
/// Empty cursor (two spaces).
pub const NO_POINTER: &str = "  ";
/// Play (running) glyph.
pub const PLAY_ICON: &str = "\u{25B6}";
/// Pause glyph.
pub const PAUSE_ICON: &str = "\u{23F8}";

/// Build the row display for one agent.
pub fn build_coordinator_row(inputs: &CoordinatorRowInputs<'_>) -> CoordinatorRowDisplay {
    let task = inputs.task;
    let highlighted = inputs.is_selected;
    let prefix = if highlighted { POINTER } else { NO_POINTER };
    let bullet = if inputs.is_viewed {
        FILLED_BULLET
    } else {
        HOLLOW_BULLET
    };
    let dim = !highlighted && !inputs.is_viewed;
    let is_running = !task.status.is_terminal();
    let sep = if is_running { PLAY_ICON } else { PAUSE_ICON };

    let elapsed_ms = if is_running {
        (inputs.now_ms - task.start_time - task.total_paused_ms as i64).max(0) as u64
    } else {
        let end = task.end_time.unwrap_or(task.start_time);
        ((end - task.start_time) - task.total_paused_ms as i64).max(0) as u64
    };
    let elapsed = format_duration_short(elapsed_ms);

    let token_text = match task.token_count {
        Some(tc) if tc > 0 => {
            let arrow = if task.last_activity_inbound {
                "\u{2193}"
            } else {
                "\u{2191}"
            };
            format!(" \u{00B7} {} {} tokens", arrow, format_number(tc))
        }
        _ => String::new(),
    };
    let queued_text = if task.queued_count > 0 {
        format!(" \u{00B7} {} queued", task.queued_count)
    } else {
        String::new()
    };
    let hint_part = if inputs.is_selected && !inputs.is_viewed {
        let action = if is_running { "stop" } else { "clear" };
        format!(" \u{00B7} x to {}", action)
    } else {
        String::new()
    };
    let suffix = format!(
        " {sep}  {}{}{}{}",
        elapsed, token_text, queued_text, hint_part
    );

    let description = task
        .progress_summary
        .clone()
        .unwrap_or_else(|| task.description.clone());
    let name_part = inputs.name.map(|n| format!("{}: ", n)).unwrap_or_default();

    CoordinatorRowDisplay {
        prefix,
        bullet,
        name_part,
        description,
        suffix,
        dim,
    }
}

/// Format a duration as `H:MM:SS`, or `M:SS` if under an hour.
pub fn format_duration_short(ms: u64) -> String {
    let total_seconds = ms / 1000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{}:{:02}", minutes, seconds)
    }
}

fn format_number(n: u64) -> String {
    crate::surface::progress_line::format_number(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> LocalAgentTask {
        LocalAgentTask {
            id: "task-1".into(),
            description: "review pull request".into(),
            progress_summary: None,
            status: LocalAgentTaskStatus::Running,
            start_time: 1000,
            end_time: None,
            total_paused_ms: 0,
            token_count: None,
            last_activity_inbound: false,
            queued_count: 0,
            evict_after: None,
        }
    }

    fn inputs(task: &LocalAgentTask) -> CoordinatorRowInputs<'_> {
        CoordinatorRowInputs {
            task,
            name: None,
            is_selected: false,
            is_viewed: false,
            now_ms: 11000,
        }
    }

    // ---- visibility filter ----

    #[test]
    fn visible_filters_evict_zero() {
        let mut t1 = task();
        t1.evict_after = Some(0);
        let mut t2 = task();
        t2.id = "task-2".into();
        t2.evict_after = None;
        let v = get_visible_agent_tasks(vec![t1, t2]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, "task-2");
    }

    #[test]
    fn visible_sorts_by_start_time() {
        let mut t1 = task();
        t1.id = "a".into();
        t1.start_time = 5000;
        let mut t2 = task();
        t2.id = "b".into();
        t2.start_time = 1000;
        let v = get_visible_agent_tasks(vec![t1, t2]);
        assert_eq!(v[0].id, "b");
        assert_eq!(v[1].id, "a");
    }

    // ---- status helpers ----

    #[test]
    fn status_is_terminal_table() {
        assert!(LocalAgentTaskStatus::Completed.is_terminal());
        assert!(LocalAgentTaskStatus::Failed.is_terminal());
        assert!(LocalAgentTaskStatus::Cancelled.is_terminal());
        assert!(!LocalAgentTaskStatus::Running.is_terminal());
        assert!(!LocalAgentTaskStatus::Paused.is_terminal());
    }

    // ---- row builder ----

    #[test]
    fn row_uses_pointer_when_selected() {
        let t = task();
        let mut i = inputs(&t);
        i.is_selected = true;
        let row = build_coordinator_row(&i);
        assert_eq!(row.prefix, POINTER);
    }

    #[test]
    fn row_uses_no_pointer_when_unselected() {
        let t = task();
        let i = inputs(&t);
        let row = build_coordinator_row(&i);
        assert_eq!(row.prefix, NO_POINTER);
    }

    #[test]
    fn row_uses_filled_bullet_when_viewed() {
        let t = task();
        let mut i = inputs(&t);
        i.is_viewed = true;
        let row = build_coordinator_row(&i);
        assert_eq!(row.bullet, FILLED_BULLET);
    }

    #[test]
    fn row_uses_hollow_bullet_when_not_viewed() {
        let t = task();
        let i = inputs(&t);
        let row = build_coordinator_row(&i);
        assert_eq!(row.bullet, HOLLOW_BULLET);
    }

    #[test]
    fn row_uses_play_icon_when_running() {
        let t = task();
        let i = inputs(&t);
        let row = build_coordinator_row(&i);
        assert_eq!(row.suffix, " ▶  0:10");
    }

    #[test]
    fn row_uses_pause_icon_when_terminal() {
        let mut t = task();
        t.status = LocalAgentTaskStatus::Completed;
        t.end_time = Some(2000);
        let i = inputs(&t);
        let row = build_coordinator_row(&i);
        assert_eq!(row.suffix, " ⏸  0:01");
    }

    #[test]
    fn row_includes_token_count_when_set() {
        let mut t = task();
        t.token_count = Some(12345);
        let i = inputs(&t);
        let row = build_coordinator_row(&i);
        assert!(row.suffix.contains("12,345 tokens"));
    }

    #[test]
    fn row_omits_tokens_when_zero() {
        let mut t = task();
        t.token_count = Some(0);
        let i = inputs(&t);
        let row = build_coordinator_row(&i);
        assert!(!row.suffix.contains("tokens"));
    }

    #[test]
    fn row_arrow_inbound() {
        let mut t = task();
        t.token_count = Some(10);
        t.last_activity_inbound = true;
        let i = inputs(&t);
        let row = build_coordinator_row(&i);
        assert!(row.suffix.contains("\u{2193}"));
    }

    #[test]
    fn row_arrow_outbound() {
        let mut t = task();
        t.token_count = Some(10);
        t.last_activity_inbound = false;
        let i = inputs(&t);
        let row = build_coordinator_row(&i);
        assert!(row.suffix.contains("\u{2191}"));
    }

    #[test]
    fn row_includes_queued_count() {
        let mut t = task();
        t.queued_count = 3;
        let i = inputs(&t);
        let row = build_coordinator_row(&i);
        assert!(row.suffix.contains("3 queued"));
    }

    #[test]
    fn row_omits_queued_when_zero() {
        let t = task();
        let i = inputs(&t);
        let row = build_coordinator_row(&i);
        assert!(!row.suffix.contains("queued"));
    }

    #[test]
    fn row_hint_when_selected_and_running() {
        let t = task();
        let mut i = inputs(&t);
        i.is_selected = true;
        let row = build_coordinator_row(&i);
        assert!(row.suffix.contains("x to stop"));
    }

    #[test]
    fn row_hint_when_selected_and_terminal() {
        let mut t = task();
        t.status = LocalAgentTaskStatus::Completed;
        t.end_time = Some(5000);
        let mut i = inputs(&t);
        i.is_selected = true;
        let row = build_coordinator_row(&i);
        assert!(row.suffix.contains("x to clear"));
    }

    #[test]
    fn row_uses_progress_summary_over_description() {
        let mut t = task();
        t.progress_summary = Some("step 3 of 5".into());
        let i = inputs(&t);
        let row = build_coordinator_row(&i);
        assert_eq!(row.description, "step 3 of 5");
    }

    #[test]
    fn row_uses_description_when_no_progress() {
        let t = task();
        let i = inputs(&t);
        let row = build_coordinator_row(&i);
        assert_eq!(row.description, "review pull request");
    }

    #[test]
    fn row_name_part_when_provided() {
        let t = task();
        let mut i = inputs(&t);
        i.name = Some("Reviewer");
        let row = build_coordinator_row(&i);
        assert_eq!(row.name_part, "Reviewer: ");
    }

    #[test]
    fn row_dim_when_unselected_and_unviewed() {
        let t = task();
        let i = inputs(&t);
        let row = build_coordinator_row(&i);
        assert!(row.dim);
    }

    #[test]
    fn row_not_dim_when_selected() {
        let t = task();
        let mut i = inputs(&t);
        i.is_selected = true;
        let row = build_coordinator_row(&i);
        assert!(!row.dim);
    }

    // ---- duration formatter ----

    #[test]
    fn duration_under_hour() {
        assert_eq!(format_duration_short(125_000), "2:05");
    }

    #[test]
    fn duration_over_hour() {
        assert_eq!(format_duration_short(3_725_000), "1:02:05");
    }

    #[test]
    fn duration_zero() {
        assert_eq!(format_duration_short(0), "0:00");
    }
}
