//! Background task one-line projection.
//!
//! The formatter maps each
//! background-task kind to a single line of text + a status pill.
//! We model the line as a discriminated enum so the test matrix can
//! pin every branch.

use crate::ui::tasks::common::{TaskKind, TaskStatus};
use crate::ui::tasks::dream_detail::plural;
use crate::ui::tasks::remote_progress::{RemoteProgressLine, RemoteSessionInput};
use crate::ui::tasks::shell_progress::{shell_progress, task_status_text, ShellProgressLine};

const DEFAULT_ACTIVITY_LIMIT: usize = 40;

/// Discriminated background-task line, one variant per task kind (the
/// remote agent has two).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundTaskLine {
    /// `local_bash` — `"{display}{progress}"`. The display is either
    /// the task's description (when the kind is `monitor`) or its
    /// command. The progress is the [`shell_progress`] line.
    LocalBash {
        /// Truncated description / command.
        display: String,
        /// Status pill.
        progress: ShellProgressLine,
    },
    /// `remote_agent` — review-only path. Triggered when the task is a
    /// remote review. The line is the embedded
    /// rainbow [`RemoteProgressLine`].
    RemoteAgentReview {
        /// The embedded rainbow line.
        progress: RemoteProgressLine,
    },
    /// `remote_agent` — non-review path: `"{title} · {progress}"`
    /// with the leading running/idle diamond glyph.
    RemoteAgent {
        /// The task title, truncated.
        title: String,
        /// True when status is running or pending — the consumer
        /// uses this to pick the diamond glyph (open vs filled).
        running: bool,
        /// Embedded progress line.
        progress: RemoteProgressLine,
    },
    /// `local_agent` — async-agent task line.
    LocalAgent {
        /// The task description, truncated.
        description: String,
        /// Status pill.
        progress: ShellProgressLine,
    },
    /// `in_process_teammate` — `@{name}: {activity}` line.
    InProcessTeammate {
        /// `@{agent_name}` string.
        agent_label: String,
        /// Pre-projected agent color (e.g. `"blue"`) — `None` if
        /// the consumer should use the default color.
        agent_color: Option<String>,
        /// Truncated activity description.
        activity: String,
    },
    /// `local_workflow` — `"{display}{progress}"` where the progress
    /// label is `"{n} agents"` while running, `"done"` while
    /// completed, `None` otherwise.
    LocalWorkflow {
        /// Truncated display text (workflow name, else summary, else
        /// description).
        display: String,
        /// Status pill.
        progress: ShellProgressLine,
    },
    /// `monitor_mcp` — same shape as `local_agent`.
    MonitorMcp {
        /// The task description, truncated.
        description: String,
        /// Status pill.
        progress: ShellProgressLine,
    },
    /// `dream` — `"{description} · {phase} · {detail} · {progress}"`.
    Dream {
        /// The task description.
        description: String,
        /// The dream's current phase.
        phase: String,
        /// `{n} files` (when phase is updating and files > 0) or
        /// `{n} sessions`.
        detail: String,
        /// Status pill.
        progress: ShellProgressLine,
    },
}

/// Generic input for the in-process teammate variant.
#[derive(Debug, Clone)]
pub struct TeammateLineInput {
    /// The teammate's agent name.
    pub agent_name: String,
    /// The teammate's color name.
    pub agent_color: Option<String>,
    /// Pre-computed activity (already passed through
    /// [`crate::ui::tasks::status_utils::describe_teammate_activity`]).
    pub activity: String,
}

/// Truncate to `max_chars`, ending in `…` when cut. We use a
/// char count rather than display-width math to avoid pulling in
/// `unicode-width`; the model's tests pin the byte/char-count
/// behavior, and the consumer can swap in a width-aware truncator
/// without changing the line shape.
pub fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_owned();
    }
    if max_chars == 0 {
        return String::new();
    }
    let head: String = s.chars().take(max_chars - 1).collect();
    format!("{head}…")
}

/// Pre-built input for [`format_local_bash_line`].
#[derive(Debug, Clone)]
pub struct LocalBashLineInput {
    /// True when the task kind is `monitor`.
    pub is_monitor: bool,
    /// The task description (only used when `is_monitor`).
    pub description: String,
    /// The shell command (used when not monitor).
    pub command: String,
    /// The task's status.
    pub status: TaskStatus,
}

