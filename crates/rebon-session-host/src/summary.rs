use rebon_types::SessionUpdateParams;

use super::{BackgroundJobState, BackgroundStore};
use crate::non_empty_trimmed;

fn background_job_summary(state: &BackgroundJobState) -> String {
    let prompt = state
        .pending_prompt()
        .map(|prompt| prompt.text.as_str())
        .unwrap_or(&state.identity.prompt);
    let prompt = rebon_tool::cron::strip_scheduled_marker(prompt).trim();
    let prompt = truncate_chars(prompt, 72);
    if prompt.is_empty() {
        "completed".to_string()
    } else {
        format!("completed: {prompt}")
    }
}

pub fn background_job_success_summary(state: &BackgroundJobState) -> String {
    state
        .outcome
        .summary
        .as_deref()
        .and_then(non_empty_trimmed)
        .filter(|summary| summary != "running detached from interactive session")
        .unwrap_or_else(|| background_job_summary(state))
}

pub fn background_job_success_summary_from_store(
    store: &BackgroundStore,
    job_id: &str,
) -> Option<String> {
    let events = store.read_events_tail(job_id, 200).ok()?;
    events
        .iter()
        .rev()
        .filter(|event| event.kind == "agent_view_summary_updated")
        .find_map(|event| {
            if event.data.get("phase").and_then(|value| value.as_str()) == Some("running") {
                return None;
            }
            event
                .data
                .get("summary")
                .and_then(|value| value.as_str())
                .and_then(non_empty_trimmed)
        })
        .or_else(|| {
            events
                .into_iter()
                .rev()
                .filter(|event| event.kind == "session_update")
                .filter_map(|event| serde_json::from_value::<SessionUpdateParams>(event.data).ok())
                .find_map(|update| summarize_session_update(&update.update))
        })
}

pub fn background_job_failure_summary(message: &str) -> String {
    let message = truncate_chars(message.trim(), 72);
    if message.is_empty() {
        "failed".to_string()
    } else {
        format!("failed: {message}")
    }
}

pub fn summarize_session_update(update: &rebon_types::SessionUpdate) -> Option<String> {
    match update {
        rebon_types::SessionUpdate::AgentMessageChunk { content } => {
            summarize_content_block(content).map(|text| format!("responding: {text}"))
        }
        rebon_types::SessionUpdate::ThinkingDelta { text } => {
            non_empty_trimmed(text).map(|text| format!("thinking: {}", truncate_chars(&text, 96)))
        }
        rebon_types::SessionUpdate::ToolCall {
            title,
            status,
            kind,
            ..
        } => Some(format!(
            "{}: {}",
            tool_status_verb(*status, *kind),
            truncate_chars(title.trim(), 96)
        )),
        rebon_types::SessionUpdate::ToolCallUpdate {
            title,
            status,
            raw_output,
            ..
        } => {
            if let Some(title) = title.as_deref().and_then(non_empty_trimmed) {
                return Some(format!(
                    "{}: {}",
                    status.map(tool_update_status_label).unwrap_or("tool"),
                    truncate_chars(&title, 96)
                ));
            }
            if let Some(status) = status {
                return Some(tool_update_status_label(*status).to_string());
            }
            raw_output
                .as_ref()
                .map(|output| format!("tool output: {} field(s)", output.len()))
        }
        rebon_types::SessionUpdate::QueuedUserMessage { content, .. } => content
            .iter()
            .find_map(summarize_content_block)
            .map(|text| format!("queued reply: {text}")),
        rebon_types::SessionUpdate::Plan { entries } => entries
            .iter()
            .find(|entry| entry.status == rebon_types::PlanEntryStatus::InProgress)
            .or_else(|| entries.first())
            .map(|entry| format!("plan: {}", truncate_chars(entry.content.trim(), 96))),
        rebon_types::SessionUpdate::CompactingStarted { .. } => {
            Some("compacting context".to_string())
        }
        rebon_types::SessionUpdate::CompactingDone { .. } => Some("compacted context".to_string()),
        rebon_types::SessionUpdate::ContextReset { .. } => Some("context reset".to_string()),
        rebon_types::SessionUpdate::SessionInfoUpdate { title, .. } => title
            .as_deref()
            .and_then(non_empty_trimmed)
            .map(|title| format!("session: {}", truncate_chars(&title, 96))),
        rebon_types::SessionUpdate::ThinkingEnd
        | rebon_types::SessionUpdate::SlashCommands { .. }
        | rebon_types::SessionUpdate::ConfigOptionUpdate { .. }
        | rebon_types::SessionUpdate::ToolCallAutoModeAllowed { .. }
        | rebon_types::SessionUpdate::TokenUsage { .. } => None,
    }
}

