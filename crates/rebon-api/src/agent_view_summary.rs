//! Model-backed summary generation for Agent View rows.
//!
//! The caller supplies the concrete small/title model. This module
//! only owns prompt shaping, bounded input extraction from persisted
//! session updates, and tolerant JSON response parsing.

use crate::client::ModelClient;
use crate::request::CreateMessageRequest;
use rebon_types::truncate_chars;

/// Maximum text fed to the row-summary model.
pub const MAX_AGENT_VIEW_SUMMARY_TEXT: usize = 2_000;

/// Maximum generated row-summary length in characters.
pub const MAX_AGENT_VIEW_SUMMARY_CHARS: usize = 96;

/// System prompt for completed background-job row summaries.
pub const AGENT_VIEW_SUMMARY_PROMPT: &str = "Summarize this completed background coding job for a terminal row.

Return JSON with a single \"summary\" field.

Rules:
- One short line, 4-12 words when possible.
- Maximum 80 characters unless a PR URL/number is essential.
- No markdown, bullets, quotes, or trailing period.
- Describe what was accomplished or the final actionable state.
- Preserve PR numbers or URLs when they are central to the result.

Good examples:
{\"summary\": \"fixed mobile login regression\"}
{\"summary\": \"opened PR #42 for auth cleanup\"}
{\"summary\": \"updated failing cache tests\"}

Bad: {\"summary\": \"Task completed successfully.\"}
Bad: {\"summary\": \"The assistant completed the user's request by making several changes to the codebase.\"}";

#[derive(Debug, serde::Deserialize)]
struct AgentViewSummaryEnvelope {
    summary: String,
}

/// Build bounded model input from the initial user prompt plus persisted
/// session updates. The output is intended for the row-summary model,
/// not for display.
pub fn extract_agent_view_summary_text(
    prompt: &str,
    updates: &[rebon_types::SessionUpdateParams],
) -> String {
    let mut lines = Vec::new();
    if let Some(prompt) = clean_inline(prompt) {
        lines.push(format!("user: {}", truncate_chars(&prompt, 500)));
    }

    let mut assistant = String::new();
    for params in updates {
        match &params.update {
            rebon_types::SessionUpdate::AgentMessageChunk { content } => {
                if let Some(text) = content_block_text(content) {
                    assistant.push_str(&text);
                } else {
                    flush_assistant(&mut lines, &mut assistant);
                    if let Some(text) = content_block_summary(content) {
                        lines.push(format!("assistant content: {text}"));
                    }
                }
            }
            rebon_types::SessionUpdate::QueuedUserMessage { content, .. } => {
                flush_assistant(&mut lines, &mut assistant);
                let text = content
                    .iter()
                    .filter_map(content_block_summary)
                    .collect::<Vec<_>>()
                    .join(" ");
                if let Some(text) = clean_inline(&text) {
                    lines.push(format!("queued user: {}", truncate_chars(&text, 500)));
                }
            }
            rebon_types::SessionUpdate::ThinkingDelta { text } => {
                if !lines.iter().any(|line| line.starts_with("thinking: ")) {
                    flush_assistant(&mut lines, &mut assistant);
                    if let Some(text) = clean_inline(text) {
                        lines.push(format!("thinking: {}", truncate_chars(&text, 240)));
                    }
                }
            }
            rebon_types::SessionUpdate::ToolCall {
                title,
                kind,
                status,
                locations,
                ..
            } => {
                flush_assistant(&mut lines, &mut assistant);
                if let Some(title) = clean_inline(title) {
                    let mut line = format!(
                        "tool {} {}: {}",
                        tool_status_label(*status),
                        tool_kind_label(*kind),
                        truncate_chars(&title, 360)
                    );
                    append_locations(&mut line, locations.as_deref());
                    lines.push(line);
                }
            }
            rebon_types::SessionUpdate::ToolCallUpdate {
                status,
                title,
                locations,
                ..
            } => {
                flush_assistant(&mut lines, &mut assistant);
                let label = status.map(tool_status_label).unwrap_or("updated");
                if let Some(title) = title.as_deref().and_then(clean_inline) {
                    let mut line = format!("tool {label}: {}", truncate_chars(&title, 360));
                    append_locations(&mut line, locations.as_deref());
                    lines.push(line);
                } else {
                    lines.push(format!("tool {label}"));
                }
            }
            rebon_types::SessionUpdate::Plan { entries } => {
                flush_assistant(&mut lines, &mut assistant);
                let plan = entries
                    .iter()
                    .take(5)
                    .filter_map(|entry| {
                        clean_inline(&entry.content).map(|content| {
                            format!(
                                "{}: {}",
                                plan_status_label(entry.status),
                                truncate_chars(&content, 240)
                            )
                        })
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                if !plan.is_empty() {
                    lines.push(format!("plan: {plan}"));
                }
            }
            rebon_types::SessionUpdate::CompactingStarted { .. } => {
                flush_assistant(&mut lines, &mut assistant);
                lines.push("context compaction started".to_string());
            }
            rebon_types::SessionUpdate::CompactingDone { used_model, .. } => {
                flush_assistant(&mut lines, &mut assistant);
                lines.push(format!("context compacted, used model: {used_model}"));
            }
            rebon_types::SessionUpdate::ContextReset { plan } => {
                flush_assistant(&mut lines, &mut assistant);
                if let Some(plan) = plan.as_deref().and_then(clean_inline) {
                    lines.push(format!(
                        "context reset plan: {}",
                        truncate_chars(&plan, 360)
                    ));
                } else {
                    lines.push("context reset".to_string());
                }
            }
            rebon_types::SessionUpdate::SessionInfoUpdate { title, .. } => {
                flush_assistant(&mut lines, &mut assistant);
                if let Some(title) = title.as_deref().and_then(clean_inline) {
                    lines.push(format!("session title: {}", truncate_chars(&title, 160)));
                }
            }
            rebon_types::SessionUpdate::ThinkingEnd
            | rebon_types::SessionUpdate::SlashCommands { .. }
            | rebon_types::SessionUpdate::ConfigOptionUpdate { .. }
            | rebon_types::SessionUpdate::ToolCallAutoModeAllowed { .. }
            | rebon_types::SessionUpdate::TokenUsage { .. } => {}
        }
    }
    flush_assistant(&mut lines, &mut assistant);
    tail_chars(&lines.join("\n"), MAX_AGENT_VIEW_SUMMARY_TEXT)
}

/// Ask the configured small/title model for a completed Agent View
/// row summary. Returns `None` on empty input, model failure, or an
/// unusable response.
pub async fn generate_agent_view_summary(
    client: &dyn ModelClient,
    model: &str,
    summary_text: &str,
) -> Option<String> {
    let model = model.trim();
    let trimmed = summary_text.trim();
    if model.is_empty() || trimmed.is_empty() {
        tracing::debug!("agent-view-summary: skipping generation - empty model or input");
        return None;
    }

    let mut request =
        CreateMessageRequest::simple(model, trimmed).with_system(AGENT_VIEW_SUMMARY_PROMPT);
    request.stream = false;
    request.max_tokens = 96;

    let message = match client.create_message(request).await {
        Ok(message) => message,
        Err(err) => {
            tracing::debug!(error = %err, "agent-view-summary: model call failed");
            return None;
        }
    };

    let raw = message.text();
    let parsed = parse_agent_view_summary_response(&raw);
    if parsed.is_none() {
        tracing::debug!(raw = %raw, "agent-view-summary: response was not usable");
    }
    parsed
}

fn flush_assistant(lines: &mut Vec<String>, assistant: &mut String) {
    if let Some(text) = clean_inline(assistant) {
        lines.push(format!("assistant: {}", truncate_chars(&text, 900)));
    }
    assistant.clear();
}

fn content_block_text(block: &rebon_types::ContentBlock) -> Option<String> {
    match block {
        rebon_types::ContentBlock::Text(text) => Some(text.text.clone()),
        _ => None,
    }
}

fn content_block_summary(block: &rebon_types::ContentBlock) -> Option<String> {
    match block {
        rebon_types::ContentBlock::Text(text) => clean_inline(&text.text),
        rebon_types::ContentBlock::Image(image) => image
            .uri
            .as_deref()
            .and_then(clean_inline)
            .map(|uri| format!("image {uri}"))
            .or_else(|| Some("image".to_string())),
        rebon_types::ContentBlock::Audio(_) => Some("audio".to_string()),
        rebon_types::ContentBlock::Resource(resource) => {
            Some(format!("resource {}", resource.resource.uri))
        }
        rebon_types::ContentBlock::ResourceLink(link) => Some(format!("resource {}", link.name)),
    }
}

fn append_locations(line: &mut String, locations: Option<&[rebon_types::ToolCallLocation]>) {
    let Some(locations) = locations else {
        return;
    };
    let paths = locations
        .iter()
        .take(3)
        .filter_map(|loc| clean_inline(&loc.path))
        .collect::<Vec<_>>();
    if !paths.is_empty() {
        line.push_str(" [");
        line.push_str(&paths.join(", "));
        line.push(']');
    }
}

fn tool_kind_label(kind: rebon_types::ToolKind) -> &'static str {
    match kind {
        rebon_types::ToolKind::Read => "read",
        rebon_types::ToolKind::Edit => "edit",
        rebon_types::ToolKind::Delete => "delete",
        rebon_types::ToolKind::Move => "move",
        rebon_types::ToolKind::Search => "search",
        rebon_types::ToolKind::Execute => "execute",
        rebon_types::ToolKind::Think => "think",
        rebon_types::ToolKind::Fetch => "fetch",
        rebon_types::ToolKind::Other => "other",
    }
}

fn tool_status_label(status: rebon_types::ToolCallStatus) -> &'static str {
    match status {
        rebon_types::ToolCallStatus::Pending => "pending",
        rebon_types::ToolCallStatus::InProgress => "running",
        rebon_types::ToolCallStatus::Completed => "completed",
        rebon_types::ToolCallStatus::Failed => "failed",
    }
}

fn plan_status_label(status: rebon_types::PlanEntryStatus) -> &'static str {
    match status {
        rebon_types::PlanEntryStatus::Pending => "pending",
        rebon_types::PlanEntryStatus::InProgress => "running",
        rebon_types::PlanEntryStatus::Completed => "completed",
    }
}

fn parse_agent_view_summary_response(raw: &str) -> Option<String> {
    let slice = extract_first_json_object(raw)?;
    let env: AgentViewSummaryEnvelope = serde_json::from_str(slice).ok()?;
    clean_generated_summary(&env.summary)
}

fn clean_generated_summary(value: &str) -> Option<String> {
    let cleaned = clean_inline(value)?;
    let stripped = cleaned
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .trim();
    if stripped.is_empty() {
        None
    } else {
        Some(truncate_chars(stripped, MAX_AGENT_VIEW_SUMMARY_CHARS))
    }
}

fn clean_inline(value: &str) -> Option<String> {
    let cleaned = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn tail_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value
        .chars()
        .rev()
        .take(max_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn extract_first_json_object(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes[start..].iter().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        match b {
            b'\\' if in_string => escape = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    let end = start + i + 1;
                    return Some(&raw[start..end]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ContentBlockDelta, ContentBlockStart, MessageDeltaFields, StreamEvent};
    use crate::mock::MockModelClient;
    use crate::types::{StopReason, Usage};
    use std::sync::Arc;

    fn params(update: rebon_types::SessionUpdate) -> rebon_types::SessionUpdateParams {
        rebon_types::SessionUpdateParams {
            session_id: "sess-one".into(),
            update,
        }
    }

    fn text_block(text: &str) -> rebon_types::ContentBlock {
        rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: text.into(),
            annotations: None,
        })
    }

    fn mock_reply(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message_id: "summary-mock".into(),
                model: "mock".into(),
                usage: Usage::default(),
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text {
                    text: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::TextDelta { text: text.into() },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Usage::default(),
                },
            },
            StreamEvent::MessageStop,
        ]
    }

    #[test]
    fn extract_groups_streaming_agent_chunks() {
        let updates = vec![
            params(rebon_types::SessionUpdate::AgentMessageChunk {
                content: text_block("Fixed "),
            }),
            params(rebon_types::SessionUpdate::AgentMessageChunk {
                content: text_block("the failing tests."),
            }),
        ];

        let got = extract_agent_view_summary_text("repair auth tests", &updates);

        assert!(got.contains("user: repair auth tests"));
        assert!(got.contains("assistant: Fixed the failing tests."));
    }

    #[test]
    fn extract_includes_tool_and_plan_context() {
        let updates = vec![
            params(rebon_types::SessionUpdate::ToolCall {
                tool_call_id: "tool-1".into(),
                title: "Edit crates/rebon-cli/src/background.rs".into(),
                kind: rebon_types::ToolKind::Edit,
                status: rebon_types::ToolCallStatus::Completed,
                content: None,
                locations: Some(vec![rebon_types::ToolCallLocation {
                    path: "background.rs".into(),
                    line: Some(42),
                }]),
                raw_input: None,
                raw_output: None,
            }),
            params(rebon_types::SessionUpdate::Plan {
                entries: vec![rebon_types::PlanEntry {
                    content: "Add model-backed row summary".into(),
                    priority: rebon_types::PlanEntryPriority::High,
                    status: rebon_types::PlanEntryStatus::Completed,
                }],
            }),
        ];

        let got = extract_agent_view_summary_text("implement summaries", &updates);

        assert!(got.contains("tool completed edit"));
        assert!(got.contains("background.rs"));
        assert!(got.contains("plan: completed: Add model-backed row summary"));
    }

    #[test]
    fn extract_tail_slices_long_input() {
        let updates = (0..10)
            .map(|idx| {
                params(rebon_types::SessionUpdate::ToolCall {
                    tool_call_id: format!("tool-{idx}"),
                    title: format!("Edit file {idx} {}", "x".repeat(500)),
                    kind: rebon_types::ToolKind::Edit,
                    status: rebon_types::ToolCallStatus::Completed,
                    content: None,
                    locations: None,
                    raw_input: None,
                    raw_output: None,
                })
            })
            .collect::<Vec<_>>();

        let got = extract_agent_view_summary_text("make lots of edits", &updates);

        assert_eq!(got.chars().count(), MAX_AGENT_VIEW_SUMMARY_TEXT);
        assert!(got.contains("tool completed edit"));
    }

    #[test]
    fn parse_summary_from_json_and_prose() {
        let got = parse_agent_view_summary_response(
            "result:\n```json\n{\"summary\":\"fixed auth regression\"}\n```",
        );
        assert_eq!(got.as_deref(), Some("fixed auth regression"));
    }

    #[test]
    fn parse_summary_rejects_empty_or_missing_field() {
        assert!(parse_agent_view_summary_response(r#"{"summary":"   "}"#).is_none());
        assert!(parse_agent_view_summary_response(r#"{"title":"nope"}"#).is_none());
        assert!(parse_agent_view_summary_response("not json").is_none());
    }

    #[tokio::test]
    async fn generate_returns_summary_and_uses_model() {
        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(mock_reply(r#"{"summary":"updated cache tests"}"#));

        let got = generate_agent_view_summary(mock.as_ref(), "small-row-model", "events").await;

        assert_eq!(got.as_deref(), Some("updated cache tests"));
        let captured = mock.captured_requests();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].model, "small-row-model");
        assert_eq!(captured[0].max_tokens, 96);
        assert!(!captured[0].stream);
    }

    #[tokio::test]
    async fn generate_skips_empty_input_or_model() {
        let mock = Arc::new(MockModelClient::new());

        assert!(
            generate_agent_view_summary(mock.as_ref(), "small-row-model", " \n")
                .await
                .is_none()
        );
        assert!(generate_agent_view_summary(mock.as_ref(), "  ", "events")
            .await
            .is_none());
        assert_eq!(mock.call_count(), 0);
    }
}