/// Project a `local_bash` task to a [`BackgroundTaskLine::LocalBash`].
pub fn format_local_bash_line(
    input: &LocalBashLineInput,
    activity_limit: usize,
) -> BackgroundTaskLine {
    let raw = if input.is_monitor {
        &input.description
    } else {
        &input.command
    };
    BackgroundTaskLine::LocalBash {
        display: truncate_with_ellipsis(raw, activity_limit),
        progress: shell_progress(input.status),
    }
}

/// Pre-built input for [`format_remote_agent_line`].
#[derive(Debug, Clone)]
pub struct RemoteAgentLineInput {
    /// The task title.
    pub title: String,
    /// Pre-built [`RemoteSessionInput`] forwarded to
    /// [`crate::ui::tasks::remote_progress::format_remote_session_progress`].
    pub session: RemoteSessionInput,
}

/// Project a `remote_agent` task to either [`BackgroundTaskLine::RemoteAgent`]
/// or [`BackgroundTaskLine::RemoteAgentReview`].
pub fn format_remote_agent_line(
    input: &RemoteAgentLineInput,
    activity_limit: usize,
) -> BackgroundTaskLine {
    use crate::ui::tasks::remote_progress::format_remote_session_progress;
    let progress = format_remote_session_progress(&input.session);
    if input.session.is_remote_review {
        return BackgroundTaskLine::RemoteAgentReview { progress };
    }
    let running = matches!(
        input.session.status,
        TaskStatus::Running | TaskStatus::Pending
    );
    BackgroundTaskLine::RemoteAgent {
        title: truncate_with_ellipsis(&input.title, activity_limit),
        running,
        progress,
    }
}

/// Pre-built input for [`format_local_agent_line`].
#[derive(Debug, Clone)]
pub struct LocalAgentLineInput {
    /// The task description.
    pub description: String,
    /// The task's status.
    pub status: TaskStatus,
    /// Whether the completion was already reported — when false and the
    /// status is `Completed`, the "(done, unread)" suffix is appended.
    pub notified: bool,
}

/// Project a `local_agent` task.
pub fn format_local_agent_line(
    input: &LocalAgentLineInput,
    activity_limit: usize,
) -> BackgroundTaskLine {
    let label = if input.status == TaskStatus::Completed {
        Some("done")
    } else {
        None
    };
    let suffix = if input.status == TaskStatus::Completed && !input.notified {
        Some(", unread")
    } else {
        None
    };
    BackgroundTaskLine::LocalAgent {
        description: truncate_with_ellipsis(&input.description, activity_limit),
        progress: task_status_text(input.status, label, suffix),
    }
}

/// Project an `in_process_teammate` task.
pub fn format_in_process_teammate_line(
    input: &TeammateLineInput,
    activity_limit: usize,
) -> BackgroundTaskLine {
    BackgroundTaskLine::InProcessTeammate {
        agent_label: format!("@{}", input.agent_name),
        agent_color: input.agent_color.clone(),
        activity: truncate_with_ellipsis(&input.activity, activity_limit),
    }
}

/// Pre-built input for [`format_local_workflow_line`].
#[derive(Debug, Clone)]
pub struct LocalWorkflowLineInput {
    /// Display text: the workflow name, else the summary, else the
    /// description.
    pub display: String,
    /// The task's status.
    pub status: TaskStatus,
    /// Number of agents the workflow is running.
    pub agent_count: u64,
    /// Whether the completion was already reported.
    pub notified: bool,
}

/// Project a `local_workflow` task.
pub fn format_local_workflow_line(
    input: &LocalWorkflowLineInput,
    activity_limit: usize,
) -> BackgroundTaskLine {
    let label_storage: String;
    let label: Option<&str> = if input.status == TaskStatus::Running {
        label_storage = format!(
            "{} {}",
            input.agent_count,
            plural(input.agent_count, "agent")
        );
        Some(label_storage.as_str())
    } else if input.status == TaskStatus::Completed {
        Some("done")
    } else {
        None
    };
    let suffix = if input.status == TaskStatus::Completed && !input.notified {
        Some(", unread")
    } else {
        None
    };
    BackgroundTaskLine::LocalWorkflow {
        display: truncate_with_ellipsis(&input.display, activity_limit),
        progress: task_status_text(input.status, label, suffix),
    }
}

