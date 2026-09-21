//! Mailbox message models for task coordination.
//!
//! Defines shutdown requests, queued messages, mailbox rows, and
//! user-visible unread/count summaries for teammate message flows.
use crate::common::humanize_bool_count;

/// Parsed shutdown request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownRequestMessage {
    /// Sender.
    pub from: String,
    /// Optional reason.
    pub reason: Option<String>,
}

/// Parsed shutdown rejected response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownRejectedMessage {
    /// Sender.
    pub from: String,
    /// Rejection reason.
    pub reason: String,
}

/// Parsed task assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskAssignmentMessage {
    /// Task id.
    pub task_id: String,
    /// Assigner.
    pub assigned_by: String,
    /// Subject.
    pub subject: String,
    /// Optional description.
    pub description: Option<String>,
}

/// Display for shutdown request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownRequestDisplay {
    /// Title text.
    pub title: String,
    /// Optional reason.
    pub reason: Option<String>,
}

/// Display for shutdown rejected response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownRejectedDisplay {
    /// Title text.
    pub title: String,
    /// Rejection reason.
    pub reason: String,
    /// Static footer.
    pub footer: &'static str,
}

/// Display for task assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskAssignmentDisplay {
    /// Title line.
    pub title: String,
    /// Subject line.
    pub subject: String,
    /// Optional description.
    pub description: Option<String>,
}

/// One text fragment of a team-memory count line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamMemPart {
    /// Text segment.
    pub text: String,
    /// Whether a comma precedes the segment.
    pub comma_before: bool,
}

/// `<n> team memory` / `<n> team memories` paired with that count, or
/// `None` when nothing was saved.
pub fn team_mem_saved_part(team_count: usize) -> Option<(String, usize)> {
    if team_count == 0 {
        return None;
    }
    Some((
        format!(
            "{team_count} team {}",
            if team_count == 1 {
                "memory"
            } else {
                "memories"
            }
        ),
        team_count,
    ))
}

/// True when any of the read, search or write counts is non-zero.
pub fn check_has_team_mem_ops(read_count: usize, search_count: usize, write_count: usize) -> bool {
    read_count > 0 || search_count > 0 || write_count > 0
}

/// Builds the fragments of a team-memory line: one per non-zero count, in
/// read → search → write order. Each verb is present-tense while the group is
/// active and past-tense once it has finished, and is capitalised only when
/// it opens the line; `comma_before` is set on every fragment after the
/// first.
pub fn render_team_mem_count_parts(
    read_count: usize,
    search_count: usize,
    write_count: usize,
    is_active_group: bool,
    has_preceding_parts: bool,
) -> Vec<TeamMemPart> {
    let mut parts = Vec::new();
    let mut count = if has_preceding_parts { 1 } else { 0 };

    if read_count > 0 {
        let verb = if is_active_group {
            if count == 0 {
                "Recalling"
            } else {
                "recalling"
            }
        } else if count == 0 {
            "Recalled"
        } else {
            "recalled"
        };
        parts.push(TeamMemPart {
            text: format!(
                "{verb} {}",
                humanize_bool_count(read_count, "team memory", "team memories")
            ),
            comma_before: count > 0,
        });
        count += 1;
    }

    if search_count > 0 {
        let verb = if is_active_group {
            if count == 0 {
                "Searching"
            } else {
                "searching"
            }
        } else if count == 0 {
            "Searched"
        } else {
            "searched"
        };
        parts.push(TeamMemPart {
            text: format!("{verb} team memories"),
            comma_before: count > 0,
        });
        count += 1;
    }

    if write_count > 0 {
        let verb = if is_active_group {
            if count == 0 {
                "Writing"
            } else {
                "writing"
            }
        } else if count == 0 {
            "Wrote"
        } else {
            "wrote"
        };
        parts.push(TeamMemPart {
            text: format!(
                "{verb} {}",
                humanize_bool_count(write_count, "team memory", "team memories")
            ),
            comma_before: count > 0,
        });
    }

    parts
}

/// Pure request display.
pub fn project_shutdown_request(request: &ShutdownRequestMessage) -> ShutdownRequestDisplay {
    ShutdownRequestDisplay {
        title: format!("Shutdown request from {}", request.from),
        reason: request.reason.clone(),
    }
}