fn summarize_content_block(block: &rebon_types::ContentBlock) -> Option<String> {
    match block {
        rebon_types::ContentBlock::Text(text) => {
            non_empty_trimmed(&text.text).map(|text| truncate_chars(&text, 96))
        }
        rebon_types::ContentBlock::Image(_) => Some("image".to_string()),
        rebon_types::ContentBlock::Audio(_) => Some("audio".to_string()),
        rebon_types::ContentBlock::Resource(resource) => {
            Some(format!("resource {}", resource.resource.uri))
        }
        rebon_types::ContentBlock::ResourceLink(link) => Some(format!("resource {}", link.name)),
    }
}

fn tool_status_verb(
    status: rebon_types::ToolCallStatus,
    kind: rebon_types::ToolKind,
) -> &'static str {
    match status {
        rebon_types::ToolCallStatus::Pending => "preparing",
        rebon_types::ToolCallStatus::InProgress => match kind {
            rebon_types::ToolKind::Read => "reading",
            rebon_types::ToolKind::Edit => "editing",
            rebon_types::ToolKind::Delete => "deleting",
            rebon_types::ToolKind::Move => "moving",
            rebon_types::ToolKind::Search => "searching",
            rebon_types::ToolKind::Execute => "running",
            rebon_types::ToolKind::Think => "thinking",
            rebon_types::ToolKind::Fetch => "fetching",
            rebon_types::ToolKind::Other => "using tool",
        },
        rebon_types::ToolCallStatus::Completed => "completed",
        rebon_types::ToolCallStatus::Failed => "failed",
    }
}

fn tool_update_status_label(status: rebon_types::ToolCallStatus) -> &'static str {
    match status {
        rebon_types::ToolCallStatus::Pending => "tool pending",
        rebon_types::ToolCallStatus::InProgress => "tool running",
        rebon_types::ToolCallStatus::Completed => "tool completed",
        rebon_types::ToolCallStatus::Failed => "tool failed",
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx == max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::BackgroundRuntimeFields;

    #[test]
    fn background_summary_helpers_are_short_and_status_prefixed() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let state = store
            .create_job(
                format!("{} tail", "x".repeat(90)),
                PathBuf::from("."),
                BackgroundRuntimeFields {
                    provider: None,
                    model: None,
                    fast_mode: None,
                    channels: Vec::new(),
                    development_channels: Vec::new(),
                    provider_format: None,
                    ui_mode: None,
                    effort_level: None,
                    permission_mode: None,
                    capability_mode: rebon_types::AgentCapabilityMode::Normal,
                    settings: Vec::new(),
                    add_dirs: Vec::new(),
                    plugin_dirs: Vec::new(),
                    mcp_configs: Vec::new(),
                    strict_mcp_config: false,
                },
            )
            .unwrap();

        let summary = background_job_summary(&state);
        assert!(summary.starts_with("completed: "));
        let mut with_live_summary = state.clone();
        with_live_summary.outcome.summary = Some("responding: final answer".into());
        assert_eq!(
            background_job_success_summary(&with_live_summary),
            "responding: final answer"
        );
        let mut pending = state.clone();
        pending.identity.pending_prompts = vec![crate::PendingPrompt::new(
            "pp-summary-test".into(),
            "reply prompt".into(),
            Vec::new(),
            crate::now_ms(),
        )
        .unwrap()];
        assert_eq!(background_job_summary(&pending), "completed: reply prompt");
        assert_eq!(
            background_job_failure_summary("permission denied"),
            "failed: permission denied"
        );
    }
}