/// Pre-built input for [`format_monitor_mcp_line`].
#[derive(Debug, Clone)]
pub struct MonitorMcpLineInput {
    /// The task description.
    pub description: String,
    /// The task's status.
    pub status: TaskStatus,
    /// Whether the completion was already reported.
    pub notified: bool,
}

/// Project a `monitor_mcp` task.
pub fn format_monitor_mcp_line(
    input: &MonitorMcpLineInput,
    activity_limit: usize,
) -> BackgroundTaskLine {
    let label = if input.status == TaskStatus::Completed {
        Some("done")
    } else {
        None
    };
    let suffix = if input.status == TaskStatus::Completed && !input.notified {
        Some(", unread")
    } else {
        None
    };
    BackgroundTaskLine::MonitorMcp {
        description: truncate_with_ellipsis(&input.description, activity_limit),
        progress: task_status_text(input.status, label, suffix),
    }
}

/// Pre-built input for [`format_dream_line`].
#[derive(Debug, Clone)]
pub struct DreamLineInput {
    /// The task description.
    pub description: String,
    /// The dream's current phase (e.g. `"reviewing"`, `"updating"`).
    pub phase: String,
    /// Number of sessions being reviewed.
    pub sessions_reviewing: u64,
    /// Number of files touched so far.
    pub files_touched: u64,
    /// The task's status.
    pub status: TaskStatus,
    /// Whether the completion was already reported.
    pub notified: bool,
}

/// Project a `dream` task.
pub fn format_dream_line(input: &DreamLineInput) -> BackgroundTaskLine {
    let detail = if input.phase == "updating" && input.files_touched > 0 {
        format!(
            "{} {}",
            input.files_touched,
            plural(input.files_touched, "file")
        )
    } else {
        format!(
            "{} {}",
            input.sessions_reviewing,
            plural(input.sessions_reviewing, "session")
        )
    };
    let label = if input.status == TaskStatus::Completed {
        Some("done")
    } else {
        None
    };
    let suffix = if input.status == TaskStatus::Completed && !input.notified {
        Some(", unread")
    } else {
        None
    };
    BackgroundTaskLine::Dream {
        description: input.description.clone(),
        phase: input.phase.clone(),
        detail,
        progress: task_status_text(input.status, label, suffix),
    }
}

/// Default activity-width limit (40 chars), used when the caller has
/// no width of its own.
pub fn default_activity_limit() -> usize {
    DEFAULT_ACTIVITY_LIMIT
}

