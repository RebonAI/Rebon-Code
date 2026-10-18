use async_trait::async_trait;
use serde_json::{json, Value};

use rebon_tool::{
    default_team_name, read_team_file, write_mailbox_message, MessageDeliveryState,
    TeamMailboxMessage, TeamManager, Tool, ToolContext,
};
use rebon_tools_core::{
    validation_outcome_from, PermissionDecision, PermissionRequest, ToolError,
    ToolErrorPresentation, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};

pub const SEND_MESSAGE_TOOL_NAME: &str = "SendMessage";
const INVALID_INPUT_CODE: i64 = 400;
const TEAM_LEAD_NAME: &str = "team-lead";
const PERMISSION_MESSAGE_PREVIEW_CHARS: usize = 200;

fn message_delivery_error(message: impl Into<String>) -> ToolErrorPresentation {
    ToolErrorPresentation::new(
        "message_delivery_failed",
        "Message could not be delivered.",
        message,
    )
}

#[derive(Debug, Clone, Default)]
pub struct SendMessageTool;

#[derive(Debug, Clone)]
struct SendMessageInput {
    to: String,
    summary: Option<String>,
    payload: MessagePayload,
}

#[derive(Debug, Clone)]
enum MessagePayload {
    Text(String),
    ShutdownRequest {
        reason: Option<String>,
    },
    /// Reply from a teammate to the leader's `shutdown_request`.
    /// `request_id` echoes the request being answered, `approve` accepts
    /// or refuses, `reason` is required when rejecting.
    ShutdownResponse {
        request_id: String,
        approve: bool,
        reason: Option<String>,
    },
    PlanApprovalResponse {
        request_id: String,
        approve: bool,
        feedback: Option<String>,
    },
}

fn message_preview(text: &str) -> String {
    let mut chars = text.chars();
    let preview = chars
        .by_ref()
        .take(PERMISSION_MESSAGE_PREVIEW_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

fn payload_description(payload: &MessagePayload) -> String {
    match payload {
        MessagePayload::Text(text) => format!("Message: {}", message_preview(text)),
        MessagePayload::ShutdownRequest { reason } => match reason
            .as_deref()
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
        {
            Some(reason) => format!("Payload: shutdown_request (reason: {reason})"),
            None => "Payload: shutdown_request".to_string(),
        },
        MessagePayload::ShutdownResponse {
            request_id,
            approve,
            reason,
        } => {
            let verdict = if *approve { "approve" } else { "reject" };
            match reason
                .as_deref()
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
            {
                Some(reason) => format!(
                    "Payload: shutdown_response ({verdict}, request {request_id}, reason: {reason})"
                ),
                None => format!("Payload: shutdown_response ({verdict}, request {request_id})"),
            }
        }
        MessagePayload::PlanApprovalResponse {
            request_id,
            approve,
            feedback,
        } => {
            let verdict = if *approve { "approve" } else { "reject" };
            match feedback
                .as_deref()
                .map(str::trim)
                .filter(|feedback| !feedback.is_empty())
            {
                Some(feedback) => format!(
                    "Payload: plan_approval_response ({verdict}, request {request_id}, feedback: {feedback})"
                ),
                None => {
                    format!("Payload: plan_approval_response ({verdict}, request {request_id})")
                }
            }
        }
    }
}

fn send_message_permission_request(parsed: &SendMessageInput) -> PermissionRequest {
    let mut message = format!("This agent wants to send a message to \"{}\".", parsed.to);
    if let Some(summary) = parsed
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|summary| !summary.is_empty())
    {
        message.push_str(&format!("\n\nSummary: {summary}"));
    }
    message.push_str(&format!("\n\n{}", payload_description(&parsed.payload)));
    PermissionRequest::new(format!("Send a message to {}", parsed.to), message)
}

/// What a successful send tells the model about delivery timing. The
/// distinction matters: a running teammate reads the message at its next tool
/// round, while an idle or finished one is resumed by it.
fn recipient_delivery_note(
    manager: Option<&std::sync::Arc<dyn TeamManager>>,
    team_name: &str,
    session_id: Option<&str>,
    recipient: &str,
) -> &'static str {
    const UNKNOWN: &str =
        "It will receive it at its next tool round, or when it next starts a turn if it is idle.";
    let Some(manager) = manager else {
        return UNKNOWN;
    };
    if let Some(state) = manager.message_delivery_state(team_name, recipient) {
        return match state {
            MessageDeliveryState::NextToolRound => {
                "It is running and will receive it at its next tool round."
            }
            MessageDeliveryState::NextTurn => "It is idle, so the message starts its next turn.",
            MessageDeliveryState::Resumed => {
                "It had finished, so it was resumed with this message."
            }
        };
    }
    let Some(session_id) = session_id else {
        return UNKNOWN;
    };
    match manager
        .teammate_roster(session_id)
        .into_iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(recipient))
        .map(|entry| entry.status)
        .as_deref()
    {
        Some("running") => "It is running and will receive it at its next tool round.",
        Some("idle") => "It is idle, so the message starts its next turn.",
        Some(_) => "It had finished, so it was resumed with this message.",
        None => UNKNOWN,
    }
}

#[async_trait]
impl Tool for SendMessageTool {
    fn id(&self) -> ToolId {
        ToolId::new(SEND_MESSAGE_TOOL_NAME)
    }

