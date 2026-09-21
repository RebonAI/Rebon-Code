//! The two task status vocabularies, defined once.
//!
//! A "task" means two different things here, and each has its own status
//! vocabulary. The names are deliberately distinct so the two cannot be
//! confused:
//!
//! * [`TaskStatus`] — the lifecycle of a *background job* owned by the task
//!   runtime (a shell command, a spawned agent, a teammate, a monitor). Five
//!   states, persisted as `pending` / `running` / `completed` / `failed` /
//!   `killed`.
//! * [`TaskListStatus`] — the state of one *row in the user-facing task list*
//!   (the `Task*` tools that replaced `TodoWrite`). Three states, persisted as
//!   `pending` / `in_progress` / `completed`.
//!
//! The two are not projections of one another: a task-list row is never
//! `killed`, and a background job has no notion of being blocked by another
//! job. They share only the words "pending" and "completed".
//!
//! [`ListTask`] is the row shape every task-list surface reads — the reducer,
//! the spinner's next-task planner, and the prompt-input list view — one
//! shared shape rather than a private copy in each.

use core::fmt;

use serde::{Deserialize, Serialize};

/// Lifecycle status of a background task owned by the task runtime.
///
/// The wire form is the persisted status literal; `cancelled` is accepted on
/// read as a legacy spelling of [`TaskStatus::Killed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskStatus {
    /// Task has been registered but has not produced any event yet.
    Pending,
    /// Task is actively running.
    Running,
    /// Task reached a successful terminal state.
    Completed,
    /// Task failed with an error.
    Failed,
    /// Task was killed by a cancel handle or a `stop_task` side effect.
    /// Uses the persisted `killed` literal.
    Killed,
}

impl TaskStatus {
    /// Whether this status is terminal.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Killed | Self::Failed)
    }

    /// String form for persisted status literals.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Running => "running",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Killed => "killed",
        }
    }

    /// Round-trip parser. Returns `None` for unknown variants.
    ///
    /// `cancelled` is accepted as a legacy alias for `killed`; everything the
    /// runtime writes today round-trips through [`TaskStatus::as_str`].
    pub fn from_str(s: &str) -> Option<TaskStatus> {
        Some(match s {
            "pending" => TaskStatus::Pending,
            "running" => TaskStatus::Running,
            "completed" => TaskStatus::Completed,
            "failed" => TaskStatus::Failed,
            "killed" | "cancelled" => TaskStatus::Killed,
            _ => return None,
        })
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Status of one row in the user-facing task list.
///
/// Serialized form is the persisted literal: `pending`, `in_progress`,
/// `completed`. This is the on-disk shape of a stored task, so the strings are
/// a compatibility contract.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskListStatus {
    /// Not yet started.
    #[default]
    Pending,
    /// Being worked on.
    InProgress,
    /// Done.
    Completed,
}

impl TaskListStatus {
    /// String form matching the serialized literal.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskListStatus::Pending => "pending",
            TaskListStatus::InProgress => "in_progress",
            TaskListStatus::Completed => "completed",
        }
    }

    /// Round-trip parser. Returns `None` for unknown variants.
    pub fn from_str(s: &str) -> Option<TaskListStatus> {
        Some(match s {
            "pending" => TaskListStatus::Pending,
            "in_progress" => TaskListStatus::InProgress,
            "completed" => TaskListStatus::Completed,
            _ => return None,
        })
    }
}

impl fmt::Display for TaskListStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row of the task list, trimmed to the fields every list surface reads.
///
/// The reducer that orders and truncates the list, the planner that picks the
/// next unblocked task, and the prompt-input view that lays it out all consume
/// exactly these five fields. The full stored task (description, active form,
/// metadata) lives in the task tool's own `Task` type, which converts into
/// this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListTask {
    /// Task id. Numeric ids sort numerically; anything else sorts as text.
    pub id: String,
    /// Short subject line.
    pub subject: String,
    /// Current status.
    pub status: TaskListStatus,
    /// Agent that owns this task, if any.
    pub owner: Option<String>,
    /// Ids of tasks that block this one.
    pub blocked_by: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_status_round_trip() {
        for s in ["pending", "running", "completed", "failed", "killed"] {
            let st = TaskStatus::from_str(s).unwrap();
            assert_eq!(st.as_str(), s);
        }
        assert!(TaskStatus::from_str("unknown").is_none());
        assert!(TaskStatus::from_str("").is_none());
        assert!(TaskStatus::from_str("PENDING").is_none(), "case-sensitive");
    }

    #[test]
    fn cancelled_is_a_legacy_alias_for_killed() {
        assert_eq!(TaskStatus::from_str("cancelled"), Some(TaskStatus::Killed));
        assert_eq!(TaskStatus::Killed.as_str(), "killed");
    }

    #[test]
    fn task_status_terminality() {
        assert!(!TaskStatus::Pending.is_terminal());
        assert!(!TaskStatus::Running.is_terminal());
        assert!(TaskStatus::Completed.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Killed.is_terminal());
    }

    #[test]
    fn task_status_display() {
        assert_eq!(format!("{}", TaskStatus::Running), "running");
    }

    #[test]
    fn task_list_status_round_trip() {
        for s in ["pending", "in_progress", "completed"] {
            let st = TaskListStatus::from_str(s).unwrap();
            assert_eq!(st.as_str(), s);
        }
        assert!(TaskListStatus::from_str("running").is_none());
        assert!(TaskListStatus::from_str("inProgress").is_none());
    }

    #[test]
    fn task_list_status_wire_form_is_snake_case() {
        for (status, wire) in [
            (TaskListStatus::Pending, "\"pending\""),
            (TaskListStatus::InProgress, "\"in_progress\""),
            (TaskListStatus::Completed, "\"completed\""),
        ] {
            assert_eq!(serde_json::to_string(&status).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<TaskListStatus>(wire).unwrap(),
                status
            );
        }
    }

    #[test]
    fn task_list_status_defaults_to_pending() {
        assert_eq!(TaskListStatus::default(), TaskListStatus::Pending);
    }
}
