//! Projection for teammate messages: the `<teammate-message>` XML wrappers
//! are parsed here, then each one becomes either plain teammate content or
//! one of the structured branches (plan approval, shutdown, task
//! assignment, turn completion, task completed).

use serde_json::Value;

use crate::mailbox::{
    get_shutdown_message_summary, get_task_assignment_summary, ShutdownRejectedMessage,
    ShutdownRequestMessage, TaskAssignmentMessage,
};
use crate::plan_approval::{
    get_plan_approval_summary, PlanApprovalRequestMessage, PlanApprovalResponseMessage,
};

/// Parsed teammate XML message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTeammateMessage {
    /// Teammate id.
    pub teammate_id: String,
    /// Raw inner content.
    pub content: String,
    /// Optional color string.
    pub color: Option<String>,
    /// Optional summary string.
    pub summary: Option<String>,
}

/// Content display for plain teammate text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeammateMessageContentDisplay {
    /// `leader` or teammate id.
    pub display_name: String,
    /// Optional color name for the teammate's row.
    pub color: Option<String>,
    /// Full content string.
    pub content: String,
    /// Optional summary.
    pub summary: Option<String>,
    /// Whether transcript mode renders the body.
    pub is_transcript_mode: bool,
}

/// User teammate message output branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeammateRenderable {
    /// Plan-approval summary string.
    PlanApprovalSummary(String),
    /// Shutdown summary string.
    ShutdownSummary(String),
    /// Task assignment summary string.
    TaskAssignmentSummary(String),
    /// Teammate turn completion — displayed as a concise lifecycle line.
    IdleNotification {
        /// Teammate display name.
        display_name: String,
        /// Human-readable terminal status.
        status_text: String,
    },
    /// Task completed summary string.
    TaskCompleted {
        /// Display name for the teammate.
        display_name: String,
        /// Completed task id.
        task_id: String,
        /// Optional task subject.
        task_subject: Option<String>,
    },
    /// Plain teammate content.
    Plain(TeammateMessageContentDisplay),
}

/// Parses all `<teammate-message ...>` wrappers in a single text block.
pub fn parse_teammate_messages(text: &str) -> Vec<ParsedTeammateMessage> {
    let mut messages = Vec::new();
    let mut cursor = 0usize;
    while let Some(idx) = text[cursor..].find("<teammate-message ") {
        let start = cursor + idx;
        let open_end = match text[start..].find('>') {
            Some(v) => start + v,
            None => break,
        };
        let attrs = &text[start + "<teammate-message ".len()..open_end];
        let close = match text[open_end + 1..].find("</teammate-message>") {
            Some(v) => open_end + 1 + v,
            None => break,
        };
        let teammate_id = extract_attr_value(attrs, "teammate_id");
        let content = text[open_end + 1..close].trim().to_string();
        if let Some(teammate_id) = teammate_id {
            messages.push(ParsedTeammateMessage {
                teammate_id,
                content,
                color: extract_attr_value(attrs, "color"),
                summary: extract_attr_value(attrs, "summary"),
            });
        }
        cursor = close + "</teammate-message>".len();
    }
    messages
}

/// `leader` remains special-cased; everything else passes through.
pub fn teammate_display_name(teammate_id: &str) -> String {
    if teammate_id == "leader" {
        "leader".to_string()
    } else {
        teammate_id.to_string()
    }
}

/// Plain teammate-content projection for the non-structured fallback branch.
pub fn project_teammate_message_content(
    display_name: &str,
    color: Option<&str>,
    content: &str,
    summary: Option<&str>,
    is_transcript_mode: bool,
) -> TeammateMessageContentDisplay {
    TeammateMessageContentDisplay {
        display_name: display_name.to_string(),
        color: color.map(ToString::to_string),
        content: content.to_string(),
        summary: summary.map(ToString::to_string),
        is_transcript_mode,
    }
}

