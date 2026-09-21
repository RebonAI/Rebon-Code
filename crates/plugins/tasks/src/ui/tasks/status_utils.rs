//! Pure status helpers.
//!
//! Terminal-status test, glyph key, semantic color, teammate activity
//! description, and the tasks-footer predicate. The functions return
//! glyph keys rather than the unicode glyphs themselves, so this module
//! stays free of any design-system dependency.

use crate::ui::tasks::common::{SemanticColor, TaskKind, TaskStatus};

/// Glyph name returned by [`task_status_icon`]: `tick`, `cross`, `play`,
/// `ellipsis`, `bullet`, `warning`, `questionMarkPrefix`. The consumer
/// maps these to the final glyph (the design-system widget owns the
/// unicode mapping so the test matrix is a clean enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusIcon {
    /// Completed checkmark.
    Tick,
    /// Failed/killed × glyph.
    Cross,
    /// Active running ▶ glyph.
    Play,
    /// Running but idle … glyph.
    Ellipsis,
    /// Pending or unknown ● glyph.
    Bullet,
    /// Shutdown-requested ⚠ glyph.
    Warning,
    /// Awaiting approval ?
    QuestionMarkPrefix,
}

impl StatusIcon {
    /// Glyph key name. Stable across all surfaces.
    pub fn glyph_key(&self) -> &'static str {
        match self {
            StatusIcon::Tick => "tick",
            StatusIcon::Cross => "cross",
            StatusIcon::Play => "play",
            StatusIcon::Ellipsis => "ellipsis",
            StatusIcon::Bullet => "bullet",
            StatusIcon::Warning => "warning",
            StatusIcon::QuestionMarkPrefix => "questionMarkPrefix",
        }
    }
}

/// State flags passed to [`task_status_icon`] / [`task_status_color`].
#[derive(Debug, Clone, Copy, Default)]
pub struct StatusFlags {
    /// Task is running but currently between activities.
    pub is_idle: bool,
    /// Task is paused waiting for plan approval.
    pub awaiting_approval: bool,
    /// Task encountered an error (overrides everything).
    pub has_error: bool,
    /// Task is being shut down gracefully.
    pub shutdown_requested: bool,
}

/// True if the status is terminal — `Completed`, `Failed` or `Killed`.
pub fn is_terminal_status(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Killed
    )
}

/// Pick the glyph key for a task.
/// Precedence (highest first):
///
/// 1. `flags.has_error` → cross
/// 2. `flags.awaiting_approval` → questionMarkPrefix
/// 3. `flags.shutdown_requested` → warning
/// 4. `status == TaskStatus::Running` + `flags.is_idle` → ellipsis
/// 5. `status == TaskStatus::Running` → play
/// 6. `status == TaskStatus::Completed` → tick
/// 7. `status` is `Failed` or `Killed` → cross
/// 8. otherwise → bullet
pub fn task_status_icon(status: TaskStatus, flags: StatusFlags) -> StatusIcon {
    if flags.has_error {
        return StatusIcon::Cross;
    }
    if flags.awaiting_approval {
        return StatusIcon::QuestionMarkPrefix;
    }
    if flags.shutdown_requested {
        return StatusIcon::Warning;
    }
    if status == TaskStatus::Running {
        return if flags.is_idle {
            StatusIcon::Ellipsis
        } else {
            StatusIcon::Play
        };
    }
    if status == TaskStatus::Completed {
        return StatusIcon::Tick;
    }
    if matches!(status, TaskStatus::Failed | TaskStatus::Killed) {
        return StatusIcon::Cross;
    }
    StatusIcon::Bullet
}