    fn description(&self) -> &str {
        // The description deliberately omits the cross-session section:
        // rebon has no cross-session peer discovery yet, so there are no
        // peer rows to advertise. Re-add them here once it lands.
        "# SendMessage\n\
         \n\
         Send a message to another agent. The main agent can address any named teammate in its current session's implicit default team without calling TeamCreate first.\n\
         \n\
         ```json\n\
         {\"to\": \"researcher\", \"summary\": \"assign task 1\", \"message\": \"start on task #1\"}\n\
         ```\n\
         \n\
         | `to` | |\n\
         |---|---|\n\
         | `\"researcher\"` | Teammate by name |\n\
         | `\"*\"` | Broadcast to all teammates — expensive (linear in team size), use only when everyone genuinely needs it |\n\
         \n\
         Your plain text output is NOT visible to other agents — to communicate, you MUST call this tool. Messages from teammates are delivered automatically; you don't check an inbox. Refer to teammates by name, never by UUID. Use SendMessage to supplement or correct a running teammate; use Agent with the same name when assigning a distinct follow-up request. When relaying, don't quote the original — it's already rendered to the user.\n\
         \n\
         ## Protocol responses (legacy)\n\
         \n\
         If you receive a JSON message with `type: \"shutdown_request\"` or `type: \"plan_approval_request\"`, respond with the matching `_response` type — echo the `request_id`, set `approve` true/false:\n\
         \n\
         ```json\n\
         {\"to\": \"team-lead\", \"message\": {\"type\": \"shutdown_response\", \"request_id\": \"...\", \"approve\": true}}\n\
         {\"to\": \"researcher\", \"message\": {\"type\": \"plan_approval_response\", \"request_id\": \"...\", \"approve\": false, \"feedback\": \"add error handling\"}}\n\
         ```\n\
         \n\
         Approving shutdown terminates your process. Rejecting plan sends the teammate back to revise. Don't originate `shutdown_request` unless asked. Don't send structured JSON status messages — use TaskUpdate."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "Recipient teammate name, or '*' to broadcast." },
                "summary": { "type": "string", "description": "Optional short preview summary for UI display." },
                "message": {
                    "oneOf": [
                        { "type": "string", "description": "Plain text message." },
                        {
                            "type": "object",
                            "properties": {
                                "type": { "type": "string", "enum": ["shutdown_request"] },
                                "reason": { "type": "string" }
                            },
                            "required": ["type"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": {
                                "type": { "type": "string", "enum": ["shutdown_response"] },
                                "request_id": { "type": "string" },
                                "approve": { "type": "boolean" },
                                "reason": { "type": "string" }
                            },
                            "required": ["type", "request_id", "approve"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": {
                                "type": { "type": "string", "enum": ["plan_approval_response"] },
                                "request_id": { "type": "string" },
                                "approve": { "type": "boolean" },
                                "feedback": { "type": "string" }
                            },
                            "required": ["type", "request_id", "approve"],
                            "additionalProperties": false
                        }
                    ]
                }
            },
            "required": ["to", "message"],
            "additionalProperties": false
        })
    }

    async fn check_permissions(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        let parsed = parse_input(input)?;
        if context.permission_mode().as_deref() != Some("auto") {
            return Ok(PermissionDecision::allow(input.clone()));
        }
        Ok(PermissionDecision::ask(
            send_message_permission_request(&parsed),
            Some(input.clone()),
        ))
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        validation_outcome_from(parse_input(input))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed = parse_input(&input)?;
        let session_id = context
            .session_id()
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty());
        let current_team = context.current_team_name().and_then(|team_name| {
            read_team_file(&team_name)
                .ok()
                .flatten()
                .filter(|team| {
                    session_id.is_none_or(|session_id| {
                        team.lead_session_id.as_deref() == Some(session_id)
                    })
                })
                .map(|_| team_name)
        });
        let team_name = context
            .team_identity()
            .map(|identity| identity.team_name.clone())
            .or_else(|| {
                let default_team = session_id.map(default_team_name).filter(|team_name| {
                    read_team_file(team_name)
                        .ok()
                        .flatten()
                        .is_some_and(|team| {
                            parsed.to == "*"
                                || team
                                    .members
                                    .iter()
                                    .any(|member| member.name.eq_ignore_ascii_case(&parsed.to))
                        })
                });
                if parsed.to == "*" {
                    current_team.or(default_team)
                } else {
                    default_team.or(current_team)
                }
            });
        if team_name.is_none() {
            return self
                .send_to_task_without_team_context(parsed, context)
                .await;
        }
        let team_name = team_name.expect("checked is_some");
        let sender = context
            .team_identity()
            .map(|identity| identity.agent_name.clone())
            .unwrap_or_else(|| TEAM_LEAD_NAME.to_string());

        // Structured messages
        // cannot be broadcast — only plain text.
        if parsed.to == "*" && !matches!(parsed.payload, MessagePayload::Text(_)) {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: "structured messages cannot be broadcast (to: \"*\")".into(),
                error_code: Some(INVALID_INPUT_CODE),
            });
        }

        // A shutdown_response must
        // target the team-lead. Also guard against the lead sending
        // themselves a response.
        if let MessagePayload::ShutdownResponse { .. } = &parsed.payload {
            if !parsed.to.eq_ignore_ascii_case(TEAM_LEAD_NAME) {
                return Err(ToolError::InvalidInput {
                    tool: self.id(),
                    reason: format!("shutdown_response must be sent to \"{TEAM_LEAD_NAME}\""),
                    error_code: Some(INVALID_INPUT_CODE),
                });
            }
            if sender.eq_ignore_ascii_case(TEAM_LEAD_NAME) {
                return Err(ToolError::InvalidInput {
                    tool: self.id(),
                    reason: "team-lead cannot send a shutdown_response (only teammates reply)"
                        .into(),
                    error_code: Some(INVALID_INPUT_CODE),
                });
            }
        }

        if parsed.to == "*" {
            let team = read_team_file(&team_name)
                .map_err(|err| ToolError::Execution {
                    tool: self.id(),
                    source: err.into(),
                })?
                .ok_or_else(|| ToolError::Execution {
                    tool: self.id(),
                    source: anyhow::anyhow!("team `{team_name}` does not exist"),
                })?;
            let mut recipients = Vec::new();
            for member in team.members {
                if member.name.eq_ignore_ascii_case(&sender) {
                    continue;
                }
                self.deliver_message(
                    context.team_manager(),
                    &team_name,
                    &sender,
                    &member.name,
                    parsed.summary.clone(),
                    parsed.payload.clone(),
                )
                .await
                .map_err(|presentation| ToolError::Presented {
                    tool: self.id(),
                    presentation,
                })?;
                recipients.push(member.name);
            }
            return Ok(json!({
                "success": true,
                "message": format!(
                    "Message broadcast to {} teammate(s). Each recipient will receive it at its next tool round; an idle recipient starts its next turn with it.",
                    recipients.len()
                ),
                "recipients": recipients,
            }));
        }

        let delivery_note =
            recipient_delivery_note(context.team_manager(), &team_name, session_id, &parsed.to);
        self.deliver_message(
            context.team_manager(),
            &team_name,
            &sender,
            &parsed.to,
            parsed.summary.clone(),
            parsed.payload,
        )
        .await
        .map_err(|presentation| ToolError::Presented {
            tool: self.id(),
            presentation,
        })?;

        Ok(json!({
            "success": true,
            "message": format!("Message sent to {}. {delivery_note}", parsed.to),
            "target": parsed.to,
        }))
    }
}