/// Pure rejected display.
pub fn project_shutdown_rejected(response: &ShutdownRejectedMessage) -> ShutdownRejectedDisplay {
    ShutdownRejectedDisplay {
        title: format!("Shutdown rejected by {}", response.from),
        reason: response.reason.clone(),
        footer: "Teammate is continuing to work. You may request shutdown again later.",
    }
}

/// One-line shutdown summary: the request when there is one, otherwise the
/// approver, otherwise the rejection.
pub fn get_shutdown_message_summary(
    request: Option<&ShutdownRequestMessage>,
    approved_from: Option<&str>,
    rejected: Option<&ShutdownRejectedMessage>,
) -> Option<String> {
    if let Some(request) = request {
        return Some(format!(
            "[Shutdown Request from {}]{}",
            request.from,
            request
                .reason
                .as_ref()
                .map(|r| format!(" {r}"))
                .unwrap_or_default()
        ));
    }
    if let Some(from) = approved_from {
        return Some(format!("[Shutdown Approved] {from} is now exiting"));
    }
    rejected.map(|r| format!("[Shutdown Rejected] {}: {}", r.from, r.reason))
}

/// Pure task assignment display.
pub fn project_task_assignment(assignment: &TaskAssignmentMessage) -> TaskAssignmentDisplay {
    TaskAssignmentDisplay {
        title: format!(
            "Task #{} assigned by {}",
            assignment.task_id, assignment.assigned_by
        ),
        subject: assignment.subject.clone(),
        description: assignment.description.clone(),
    }
}

/// One-line task-assignment summary: `[Task Assigned] #<task id> - <subject>`.
pub fn get_task_assignment_summary(assignment: &TaskAssignmentMessage) -> String {
    format!(
        "[Task Assigned] #{} - {}",
        assignment.task_id, assignment.subject
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_mem_saved_part_zero_hides() {
        assert_eq!(team_mem_saved_part(0), None);
        assert_eq!(
            team_mem_saved_part(2),
            Some(("2 team memories".to_string(), 2))
        );
    }

    #[test]
    fn check_has_team_mem_ops_matches_any_non_zero() {
        assert!(!check_has_team_mem_ops(0, 0, 0));
        assert!(check_has_team_mem_ops(1, 0, 0));
        assert!(check_has_team_mem_ops(0, 1, 0));
        assert!(check_has_team_mem_ops(0, 0, 1));
    }

    #[test]
    fn render_team_mem_count_parts_respects_verbs_and_commas() {
        let parts = render_team_mem_count_parts(1, 1, 2, true, false);
        assert_eq!(parts.len(), 3);
        assert!(!parts[0].comma_before);
        assert!(parts[1].comma_before);
        assert!(parts[2].comma_before);
        assert!(parts[0].text.starts_with("Recalling"));
        assert!(parts[1].text.starts_with("searching"));
    }

    #[test]
    fn shutdown_summary_prioritizes_request_then_approved_then_rejected() {
        let request = ShutdownRequestMessage {
            from: "alice".into(),
            reason: Some("idle".into()),
        };
        assert_eq!(
            get_shutdown_message_summary(Some(&request), Some("bob"), None).as_deref(),
            Some("[Shutdown Request from alice] idle")
        );
        assert_eq!(
            get_shutdown_message_summary(None, Some("bob"), None).as_deref(),
            Some("[Shutdown Approved] bob is now exiting")
        );
    }

    #[test]
    fn project_shutdown_rejected_threads_footer() {
        let d = project_shutdown_rejected(&ShutdownRejectedMessage {
            from: "eve".into(),
            reason: "busy".into(),
        });
        assert!(d.title.contains("eve"));
        assert_eq!(
            d.footer,
            "Teammate is continuing to work. You may request shutdown again later."
        );
    }

    #[test]
    fn task_assignment_summary_has_expected_shape() {
        let assignment = TaskAssignmentMessage {
            task_id: "7".into(),
            assigned_by: "alice".into(),
            subject: "Fix bug".into(),
            description: None,
        };
        assert_eq!(
            get_task_assignment_summary(&assignment),
            "[Task Assigned] #7 - Fix bug"
        );
    }
}
