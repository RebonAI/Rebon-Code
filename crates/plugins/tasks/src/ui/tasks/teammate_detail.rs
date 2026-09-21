//! In-process teammate detail dialog projection.
//!
//! Mostly the same shape as the async-agent detail dialog with two
//! extra wrinkles:
//!
//! 1. The activity string is computed via
//!    [`crate::ui::tasks::status_utils::describe_teammate_activity`].
//! 2. There's an extra `f` keybinding for "foreground".
//! 3. The status pill uses a different palette (success/warning/error
//!    instead of the generic `task_status_color`).

use crate::ui::tasks::common::{SemanticColor, TaskStatus};
use crate::ui::tasks::status_utils::{describe_teammate_activity, TeammateActivityInput};

const PROMPT_DISPLAY_MAX: usize = 300;

/// Pre-built input for [`build_teammate_detail`].
#[derive(Debug, Clone)]
pub struct TeammateDetailInput {
    /// The teammate's agent name. Rendered as `@{name}`.
    pub agent_name: String,
    /// The teammate's color, pre-projected to a string color name.
    pub agent_color: Option<String>,
    /// The teammate task's status.
    pub status: TaskStatus,
    /// Pre-formatted elapsed-time string.
    pub elapsed_time: String,
    /// Token count for the subtitle. The caller resolves it from the
    /// teammate's result, falling back to its progress counter; `None`
    /// when neither is available.
    pub token_count: Option<u64>,
    /// Tool-use count for the subtitle. The caller resolves it from the
    /// teammate's result, falling back to its progress counter; `None`
    /// when neither is available.
    pub tool_use_count: Option<u64>,
    /// The teammate's prompt. Truncated to 300 chars before display.
    pub prompt: String,
    /// The teammate's error text, if any — only displayed when the
    /// status is `TaskStatus::Failed`.
    pub error: Option<String>,
    /// True when the dialog can stop the teammate.
    pub can_kill: bool,
    /// True when the dialog can go back to a parent view.
    pub can_back: bool,
    /// True when the dialog can bring the teammate to the foreground.
    pub can_foreground: bool,
    /// Activity-state input — see
    /// [`crate::ui::tasks::status_utils::TeammateActivityInput`].
    pub activity: TeammateActivityInput,
}

/// Projected teammate detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeammateDetail {
    /// `@{agent_name}` + `(activity)` parts. The consumer paints the
    /// `@{name}` chunk in `agent_color` and the `(activity)` chunk
    /// dimmed.
    pub title: TeammateDetailTitle,
    /// Subtitle row.
    pub subtitle: TeammateDetailSubtitle,
    /// Truncated prompt body.
    pub prompt: String,
    /// Optional error block (failed status only).
    pub error: Option<String>,
    /// Byline hints.
    pub byline: TeammateByline,
}

/// Title row parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeammateDetailTitle {
    /// `@{agent_name}` text.
    pub agent_label: String,
    /// `agent_color` (pre-projected terminal color name) — `None` falls
    /// back to the consumer's default.
    pub agent_color: Option<String>,
    /// `(activity)` suffix when activity is non-empty; `None` when it
    /// is empty.
    pub activity_suffix: Option<String>,
}

/// Subtitle row parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeammateDetailSubtitle {
    /// `Completed · ` / `Failed · ` / `Stopped · ` label, plus the
    /// color used to render the label.
    pub status_label: Option<(String, SemanticColor)>,
    /// `elapsed_time`.
    pub elapsed_time: String,
    /// `· {n} tokens` suffix when token_count > 0.
    pub tokens_suffix: Option<String>,
    /// `· {n} tool[s]` suffix when tool_use_count > 0.
    pub tools_suffix: Option<String>,
}

/// Byline hints — has the extra `f` foreground hint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TeammateByline {
    /// Show the `← go back` hint.
    pub show_back: bool,
    /// Always shown.
    pub show_close: bool,
    /// Show the `x stop` hint.
    pub show_stop: bool,
    /// Show the `f foreground` hint.
    pub show_foreground: bool,
}

/// Keyboard event for the teammate dialog. Adds an `f` key compared
/// to the async-agent dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeammateDetailEvent {
    /// Space key — close the dialog.
    Space,
    /// Left-arrow key — back to parent.
    Left,
    /// `x` key — kill the teammate.
    XKey,
    /// `f` key — bring the teammate to the foreground.
    FKey,
}

/// Action for the teammate dialog reducer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeammateDetailAction {
    /// Close the dialog.
    Done,
    /// Go back to the parent view.
    Back,
    /// Stop the teammate.
    Kill,
    /// Bring the teammate to the foreground.
    Foreground,
    /// Event ignored — gating flag was off.
    Ignore,
}