impl SendMessageTool {
    async fn send_to_task_without_team_context(
        &self,
        parsed: SendMessageInput,
        context: &ToolContext,
    ) -> ToolResult<Value> {
        let MessagePayload::Text(text) = parsed.payload else {
            return Err(ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(
                    "SendMessage requires an active team context for structured messages"
                ),
            });
        };
        if let Some(controller) = context.task_runtime_controller() {
            let session_id = context
                .session_id()
                .ok_or_else(|| ToolError::InvalidInput {
                    tool: self.id(),
                    reason: "SendMessage runtime operations require a session_id".into(),
                    error_code: Some(400),
                })?;
            controller
                .send_message_to_task(session_id, &parsed.to, text.clone())
                .await
                .map_err(|presentation| ToolError::Presented {
                    tool: self.id(),
                    presentation,
                })?;
            return Ok(json!({
                "success": true,
                "message": format!(
                    "Message queued for task {}. It will receive it at its next tool round, or when it next starts a turn if it is idle.",
                    parsed.to
                ),
                "target": parsed.to,
                "mode": "runtime",
            }));
        }
        let manager = context.team_manager().ok_or_else(|| ToolError::Execution {
            tool: self.id(),
            source: anyhow::anyhow!("SendMessage requires an active team context"),
        })?;
        manager
            .send_message_to_task(&parsed.to, text)
            .await
            .map_err(|presentation| ToolError::Presented {
                tool: self.id(),
                presentation,
            })?;
        Ok(json!({
            "success": true,
            "message": format!(
                "Message queued for task {}. It will receive it at its next tool round, or when it next starts a turn if it is idle.",
                parsed.to
            ),
            "target": parsed.to,
        }))
    }

    async fn deliver_message(
        &self,
        manager: Option<&std::sync::Arc<dyn TeamManager>>,
        team_name: &str,
        sender: &str,
        recipient: &str,
        summary: Option<String>,
        payload: MessagePayload,
    ) -> Result<(), ToolErrorPresentation> {
        match payload {
            MessagePayload::Text(text) => {
                if recipient.eq_ignore_ascii_case(TEAM_LEAD_NAME) || manager.is_none() {
                    write_mailbox_message(
                        team_name,
                        recipient,
                        TeamMailboxMessage {
                            from: sender.to_string(),
                            text,
                            timestamp: current_timestamp(),
                            read: false,
                            color: None,
                            summary,
                        },
                    )
                    .map_err(|err| message_delivery_error(err.to_string()))
                } else {
                    manager
                        .expect("checked is_some")
                        .send_message(team_name, recipient, text)
                        .await
                }
            }
            MessagePayload::ShutdownRequest { reason } => {
                if recipient.eq_ignore_ascii_case(TEAM_LEAD_NAME) {
                    return Err(message_delivery_error(
                        "shutdown_request must target a teammate, not team-lead",
                    ));
                }
                manager
                    .ok_or_else(|| {
                        message_delivery_error("shutdown_request requires a TeamManager")
                    })?
                    .request_shutdown(team_name, recipient, reason)
                    .await
                    .map(|_| ())
                    .map_err(message_delivery_error)
            }
            MessagePayload::ShutdownResponse {
                request_id,
                approve,
                reason,
            } => {
                // Write the response to the team-lead's mailbox using the
                // wire shapes already understood by the renderer
                // (`rebon-render::attachment::parse_shutdown_summary`
                // accepts `shutdown_approved` / `shutdown_rejected`).
                // approve=true produces the short "X is now exiting"
                // banner; approve=false surfaces the reason so the lead
                // can decide the next move.
                let payload = if approve {
                    serde_json::json!({
                        "type": "shutdown_approved",
                        "from": sender,
                        "request_id": request_id,
                        "timestamp": current_timestamp(),
                    })
                } else {
                    serde_json::json!({
                        "type": "shutdown_rejected",
                        "from": sender,
                        "request_id": request_id,
                        "reason": reason.clone().unwrap_or_default(),
                        "timestamp": current_timestamp(),
                    })
                };
                let summary = Some(if approve {
                    format!("{sender} acknowledges shutdown")
                } else {
                    format!("{sender} rejects shutdown")
                });
                write_mailbox_message(
                    team_name,
                    recipient,
                    TeamMailboxMessage {
                        from: sender.to_string(),
                        text: payload.to_string(),
                        timestamp: current_timestamp(),
                        read: false,
                        color: None,
                        summary,
                    },
                )
                .map_err(|err| message_delivery_error(err.to_string()))
            }
            MessagePayload::PlanApprovalResponse {
                request_id,
                approve,
                feedback,
            } => {
                let payload = serde_json::json!({
                    "type": "plan_approval_response",
                    "requestId": request_id,
                    "approved": approve,
                    "feedback": feedback,
                    "timestamp": current_timestamp(),
                    "permissionMode": if approve { Some("default") } else { None::<&str> },
                });
                write_mailbox_message(
                    team_name,
                    recipient,
                    TeamMailboxMessage {
                        from: sender.to_string(),
                        text: payload.to_string(),
                        timestamp: current_timestamp(),
                        read: false,
                        color: None,
                        summary: Some(if approve {
                            "plan approved".into()
                        } else {
                            "plan rejected".into()
                        }),
                    },
                )
                .map_err(|err| message_delivery_error(err.to_string()))
            }
        }
    }
}

