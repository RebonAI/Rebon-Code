//! Shell progress projection.
//!
//! Two exports:
//!
//! * [`task_status_text`] — pure projection from a `(status, label,
//!   suffix)` triple to a `(display_label, color)` pair. The consumer
//!   wraps the pair in parens and dims it; this module returns the inner
//!   string plus the colour so the model stays free of design-system
//!   primitives.
//! * [`shell_progress`] — maps a shell-task status to a
//!   fixed `(label, color)` pair.

use crate::ui::tasks::common::{SemanticColor, TaskStatus};

/// Projection result for a `(label?, suffix?)` task status row.
///
/// The renderer displays `({display_label}{suffix})` dimmed, with the
/// color set per status. We expose the bare parts
/// so the consumer can format the parens / dim itself — that keeps the
/// model free of design-system primitives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellProgressLine {
    /// `display_label = label fallback status.as_str()`. The projection falls
    /// through to the literal status string when no label is provided.
    pub display_label: String,
    /// Optional trailing suffix appended *after* the label, before the
    /// closing paren. Empty string in the common case.
    pub suffix: String,
    /// Semantic color for the rendered text. `None` is the dimmed
    /// default.
    pub color: Option<SemanticColor>,
}

/// Project a task status plus optional label and suffix.
///
/// Color rule:
/// * `completed` → success
/// * `failed`    → error
/// * `killed`    → warning
/// * else        → no explicit color (the consumer paints with the
///   default dim style)
pub fn task_status_text(
    status: TaskStatus,
    label: Option<&str>,
    suffix: Option<&str>,
) -> ShellProgressLine {
    let display_label = label
        .map(str::to_owned)
        .unwrap_or_else(|| status.as_str().to_owned());
    let color = match status {
        TaskStatus::Completed => Some(SemanticColor::Success),
        TaskStatus::Failed => Some(SemanticColor::Error),
        TaskStatus::Killed => Some(SemanticColor::Warning),
        TaskStatus::Pending | TaskStatus::Running => None,
    };
    ShellProgressLine {
        display_label,
        suffix: suffix.unwrap_or("").to_owned(),
        color,
    }
}

/// Map a shell-task status to a label.
///
/// * `completed` → label `"done"`
/// * `failed`    → label `"error"`
/// * `killed`    → label `"stopped"`
/// * `running` | `pending` → no label, so the status' own string is used
pub fn shell_progress(status: TaskStatus) -> ShellProgressLine {
    match status {
        TaskStatus::Completed => task_status_text(TaskStatus::Completed, Some("done"), None),
        TaskStatus::Failed => task_status_text(TaskStatus::Failed, Some("error"), None),
        TaskStatus::Killed => task_status_text(TaskStatus::Killed, Some("stopped"), None),
        TaskStatus::Running | TaskStatus::Pending => {
            task_status_text(TaskStatus::Running, None, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_status_text_label_fallback_to_status() {
        let line = task_status_text(TaskStatus::Running, None, None);
        assert_eq!(line.display_label, "running");
        assert_eq!(line.suffix, "");
        assert_eq!(line.color, None);
    }

    #[test]
    fn task_status_text_explicit_label_wins() {
        let line = task_status_text(TaskStatus::Completed, Some("done"), None);
        assert_eq!(line.display_label, "done");
        assert_eq!(line.color, Some(SemanticColor::Success));
    }

    #[test]
    fn task_status_text_suffix_passes_through() {
        let line = task_status_text(TaskStatus::Completed, Some("done"), Some(", unread"));
        assert_eq!(line.suffix, ", unread");
    }

    #[test]
    fn task_status_text_color_table() {
        assert_eq!(
            task_status_text(TaskStatus::Completed, None, None).color,
            Some(SemanticColor::Success)
        );
        assert_eq!(
            task_status_text(TaskStatus::Failed, None, None).color,
            Some(SemanticColor::Error)
        );
        assert_eq!(
            task_status_text(TaskStatus::Killed, None, None).color,
            Some(SemanticColor::Warning)
        );
        assert_eq!(
            task_status_text(TaskStatus::Running, None, None).color,
            None
        );
        assert_eq!(
            task_status_text(TaskStatus::Pending, None, None).color,
            None
        );
    }

    #[test]
    fn shell_progress_completed() {
        let line = shell_progress(TaskStatus::Completed);
        assert_eq!(line.display_label, "done");
        assert_eq!(line.color, Some(SemanticColor::Success));
    }

    #[test]
    fn shell_progress_failed() {
        let line = shell_progress(TaskStatus::Failed);
        assert_eq!(line.display_label, "error");
        assert_eq!(line.color, Some(SemanticColor::Error));
    }

    #[test]
    fn shell_progress_killed() {
        let line = shell_progress(TaskStatus::Killed);
        assert_eq!(line.display_label, "stopped");
        assert_eq!(line.color, Some(SemanticColor::Warning));
    }

    #[test]
    fn shell_progress_running_and_pending_collapse_to_running() {
        let r = shell_progress(TaskStatus::Running);
        let p = shell_progress(TaskStatus::Pending);
        assert_eq!(r, p);
        assert_eq!(r.display_label, "running");
    }
}