/// Reducer for the teammate-detail dialog.
pub fn handle_teammate_event(
    event: TeammateDetailEvent,
    status: TaskStatus,
    can_back: bool,
    can_kill: bool,
    can_foreground: bool,
) -> TeammateDetailAction {
    match event {
        TeammateDetailEvent::Space => TeammateDetailAction::Done,
        TeammateDetailEvent::Left => {
            if can_back {
                TeammateDetailAction::Back
            } else {
                TeammateDetailAction::Ignore
            }
        }
        TeammateDetailEvent::XKey => {
            if status == TaskStatus::Running && can_kill {
                TeammateDetailAction::Kill
            } else {
                TeammateDetailAction::Ignore
            }
        }
        TeammateDetailEvent::FKey => {
            if status == TaskStatus::Running && can_foreground {
                TeammateDetailAction::Foreground
            } else {
                TeammateDetailAction::Ignore
            }
        }
    }
}

fn truncate_prompt(prompt: &str) -> String {
    if prompt.chars().count() <= PROMPT_DISPLAY_MAX {
        return prompt.to_owned();
    }
    let head: String = prompt.chars().take(PROMPT_DISPLAY_MAX - 1).collect();
    format!("{head}…")
}

/// Project the teammate detail dialog.
pub fn build_teammate_detail(input: &TeammateDetailInput) -> TeammateDetail {
    let activity = describe_teammate_activity(&input.activity);
    let activity_suffix = if activity.is_empty() {
        None
    } else {
        Some(format!(" ({activity})"))
    };

    let title = TeammateDetailTitle {
        agent_label: format!("@{}", input.agent_name),
        agent_color: input.agent_color.clone(),
        activity_suffix,
    };

    let status_label = match input.status {
        TaskStatus::Running => None,
        TaskStatus::Completed => Some(("Completed · ".into(), SemanticColor::Success)),
        TaskStatus::Killed => Some(("Stopped · ".into(), SemanticColor::Warning)),
        TaskStatus::Failed | TaskStatus::Pending => {
            // Anything that is not running, completed or killed uses the
            // error palette; only `Failed` is labelled so.
            let label = if input.status == TaskStatus::Failed {
                "Failed · "
            } else {
                "Stopped · "
            };
            Some((label.into(), SemanticColor::Error))
        }
    };

    let tokens_suffix = match input.token_count {
        Some(n) if n > 0 => Some(format!(" · {n} tokens")),
        _ => None,
    };

    let tools_suffix = match input.tool_use_count {
        Some(n) if n > 0 => {
            let word = if n == 1 { "tool" } else { "tools" };
            Some(format!(" · {n} {word}"))
        }
        _ => None,
    };

    let subtitle = TeammateDetailSubtitle {
        status_label,
        elapsed_time: input.elapsed_time.clone(),
        tokens_suffix,
        tools_suffix,
    };

    let prompt = truncate_prompt(&input.prompt);

    let error = if input.status == TaskStatus::Failed {
        input.error.as_ref().filter(|s| !s.is_empty()).cloned()
    } else {
        None
    };

    let byline = TeammateByline {
        show_back: input.can_back,
        show_close: true,
        show_stop: input.status == TaskStatus::Running && input.can_kill,
        show_foreground: input.status == TaskStatus::Running && input.can_foreground,
    };

    TeammateDetail {
        title,
        subtitle,
        prompt,
        error,
        byline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(status: TaskStatus) -> TeammateDetailInput {
        TeammateDetailInput {
            agent_name: "researcher".into(),
            agent_color: Some("blue".into()),
            status,
            elapsed_time: "00:30".into(),
            token_count: None,
            tool_use_count: None,
            prompt: "find the bug".into(),
            error: None,
            can_kill: false,
            can_back: false,
            can_foreground: false,
            activity: TeammateActivityInput::default(),
        }
    }

    #[test]
    fn handle_event_space_always_done() {
        for status in [TaskStatus::Running, TaskStatus::Completed] {
            assert_eq!(
                handle_teammate_event(TeammateDetailEvent::Space, status, false, false, false),
                TeammateDetailAction::Done
            );
        }
    }

    #[test]
    fn handle_event_left_requires_can_back() {
        assert_eq!(
            handle_teammate_event(
                TeammateDetailEvent::Left,
                TaskStatus::Running,
                false,
                false,
                false
            ),
            TeammateDetailAction::Ignore
        );
        assert_eq!(
            handle_teammate_event(
                TeammateDetailEvent::Left,
                TaskStatus::Running,
                true,
                false,
                false
            ),
            TeammateDetailAction::Back
        );
    }

    #[test]
    fn handle_event_x_requires_running() {
        assert_eq!(
            handle_teammate_event(
                TeammateDetailEvent::XKey,
                TaskStatus::Completed,
                false,
                true,
                false
            ),
            TeammateDetailAction::Ignore
        );
        assert_eq!(
            handle_teammate_event(
                TeammateDetailEvent::XKey,
                TaskStatus::Running,
                false,
                true,
                false
            ),
            TeammateDetailAction::Kill
        );
    }

    #[test]
    fn handle_event_f_requires_running() {
        assert_eq!(
            handle_teammate_event(
                TeammateDetailEvent::FKey,
                TaskStatus::Completed,
                false,
                false,
                true
            ),
            TeammateDetailAction::Ignore
        );
        assert_eq!(
            handle_teammate_event(
                TeammateDetailEvent::FKey,
                TaskStatus::Running,
                false,
                false,
                true
            ),
            TeammateDetailAction::Foreground
        );
        // f without can_foreground
        assert_eq!(
            handle_teammate_event(
                TeammateDetailEvent::FKey,
                TaskStatus::Running,
                false,
                false,
                false
            ),
            TeammateDetailAction::Ignore
        );
    }

    #[test]
    fn title_with_activity() {
        let mut i = input(TaskStatus::Running);
        i.activity.is_idle = true;
        let d = build_teammate_detail(&i);
        assert_eq!(d.title.agent_label, "@researcher");
        assert_eq!(d.title.activity_suffix, Some(" (idle)".into()));
    }

    #[test]
    fn title_without_activity() {
        // describe_teammate_activity always returns at least "working",
        // never empty — so the suffix is always present in practice.
        // We pin "working" here to confirm the format.
        let i = input(TaskStatus::Running);
        let d = build_teammate_detail(&i);
        assert_eq!(d.title.activity_suffix, Some(" (working)".into()));
    }

    #[test]
    fn status_label_table() {
        let cases = [
            (TaskStatus::Running, None),
            (
                TaskStatus::Completed,
                Some(("Completed · ".to_owned(), SemanticColor::Success)),
            ),
            (
                TaskStatus::Killed,
                Some(("Stopped · ".to_owned(), SemanticColor::Warning)),
            ),
            (
                TaskStatus::Failed,
                Some(("Failed · ".to_owned(), SemanticColor::Error)),
            ),
        ];
        for (status, expected) in cases {
            let d = build_teammate_detail(&input(status));
            assert_eq!(d.subtitle.status_label, expected);
        }
    }

    #[test]
    fn tokens_and_tools_suffix() {
        let mut i = input(TaskStatus::Running);
        i.token_count = Some(2500);
        i.tool_use_count = Some(1);
        let d = build_teammate_detail(&i);
        assert_eq!(d.subtitle.tokens_suffix, Some(" · 2500 tokens".into()));
        assert_eq!(d.subtitle.tools_suffix, Some(" · 1 tool".into()));

        i.tool_use_count = Some(3);
        let d = build_teammate_detail(&i);
        assert_eq!(d.subtitle.tools_suffix, Some(" · 3 tools".into()));
    }

    #[test]
    fn truncate_prompt_long() {
        let mut i = input(TaskStatus::Running);
        i.prompt = "x".repeat(400);
        let d = build_teammate_detail(&i);
        assert_eq!(d.prompt.chars().count(), PROMPT_DISPLAY_MAX);
        assert!(d.prompt.ends_with('…'));
    }

    #[test]
    fn truncate_prompt_short_passes_through() {
        let i = input(TaskStatus::Running);
        let d = build_teammate_detail(&i);
        assert_eq!(d.prompt, "find the bug");
    }

    #[test]
    fn error_only_when_failed() {
        let mut i = input(TaskStatus::Running);
        i.error = Some("oops".into());
        let d = build_teammate_detail(&i);
        assert!(d.error.is_none());

        i.status = TaskStatus::Failed;
        let d = build_teammate_detail(&i);
        assert_eq!(d.error.as_deref(), Some("oops"));
    }

    #[test]
    fn byline_running_with_all_actions() {
        let mut i = input(TaskStatus::Running);
        i.can_back = true;
        i.can_kill = true;
        i.can_foreground = true;
        let d = build_teammate_detail(&i);
        assert!(d.byline.show_back);
        assert!(d.byline.show_close);
        assert!(d.byline.show_stop);
        assert!(d.byline.show_foreground);
    }

    #[test]
    fn byline_completed_hides_stop_and_foreground() {
        let mut i = input(TaskStatus::Completed);
        i.can_kill = true;
        i.can_foreground = true;
        let d = build_teammate_detail(&i);
        assert!(!d.byline.show_stop);
        assert!(!d.byline.show_foreground);
    }
}