fn parse_input(input: &Value) -> ToolResult<SendMessageInput> {
    let tool = ToolId::new(SEND_MESSAGE_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "SendMessage input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let to = object
        .get("to")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "SendMessage requires a non-empty string `to`".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?
        .to_string();

    let summary = match object.get("summary") {
        Some(Value::String(v)) => Some(v.clone()),
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: "`summary` must be a string when provided".into(),
                error_code: Some(INVALID_INPUT_CODE),
            });
        }
    };

    let payload = match object.get("message") {
        Some(Value::String(text)) => MessagePayload::Text(text.clone()),
        Some(Value::Object(msg)) => match msg.get("type").and_then(Value::as_str) {
            Some("shutdown_request") => MessagePayload::ShutdownRequest {
                reason: msg
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(|v| v.to_string()),
            },
            Some("shutdown_response") => {
                let request_id = msg
                    .get("request_id")
                    .and_then(Value::as_str)
                    .filter(|v| !v.trim().is_empty())
                    .ok_or_else(|| ToolError::InvalidInput {
                        tool: tool.clone(),
                        reason: "`message.request_id` is required for shutdown_response".into(),
                        error_code: Some(INVALID_INPUT_CODE),
                    })?
                    .to_string();
                let approve = msg.get("approve").and_then(Value::as_bool).ok_or_else(|| {
                    ToolError::InvalidInput {
                        tool: tool.clone(),
                        reason: "`message.approve` must be a boolean for shutdown_response".into(),
                        error_code: Some(INVALID_INPUT_CODE),
                    }
                })?;
                let reason = msg
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(|v| v.to_string());
                // reason is
                // mandatory when rejecting — otherwise the team-lead
                // can't tell the agent how to adjust.
                if !approve && reason.as_ref().map(|r| r.trim().is_empty()).unwrap_or(true) {
                    return Err(ToolError::InvalidInput {
                        tool,
                        reason: "`reason` is required when rejecting a shutdown_request".into(),
                        error_code: Some(INVALID_INPUT_CODE),
                    });
                }
                MessagePayload::ShutdownResponse {
                    request_id,
                    approve,
                    reason,
                }
            }
            Some("plan_approval_response") => {
                let request_id = msg
                    .get("request_id")
                    .and_then(Value::as_str)
                    .filter(|v| !v.trim().is_empty())
                    .ok_or_else(|| ToolError::InvalidInput {
                        tool: tool.clone(),
                        reason: "`message.request_id` is required for plan_approval_response"
                            .into(),
                        error_code: Some(INVALID_INPUT_CODE),
                    })?
                    .to_string();
                MessagePayload::PlanApprovalResponse {
                    request_id,
                    approve: msg.get("approve").and_then(Value::as_bool).ok_or_else(|| {
                        ToolError::InvalidInput {
                            tool: tool.clone(),
                            reason:
                                "`message.approve` must be a boolean for plan_approval_response"
                                    .into(),
                            error_code: Some(INVALID_INPUT_CODE),
                        }
                    })?,
                    feedback: msg
                        .get("feedback")
                        .and_then(Value::as_str)
                        .map(|v| v.to_string()),
                }
            }
            _ => {
                return Err(ToolError::InvalidInput {
                    tool,
                    reason: "unsupported structured SendMessage payload".into(),
                    error_code: Some(INVALID_INPUT_CODE),
                });
            }
        },
        _ => {
            return Err(ToolError::InvalidInput {
                tool,
                reason: "SendMessage requires `message` as a string or supported structured object"
                    .into(),
                error_code: Some(INVALID_INPUT_CODE),
            });
        }
    };

    Ok(SendMessageInput {
        to,
        summary,
        payload,
    })
}

