//! Plan-approval and idle-notification message projections: titles, bodies
//! and one-line summaries.

/// Parsed plan approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanApprovalRequestMessage {
    /// Sender name.
    pub from: String,
    /// Markdown plan content.
    pub plan_content: String,
    /// Plan file path.
    pub plan_file_path: String,
}

/// Parsed plan approval response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanApprovalResponseMessage {
    /// Whether approved.
    pub approved: bool,
    /// Optional feedback.
    pub feedback: Option<String>,
}

/// Idle notification summary input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleNotificationMessage {
    /// Completed task id.
    pub completed_task_id: Option<String>,
    /// Completed status.
    pub completed_status: Option<String>,
    /// Optional summary.
    pub summary: Option<String>,
}

/// Request display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanApprovalRequestDisplay {
    /// Title line.
    pub title: String,
    /// Plan content.
    pub plan_content: String,
    /// Plan file path line.
    pub plan_file_path: String,
}

/// Response display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanApprovalResponseDisplay {
    /// Approved response.
    Approved {
        /// Title line.
        title: String,
        /// Body text.
        body: &'static str,
    },
    /// Rejected response.
    Rejected {
        /// Title line.
        title: String,
        /// Optional feedback.
        feedback: Option<String>,
        /// Footer text.
        footer: &'static str,
    },
}

/// Combined renderable output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanApprovalRenderable {
    /// Request branch.
    Request(PlanApprovalRequestDisplay),
    /// Response branch.
    Response(PlanApprovalResponseDisplay),
}

/// Pure projection of a plan-approval request.
pub fn project_plan_approval_request(
    request: &PlanApprovalRequestMessage,
) -> PlanApprovalRequestDisplay {
    PlanApprovalRequestDisplay {
        title: format!("Plan Approval Request from {}", request.from),
        plan_content: request.plan_content.clone(),
        plan_file_path: request.plan_file_path.clone(),
    }
}

/// Pure projection of a plan-approval response.
pub fn project_plan_approval_response(
    response: &PlanApprovalResponseMessage,
    sender_name: &str,
) -> PlanApprovalResponseDisplay {
    if response.approved {
        PlanApprovalResponseDisplay::Approved {
            title: format!("✓ Plan Approved by {sender_name}"),
            body: "You can now proceed with implementation. Your plan mode restrictions have been lifted.",
        }
    } else {
        PlanApprovalResponseDisplay::Rejected {
            title: format!("✗ Plan Rejected by {sender_name}"),
            feedback: response.feedback.clone(),
            footer: "Please revise your plan based on the feedback and call ExitPlanMode again.",
        }
    }
}

/// One-line summary of a plan-approval request or response, `None` when
/// neither is present.
pub fn get_plan_approval_summary(
    request: Option<&PlanApprovalRequestMessage>,
    response: Option<&PlanApprovalResponseMessage>,
) -> Option<String> {
    if let Some(request) = request {
        return Some(format!("[Plan Approval Request from {}]", request.from));
    }
    if let Some(response) = response {
        if response.approved {
            Some("[Plan Approved] You can now proceed with implementation".to_string())
        } else {
            Some(format!(
                "[Plan Rejected] {}",
                response
                    .feedback
                    .as_deref()
                    .unwrap_or("Please revise your plan")
            ))
        }
    } else {
        None
    }
}

/// Brief summary helper for idle notifications.
pub fn get_idle_notification_summary(msg: &IdleNotificationMessage) -> String {
    let mut parts = vec!["Agent idle".to_string()];
    if let Some(task_id) = &msg.completed_task_id {
        let status = msg.completed_status.as_deref().unwrap_or("completed");
        parts.push(format!("Task {task_id} {status}"));
    }
    if let Some(summary) = &msg.summary {
        parts.push(format!("Last DM: {summary}"));
    }
    parts.join(" · ")
}

/// Shared formatter used by teammate-message renderers after structured
/// mailbox/plan/idle parsing has already been attempted.
pub fn format_teammate_message_content(
    content: &str,
    plan_request: Option<&PlanApprovalRequestMessage>,
    plan_response: Option<&PlanApprovalResponseMessage>,
    shutdown_summary: Option<String>,
    idle_summary: Option<String>,
    task_assignment_summary: Option<String>,
    terminated_message: Option<&str>,
) -> String {
    if let Some(summary) = get_plan_approval_summary(plan_request, plan_response) {
        return summary;
    }
    if let Some(summary) = shutdown_summary {
        return summary;
    }
    if let Some(summary) = idle_summary {
        return summary;
    }
    if let Some(summary) = task_assignment_summary {
        return summary;
    }
    if let Some(message) = terminated_message {
        return message.to_string();
    }
    content.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_response_projection_shape() {
        let request = project_plan_approval_request(&PlanApprovalRequestMessage {
            from: "alice".into(),
            plan_content: "plan".into(),
            plan_file_path: "/tmp/p.md".into(),
        });
        assert!(request.title.contains("alice"));

        let approved = project_plan_approval_response(
            &PlanApprovalResponseMessage {
                approved: true,
                feedback: None,
            },
            "bob",
        );
        assert!(matches!(
            approved,
            PlanApprovalResponseDisplay::Approved { .. }
        ));
    }

    #[test]
    fn summaries_match_request_and_rejected_response() {
        let request = PlanApprovalRequestMessage {
            from: "alice".into(),
            plan_content: "plan".into(),
            plan_file_path: "f".into(),
        };
        assert_eq!(
            get_plan_approval_summary(Some(&request), None).as_deref(),
            Some("[Plan Approval Request from alice]")
        );

        let response = PlanApprovalResponseMessage {
            approved: false,
            feedback: Some("change it".into()),
        };
        assert_eq!(
            get_plan_approval_summary(None, Some(&response)).as_deref(),
            Some("[Plan Rejected] change it")
        );
    }

    #[test]
    fn idle_notification_summary_builds_parts() {
        let text = get_idle_notification_summary(&IdleNotificationMessage {
            completed_task_id: Some("42".into()),
            completed_status: Some("failed".into()),
            summary: Some("last step".into()),
        });
        assert!(text.contains("Task 42 failed"));
        assert!(text.contains("Last DM: last step"));
    }
}