/// Project every teammate message in `text` into the branch a renderer
/// draws, dropping the shutdown-approved and terminated payloads.
pub fn project_user_teammate_messages(
    text: &str,
    is_transcript_mode: bool,
) -> Vec<TeammateRenderable> {
    parse_teammate_messages(text)
        .into_iter()
        .filter(|msg| !is_shutdown_approved_json(&msg.content) && !is_terminated_json(&msg.content))
        .map(|msg| {
            let display_name = teammate_display_name(&msg.teammate_id);
            if let Some(summary) = maybe_plan_approval_summary(&msg.content) {
                return TeammateRenderable::PlanApprovalSummary(summary);
            }
            if let Some(summary) = maybe_shutdown_summary(&msg.content) {
                return TeammateRenderable::ShutdownSummary(summary);
            }
            if let Some(summary) = maybe_task_assignment_summary(&msg.content) {
                return TeammateRenderable::TaskAssignmentSummary(summary);
            }
            if is_idle_notification_json(&msg.content) {
                return TeammateRenderable::IdleNotification {
                    display_name,
                    status_text: idle_notification_status_from_json(&msg.content),
                };
            }
            if let Some((task_id, task_subject)) = parse_task_completed_json(&msg.content) {
                return TeammateRenderable::TaskCompleted {
                    display_name,
                    task_id,
                    task_subject,
                };
            }
            TeammateRenderable::Plain(project_teammate_message_content(
                &display_name,
                msg.color.as_deref(),
                &msg.content,
                msg.summary.as_deref(),
                is_transcript_mode,
            ))
        })
        .collect()
}