fn current_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    millis.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::{
        clear_current_team_name, StopTaskOutcome, TaskRuntimeController, TeammateRosterEntry,
        TeammateSpawnResult, TeammateSpawnSpec,
    };
    use std::sync::{Arc, Mutex};

    struct ScriptedTeamManager {
        sent: Mutex<Vec<(String, String, String)>>,
        task_messages: Mutex<Vec<(String, String)>>,
        shutdowns: Mutex<Vec<(String, String)>>,
        roster: Vec<TeammateRosterEntry>,
    }

    #[async_trait]
    impl TeamManager for ScriptedTeamManager {
        async fn spawn_teammate(
            &self,
            _spec: TeammateSpawnSpec,
        ) -> Result<TeammateSpawnResult, String> {
            unreachable!()
        }

        async fn send_message(
            &self,
            team_name: &str,
            recipient: &str,
            message: String,
        ) -> Result<(), rebon_tools_core::ToolErrorPresentation> {
            self.sent
                .lock()
                .unwrap()
                .push((team_name.into(), recipient.into(), message));
            Ok(())
        }

        async fn send_message_to_task(
            &self,
            task_id: &str,
            message: String,
        ) -> Result<(), rebon_tools_core::ToolErrorPresentation> {
            self.task_messages
                .lock()
                .unwrap()
                .push((task_id.into(), message));
            Ok(())
        }

        fn teammate_roster(&self, _session_id: &str) -> Vec<TeammateRosterEntry> {
            self.roster.clone()
        }

        async fn request_shutdown(
            &self,
            team_name: &str,
            recipient: &str,
            _reason: Option<String>,
        ) -> Result<String, String> {
            self.shutdowns
                .lock()
                .unwrap()
                .push((team_name.into(), recipient.into()));
            Ok("req-1".into())
        }

        async fn request_plan_approval(
            &self,
            _team_name: &str,
            _agent_name: &str,
            _plan_content: String,
        ) -> Result<String, String> {
            Ok("plan-1".into())
        }

        async fn delete_team(&self, _team_name: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn plain_message_routes_through_manager() {
        let _home =
            rebon_tool::tasks::test_support::TestConfigHome::new("sm-plain_message_routes_thr");
        clear_current_team_name();
        let manager = Arc::new(ScriptedTeamManager {
            sent: Mutex::new(Vec::new()),
            task_messages: Mutex::new(Vec::new()),
            shutdowns: Mutex::new(Vec::new()),
            roster: Vec::new(),
        });
        let context = ToolContext::new()
            .with_team_manager(manager.clone() as Arc<dyn TeamManager>)
            .with_team_identity(rebon_tool::TeamIdentityContext {
                agent_id: "alice@alpha".into(),
                agent_name: "alice".into(),
                team_name: "alpha".into(),
                permission_mode: Some("default".into()),
            });
        let tool = SendMessageTool;
        let out = tool
            .call(
                json!({"to":"bob","summary":"handoff","message":"take task 2"}),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(out["success"], json!(true));
        let sent = manager.sent.lock().unwrap();
        assert_eq!(sent[0].1, "bob");
    }

    fn roster_entry(status: &str) -> TeammateRosterEntry {
        TeammateRosterEntry {
            name: "bob".into(),
            agent_type: None,
            description: None,
            status: status.into(),
            current_task: None,
            last_result: None,
        }
    }

    fn team_context_with_roster(status: &str) -> (Arc<ScriptedTeamManager>, ToolContext) {
        let manager = Arc::new(ScriptedTeamManager {
            sent: Mutex::new(Vec::new()),
            task_messages: Mutex::new(Vec::new()),
            shutdowns: Mutex::new(Vec::new()),
            roster: vec![roster_entry(status)],
        });
        let context = ToolContext::new()
            .with_session_id("session-timing")
            .with_team_manager(manager.clone() as Arc<dyn TeamManager>)
            .with_team_identity(rebon_tool::TeamIdentityContext {
                agent_id: "alice@alpha".into(),
                agent_name: "alice".into(),
                team_name: "alpha".into(),
                permission_mode: Some("default".into()),
            });
        (manager, context)
    }

    #[tokio::test]
    async fn permission_gate_asks_in_auto_mode_with_a_useful_request() {
        let tool = SendMessageTool;
        let long_text = format!("{}TAIL", "a".repeat(300));
        let input = json!({
            "to": "bob",
            "summary": "handoff",
            "message": long_text,
        });
        let context = ToolContext::new().with_permission_mode("auto");
        let decision = tool.check_permissions(&input, &context).await.unwrap();
        assert_eq!(decision.behavior, rebon_tools_core::PermissionBehavior::Ask);
        let request = decision.request.expect("auto mode must carry a request");
        assert!(request.message.contains("bob"), "{}", request.message);
        assert!(request.message.contains("handoff"), "{}", request.message);
        assert!(
            request.message.contains(&"a".repeat(200)),
            "{}",
            request.message
        );
        assert!(request.message.contains('…'), "{}", request.message);
        assert!(
            !request.message.contains("TAIL"),
            "long text must be truncated: {}",
            request.message
        );
    }

    #[tokio::test]
    async fn permission_gate_describes_structured_payloads_in_auto_mode() {
        let tool = SendMessageTool;
        let context = ToolContext::new().with_permission_mode("auto");
        let input = json!({
            "to": "bob",
            "message": {"type": "shutdown_request", "reason": "work is done"},
        });
        let decision = tool.check_permissions(&input, &context).await.unwrap();
        assert_eq!(decision.behavior, rebon_tools_core::PermissionBehavior::Ask);
        let request = decision.request.expect("auto mode must carry a request");
        assert!(
            request.message.contains("shutdown_request"),
            "{}",
            request.message
        );
        assert!(request.message.contains("bob"), "{}", request.message);

        let input = json!({
            "to": "team-lead",
            "message": {
                "type": "plan_approval_response",
                "request_id": "plan-1",
                "approve": false,
                "feedback": "add tests",
            },
        });
        let decision = tool.check_permissions(&input, &context).await.unwrap();
        let request = decision.request.expect("auto mode must carry a request");
        assert!(
            request.message.contains("plan_approval_response"),
            "{}",
            request.message
        );
        assert!(request.message.contains("add tests"), "{}", request.message);
    }

    #[tokio::test]
    async fn permission_gate_allows_every_other_mode() {
        let tool = SendMessageTool;
        let input = json!({"to": "bob", "message": "take task 2"});
        let modes = [
            Some("default"),
            Some("acceptEdits"),
            Some("plan"),
            Some("bypassPermissions"),
            Some("dontAsk"),
            Some("bubble"),
            None,
        ];
        for mode in modes {
            let context = match mode {
                Some(mode) => ToolContext::new().with_permission_mode(mode),
                None => ToolContext::new(),
            };
            let decision = tool.check_permissions(&input, &context).await.unwrap();
            assert_eq!(
                decision.behavior,
                rebon_tools_core::PermissionBehavior::Allow,
                "mode {mode:?} must not prompt"
            );
            assert_eq!(decision.updated_input, Some(input.clone()));
        }
    }

    #[tokio::test]
    async fn permission_gate_reports_invalid_input_instead_of_asking() {
        let tool = SendMessageTool;
        let context = ToolContext::new().with_permission_mode("auto");
        let error = tool
            .check_permissions(&json!({"message": "missing recipient"}), &context)
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidInput { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn send_result_says_a_running_recipient_reads_it_next_tool_round() {
        let (_manager, context) = team_context_with_roster("running");
        let out = SendMessageTool
            .call(json!({"to": "bob", "message": "take task 2"}), &context)
            .await
            .unwrap();
        let message = out["message"].as_str().unwrap();
        assert!(message.contains("at its next tool round"), "{message}");
    }

    #[tokio::test]
    async fn send_result_says_an_idle_recipient_starts_its_next_turn() {
        let (_manager, context) = team_context_with_roster("idle");
        let out = SendMessageTool
            .call(json!({"to": "bob", "message": "take task 2"}), &context)
            .await
            .unwrap();
        let message = out["message"].as_str().unwrap();
        assert!(message.contains("starts its next turn"), "{message}");
    }

    #[tokio::test]
    async fn send_result_says_a_finished_recipient_is_resumed() {
        let (_manager, context) = team_context_with_roster("failed-restartable");
        let out = SendMessageTool
            .call(json!({"to": "bob", "message": "take task 2"}), &context)
            .await
            .unwrap();
        let message = out["message"].as_str().unwrap();
        assert!(message.contains("resumed with this message"), "{message}");
    }

    #[tokio::test]
    async fn send_result_tells_when_a_lead_mailbox_is_read() {
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("sm-lead-timing");
        clear_current_team_name();
        let manager = Arc::new(ScriptedTeamManager {
            sent: Mutex::new(Vec::new()),
            task_messages: Mutex::new(Vec::new()),
            shutdowns: Mutex::new(Vec::new()),
            roster: Vec::new(),
        });
        let context = ToolContext::new()
            .with_team_manager(manager as Arc<dyn TeamManager>)
            .with_team_identity(rebon_tool::TeamIdentityContext {
                agent_id: "alice@alpha".into(),
                agent_name: "alice".into(),
                team_name: "alpha".into(),
                permission_mode: Some("default".into()),
            });
        let out = SendMessageTool
            .call(json!({"to": "team-lead", "message": "done"}), &context)
            .await
            .unwrap();
        let message = out["message"].as_str().unwrap();
        assert!(
            message.contains("when it next starts a turn if it is idle"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn main_session_routes_named_recipient_through_default_team() {
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("sm-default-team");
        clear_current_team_name();
        let session_id = "send-message-default-session";
        let team_name = rebon_tool::ensure_session_default_team(session_id).unwrap();
        rebon_tool::append_team_member(
            &team_name,
            rebon_tool::TeamMember {
                agent_id: rebon_tool::format_agent_id("researcher", &team_name),
                name: "researcher".into(),
                agent_type: Some("Explore".into()),
                model: None,
                model_profile: None,
                prompt: Some("inspect the codebase".into()),
                color: None,
                plan_mode_required: None,
                joined_at: rebon_tool::team_files::now_wall_ms(),
                tmux_pane_id: "teammate-task".into(),
                cwd: ".".into(),
                worktree_path: None,
                backend_type: Some("in-process".into()),
                is_active: Some(false),
                mode: Some("default".into()),
                subscriptions: Vec::new(),
            },
        )
        .unwrap();
        let manager = Arc::new(ScriptedTeamManager {
            sent: Mutex::new(Vec::new()),
            task_messages: Mutex::new(Vec::new()),
            shutdowns: Mutex::new(Vec::new()),
            roster: Vec::new(),
        });
        let context = ToolContext::new()
            .with_session_id(session_id)
            .with_team_manager(manager.clone() as Arc<dyn TeamManager>);

        let out = SendMessageTool
            .call(
                json!({"to":"researcher","message":"check the parser next"}),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(out["success"], json!(true));
        let sent = manager.sent.lock().unwrap();
        assert_eq!(
            sent[0],
            (
                team_name,
                "researcher".into(),
                "check the parser next".into()
            )
        );
    }

    #[tokio::test]
    async fn main_session_does_not_route_through_another_sessions_current_team() {
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("sm-session-isolation");
        clear_current_team_name();
        let foreign_team = "foreign-explicit-team";
        rebon_tool::write_team_file(
            foreign_team,
            &rebon_tool::TeamFile {
                name: foreign_team.into(),
                description: Some("Owned by another session".into()),
                created_at: rebon_tool::team_files::now_wall_ms(),
                lead_agent_id: rebon_tool::format_agent_id(TEAM_LEAD_NAME, foreign_team),
                lead_session_id: Some("session-a".into()),
                hidden_pane_ids: Vec::new(),
                members: vec![rebon_tool::TeamMember {
                    agent_id: rebon_tool::format_agent_id("researcher", foreign_team),
                    name: "researcher".into(),
                    agent_type: Some("Explore".into()),
                    model: None,
                    model_profile: None,
                    prompt: Some("inspect the codebase".into()),
                    color: None,
                    plan_mode_required: None,
                    joined_at: rebon_tool::team_files::now_wall_ms(),
                    tmux_pane_id: "foreign-teammate-task".into(),
                    cwd: ".".into(),
                    worktree_path: None,
                    backend_type: Some("in-process".into()),
                    is_active: Some(false),
                    mode: Some("default".into()),
                    subscriptions: Vec::new(),
                }],
            },
        )
        .unwrap();
        rebon_tool::set_current_team_name(foreign_team);
        let manager = Arc::new(ScriptedTeamManager {
            sent: Mutex::new(Vec::new()),
            task_messages: Mutex::new(Vec::new()),
            shutdowns: Mutex::new(Vec::new()),
            roster: Vec::new(),
        });
        let context = ToolContext::new()
            .with_session_id("session-b")
            .with_team_manager(manager.clone() as Arc<dyn TeamManager>);

        let out = SendMessageTool
            .call(
                json!({"to":"researcher","message":"check the parser next"}),
                &context,
            )
            .await
            .unwrap();

        clear_current_team_name();
        assert_eq!(out["success"], json!(true));
        assert!(manager.sent.lock().unwrap().is_empty());
        assert_eq!(
            manager.task_messages.lock().unwrap().as_slice(),
            &[("researcher".into(), "check the parser next".into())]
        );
    }

    struct ScriptedRuntimeController {
        messages: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl TaskRuntimeController for ScriptedRuntimeController {
        async fn stop_task(
            &self,
            _session_id: &str,
            _task_id: &str,
        ) -> Result<StopTaskOutcome, String> {
            unreachable!()
        }

        async fn send_message_to_task(
            &self,
            _session_id: &str,
            task_id: &str,
            message: String,
        ) -> Result<(), rebon_tools_core::ToolErrorPresentation> {
            self.messages
                .lock()
                .expect("scripted runtime messages poisoned")
                .push((task_id.into(), message));
            Ok(())
        }
    }

    #[tokio::test]
    async fn plain_message_without_team_context_prefers_runtime_controller() {
        let _home =
            rebon_tool::tasks::test_support::TestConfigHome::new("sm-plain_message_without_te");
        clear_current_team_name();
        let controller = Arc::new(ScriptedRuntimeController {
            messages: Mutex::new(Vec::new()),
        });
        let context = ToolContext::new()
            .with_session_id("session-runtime")
            .with_task_runtime_controller(controller.clone() as Arc<dyn TaskRuntimeController>);
        let tool = SendMessageTool;
        let out = tool
            .call(
                json!({"to":"agent-runtime","summary":"handoff","message":"include this finding"}),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(out["success"], json!(true));
        assert_eq!(out["mode"], json!("runtime"));
        assert!(out["message"]
            .as_str()
            .unwrap()
            .contains("at its next tool round"));
        let messages = controller
            .messages
            .lock()
            .expect("scripted runtime messages poisoned");
        assert_eq!(
            messages[0],
            ("agent-runtime".into(), "include this finding".into())
        );
    }

    struct ClosedRuntimeController;

    #[async_trait]
    impl TaskRuntimeController for ClosedRuntimeController {
        async fn stop_task(
            &self,
            _session_id: &str,
            _task_id: &str,
        ) -> Result<StopTaskOutcome, String> {
            unreachable!()
        }

        async fn send_message_to_task(
            &self,
            _session_id: &str,
            task_id: &str,
            _message: String,
        ) -> Result<(), rebon_tools_core::ToolErrorPresentation> {
            Err(ToolErrorPresentation::new(
                "agent_closed",
                format!("Agent \"{task_id}\" is no longer available."),
                format!(
                    "Agent \"{task_id}\" expired. Spawn a fresh worker with the follow-up instructions."
                ),
            ))
        }
    }

    #[tokio::test]
    async fn runtime_delivery_error_preserves_separate_model_and_display_messages() {
        let _home =
            rebon_tool::tasks::test_support::TestConfigHome::new("sm-runtime_delivery_error_p");
        clear_current_team_name();
        let context = ToolContext::new()
            .with_session_id("session-runtime")
            .with_task_runtime_controller(Arc::new(ClosedRuntimeController));

        let error = SendMessageTool
            .call(json!({"to":"old-agent","message":"continue"}), &context)
            .await
            .unwrap_err();

        let ToolError::Presented { presentation, .. } = error else {
            panic!("expected structured tool error presentation");
        };
        assert_eq!(presentation.code, "agent_closed");
        assert_eq!(
            presentation.display_message,
            "Agent \"old-agent\" is no longer available."
        );
        assert!(presentation.model_message.contains("Spawn a fresh worker"));
    }

    #[tokio::test]
    async fn plain_message_without_team_context_routes_to_task_id() {
        let _home =
            rebon_tool::tasks::test_support::TestConfigHome::new("sm-plain_message_without_te");
        clear_current_team_name();
        let manager = Arc::new(ScriptedTeamManager {
            sent: Mutex::new(Vec::new()),
            task_messages: Mutex::new(Vec::new()),
            shutdowns: Mutex::new(Vec::new()),
            roster: Vec::new(),
        });
        let context = ToolContext::new().with_team_manager(manager.clone() as Arc<dyn TeamManager>);
        let tool = SendMessageTool;
        let out = tool
            .call(
                json!({"to":"agent-abc123","summary":"handoff","message":"include this finding"}),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(out["success"], json!(true));
        assert!(out["message"]
            .as_str()
            .unwrap()
            .contains("at its next tool round"));
        let messages = manager.task_messages.lock().unwrap();
        assert_eq!(
            messages[0],
            ("agent-abc123".into(), "include this finding".into())
        );
    }

    #[tokio::test]
    async fn shutdown_request_routes_through_manager() {
        let manager = Arc::new(ScriptedTeamManager {
            sent: Mutex::new(Vec::new()),
            task_messages: Mutex::new(Vec::new()),
            shutdowns: Mutex::new(Vec::new()),
            roster: Vec::new(),
        });
        let context = ToolContext::new()
            .with_team_manager(manager.clone() as Arc<dyn TeamManager>)
            .with_team_identity(rebon_tool::TeamIdentityContext {
                agent_id: "lead@alpha".into(),
                agent_name: "team-lead".into(),
                team_name: "alpha".into(),
                permission_mode: Some("default".into()),
            });
        let tool = SendMessageTool;
        let _ = tool
            .call(
                json!({"to":"bob","message":{"type":"shutdown_request","reason":"done"}}),
                &context,
            )
            .await
            .unwrap();
        let shutdowns = manager.shutdowns.lock().unwrap();
        assert_eq!(shutdowns[0].1, "bob");
    }

    fn teammate_context(agent_name: &str, team: &str) -> ToolContext {
        ToolContext::new().with_team_identity(rebon_tool::TeamIdentityContext {
            agent_id: format!("{agent_name}@{team}"),
            agent_name: agent_name.into(),
            team_name: team.into(),
            permission_mode: Some("default".into()),
        })
    }

    #[tokio::test]
    async fn shutdown_response_approve_writes_to_lead_mailbox() {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("sm-shutdown-ok");
        let team = format!(
            "alpha-{}",
            home.path().file_name().unwrap().to_string_lossy()
        );
        let context = teammate_context("alice", &team);
        let tool = SendMessageTool;
        let out = tool
            .call(
                json!({
                    "to": "team-lead",
                    "message": {
                        "type": "shutdown_response",
                        "request_id": "req-42",
                        "approve": true
                    }
                }),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(out["success"], json!(true));
        let inbox = rebon_tool::read_mailbox(&team, "team-lead").unwrap();
        assert_eq!(inbox.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&inbox[0].text).unwrap();
        assert_eq!(parsed["type"], "shutdown_approved");
        assert_eq!(parsed["from"], "alice");
        assert_eq!(parsed["request_id"], "req-42");
    }

    #[tokio::test]
    async fn shutdown_response_reject_writes_rejected_with_reason() {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("sm-shutdown-rej");
        let team = format!(
            "alpha-{}",
            home.path().file_name().unwrap().to_string_lossy()
        );
        let context = teammate_context("alice", &team);
        let tool = SendMessageTool;
        let _ = tool
            .call(
                json!({
                    "to": "team-lead",
                    "message": {
                        "type": "shutdown_response",
                        "request_id": "req-7",
                        "approve": false,
                        "reason": "still running tests"
                    }
                }),
                &context,
            )
            .await
            .unwrap();
        let inbox = rebon_tool::read_mailbox(&team, "team-lead").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&inbox[0].text).unwrap();
        assert_eq!(parsed["type"], "shutdown_rejected");
        assert_eq!(parsed["reason"], "still running tests");
    }

    #[tokio::test]
    async fn shutdown_response_reject_without_reason_is_invalid() {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("sm-shutdown-noreason");
        let team = format!(
            "alpha-{}",
            home.path().file_name().unwrap().to_string_lossy()
        );
        let context = teammate_context("alice", &team);
        let tool = SendMessageTool;
        let err = tool
            .call(
                json!({
                    "to": "team-lead",
                    "message": {
                        "type": "shutdown_response",
                        "request_id": "r",
                        "approve": false
                    }
                }),
                &context,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("reason"), "got: {err}");
    }

    #[tokio::test]
    async fn shutdown_response_missing_request_id_is_invalid() {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("sm-shutdown-noreqid");
        let team = format!(
            "alpha-{}",
            home.path().file_name().unwrap().to_string_lossy()
        );
        let context = teammate_context("alice", &team);
        let tool = SendMessageTool;
        let err = tool
            .call(
                json!({
                    "to": "team-lead",
                    "message": {"type": "shutdown_response", "approve": true}
                }),
                &context,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("request_id"), "got: {err}");
    }

    #[tokio::test]
    async fn shutdown_response_to_non_lead_is_invalid() {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("sm-shutdown-wrong-target");
        let team = format!(
            "alpha-{}",
            home.path().file_name().unwrap().to_string_lossy()
        );
        let context = teammate_context("alice", &team);
        let tool = SendMessageTool;
        let err = tool
            .call(
                json!({
                    "to": "bob",
                    "message": {"type": "shutdown_response", "request_id": "r", "approve": true}
                }),
                &context,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("team-lead"), "got: {err}");
    }

    #[tokio::test]
    async fn shutdown_response_from_lead_is_invalid() {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("sm-shutdown-lead-sender");
        let team = format!(
            "alpha-{}",
            home.path().file_name().unwrap().to_string_lossy()
        );
        let context = teammate_context("team-lead", &team);
        let tool = SendMessageTool;
        let err = tool
            .call(
                json!({
                    "to": "team-lead",
                    "message": {"type": "shutdown_response", "request_id": "r", "approve": true}
                }),
                &context,
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("team-lead cannot send"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn plan_approval_response_requires_request_id() {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("sm-plan-noreqid");
        let team = format!(
            "alpha-{}",
            home.path().file_name().unwrap().to_string_lossy()
        );
        let context = teammate_context("alice", &team);
        let tool = SendMessageTool;
        let err = tool
            .call(
                json!({
                    "to": "bob",
                    "message": {"type": "plan_approval_response", "approve": true}
                }),
                &context,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("request_id"), "got: {err}");
    }

    #[tokio::test]
    async fn structured_broadcast_is_rejected() {
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("sm-broadcast-struct");
        let team = format!(
            "alpha-{}",
            home.path().file_name().unwrap().to_string_lossy()
        );
        let context = teammate_context("alice", &team);
        let tool = SendMessageTool;
        let err = tool
            .call(
                json!({
                    "to": "*",
                    "message": {"type": "shutdown_request", "reason": "x"}
                }),
                &context,
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("cannot be broadcast"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn description_uses_prompt_layout() {
        let tool = SendMessageTool;
        let d = tool.description();
        assert!(d.contains("# SendMessage"));
        assert!(d.contains("## Protocol responses (legacy)"));
        assert!(d.contains("shutdown_response"));
        assert!(d.contains("plan_approval_response"));
        assert!(d.contains("Don't originate `shutdown_request` unless asked"));
    }
}