/// Pick the semantic color for a task.
/// Precedence (highest first):
///
/// 1. `flags.has_error` → error
/// 2. `flags.awaiting_approval` → warning
/// 3. `flags.shutdown_requested` → warning
/// 4. `flags.is_idle` → background
/// 5. `status == TaskStatus::Completed` → success
/// 6. `status == TaskStatus::Failed` → error
/// 7. `status == TaskStatus::Killed` → warning
/// 8. otherwise → background
pub fn task_status_color(status: TaskStatus, flags: StatusFlags) -> SemanticColor {
    if flags.has_error {
        return SemanticColor::Error;
    }
    if flags.awaiting_approval {
        return SemanticColor::Warning;
    }
    if flags.shutdown_requested {
        return SemanticColor::Warning;
    }
    if flags.is_idle {
        return SemanticColor::Background;
    }
    match status {
        TaskStatus::Completed => SemanticColor::Success,
        TaskStatus::Failed => SemanticColor::Error,
        TaskStatus::Killed => SemanticColor::Warning,
        _ => SemanticColor::Background,
    }
}

/// Pre-built input shape for a teammate activity description: the
/// fields [`describe_teammate_activity`] reads.
///
/// Recent-activity summarization is done by the consumer, so no
/// read/search-collapsing logic lives here.
#[derive(Debug, Clone, Default)]
pub struct TeammateActivityInput {
    /// Teammate is being shut down.
    pub shutdown_requested: bool,
    /// Teammate is paused waiting for plan approval.
    pub awaiting_plan_approval: bool,
    /// Teammate is between activities.
    pub is_idle: bool,
    /// Pre-summarized recent-activity string. The consumer is expected
    /// to skip this when there are no activities (so we can distinguish
    /// "summary is empty" from "no activities").
    pub recent_activity_summary: Option<String>,
    /// Description of the most recent activity.
    pub last_activity_description: Option<String>,
}

/// Derive the human-readable activity string for an in-process teammate.
/// Fall-through:
///
/// 1. `shutdown_requested` → `"stopping"`
/// 2. `awaiting_plan_approval` → `"awaiting approval"`
/// 3. `is_idle` → `"idle"`
/// 4. `recent_activity_summary` (when present)
/// 5. `last_activity_description` (when present)
/// 6. `'working'`
pub fn describe_teammate_activity(t: &TeammateActivityInput) -> String {
    if t.shutdown_requested {
        return "stopping".to_owned();
    }
    if t.awaiting_plan_approval {
        return "awaiting approval".to_owned();
    }
    if t.is_idle {
        return "idle".to_owned();
    }
    if let Some(summary) = &t.recent_activity_summary {
        return summary.clone();
    }
    if let Some(last) = &t.last_activity_description {
        return last.clone();
    }
    "working".to_owned()
}

/// Visible-task input for [`should_hide_tasks_footer`]. The consumer
/// pre-projects each task to an `(is_background, is_panel_agent, kind)`
/// triple — that's all the predicate looks at, and it keeps the crate
/// from depending on the task-state types.
#[derive(Debug, Clone, Copy)]
pub struct FooterTaskInput {
    /// True if the task is a background task.
    pub is_background: bool,
    /// True if the task is a panel-managed agent. **Only** used to
    /// filter out panel-managed local agents on an internal build.
    pub is_panel_agent: bool,
    /// Task kind discriminant.
    pub kind: TaskKind,
}