fn extract_attr_value(attrs: &str, key: &str) -> Option<String> {
    let needle = format!(r#"{key}=""#);
    let start = attrs.find(&needle)? + needle.len();
    let rest = &attrs[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn parse_json(content: &str) -> Option<Value> {
    serde_json::from_str(content).ok()
}

fn is_shutdown_approved_json(content: &str) -> bool {
    parse_json(content)
        .and_then(|v| {
            v.get("type")
                .and_then(Value::as_str)
                .map(|t| t == "shutdown_approved")
        })
        .unwrap_or(false)
}

fn is_terminated_json(content: &str) -> bool {
    parse_json(content)
        .and_then(|v| {
            v.get("type")
                .and_then(Value::as_str)
                .map(|t| t == "teammate_terminated")
        })
        .unwrap_or(false)
}

fn is_idle_notification_json(content: &str) -> bool {
    parse_json(content)
        .and_then(|v| {
            v.get("type")
                .and_then(Value::as_str)
                .map(|t| t == "idle_notification")
        })
        .unwrap_or(false)
}

fn idle_notification_status_from_json(content: &str) -> String {
    match parse_json(content)
        .and_then(|parsed| {
            parsed
                .get("idleReason")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .as_deref()
    {
        Some("failed") => "failed".to_string(),
        Some("interrupted") | Some("cancelled") | Some("canceled") => "stopped".to_string(),
        _ => "finished".to_string(),
    }
}

fn parse_task_completed_json(content: &str) -> Option<(String, Option<String>)> {
    let parsed = parse_json(content)?;
    if parsed.get("type")?.as_str()? != "task_completed" {
        return None;
    }
    let task_id = parsed.get("taskId")?.as_str()?.to_string();
    let task_subject = parsed
        .get("taskSubject")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    Some((task_id, task_subject))
}

fn maybe_plan_approval_summary(content: &str) -> Option<String> {
    let parsed = parse_json(content)?;
    let kind = parsed.get("type")?.as_str()?;
    match kind {
        "plan_approval_request" => get_plan_approval_summary(
            Some(&PlanApprovalRequestMessage {
                from: parsed.get("from")?.as_str()?.to_string(),
                plan_content: parsed
                    .get("planContent")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                plan_file_path: parsed
                    .get("planFilePath")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
            None,
        ),
        "plan_approval_response" => get_plan_approval_summary(
            None,
            Some(&PlanApprovalResponseMessage {
                approved: parsed
                    .get("approved")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                feedback: parsed
                    .get("feedback")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
            }),
        ),
        _ => None,
    }
}

fn maybe_shutdown_summary(content: &str) -> Option<String> {
    let parsed = parse_json(content)?;
    match parsed.get("type")?.as_str()? {
        "shutdown_request" => get_shutdown_message_summary(
            Some(&ShutdownRequestMessage {
                from: parsed.get("from")?.as_str()?.to_string(),
                reason: parsed
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
            }),
            None,
            None,
        ),
        "shutdown_rejected" => get_shutdown_message_summary(
            None,
            None,
            Some(&ShutdownRejectedMessage {
                from: parsed.get("from")?.as_str()?.to_string(),
                reason: parsed
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
        ),
        "shutdown_approved" => {
            get_shutdown_message_summary(None, parsed.get("from").and_then(Value::as_str), None)
        }
        _ => None,
    }
}

fn maybe_task_assignment_summary(content: &str) -> Option<String> {
    let parsed = parse_json(content)?;
    if parsed.get("type")?.as_str()? != "task_assignment" {
        return None;
    }
    Some(get_task_assignment_summary(&TaskAssignmentMessage {
        task_id: parsed.get("taskId")?.as_str()?.to_string(),
        assigned_by: parsed.get("assignedBy")?.as_str()?.to_string(),
        subject: parsed.get("subject")?.as_str()?.to_string(),
        description: parsed
            .get("description")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan_approval::{
        format_teammate_message_content, get_idle_notification_summary, IdleNotificationMessage,
    };

    #[test]
    fn parse_teammate_messages_handles_multiple_wrappers() {
        let text = concat!(
            "<teammate-message teammate_id=\"alice\" color=\"red\" summary=\"s1\">one</teammate-message>",
            "<teammate-message teammate_id=\"leader\">two</teammate-message>"
        );
        let parsed = parse_teammate_messages(text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].summary.as_deref(), Some("s1"));
        assert_eq!(parsed[1].teammate_id, "leader");
    }

    #[test]
    fn project_user_teammate_messages_filters_and_routes_structured_cases() {
        let text = concat!(
            "<teammate-message teammate_id=\"alice\">{\"type\":\"plan_approval_request\",\"from\":\"alice\",\"planContent\":\"x\",\"planFilePath\":\"/tmp/p\"}</teammate-message>",
            "<teammate-message teammate_id=\"alice\">{\"type\":\"idle_notification\",\"idleReason\":\"available\"}</teammate-message>",
            "<teammate-message teammate_id=\"alice\">{\"type\":\"task_completed\",\"taskId\":\"7\",\"taskSubject\":\"bug\"}</teammate-message>",
            "<teammate-message teammate_id=\"alice\" summary=\"hi\">plain</teammate-message>"
        );
        let rendered = project_user_teammate_messages(text, true);
        assert!(matches!(
            rendered[0],
            TeammateRenderable::PlanApprovalSummary(_)
        ));
        assert!(matches!(
            &rendered[1],
            TeammateRenderable::IdleNotification { status_text, .. } if status_text == "finished"
        ));
        assert!(matches!(
            rendered[2],
            TeammateRenderable::TaskCompleted { .. }
        ));
        assert!(matches!(rendered[3], TeammateRenderable::Plain(_)));
    }

    #[test]
    fn idle_notification_status_covers_terminal_outcomes() {
        for (reason, expected) in [
            ("available", "finished"),
            ("failed", "failed"),
            ("interrupted", "stopped"),
            ("cancelled", "stopped"),
        ] {
            let content = format!("{{\"type\":\"idle_notification\",\"idleReason\":\"{reason}\"}}");
            assert_eq!(idle_notification_status_from_json(&content), expected);
        }
    }

    #[test]
    fn format_helper_prefers_structured_summaries() {
        let out = format_teammate_message_content(
            "plain",
            Some(&PlanApprovalRequestMessage {
                from: "a".into(),
                plan_content: "p".into(),
                plan_file_path: "f".into(),
            }),
            None,
            None,
            Some(get_idle_notification_summary(&IdleNotificationMessage {
                completed_task_id: None,
                completed_status: None,
                summary: None,
            })),
            None,
            None,
        );
        assert_eq!(out, "[Plan Approval Request from a]");
    }
}