/// Distinguish [`TaskKind`] without exposing the consumer to the
/// per-kind input shapes — convenience function for sanity checks.
pub fn classify(kind: TaskKind) -> &'static str {
    kind.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tasks::common::ReviewStage;
    use crate::ui::tasks::common::SemanticColor;
    use crate::ui::tasks::remote_progress::ReviewProgressInput;

    #[test]
    fn truncate_pass_through() {
        assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
        assert_eq!(truncate_with_ellipsis("hello", 5), "hello");
    }

    #[test]
    fn truncate_with_ellipsis_when_over_limit() {
        let s = "abcdefghij"; // 10 chars
                              // limit=5 → 4 chars + ellipsis = 5
        assert_eq!(truncate_with_ellipsis(s, 5).chars().count(), 5);
        assert!(truncate_with_ellipsis(s, 5).ends_with('…'));
    }

    #[test]
    fn truncate_zero_returns_empty() {
        assert_eq!(truncate_with_ellipsis("abc", 0), "");
    }

    #[test]
    fn local_bash_command_branch() {
        let input = LocalBashLineInput {
            is_monitor: false,
            description: "watch logs".into(),
            command: "ls -la".into(),
            status: TaskStatus::Running,
        };
        let line = format_local_bash_line(&input, 40);
        match line {
            BackgroundTaskLine::LocalBash { display, progress } => {
                assert_eq!(display, "ls -la");
                assert_eq!(progress.display_label, "running");
            }
            _ => panic!("expected LocalBash"),
        }
    }

    #[test]
    fn local_bash_monitor_branch_uses_description() {
        let input = LocalBashLineInput {
            is_monitor: true,
            description: "watch logs".into(),
            command: "ls".into(),
            status: TaskStatus::Completed,
        };
        let line = format_local_bash_line(&input, 40);
        match line {
            BackgroundTaskLine::LocalBash { display, progress } => {
                assert_eq!(display, "watch logs");
                assert_eq!(progress.display_label, "done");
                assert_eq!(progress.color, Some(SemanticColor::Success));
            }
            _ => panic!(),
        }
    }

    fn session(status: TaskStatus, is_review: bool) -> RemoteSessionInput {
        RemoteSessionInput {
            status,
            is_remote_review: is_review,
            todo_completed: 0,
            todo_total: 0,
            review: None,
        }
    }

    #[test]
    fn remote_agent_review_branch() {
        let input = RemoteAgentLineInput {
            title: "review".into(),
            session: RemoteSessionInput {
                review: Some(ReviewProgressInput {
                    stage: Some(ReviewStage::Verifying),
                    bugs_found: 3,
                    bugs_verified: 2,
                    bugs_refuted: 0,
                }),
                ..session(TaskStatus::Running, true)
            },
        };
        let line = format_remote_agent_line(&input, 40);
        assert!(matches!(line, BackgroundTaskLine::RemoteAgentReview { .. }));
    }

    #[test]
    fn remote_agent_non_review_running() {
        let input = RemoteAgentLineInput {
            title: "feature work".into(),
            session: RemoteSessionInput {
                todo_completed: 1,
                todo_total: 3,
                ..session(TaskStatus::Running, false)
            },
        };
        let line = format_remote_agent_line(&input, 40);
        match line {
            BackgroundTaskLine::RemoteAgent {
                title,
                running,
                progress,
            } => {
                assert_eq!(title, "feature work");
                assert!(running);
                assert!(matches!(
                    progress,
                    RemoteProgressLine::TodoCounts {
                        completed: 1,
                        total: 3
                    }
                ));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn remote_agent_non_review_completed_not_running() {
        let input = RemoteAgentLineInput {
            title: "done work".into(),
            session: session(TaskStatus::Completed, false),
        };
        let line = format_remote_agent_line(&input, 40);
        match line {
            BackgroundTaskLine::RemoteAgent { running, .. } => {
                assert!(!running);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn local_agent_unread_suffix_when_completed_not_notified() {
        let input = LocalAgentLineInput {
            description: "agent task".into(),
            status: TaskStatus::Completed,
            notified: false,
        };
        let line = format_local_agent_line(&input, 40);
        match line {
            BackgroundTaskLine::LocalAgent { progress, .. } => {
                assert_eq!(progress.display_label, "done");
                assert_eq!(progress.suffix, ", unread");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn local_agent_completed_notified_no_suffix() {
        let input = LocalAgentLineInput {
            description: "agent task".into(),
            status: TaskStatus::Completed,
            notified: true,
        };
        let line = format_local_agent_line(&input, 40);
        match line {
            BackgroundTaskLine::LocalAgent { progress, .. } => {
                assert_eq!(progress.suffix, "");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn local_agent_running_no_label_no_suffix() {
        let input = LocalAgentLineInput {
            description: "agent task".into(),
            status: TaskStatus::Running,
            notified: true,
        };
        let line = format_local_agent_line(&input, 40);
        match line {
            BackgroundTaskLine::LocalAgent { progress, .. } => {
                assert_eq!(progress.display_label, "running");
                assert_eq!(progress.suffix, "");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn in_process_teammate_label_color() {
        let input = TeammateLineInput {
            agent_name: "researcher".into(),
            agent_color: Some("blue".into()),
            activity: "looking up references".into(),
        };
        let line = format_in_process_teammate_line(&input, 40);
        match line {
            BackgroundTaskLine::InProcessTeammate {
                agent_label,
                agent_color,
                activity,
            } => {
                assert_eq!(agent_label, "@researcher");
                assert_eq!(agent_color.as_deref(), Some("blue"));
                assert_eq!(activity, "looking up references");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn local_workflow_running_uses_agent_count() {
        let input = LocalWorkflowLineInput {
            display: "wf".into(),
            status: TaskStatus::Running,
            agent_count: 3,
            notified: true,
        };
        let line = format_local_workflow_line(&input, 40);
        match line {
            BackgroundTaskLine::LocalWorkflow { progress, .. } => {
                assert_eq!(progress.display_label, "3 agents");
            }
            _ => panic!(),
        }
        // singular branch
        let input2 = LocalWorkflowLineInput {
            agent_count: 1,
            ..input
        };
        let line = format_local_workflow_line(&input2, 40);
        match line {
            BackgroundTaskLine::LocalWorkflow { progress, .. } => {
                assert_eq!(progress.display_label, "1 agent");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn local_workflow_completed_label_done_with_unread() {
        let input = LocalWorkflowLineInput {
            display: "wf".into(),
            status: TaskStatus::Completed,
            agent_count: 5,
            notified: false,
        };
        let line = format_local_workflow_line(&input, 40);
        match line {
            BackgroundTaskLine::LocalWorkflow { progress, .. } => {
                assert_eq!(progress.display_label, "done");
                assert_eq!(progress.suffix, ", unread");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn dream_phase_updating_files_branch() {
        let input = DreamLineInput {
            description: "consolidating".into(),
            phase: "updating".into(),
            sessions_reviewing: 4,
            files_touched: 7,
            status: TaskStatus::Running,
            notified: true,
        };
        let line = format_dream_line(&input);
        match line {
            BackgroundTaskLine::Dream { detail, .. } => {
                assert_eq!(detail, "7 files");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn dream_phase_updating_no_files_falls_back_to_sessions() {
        let input = DreamLineInput {
            description: "consolidating".into(),
            phase: "updating".into(),
            sessions_reviewing: 4,
            files_touched: 0,
            status: TaskStatus::Running,
            notified: true,
        };
        let line = format_dream_line(&input);
        match line {
            BackgroundTaskLine::Dream { detail, .. } => {
                assert_eq!(detail, "4 sessions");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn dream_phase_reviewing_uses_sessions() {
        let input = DreamLineInput {
            description: "consolidating".into(),
            phase: "reviewing".into(),
            sessions_reviewing: 1,
            files_touched: 5,
            status: TaskStatus::Running,
            notified: true,
        };
        let line = format_dream_line(&input);
        match line {
            BackgroundTaskLine::Dream { detail, .. } => {
                assert_eq!(detail, "1 session");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn dream_completed_unread_suffix() {
        let input = DreamLineInput {
            description: "consolidating".into(),
            phase: "updating".into(),
            sessions_reviewing: 1,
            files_touched: 0,
            status: TaskStatus::Completed,
            notified: false,
        };
        let line = format_dream_line(&input);
        match line {
            BackgroundTaskLine::Dream { progress, .. } => {
                assert_eq!(progress.display_label, "done");
                assert_eq!(progress.suffix, ", unread");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn monitor_mcp_smoke() {
        let input = MonitorMcpLineInput {
            description: "fetching schemas".into(),
            status: TaskStatus::Running,
            notified: true,
        };
        let line = format_monitor_mcp_line(&input, 40);
        match line {
            BackgroundTaskLine::MonitorMcp {
                description,
                progress,
            } => {
                assert_eq!(description, "fetching schemas");
                assert_eq!(progress.display_label, "running");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn classify_round_trip() {
        for k in [
            TaskKind::LocalShell,
            TaskKind::Monitor,
            TaskKind::RemoteAgent,
            TaskKind::LocalAgent,
            TaskKind::LocalWorkflow,
            TaskKind::MonitorMcp,
            TaskKind::Dream,
            TaskKind::InProcessTeammate,
        ] {
            let s = classify(k);
            assert_eq!(TaskKind::from_str(s).unwrap(), k);
        }
    }

    #[test]
    fn default_limit_pinned() {
        assert_eq!(default_activity_limit(), 40);
    }
}