/// Returns true when the tasks footer would render nothing because the
/// spinner tree is active and every visible background task is an
/// in-process teammate.
///
/// On an internal build panel-managed local agents are left out of the
/// visible set. That choice is the `is_internal_build` parameter rather
/// than a build flag, so the test matrix can pin both branches.
pub fn should_hide_tasks_footer(
    tasks: &[FooterTaskInput],
    show_spinner_tree: bool,
    is_internal_build: bool,
) -> bool {
    if !show_spinner_tree {
        return false;
    }
    let mut has_visible_task = false;
    for t in tasks {
        if !t.is_background || (is_internal_build && t.is_panel_agent) {
            continue;
        }
        has_visible_task = true;
        if t.kind != TaskKind::InProcessTeammate {
            return false;
        }
    }
    has_visible_task
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_status_table() {
        assert!(is_terminal_status(TaskStatus::Completed));
        assert!(is_terminal_status(TaskStatus::Failed));
        assert!(is_terminal_status(TaskStatus::Killed));
        assert!(!is_terminal_status(TaskStatus::Pending));
        assert!(!is_terminal_status(TaskStatus::Running));
    }

    fn flags() -> StatusFlags {
        StatusFlags::default()
    }

    #[test]
    fn icon_precedence_has_error_wins() {
        let mut f = flags();
        f.has_error = true;
        // Even when other flags are set
        f.awaiting_approval = true;
        f.shutdown_requested = true;
        f.is_idle = true;
        assert_eq!(task_status_icon(TaskStatus::Running, f), StatusIcon::Cross);
    }

    #[test]
    fn icon_precedence_awaiting_approval() {
        let mut f = flags();
        f.awaiting_approval = true;
        f.shutdown_requested = true;
        assert_eq!(
            task_status_icon(TaskStatus::Running, f),
            StatusIcon::QuestionMarkPrefix
        );
    }

    #[test]
    fn icon_precedence_shutdown_requested() {
        let mut f = flags();
        f.shutdown_requested = true;
        assert_eq!(
            task_status_icon(TaskStatus::Running, f),
            StatusIcon::Warning
        );
    }

    #[test]
    fn icon_running_idle_vs_active() {
        let mut f = flags();
        f.is_idle = true;
        assert_eq!(
            task_status_icon(TaskStatus::Running, f),
            StatusIcon::Ellipsis
        );
        f.is_idle = false;
        assert_eq!(task_status_icon(TaskStatus::Running, f), StatusIcon::Play);
    }

    #[test]
    fn icon_terminal_states() {
        assert_eq!(
            task_status_icon(TaskStatus::Completed, flags()),
            StatusIcon::Tick
        );
        assert_eq!(
            task_status_icon(TaskStatus::Failed, flags()),
            StatusIcon::Cross
        );
        assert_eq!(
            task_status_icon(TaskStatus::Killed, flags()),
            StatusIcon::Cross
        );
    }

    #[test]
    fn icon_pending_default_bullet() {
        assert_eq!(
            task_status_icon(TaskStatus::Pending, flags()),
            StatusIcon::Bullet
        );
    }

    #[test]
    fn color_precedence_has_error() {
        let mut f = flags();
        f.has_error = true;
        f.awaiting_approval = true;
        f.is_idle = true;
        assert_eq!(
            task_status_color(TaskStatus::Completed, f),
            SemanticColor::Error
        );
    }

    #[test]
    fn color_precedence_awaiting_approval() {
        let mut f = flags();
        f.awaiting_approval = true;
        assert_eq!(
            task_status_color(TaskStatus::Running, f),
            SemanticColor::Warning
        );
    }

    #[test]
    fn color_precedence_shutdown_then_idle() {
        let mut f = flags();
        f.shutdown_requested = true;
        assert_eq!(
            task_status_color(TaskStatus::Running, f),
            SemanticColor::Warning
        );
        f.shutdown_requested = false;
        f.is_idle = true;
        assert_eq!(
            task_status_color(TaskStatus::Running, f),
            SemanticColor::Background
        );
    }

    #[test]
    fn color_terminal_states() {
        assert_eq!(
            task_status_color(TaskStatus::Completed, flags()),
            SemanticColor::Success
        );
        assert_eq!(
            task_status_color(TaskStatus::Failed, flags()),
            SemanticColor::Error
        );
        assert_eq!(
            task_status_color(TaskStatus::Killed, flags()),
            SemanticColor::Warning
        );
        assert_eq!(
            task_status_color(TaskStatus::Pending, flags()),
            SemanticColor::Background
        );
    }

    fn input() -> TeammateActivityInput {
        TeammateActivityInput::default()
    }

    #[test]
    fn teammate_activity_stopping_wins() {
        let mut t = input();
        t.shutdown_requested = true;
        t.awaiting_plan_approval = true;
        t.is_idle = true;
        t.recent_activity_summary = Some("doing thing".into());
        assert_eq!(describe_teammate_activity(&t), "stopping");
    }

    #[test]
    fn teammate_activity_awaiting_approval() {
        let mut t = input();
        t.awaiting_plan_approval = true;
        t.is_idle = true;
        t.recent_activity_summary = Some("doing thing".into());
        assert_eq!(describe_teammate_activity(&t), "awaiting approval");
    }

    #[test]
    fn teammate_activity_idle() {
        let mut t = input();
        t.is_idle = true;
        t.recent_activity_summary = Some("doing thing".into());
        assert_eq!(describe_teammate_activity(&t), "idle");
    }

    #[test]
    fn teammate_activity_recent_summary_then_last_activity() {
        let mut t = input();
        t.recent_activity_summary = Some("recent".into());
        t.last_activity_description = Some("last".into());
        assert_eq!(describe_teammate_activity(&t), "recent");
        t.recent_activity_summary = None;
        assert_eq!(describe_teammate_activity(&t), "last");
        t.last_activity_description = None;
        assert_eq!(describe_teammate_activity(&t), "working");
    }

    fn fti(kind: TaskKind, is_background: bool, is_panel_agent: bool) -> FooterTaskInput {
        FooterTaskInput {
            kind,
            is_background,
            is_panel_agent,
        }
    }

    #[test]
    fn footer_no_spinner_tree_returns_false() {
        // The early-out: with show_spinner_tree=false, always return false
        let tasks = [fti(TaskKind::InProcessTeammate, true, false)];
        assert!(!should_hide_tasks_footer(&tasks, false, false));
        assert!(!should_hide_tasks_footer(&[], false, false));
    }

    #[test]
    fn footer_no_visible_tasks_returns_false() {
        // No background tasks at all
        let tasks = [fti(TaskKind::LocalShell, false, false)];
        assert!(!should_hide_tasks_footer(&tasks, true, false));
    }

    #[test]
    fn footer_only_teammates_returns_true() {
        let tasks = [
            fti(TaskKind::InProcessTeammate, true, false),
            fti(TaskKind::InProcessTeammate, true, false),
        ];
        assert!(should_hide_tasks_footer(&tasks, true, false));
    }

    #[test]
    fn footer_mixed_returns_false() {
        let tasks = [
            fti(TaskKind::InProcessTeammate, true, false),
            fti(TaskKind::LocalShell, true, false),
        ];
        assert!(!should_hide_tasks_footer(&tasks, true, false));
    }

    #[test]
    fn footer_internal_build_filters_panel_agents() {
        // On an internal build, panel agents are excluded so a
        // remaining-only-teammate set still hides the footer.
        let tasks = [
            fti(TaskKind::LocalAgent, true, true),
            fti(TaskKind::InProcessTeammate, true, false),
        ];
        assert!(should_hide_tasks_footer(&tasks, true, true));
        // But not otherwise (panel agents stay visible and
        // they're not teammates → return false).
        assert!(!should_hide_tasks_footer(&tasks, true, false));
    }

    #[test]
    fn status_icon_glyph_keys() {
        assert_eq!(StatusIcon::Tick.glyph_key(), "tick");
        assert_eq!(StatusIcon::Cross.glyph_key(), "cross");
        assert_eq!(StatusIcon::Play.glyph_key(), "play");
        assert_eq!(StatusIcon::Ellipsis.glyph_key(), "ellipsis");
        assert_eq!(StatusIcon::Bullet.glyph_key(), "bullet");
        assert_eq!(StatusIcon::Warning.glyph_key(), "warning");
        assert_eq!(
            StatusIcon::QuestionMarkPrefix.glyph_key(),
            "questionMarkPrefix"
        );
    }
}
