//! Live/foreground agent transcript projection. Synchronizes the runner's transcript and
//! streaming overlay from a coordinator `TaskSnapshot`/`LocalAgentData` so a backgrounded
//! agent can be re-foregrounded with its current state, and swaps the main-agent view in
//! and out as the user pauses, resumes, or switches between live agents.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, UNIX_EPOCH},
};

use rebon_acp::trim_raw_output_for_transcript;
use rebon_plugin_tasks::runtime::TaskRegistry;
use rebon_tui::{StreamingContentBlock, StreamingToolUse};
use rebon_types::{format_system_time_iso_ms, SessionUpdateParams, ToolCallStatus, ToolKind};

use crate::tui::app::{AppState, StoredTranscriptView};

use super::render::drain_pending_updates;
use super::{
    inject_system_message, repin_transcript_to_bottom, reset_transcript_page_state, ActivePrompt,
};

fn display_tool_kind(name: &str) -> ToolKind {
    match name {
        // Names no rebon tool registers, so nothing declares a kind for them.
        "Delete" | "FileDeleteTool" => ToolKind::Delete,
        "Move" | "FileMoveTool" => ToolKind::Move,
        "Search" => ToolKind::Search,
        "Think" | "ThinkTool" => ToolKind::Think,
        _ => rebon_tool::render_tool_kind_for_name(name),
    }
}

fn display_prompt_text(text: &str) -> String {
    let mut lines = text.lines().collect::<Vec<_>>();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn snapshot_timestamp(snapshot: &rebon_plugin_tasks::runtime::TaskSnapshot) -> String {
    format_system_time_iso_ms(UNIX_EPOCH + Duration::from_millis(snapshot.start_time_ms))
}

fn user_message(uuid: String, timestamp: &str, text: String) -> rebon_tui::Message {
    rebon_tui::Message::User(rebon_tui::UserMessage {
        uuid,
        timestamp: timestamp.to_string(),
        message: rebon_tui::UserMessageInner {
            role: rebon_tui::UserRole::User,
            content: vec![rebon_tui::UserContentBlock::Text(
                rebon_tui::UserTextBlock { text },
            )],
        },
        is_compact_summary: None,
        is_meta: None,
        is_visible_in_transcript_only: None,
        image_paste_ids: None,
        plan_content: None,
    })
}

fn assistant_message(uuid: String, timestamp: &str, text: String) -> rebon_tui::Message {
    rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
        uuid,
        timestamp: timestamp.to_string(),
        message: rebon_tui::AssistantMessageInner {
            role: rebon_tui::AssistantRole::Assistant,
            content: vec![rebon_tui::AssistantContentBlock::Text(
                rebon_tui::AssistantTextBlock { text },
            )],
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    })
}

fn thinking_message(uuid: String, timestamp: &str, text: String) -> rebon_tui::Message {
    rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
        uuid,
        timestamp: timestamp.to_string(),
        message: rebon_tui::AssistantMessageInner {
            role: rebon_tui::AssistantRole::Assistant,
            content: vec![rebon_tui::AssistantContentBlock::Thinking(
                rebon_tui::AssistantThinkingBlock {
                    thinking: text,
                    signature: None,
                },
            )],
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    })
}

fn system_message(
    uuid: String,
    timestamp: &str,
    subtype: &str,
    text: String,
) -> rebon_tui::Message {
    rebon_tui::Message::System(rebon_tui::SystemMessage {
        uuid,
        timestamp: timestamp.to_string(),
        subtype: subtype.into(),
        content: Some(text),
        level: Some(rebon_tui::SystemLevel::Info),
        is_meta: None,
    })
}

fn sync_projected_transcript(state: &mut rebon_tui::AppState, desired: Vec<rebon_tui::Message>) {
    let current = state.transcript.rows();
    let same_order = current
        .iter()
        .zip(&desired)
        .all(|(left, right)| left.uuid() == right.uuid());
    if !same_order {
        state.transcript.clear();
        for message in desired {
            state.transcript.push(message);
        }
        return;
    }

    let replacements = current
        .iter()
        .zip(&desired)
        .filter(|(left, right)| left != right)
        .map(|(_, right)| right.clone())
        .collect::<Vec<_>>();
    for message in replacements {
        state.transcript.upsert(message);
    }
    state.transcript.truncate(desired.len());
    let append_from = state.transcript.len();
    for message in desired.into_iter().skip(append_from) {
        state.transcript.push(message);
    }
}

#[cfg(test)]
pub(super) fn local_agent_state_from_snapshot(
    snapshot: &rebon_plugin_tasks::runtime::TaskSnapshot,
) -> rebon_tui::AppState {
    let mut state = rebon_tui::AppState::default();
    sync_local_agent_state_from_snapshot(&mut state, snapshot);
    state
}

pub(super) fn sync_local_agent_state_from_snapshot(
    state: &mut rebon_tui::AppState,
    snapshot: &rebon_plugin_tasks::runtime::TaskSnapshot,
) {
    use rebon_plugin_tasks::runtime::TaskData;

    let timestamp = snapshot_timestamp(snapshot);
    let mut messages = Vec::new();
    if let TaskData::InProcessTeammate(data) = &snapshot.data {
        // Status header goes FIRST with a stable uuid, so transcript growth stays pure
        // tail-append and `sync_projected_transcript` keeps its incremental path.
        let status_label = if snapshot.status == rebon_plugin_tasks::runtime::TaskStatus::Running
            && data.is_idle
        {
            "idle"
        } else {
            snapshot.status.as_str()
        };
        let mut lines = vec![format!("{} · {}", data.identity.agent_name, status_label)];
        if data.awaiting_plan_approval {
            lines.push("awaiting plan approval".to_string());
        }
        if !data.pending_user_messages.is_empty() {
            lines.push(format!(
                "{} pending user message(s)",
                data.pending_user_messages.len()
            ));
        }
        if let Some(error) = snapshot.error.as_ref() {
            lines.push(format!("error: {error}"));
        }
        messages.push(system_message(
            format!("s-teammate-status-{}", snapshot.id.as_str()),
            &timestamp,
            "teammate_status",
            lines.join("\n"),
        ));
        // Teammate turns append their prompt to the transcript, so the standalone prompt
        // message is only needed before the first turn has started.
        if data.transcript.is_empty() {
            messages.push(user_message(
                format!("u-teammate-prompt-{}", snapshot.id.as_str()),
                &timestamp,
                display_prompt_text(&data.prompt),
            ));
        }
        messages.extend(project_agent_transcript(
            snapshot.id.as_str(),
            &timestamp,
            &data.transcript,
            data.streaming_text.as_deref(),
        ));
        sync_projected_transcript(state, messages);
        rebuild_agent_overlay(state, &data.transcript, data.streaming_text.as_deref());
        return;
    }
    if let TaskData::LocalAgent(data) = &snapshot.data {
        messages.push(user_message(
            format!("u-agent-prompt-{}", snapshot.id.as_str()),
            &timestamp,
            display_prompt_text(&data.prompt),
        ));

        messages.extend(project_agent_transcript(
            snapshot.id.as_str(),
            &timestamp,
            &data.transcript,
            data.streaming_text.as_deref(),
        ));

        sync_projected_transcript(state, messages);
        rebuild_agent_overlay(state, &data.transcript, data.streaming_text.as_deref());
        return;
    }

    sync_projected_transcript(state, messages);
    state.overlay.clear();
}

struct PendingAgentTool {
    name: String,
    input: serde_json::Value,
}

fn project_agent_transcript(
    snapshot_id: &str,
    timestamp: &str,
    transcript: &[rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry],
    streaming_text: Option<&str>,
) -> Vec<rebon_tui::Message> {
    use rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry;

    let streaming_assistant_index = streaming_text.and_then(|streaming_text| {
        transcript.last().and_then(|entry| match entry {
            LocalAgentTranscriptEntry::Assistant { text } if streaming_text.starts_with(text) => {
                Some(transcript.len() - 1)
            }
            _ => None,
        })
    });
    let mut pending_tools = HashMap::<String, PendingAgentTool>::new();
    let mut messages = Vec::new();

    for (index, entry) in transcript.iter().enumerate() {
        match entry {
            LocalAgentTranscriptEntry::User { text } => messages.push(user_message(
                format!("u-agent-{snapshot_id}-{index}"),
                timestamp,
                text.clone(),
            )),
            LocalAgentTranscriptEntry::Thinking { text } => messages.push(thinking_message(
                format!("t-agent-{snapshot_id}-{index}"),
                timestamp,
                text.clone(),
            )),
            LocalAgentTranscriptEntry::Assistant { text }
                if streaming_assistant_index != Some(index) =>
            {
                messages.push(assistant_message(
                    format!("a-agent-{snapshot_id}-{index}"),
                    timestamp,
                    text.clone(),
                ));
            }
            LocalAgentTranscriptEntry::Assistant { .. } => {}
            LocalAgentTranscriptEntry::ToolStart {
                tool_use_id,
                name,
                input,
                ..
            } => {
                pending_tools.insert(
                    tool_use_id.clone(),
                    PendingAgentTool {
                        name: name.clone(),
                        input: input.clone(),
                    },
                );
            }
            LocalAgentTranscriptEntry::ToolProgress { .. } => {}
            LocalAgentTranscriptEntry::ToolFinish {
                tool_use_id,
                name,
                outcome,
                ..
            } => {
                let pending = pending_tools.remove(tool_use_id);
                let tool_name = pending
                    .as_ref()
                    .map(|tool| tool.name.clone())
                    .unwrap_or_else(|| name.clone());
                let input = pending
                    .map(|tool| tool.input)
                    .unwrap_or(serde_json::Value::Null);
                messages.push(completed_tool_message(
                    format!("a-agent-tool-{snapshot_id}-{index}"),
                    timestamp,
                    tool_use_id.clone(),
                    tool_name,
                    input,
                    outcome,
                ));
            }
        }
    }

    messages
}

fn completed_tool_message(
    uuid: String,
    timestamp: &str,
    tool_use_id: String,
    name: String,
    input: serde_json::Value,
    outcome: &Result<serde_json::Value, String>,
) -> rebon_tui::Message {
    let (status, raw_output, title) = match outcome {
        Ok(output) => {
            let raw_output = if matches!(display_tool_kind(&name), ToolKind::Edit) {
                trim_raw_output_for_transcript(&name, output.clone())
            } else {
                output.clone()
            };
            (ToolCallStatus::Completed, raw_output, None)
        }
        Err(error) => {
            let raw_output = if matches!(display_tool_kind(&name), ToolKind::Execute) {
                serde_json::json!({
                    "stdout": "",
                    "stderr": error,
                    "error": error,
                })
            } else {
                serde_json::json!({ "error": error })
            };
            (ToolCallStatus::Failed, raw_output, Some(error.clone()))
        }
    };
    rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
        uuid,
        timestamp: timestamp.to_string(),
        message: rebon_tui::AssistantMessageInner {
            role: rebon_tui::AssistantRole::Assistant,
            content: vec![rebon_tui::AssistantContentBlock::ToolUse(
                rebon_tui::AssistantToolUseBlock {
                    id: tool_use_id,
                    name,
                    input,
                    tool_call_content: None,
                    raw_output: Some(raw_output),
                    title,
                    locations: None,
                    status: Some(status),
                },
            )],
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    })
}

fn rebuild_agent_overlay(
    state: &mut rebon_tui::AppState,
    transcript: &[rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry],
    streaming_text: Option<&str>,
) {
    use rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry;

    state.overlay.clear();
    if let Some(text) = streaming_text.filter(|text| !text.trim().is_empty()) {
        state.overlay.set_streaming_text(text.to_string());
    }
    let finished_tool_ids = transcript
        .iter()
        .filter_map(|entry| match entry {
            LocalAgentTranscriptEntry::ToolFinish { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    for entry in transcript.iter().cloned() {
        match entry {
            LocalAgentTranscriptEntry::ToolStart {
                tool_use_id,
                name,
                input,
                activity,
            } => {
                if finished_tool_ids.contains(tool_use_id.as_str()) {
                    continue;
                }
                state.overlay.upsert_streaming_tool_use(StreamingToolUse {
                    call_id: tool_use_id,
                    tool_name: name.clone(),
                    kind: display_tool_kind(&name),
                    status: ToolCallStatus::InProgress,
                    title: Some(activity),
                    content: None,
                    locations: None,
                    raw_input: input
                        .as_object()
                        .map(|map| map.clone().into_iter().collect()),
                    raw_output: None,
                });
            }
            LocalAgentTranscriptEntry::ToolProgress {
                tool_use_id,
                name,
                message,
            } => {
                if finished_tool_ids.contains(tool_use_id.as_str()) {
                    continue;
                }
                if !state
                    .overlay
                    .blocks
                    .iter()
                    .any(|block| matches!(block, StreamingContentBlock::ToolUse(tool) if tool.call_id == tool_use_id))
                {
                    state.overlay.upsert_streaming_tool_use(StreamingToolUse {
                        call_id: tool_use_id.clone(),
                        tool_name: name.clone(),
                        kind: display_tool_kind(&name),
                        status: ToolCallStatus::InProgress,
                        title: None,
                        content: None,
                        locations: None,
                        raw_input: None,
                        raw_output: None,
                    });
                }
                state.overlay.update_streaming_tool_use(
                    &tool_use_id,
                    Some(ToolCallStatus::InProgress),
                    Some(format!("{name}: {message}")),
                    None,
                    None,
                    None,
                );
            }
            LocalAgentTranscriptEntry::ToolFinish { .. } => {}
            LocalAgentTranscriptEntry::User { .. }
            | LocalAgentTranscriptEntry::Thinking { .. }
            | LocalAgentTranscriptEntry::Assistant { .. } => {}
        }
    }
}

fn is_interactable_agent_snapshot(snapshot: &rebon_plugin_tasks::runtime::TaskSnapshot) -> bool {
    use rebon_plugin_tasks::runtime::{TaskKind, TaskStatus};

    match snapshot.kind {
        TaskKind::LocalAgent => {
            matches!(snapshot.status, TaskStatus::Pending | TaskStatus::Running)
        }
        TaskKind::InProcessTeammate => matches!(
            snapshot.status,
            TaskStatus::Pending | TaskStatus::Running | TaskStatus::Failed
        ),
        _ => false,
    }
}

fn is_read_only_agent_snapshot(
    app: &AppState,
    task_id: &str,
    snapshot: &rebon_plugin_tasks::runtime::TaskSnapshot,
) -> bool {
    if app.is_remote_agent_task(task_id) {
        return snapshot.status.is_terminal();
    }
    crate::tui::agent_switcher::is_external_acp_agent_snapshot(snapshot)
}

pub(super) fn is_read_only_agent_task(app: &AppState, task_id: &str) -> bool {
    app.agent_task_snapshot(task_id)
        .as_ref()
        .is_some_and(|snapshot| is_read_only_agent_snapshot(app, task_id, snapshot))
}

pub(super) fn prune_stored_agent_views(
    app: &mut AppState,
    local_snapshots: &[rebon_plugin_tasks::runtime::TaskSnapshot],
) {
    if app.local_agent_views.is_empty() {
        return;
    }
    let remote_snapshots = &app.remote_background_tasks;
    app.local_agent_views.retain(|task_id, _| {
        if let Some(snapshot) = local_snapshots
            .iter()
            .find(|snapshot| snapshot.id.as_str() == task_id)
        {
            return is_interactable_agent_snapshot(snapshot);
        }
        remote_snapshots
            .get(task_id)
            .is_some_and(|snapshot| matches!(snapshot.task.status.as_str(), "pending" | "running"))
    });
}

fn take_synced_local_agent_view(
    app: &mut AppState,
    task_id: &str,
    snapshot: &rebon_plugin_tasks::runtime::TaskSnapshot,
) -> StoredTranscriptView {
    let mut view = app.local_agent_views.remove(task_id).unwrap_or_default();
    sync_local_agent_state_from_snapshot(&mut view.tui, snapshot);
    view
}

#[derive(Clone, Copy)]
enum ForegroundViewDisposition {
    RetainIfInteractable,
    Drop,
}

fn leave_foreground_for_main(app: &mut AppState, disposition: ForegroundViewDisposition) -> bool {
    let Some(task_id) = app.foregrounded_task_id.clone() else {
        return false;
    };
    let snapshot = app.agent_task_snapshot(&task_id);
    let retain = matches!(disposition, ForegroundViewDisposition::RetainIfInteractable)
        && snapshot
            .as_ref()
            .is_some_and(is_interactable_agent_snapshot);
    if retain {
        sync_local_agent_state_from_snapshot(
            &mut app.rebon_tui,
            snapshot.as_ref().expect("retain requires a task snapshot"),
        );
    }

    let main = app.main_agent_view.take();
    debug_assert!(
        main.is_some(),
        "foreground agent must preserve the main view"
    );
    let foreground = app.replace_active_transcript_view(main.unwrap_or_default());
    app.local_agent_views.remove(&task_id);
    if retain {
        app.local_agent_views.insert(task_id, foreground);
    }

    app.foregrounded_task_id = None;
    app.default_placeholder = Some(String::from("Type to start a session, Enter to submit"));
    app.teammate_footer_index = 0;
    reset_transcript_page_state(app);
    app.pending_page_hard_refresh = Some(current_page_name(app));
    true
}

pub(super) fn with_main_agent_view<R>(app: &mut AppState, f: impl FnOnce(&mut AppState) -> R) -> R {
    let Some(foregrounded_task_id) = app.foregrounded_task_id.clone() else {
        return f(app);
    };

    let main = app.main_agent_view.take();
    debug_assert!(
        main.is_some(),
        "foreground agent must preserve the main view"
    );
    let foreground = app.replace_active_transcript_view(main.unwrap_or_default());
    let result = f(app);
    if app.foregrounded_task_id.as_deref() == Some(foregrounded_task_id.as_str())
        && app.main_agent_view.is_none()
    {
        let main = app.replace_active_transcript_view(foreground);
        app.main_agent_view = Some(main);
    }
    result
}

pub(super) fn drain_main_agent_updates(
    app: &mut AppState,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionUpdateParams>,
) -> usize {
    with_main_agent_view(app, |app| drain_pending_updates(app, rx))
}

pub(super) fn release_terminal_foreground_agent(app: &mut AppState, tasks: &TaskRegistry) -> bool {
    use rebon_plugin_tasks::runtime::{TaskId, TaskKind};

    let Some(task_id) = app.foregrounded_task_id.clone() else {
        return false;
    };
    let Some(snapshot) = app.agent_task_snapshot(&task_id) else {
        return leave_foreground_for_main(app, ForegroundViewDisposition::Drop);
    };
    if is_interactable_agent_snapshot(&snapshot) {
        return false;
    }
    if !matches!(
        snapshot.kind,
        TaskKind::LocalAgent | TaskKind::InProcessTeammate
    ) || !snapshot.status.is_terminal()
    {
        return false;
    }

    tasks.set_backgrounded(&TaskId::new(&task_id));
    leave_foreground_for_main(app, ForegroundViewDisposition::Drop);
    inject_system_message(
        app,
        "local_command",
        &format!(
            "Agent \"{}\" is {} and can no longer receive messages. Use /tasks to view its result or start a new Agent for follow-up work.",
            snapshot.title,
            snapshot.status.as_str()
        ),
    );
    app.follow_transcript_tail = true;
    true
}

pub(super) fn sync_foreground_agent_view(app: &mut AppState, tasks: &TaskRegistry) {
    let Some(task_id) = app.foregrounded_task_id.clone() else {
        return;
    };
    let Some(snapshot) = app.agent_task_snapshot(&task_id) else {
        leave_foreground_for_main(app, ForegroundViewDisposition::Drop);
        return;
    };
    if snapshot.status.is_terminal() && !is_interactable_agent_snapshot(&snapshot) {
        if is_read_only_agent_snapshot(app, &task_id, &snapshot) {
            sync_local_agent_state_from_snapshot(&mut app.rebon_tui, &snapshot);
            if app.follow_transcript_tail {
                repin_transcript_to_bottom(app);
            }
        } else {
            release_terminal_foreground_agent(app, tasks);
        }
        return;
    }
    if !is_interactable_agent_snapshot(&snapshot) {
        leave_foreground_for_main(app, ForegroundViewDisposition::Drop);
        return;
    }

    debug_assert!(
        !app.local_agent_views.contains_key(&task_id),
        "the active agent view must not also be stored"
    );
    sync_local_agent_state_from_snapshot(&mut app.rebon_tui, &snapshot);
    if app.follow_transcript_tail {
        repin_transcript_to_bottom(app);
    }
}

pub(super) fn pause_preserve_task(
    app: &mut AppState,
    _active_prompt: &mut Option<ActivePrompt>,
    tasks: &TaskRegistry,
    task_id: &str,
) -> bool {
    let was_foreground = app.foregrounded_task_id.as_deref() == Some(task_id);
    let updated = tasks.set_backgrounded(&rebon_plugin_tasks::runtime::TaskId::new(task_id));
    if updated && was_foreground {
        leave_foreground_for_main(app, ForegroundViewDisposition::RetainIfInteractable);
    }
    updated
}

pub(in crate::tui::runner) fn current_page_name(app: &AppState) -> String {
    use rebon_plugin_tasks::runtime::TaskData;

    let Some(task_id) = app.foregrounded_task_id.as_deref() else {
        return "Main".to_string();
    };
    let Some(snapshot) = app.agent_task_snapshot(task_id) else {
        return format!("Agent: {task_id}");
    };
    let title = snapshot.title.trim();
    let candidate = if !title.is_empty() {
        title
    } else {
        match &snapshot.data {
            TaskData::LocalAgent(data) => data.agent_type.trim(),
            TaskData::InProcessTeammate(data) => data.identity.agent_name.trim(),
            _ => task_id,
        }
    };
    let name = if candidate.is_empty() {
        task_id
    } else {
        candidate
    };
    format!("Agent: {name}")
}

pub(super) fn switch_to_main_agent(app: &mut AppState) {
    leave_foreground_for_main(app, ForegroundViewDisposition::RetainIfInteractable);
    app.default_placeholder = Some(String::from("Type to start a session, Enter to submit"));
    app.teammate_footer_index = 0;
}

pub(super) fn switch_to_live_agent(
    app: &mut AppState,
    _active_prompt: &mut Option<ActivePrompt>,
    task_id: &str,
) -> bool {
    let Some(snapshot) = app.agent_task_snapshot(task_id) else {
        return false;
    };
    let read_only = is_read_only_agent_snapshot(app, task_id, &snapshot);
    if !is_interactable_agent_snapshot(&snapshot) && !read_only {
        return false;
    }

    let rows =
        crate::tui::agent_switcher::build_agent_switcher_rows(&app.agent_task_snapshots(), false);
    let footer_index = rows
        .iter()
        .position(|row| row.task_id.as_deref() == Some(task_id))
        .unwrap_or(0);
    let placeholder = if read_only {
        if snapshot.status.is_terminal() {
            format!(
                "Viewing {} (read-only, {}) — switch to Main to continue",
                snapshot.title,
                snapshot.status.as_str()
            )
        } else {
            format!("Viewing {} (read-only) — Ctrl+C stops it", snapshot.title)
        }
    } else {
        format!("Message {}", snapshot.title)
    };

    if app.foregrounded_task_id.as_deref() == Some(task_id) {
        debug_assert!(
            !app.local_agent_views.contains_key(task_id),
            "the active agent view must not also be stored"
        );
        app.local_agent_views.remove(task_id);
        sync_local_agent_state_from_snapshot(&mut app.rebon_tui, &snapshot);
        repin_transcript_to_bottom(app);
        app.teammate_footer_index = footer_index;
        app.default_placeholder = Some(placeholder);
        return true;
    }

    let next = take_synced_local_agent_view(app, task_id, &snapshot);
    if let Some(previous_id) = app.foregrounded_task_id.clone() {
        let previous_snapshot = app.agent_task_snapshot(&previous_id);
        let retain_previous = previous_snapshot
            .as_ref()
            .is_some_and(is_interactable_agent_snapshot);
        if retain_previous {
            sync_local_agent_state_from_snapshot(
                &mut app.rebon_tui,
                previous_snapshot
                    .as_ref()
                    .expect("retain requires a task snapshot"),
            );
        }
        app.local_agent_views.remove(&previous_id);
        let previous = app.replace_active_transcript_view(next);
        if retain_previous {
            app.local_agent_views.insert(previous_id, previous);
        }
        debug_assert!(
            app.main_agent_view.is_some(),
            "foreground agent must preserve the main view"
        );
    } else {
        debug_assert!(
            app.main_agent_view.is_none(),
            "main view must not be stored while main is active"
        );
        app.main_agent_view = None;
        let main = app.replace_active_transcript_view(next);
        app.main_agent_view = Some(main);
    }

    app.foregrounded_task_id = Some(task_id.to_string());
    app.teammate_footer_index = footer_index;
    app.default_placeholder = Some(placeholder);
    reset_transcript_page_state(app);
    app.pending_page_hard_refresh = Some(current_page_name(app));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use super::super::test_support::{insert_local_agent_task, local_agent_snapshot};

    fn remote_task_snapshot(
        task_id: &str,
        status: &str,
    ) -> rebon_session_host::BackgroundTaskSnapshot {
        rebon_session_host::BackgroundTaskSnapshot {
            task: rebon_session_host::BackgroundTaskDescriptor {
                task_id: task_id.into(),
                title: "Inspect remote session".into(),
                kind: "local_agent".into(),
                status: status.into(),
                is_backgrounded: true,
                start_time_ms: 1,
                end_time_ms: None,
                last_progress: Some("reading source".into()),
                error: None,
                prompt: None,
                parent_tool_call_id: Some("tool-remote".into()),
                agent_id: Some(task_id.into()),
                agent_name: Some("Explore".into()),
                agent_type: Some("Explore".into()),
                model: None,
                token_count: Some(10),
                tool_use_count: Some(2),
                result: None,
            },
            updated_at_ms: 2,
            log_preview: vec!["agent started".into(), "Grep src/".into()],
            transcript: Vec::new(),
        }
    }

    fn external_acp_task_snapshot(
        task_id: &str,
        status: rebon_plugin_tasks::runtime::TaskStatus,
    ) -> rebon_plugin_tasks::runtime::TaskSnapshot {
        let mut snapshot = local_agent_snapshot(task_id, status, true);
        let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data else {
            panic!("expected local agent");
        };
        data.model = Some("acp:claude:agent-default".into());
        data.transcript = vec![
            rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry::Assistant {
                text: "Visible ACP answer".into(),
            },
        ];
        snapshot
    }

    fn warm_active_measure_cache(app: &mut AppState, uuid: &str) {
        if app.rebon_tui.transcript.len() == 0 {
            app.rebon_tui
                .transcript
                .push(user_message(uuid.into(), "t", "cache row".into()));
        }
        let area = ratatui::layout::Rect::new(0, 0, 48, 12);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        rebon_tui::render_transcript_cached_with_running_hints(
            &app.rebon_tui,
            area,
            &mut buffer,
            &rebon_tui::RenderTheme::default(),
            0,
            rebon_tui::ToolOutputVerbosity::Compact,
            0,
            None,
            &mut app.transcript_measure_cache,
            false,
            rebon_tui::TranscriptRenderExtras::empty(),
        );
        assert!(!app.transcript_measure_cache.is_empty());
    }

    #[test]
    fn completed_tool_message_trims_updated_file_copy() {
        let message = completed_tool_message(
            "tool-row".into(),
            "timestamp",
            "tool-1".into(),
            "Edit".into(),
            serde_json::json!({"file_path": "src/lib.rs"}),
            &Ok(serde_json::json!({
                "filePath": "src/lib.rs",
                "oldString": "before",
                "newString": "after",
                "content": "COMPLETE UPDATED FILE",
                "originalFile": "COMPLETE ORIGINAL FILE"
            })),
        );

        let rebon_tui::Message::Assistant(message) = message else {
            panic!("expected assistant message");
        };
        let rebon_tui::AssistantContentBlock::ToolUse(tool) = &message.message.content[0] else {
            panic!("expected tool block");
        };
        let raw_output = tool.raw_output.as_ref().expect("raw output");
        assert!(raw_output.get("content").is_none());
        assert_eq!(raw_output["filePath"], "src/lib.rs");
        assert_eq!(raw_output["oldString"], "before");
        assert_eq!(raw_output["newString"], "after");
        assert_eq!(raw_output["originalFile"], "COMPLETE ORIGINAL FILE");
    }

    #[test]
    fn completed_tool_message_keeps_read_content_for_rendering() {
        let message = completed_tool_message(
            "tool-row".into(),
            "timestamp",
            "tool-1".into(),
            "Read".into(),
            serde_json::json!({"file_path": "src/lib.rs"}),
            &Ok(serde_json::json!({
                "file": {"filePath": "src/lib.rs", "content": "visible file body"}
            })),
        );

        let rebon_tui::Message::Assistant(message) = message else {
            panic!("expected assistant message");
        };
        let rebon_tui::AssistantContentBlock::ToolUse(tool) = &message.message.content[0] else {
            panic!("expected tool block");
        };
        assert_eq!(
            tool.raw_output.as_ref().expect("raw output")["file"]["content"],
            "visible file body"
        );
    }

    #[test]
    fn switch_to_live_agent_opens_running_external_acp_task_read_only() {
        let mut app = AppState::new();
        let registry = rebon_plugin_tasks::runtime::TaskRegistry::new();
        registry.insert(
            rebon_plugin_tasks::runtime::TaskId::new("agent-acp"),
            external_acp_task_snapshot(
                "agent-acp",
                rebon_plugin_tasks::runtime::TaskStatus::Running,
            ),
            rebon_types::PromptCancel::new(),
        );
        app.tasks = Arc::new(registry);
        let mut active_prompt = None;

        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-acp"
        ));

        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-acp"));
        assert!(app
            .default_placeholder
            .as_deref()
            .is_some_and(|placeholder| placeholder.contains("read-only")));
        assert!(app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::Assistant(message) if message.message.content.iter().any(|block| matches!(
                block,
                rebon_tui::AssistantContentBlock::Text(text) if text.text == "Visible ACP answer"
            ))
        )));
    }

    #[test]
    fn switch_to_live_agent_keeps_completed_external_acp_task_visible() {
        let mut app = AppState::new();
        let registry = rebon_plugin_tasks::runtime::TaskRegistry::new();
        registry.insert(
            rebon_plugin_tasks::runtime::TaskId::new("agent-acp"),
            external_acp_task_snapshot(
                "agent-acp",
                rebon_plugin_tasks::runtime::TaskStatus::Completed,
            ),
            rebon_types::PromptCancel::new(),
        );
        app.tasks = Arc::new(registry);
        let mut active_prompt = None;

        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-acp"
        ));
        let tasks = app.tasks.clone();
        sync_foreground_agent_view(&mut app, tasks.as_ref());

        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-acp"));
        assert!(app
            .default_placeholder
            .as_deref()
            .is_some_and(|placeholder| placeholder.contains("completed")));
        assert!(app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::Assistant(message) if message.message.content.iter().any(|block| matches!(
                block,
                rebon_tui::AssistantContentBlock::Text(text) if text.text == "Visible ACP answer"
            ))
        )));
    }

    #[test]
    fn switch_to_live_agent_opens_remote_task_as_an_interactive_mirror() {
        let mut app = AppState::new();
        app.remote_background_tasks.insert(
            "agent-remote".into(),
            remote_task_snapshot("agent-remote", "running"),
        );
        let mut active_prompt = None;

        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-remote"
        ));

        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-remote"));
        let placeholder = app.default_placeholder.as_deref().unwrap_or_default();
        assert!(
            placeholder.starts_with("Message "),
            "remote agents must remain interactive through their owner, got: {placeholder}"
        );
        assert!(
            app.rebon_tui.transcript.rows().iter().any(|row| matches!(
                row,
                rebon_tui::Message::Assistant(msg) if msg.message.content.iter().any(|block| matches!(
                    block,
                    rebon_tui::AssistantContentBlock::Text(text) if text.text.contains("Grep src/")
                ))
            )),
            "remote log preview must surface in the synthesized view"
        );
    }

    #[test]
    fn switch_to_live_agent_keeps_completed_remote_task_read_only() {
        let mut app = AppState::new();
        app.remote_background_tasks.insert(
            "agent-remote".into(),
            remote_task_snapshot("agent-remote", "completed"),
        );
        let mut active_prompt = None;

        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-remote"
        ));

        assert!(app
            .default_placeholder
            .as_deref()
            .is_some_and(|placeholder| placeholder.contains("read-only, completed")));
    }

    #[test]
    fn switch_to_live_agent_renders_remote_structured_tool_result() {
        let mut app = AppState::new();
        let mut snapshot = remote_task_snapshot("agent-remote", "running");
        snapshot.transcript = vec![
            rebon_session_host::BackgroundTaskTranscriptEntry::ToolStart {
                tool_use_id: "tool-bash".into(),
                name: "Bash".into(),
                input: serde_json::json!({ "command": "pwd" }),
                activity: "Running command".into(),
                timestamp_ms: 0,
            },
            rebon_session_host::BackgroundTaskTranscriptEntry::ToolFinish {
                tool_use_id: "tool-bash".into(),
                name: "Bash".into(),
                output: Some(serde_json::json!({
                    "stdout": "F:/remote/repo\n",
                    "stderr": "",
                    "exit_code": 0,
                })),
                error: None,
                timestamp_ms: 0,
            },
        ];
        app.remote_background_tasks
            .insert("agent-remote".into(), snapshot);
        let mut active_prompt = None;

        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-remote"
        ));

        assert!(app
            .rebon_tui
            .overlay
            .blocks
            .iter()
            .all(|block| !matches!(block, StreamingContentBlock::ToolUse(_))));
        let tool =
            app.rebon_tui
                .transcript
                .rows()
                .iter()
                .find_map(|row| match row {
                    rebon_tui::Message::Assistant(message) => message
                        .message
                        .content
                        .iter()
                        .find_map(|block| match block {
                            rebon_tui::AssistantContentBlock::ToolUse(tool) => Some(tool),
                            _ => None,
                        }),
                    _ => None,
                })
                .expect("remote completed tool row");
        assert_eq!(tool.status, Some(ToolCallStatus::Completed));
        assert_eq!(
            tool.raw_output
                .as_ref()
                .and_then(|output| output.get("stdout"))
                .and_then(serde_json::Value::as_str),
            Some("F:/remote/repo\n")
        );
    }

    #[test]
    fn sync_foreground_agent_view_keeps_terminal_remote_task_visible() {
        let mut app = AppState::new();
        app.remote_background_tasks.insert(
            "agent-remote".into(),
            remote_task_snapshot("agent-remote", "running"),
        );
        let mut active_prompt = None;
        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-remote"
        ));

        app.remote_background_tasks
            .get_mut("agent-remote")
            .expect("remote task")
            .task
            .status = "completed".into();
        let tasks = app.tasks.clone();
        sync_foreground_agent_view(&mut app, tasks.as_ref());

        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-remote"));
    }

    #[test]
    fn local_agent_snapshot_view_trims_prompt_trailing_blank_lines() {
        let mut snapshot = local_agent_snapshot(
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data else {
            panic!("expected local agent");
        };
        data.prompt = "Search the repo\n\n\n".into();

        let state = local_agent_state_from_snapshot(&snapshot);

        let Some(rebon_tui::Message::User(user)) = state.transcript.rows().first() else {
            panic!("expected initial user prompt");
        };
        assert!(user.message.content.iter().any(|block| matches!(
            block,
            rebon_tui::UserContentBlock::Text(text) if text.text == "Search the repo"
        )));
    }

    #[test]
    fn local_agent_snapshot_view_replays_thinking_blocks() {
        use rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry;

        let mut snapshot = local_agent_snapshot(
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data else {
            panic!("expected local agent");
        };
        data.transcript.push(LocalAgentTranscriptEntry::Thinking {
            text: "Inspecting the event flow".into(),
        });

        let state = local_agent_state_from_snapshot(&snapshot);
        let thinking =
            state
                .transcript
                .rows()
                .iter()
                .find_map(|row| match row {
                    rebon_tui::Message::Assistant(message) => message
                        .message
                        .content
                        .iter()
                        .find_map(|block| match block {
                            rebon_tui::AssistantContentBlock::Thinking(thinking) => Some(thinking),
                            _ => None,
                        }),
                    _ => None,
                })
                .expect("thinking row");

        assert_eq!(thinking.thinking, "Inspecting the event flow");
        assert!(thinking.signature.is_none());
    }

    #[test]
    fn completed_agent_tool_is_committed_with_real_output() {
        use rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry;

        let mut snapshot = local_agent_snapshot(
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data else {
            panic!("expected local agent");
        };
        data.transcript.extend([
            LocalAgentTranscriptEntry::ToolStart {
                tool_use_id: "tool-bash".into(),
                name: "Bash".into(),
                input: serde_json::json!({ "command": "pwd" }),
                activity: "Running command".into(),
            },
            LocalAgentTranscriptEntry::ToolProgress {
                tool_use_id: "tool-bash".into(),
                name: "Bash".into(),
                message: "waiting".into(),
            },
            LocalAgentTranscriptEntry::ToolFinish {
                tool_use_id: "tool-bash".into(),
                name: "Bash".into(),
                ok: true,
                summary: "Bash ok".into(),
                outcome: Ok(serde_json::json!({
                    "stdout": "C:/projects/example\n",
                    "stderr": "",
                    "exit_code": 0,
                })),
            },
        ]);

        let state = local_agent_state_from_snapshot(&snapshot);

        assert!(state
            .overlay
            .blocks
            .iter()
            .all(|block| !matches!(block, StreamingContentBlock::ToolUse(_))));
        assert!(state.transcript.rows().iter().all(|row| !matches!(
            row,
            rebon_tui::Message::System(message) if message.subtype == "agent_tool"
        )));
        let tool =
            state
                .transcript
                .rows()
                .iter()
                .find_map(|row| match row {
                    rebon_tui::Message::Assistant(message) => message
                        .message
                        .content
                        .iter()
                        .find_map(|block| match block {
                            rebon_tui::AssistantContentBlock::ToolUse(tool) => Some(tool),
                            _ => None,
                        }),
                    _ => None,
                })
                .expect("completed tool row");
        assert_eq!(tool.id, "tool-bash");
        assert_eq!(
            tool.input
                .get("command")
                .and_then(serde_json::Value::as_str),
            Some("pwd")
        );
        assert_eq!(tool.status, Some(ToolCallStatus::Completed));
        assert_eq!(
            tool.raw_output
                .as_ref()
                .and_then(|output| output.get("stdout"))
                .and_then(serde_json::Value::as_str),
            Some("C:/projects/example\n")
        );

        let area = ratatui::layout::Rect::new(0, 0, 80, 12);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        let mut cache = rebon_tui::TranscriptMeasureCache::new();
        rebon_tui::render_transcript_cached_with_running_hints(
            &state,
            area,
            &mut buffer,
            &rebon_tui::RenderTheme::plain(),
            usize::MAX,
            rebon_tui::ToolOutputVerbosity::Compact,
            0,
            None,
            &mut cache,
            false,
            rebon_tui::TranscriptRenderExtras::empty(),
        );
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            rendered.contains("C:/projects/example"),
            "the committed tool result must be rendered: {rendered:?}"
        );
    }

    #[test]
    fn unfinished_agent_tool_stays_in_streaming_overlay() {
        use rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry;

        let mut snapshot = local_agent_snapshot(
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data else {
            panic!("expected local agent");
        };
        data.transcript.extend([
            LocalAgentTranscriptEntry::ToolStart {
                tool_use_id: "tool-bash".into(),
                name: "Bash".into(),
                input: serde_json::json!({ "command": "cargo test" }),
                activity: "Running tests".into(),
            },
            LocalAgentTranscriptEntry::ToolProgress {
                tool_use_id: "tool-bash".into(),
                name: "Bash".into(),
                message: "still running".into(),
            },
        ]);

        let state = local_agent_state_from_snapshot(&snapshot);

        assert_eq!(state.transcript.len(), 1, "only the prompt is committed");
        let tool = state
            .overlay
            .blocks
            .iter()
            .find_map(|block| match block {
                StreamingContentBlock::ToolUse(tool) => Some(tool),
                _ => None,
            })
            .expect("pending tool overlay");
        assert_eq!(tool.call_id, "tool-bash");
        assert_eq!(tool.status, ToolCallStatus::InProgress);
        assert_eq!(tool.title.as_deref(), Some("Bash: still running"));
    }

    #[test]
    fn failed_agent_tool_commits_error_output() {
        use rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry;

        let mut snapshot = local_agent_snapshot(
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data else {
            panic!("expected local agent");
        };
        data.transcript.extend([
            LocalAgentTranscriptEntry::ToolStart {
                tool_use_id: "tool-bash".into(),
                name: "Bash".into(),
                input: serde_json::json!({ "command": "false" }),
                activity: "Running command".into(),
            },
            LocalAgentTranscriptEntry::ToolFinish {
                tool_use_id: "tool-bash".into(),
                name: "Bash".into(),
                ok: false,
                summary: "Bash error: exit 1".into(),
                outcome: Err("exit 1".into()),
            },
        ]);

        let state = local_agent_state_from_snapshot(&snapshot);
        let tool =
            state
                .transcript
                .rows()
                .iter()
                .find_map(|row| match row {
                    rebon_tui::Message::Assistant(message) => message
                        .message
                        .content
                        .iter()
                        .find_map(|block| match block {
                            rebon_tui::AssistantContentBlock::ToolUse(tool) => Some(tool),
                            _ => None,
                        }),
                    _ => None,
                })
                .expect("failed tool row");
        assert_eq!(tool.status, Some(ToolCallStatus::Failed));
        assert_eq!(tool.title.as_deref(), Some("exit 1"));
        assert_eq!(
            tool.raw_output
                .as_ref()
                .and_then(|output| output.get("stderr"))
                .and_then(serde_json::Value::as_str),
            Some("exit 1")
        );
    }

    #[test]
    fn stable_agent_snapshot_does_not_rebuild_transcript() {
        let mut snapshot = local_agent_snapshot(
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data else {
            panic!("expected local agent");
        };
        data.transcript.push(
            rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry::Assistant {
                text: "current output".into(),
            },
        );
        let mut state = rebon_tui::AppState::default();
        sync_local_agent_state_from_snapshot(&mut state, &snapshot);
        let revision = state.transcript.revision();
        let row_revisions = state.transcript.row_revisions().to_vec();

        sync_local_agent_state_from_snapshot(&mut state, &snapshot);

        assert_eq!(state.transcript.revision(), revision);
        assert_eq!(state.transcript.row_revisions(), row_revisions);
    }

    #[test]
    fn agent_snapshot_sync_appends_new_rows_without_rebuilding_prefix() {
        let mut snapshot = local_agent_snapshot(
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data else {
            panic!("expected local agent");
        };
        data.transcript.push(
            rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry::Assistant {
                text: "first output".into(),
            },
        );
        let mut state = rebon_tui::AppState::default();
        sync_local_agent_state_from_snapshot(&mut state, &snapshot);
        let prefix_revisions = state.transcript.row_revisions().to_vec();

        let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data else {
            panic!("expected local agent");
        };
        data.transcript.push(
            rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry::Assistant {
                text: "second output".into(),
            },
        );
        sync_local_agent_state_from_snapshot(&mut state, &snapshot);

        assert_eq!(state.transcript.len(), 3);
        assert_eq!(
            &state.transcript.row_revisions()[..prefix_revisions.len()],
            prefix_revisions
        );
        assert_eq!(state.transcript.rows()[2].uuid(), Some("a-agent-agent-1-1"));
    }

    fn teammate_snapshot(
        id: &str,
        transcript: Vec<rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry>,
        streaming_text: Option<&str>,
        is_idle: bool,
        pending: Vec<String>,
    ) -> rebon_plugin_tasks::runtime::TaskSnapshot {
        use rebon_plugin_tasks::runtime::{
            InProcessTeammateData, TaskData, TaskId, TaskKind, TaskSnapshot, TaskStatus,
            TeammateIdentity, TeammateRequest,
        };
        TaskSnapshot {
            id: TaskId::new(id),
            kind: TaskKind::InProcessTeammate,
            status: TaskStatus::Running,
            title: format!("@{id}"),
            last_progress: None,
            error: None,
            result: None,
            is_backgrounded: true,
            notified: false,
            start_time_ms: 1_000,
            end_time_ms: None,
            metadata: serde_json::json!({}),
            data: TaskData::InProcessTeammate(Box::new(InProcessTeammateData {
                identity: TeammateIdentity {
                    agent_id: format!("{id}@team"),
                    agent_name: id.into(),
                    team_name: "team".into(),
                    color: None,
                    plan_mode_required: false,
                    parent_session_id: "leader".into(),
                },
                prompt: "initial prompt".into(),
                model: None,
                model_profile: None,
                permission_mode: "default".into(),
                awaiting_plan_approval: false,
                is_idle,
                shutdown_requested: false,
                pending_user_messages: pending
                    .into_iter()
                    .enumerate()
                    .map(|(index, message)| TeammateRequest {
                        request_id: format!("test-request-{index}"),
                        message,
                    })
                    .collect(),
                tool_use_count: 0,
                token_count: 0,
                transcript,
                streaming_text: streaming_text.map(str::to_string),
            })),
        }
    }

    #[test]
    fn failed_teammate_remains_interactable_for_restart() {
        let mut teammate = teammate_snapshot("mate-failed", Vec::new(), None, false, Vec::new());
        teammate.status = rebon_plugin_tasks::runtime::TaskStatus::Failed;
        assert!(is_interactable_agent_snapshot(&teammate));

        let mut local = local_agent_snapshot(
            "agent-failed",
            rebon_plugin_tasks::runtime::TaskStatus::Failed,
            true,
        );
        local.status = rebon_plugin_tasks::runtime::TaskStatus::Failed;
        assert!(!is_interactable_agent_snapshot(&local));
    }

    #[test]
    fn teammate_snapshot_projects_status_header_then_transcript() {
        use rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry;

        let snapshot = teammate_snapshot(
            "mate-1",
            vec![
                LocalAgentTranscriptEntry::User {
                    text: "investigate x".into(),
                },
                LocalAgentTranscriptEntry::Assistant {
                    text: "done".into(),
                },
            ],
            None,
            true,
            vec!["follow up".into()],
        );

        let state = local_agent_state_from_snapshot(&snapshot);

        let rows = state.transcript.rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].uuid(), Some("s-teammate-status-mate-1"));
        let Some(rebon_tui::Message::System(status)) = rows.first() else {
            panic!("expected status header");
        };
        let content = status.content.as_deref().unwrap_or_default();
        assert!(content.contains("mate-1 · idle"), "got: {content}");
        assert!(
            content.contains("1 pending user message(s)"),
            "got: {content}"
        );
        assert_eq!(rows[1].uuid(), Some("u-agent-mate-1-0"));
        assert_eq!(rows[2].uuid(), Some("a-agent-mate-1-1"));
    }

    #[test]
    fn teammate_streaming_assistant_stays_in_overlay_until_final() {
        use rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry;

        let snapshot = teammate_snapshot(
            "mate-2",
            vec![
                LocalAgentTranscriptEntry::User {
                    text: "investigate x".into(),
                },
                LocalAgentTranscriptEntry::Assistant {
                    text: "partial".into(),
                },
            ],
            Some("partial output still streaming"),
            false,
            Vec::new(),
        );

        let state = local_agent_state_from_snapshot(&snapshot);

        // The in-flight assistant entry is skipped in the committed transcript (status
        // header + user prompt only) and rendered through the streaming overlay instead.
        let rows = state.transcript.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].uuid(), Some("u-agent-mate-2-0"));
        assert_eq!(
            state.overlay.combined_streaming_text().as_deref(),
            Some("partial output still streaming")
        );
    }

    #[test]
    fn teammate_snapshot_sync_appends_new_rows_without_rebuilding_prefix() {
        use rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry;

        let mut snapshot = teammate_snapshot(
            "mate-3",
            vec![LocalAgentTranscriptEntry::User {
                text: "investigate x".into(),
            }],
            None,
            false,
            Vec::new(),
        );
        let mut state = rebon_tui::AppState::default();
        sync_local_agent_state_from_snapshot(&mut state, &snapshot);
        let prefix_revisions = state.transcript.row_revisions().to_vec();

        let rebon_plugin_tasks::runtime::TaskData::InProcessTeammate(data) = &mut snapshot.data
        else {
            panic!("expected teammate");
        };
        data.transcript.push(LocalAgentTranscriptEntry::Assistant {
            text: "answer".into(),
        });
        sync_local_agent_state_from_snapshot(&mut state, &snapshot);

        // Status header keeps position 0, so growth is a pure tail
        // append — the prefix must not be rebuilt.
        assert_eq!(state.transcript.len(), 3);
        assert_eq!(
            &state.transcript.row_revisions()[..prefix_revisions.len()],
            prefix_revisions
        );
        assert_eq!(state.transcript.rows()[2].uuid(), Some("a-agent-mate-3-1"));
    }

    #[test]
    fn streaming_agent_assistant_commits_only_after_final_text_arrives() {
        use super::super::inline_commit_cursor::{flush_inline_commits, InlineRuntimeState};
        use ratatui::{backend::TestBackend, Terminal, TerminalOptions, Viewport};

        fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
            buf.content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        }

        let mut snapshot = local_agent_snapshot(
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data else {
            panic!("expected local agent");
        };
        data.transcript.push(
            rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry::Assistant {
                text: "first-stream-chunk".into(),
            },
        );
        data.streaming_text = Some("first-stream-chunk".into());

        let mut app = AppState::default();
        sync_local_agent_state_from_snapshot(&mut app.rebon_tui, &snapshot);
        assert_eq!(app.rebon_tui.transcript.len(), 1);

        let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data else {
            panic!("expected local agent");
        };
        data.streaming_text = Some("first-stream-chunk plus later deltas".into());
        sync_local_agent_state_from_snapshot(&mut app.rebon_tui, &snapshot);
        assert_eq!(
            app.rebon_tui.transcript.len(),
            1,
            "the first assistant chunk must remain live when later cumulative deltas arrive"
        );
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("first-stream-chunk plus later deltas")
        );

        let backend = TestBackend::new(80, 8);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(4),
            },
        )
        .expect("inline terminal constructs");
        let mut runtime = InlineRuntimeState::with_initial_viewport_height(4);
        flush_inline_commits(
            &mut terminal,
            &mut app,
            &rebon_tui::RenderTheme::plain(),
            &mut runtime,
            1,
        )
        .expect("commit prompt while assistant text remains live");

        let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data else {
            panic!("expected local agent");
        };
        data.streaming_text = None;
        let Some(rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry::Assistant { text }) =
            data.transcript.last_mut()
        else {
            panic!("expected streaming assistant entry");
        };
        *text = "final complete answer".into();
        sync_local_agent_state_from_snapshot(&mut app.rebon_tui, &snapshot);

        assert_eq!(app.rebon_tui.transcript.len(), 2);
        flush_inline_commits(
            &mut terminal,
            &mut app,
            &rebon_tui::RenderTheme::plain(),
            &mut runtime,
            2,
        )
        .expect("commit finalized assistant answer");

        let output = format!(
            "{}{}",
            buffer_text(terminal.backend().scrollback()),
            buffer_text(terminal.backend().buffer())
        );
        assert!(
            output.contains("final complete answer"),
            "finalized assistant text must reach physical inline output: {output:?}"
        );
        assert_eq!(runtime.commit_cursor.committed_row_count(), 2);
    }

    #[test]
    fn switch_to_live_agent_opens_view_without_changing_background_state() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        let cancel = insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        let reg = Arc::new(reg);
        app.tasks = reg.clone();
        app.follow_transcript_tail = false;
        app.scroll_offset = 80;
        app.prev_scroll_offset = 75;
        app.total_content_lines = 100;
        app.prev_frame_area = Some(ratatui::layout::Rect::new(0, 0, 80, 20));
        let mut active_prompt = None;

        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-1"
        ));

        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-1"));
        assert!(app.follow_transcript_tail);
        assert_eq!(app.scroll_offset, 0);
        assert_eq!(app.prev_scroll_offset, 0);
        assert_eq!(app.total_content_lines, 0);
        assert_eq!(app.prev_frame_area, None);
        assert!(
            reg.snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-1"))
                .unwrap()
                .is_backgrounded
        );
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn switch_to_live_agent_saves_previous_view_without_backgrounding_it() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        insert_local_agent_task(
            &reg,
            "agent-2",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        let reg = Arc::new(reg);
        app.tasks = reg.clone();
        let mut active_prompt = None;
        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-1"
        ));
        app.pending_page_hard_refresh = None;

        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-2"
        ));

        assert!(
            !reg.snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-1"))
                .unwrap()
                .is_backgrounded
        );
        assert!(
            reg.snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-2"))
                .unwrap()
                .is_backgrounded
        );
        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-2"));
    }

    #[test]
    fn pause_preserve_backgrounds_without_cancel() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        let cancel = insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let reg = Arc::new(reg);
        app.tasks = reg.clone();
        let mut active_prompt = None;
        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-1"
        ));

        let tasks = app.tasks.clone();
        assert!(pause_preserve_task(
            &mut app,
            &mut active_prompt,
            tasks.as_ref(),
            "agent-1",
        ));

        assert!(
            reg.snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-1"))
                .unwrap()
                .is_backgrounded
        );
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn main_and_agent_measure_caches_follow_their_views() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = Arc::new(reg);
        warm_active_measure_cache(&mut app, "main-cache");
        let mut active_prompt = None;

        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-1"
        ));
        assert!(app.transcript_measure_cache.is_empty());
        assert!(!app
            .main_agent_view
            .as_ref()
            .expect("main view")
            .measure_cache
            .is_empty());
        warm_active_measure_cache(&mut app, "agent-cache");
        app.follow_transcript_tail = false;
        app.scroll_offset = 30;
        app.prev_scroll_offset = 25;
        app.total_content_lines = 50;
        app.prev_frame_area = Some(ratatui::layout::Rect::new(0, 0, 80, 20));

        switch_to_main_agent(&mut app);

        assert!(app.main_agent_view.is_none());
        assert!(app.follow_transcript_tail);
        assert_eq!(app.scroll_offset, 0);
        assert_eq!(app.prev_scroll_offset, 0);
        assert_eq!(app.total_content_lines, 0);
        assert_eq!(app.prev_frame_area, None);
        assert!(!app.transcript_measure_cache.is_empty());
        assert!(!app
            .local_agent_views
            .get("agent-1")
            .expect("stored agent view")
            .measure_cache
            .is_empty());
    }

    #[test]
    fn switching_agents_restores_each_warm_measure_cache() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-a",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        insert_local_agent_task(
            &reg,
            "agent-b",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = Arc::new(reg);
        let mut active_prompt = None;

        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-a"
        ));
        warm_active_measure_cache(&mut app, "agent-a-cache");
        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-b"
        ));
        assert!(!app
            .local_agent_views
            .get("agent-a")
            .expect("agent A stored")
            .measure_cache
            .is_empty());
        assert!(app.transcript_measure_cache.is_empty());
        warm_active_measure_cache(&mut app, "agent-b-cache");

        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-a"
        ));

        assert!(!app.transcript_measure_cache.is_empty());
        assert!(!app.local_agent_views.contains_key("agent-a"));
        assert!(!app
            .local_agent_views
            .get("agent-b")
            .expect("agent B stored")
            .measure_cache
            .is_empty());
    }

    #[test]
    fn hidden_agent_updates_keep_its_measure_cache_for_reactivation() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        let reg = Arc::new(reg);
        app.tasks = reg.clone();
        let mut active_prompt = None;
        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-1"
        ));
        warm_active_measure_cache(&mut app, "agent-cache");
        switch_to_main_agent(&mut app);
        reg.update(
            &rebon_plugin_tasks::runtime::TaskId::new("agent-1"),
            |snapshot| {
                let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data
                else {
                    panic!("expected local agent");
                };
                data.transcript.push(
                    rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry::Assistant {
                        text: "hidden update".into(),
                    },
                );
            },
        );

        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-1"
        ));

        assert!(!app.transcript_measure_cache.is_empty());
        assert!(app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::Assistant(assistant)
                if assistant.message.content.iter().any(|block| matches!(
                    block,
                    rebon_tui::AssistantContentBlock::Text(text)
                        if text.text == "hidden update"
                ))
        )));
    }

    #[test]
    fn same_agent_selection_keeps_cache_active_without_duplicate_storage() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = Arc::new(reg);
        let mut active_prompt = None;
        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-1"
        ));
        app.pending_page_hard_refresh = None;
        warm_active_measure_cache(&mut app, "agent-cache");

        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-1"
        ));

        assert!(!app.transcript_measure_cache.is_empty());
        assert!(!app.local_agent_views.contains_key("agent-1"));
        assert!(app.pending_page_hard_refresh.is_none());
    }

    #[test]
    fn temporary_main_access_moves_both_measure_caches() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = Arc::new(reg);
        warm_active_measure_cache(&mut app, "main-cache");
        let mut active_prompt = None;
        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-1"
        ));
        warm_active_measure_cache(&mut app, "agent-cache");

        with_main_agent_view(&mut app, |app| {
            assert!(!app.transcript_measure_cache.is_empty());
            app.rebon_tui.flush_counter = 17;
        });

        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-1"));
        assert!(!app.transcript_measure_cache.is_empty());
        let main = app.main_agent_view.as_ref().expect("main view");
        assert_eq!(main.tui.flush_counter, 17);
        assert!(!main.measure_cache.is_empty());
    }

    #[test]
    fn context_reset_during_main_update_drops_the_foreground_agent_pair() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = Arc::new(reg);
        warm_active_measure_cache(&mut app, "main-cache");
        let mut active_prompt = None;
        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-1"
        ));
        warm_active_measure_cache(&mut app, "agent-cache");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(SessionUpdateParams {
            session_id: "main-session".into(),
            update: rebon_types::SessionUpdate::ContextReset {
                plan: Some("fresh plan".into()),
            },
        })
        .expect("send context reset");

        assert_eq!(drain_main_agent_updates(&mut app, &mut rx), 1);

        assert_eq!(app.foregrounded_task_id, None);
        assert!(app.main_agent_view.is_none());
        assert!(app.local_agent_views.is_empty());
        assert!(app.transcript_measure_cache.is_empty());
        assert_eq!(app.rebon_tui.transcript.len(), 1);
        let rebon_tui::Message::User(user) = &app.rebon_tui.transcript.rows()[0] else {
            panic!("expected fresh plan row");
        };
        assert_eq!(user.plan_content.as_deref(), Some("fresh plan"));
    }

    #[test]
    fn pruning_terminal_hidden_agent_drops_its_view_and_cache() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        let reg = Arc::new(reg);
        app.tasks = reg.clone();
        let mut active_prompt = None;
        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-1"
        ));
        warm_active_measure_cache(&mut app, "agent-cache");
        switch_to_main_agent(&mut app);
        reg.update(
            &rebon_plugin_tasks::runtime::TaskId::new("agent-1"),
            |snapshot| {
                snapshot.status = rebon_plugin_tasks::runtime::TaskStatus::Completed;
            },
        );

        prune_stored_agent_views(&mut app, &reg.snapshots());

        assert!(!app.local_agent_views.contains_key("agent-1"));
    }

    #[test]
    fn pruning_disappeared_hidden_agent_drops_its_view_and_cache() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        let reg = Arc::new(reg);
        app.tasks = reg.clone();
        let mut active_prompt = None;
        assert!(switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-1"
        ));
        warm_active_measure_cache(&mut app, "agent-cache");
        switch_to_main_agent(&mut app);
        reg.remove(&rebon_plugin_tasks::runtime::TaskId::new("agent-1"));

        prune_stored_agent_views(&mut app, &reg.snapshots());

        assert!(!app.local_agent_views.contains_key("agent-1"));
    }

    #[test]
    fn sync_foreground_agent_view_pulls_live_registry_updates() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let reg = Arc::new(reg);
        app.tasks = reg.clone();
        app.foregrounded_task_id = Some("agent-1".into());

        reg.update(
            &rebon_plugin_tasks::runtime::TaskId::new("agent-1"),
            |snapshot| {
                let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data
                else {
                    panic!("expected local agent");
                };
                data.transcript.push(
                    rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry::Assistant {
                        text: "live output".into(),
                    },
                );
                data.streaming_text = Some("still thinking".into());
            },
        );

        let tasks = app.tasks.clone();
        sync_foreground_agent_view(&mut app, tasks.as_ref());

        assert!(app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::Assistant(assistant)
                if assistant.message.content.iter().any(|block| matches!(
                    block,
                    rebon_tui::AssistantContentBlock::Text(text) if text.text == "live output"
                ))
        )));
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("still thinking")
        );
        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-1"));
    }

    #[test]
    fn sync_foreground_agent_view_releases_terminal_agent() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let reg = Arc::new(reg);
        app.tasks = reg.clone();
        app.main_agent_view = Some(StoredTranscriptView::default());
        app.foregrounded_task_id = Some("agent-1".into());

        reg.update(
            &rebon_plugin_tasks::runtime::TaskId::new("agent-1"),
            |snapshot| {
                snapshot.status = rebon_plugin_tasks::runtime::TaskStatus::Completed;
                let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data
                else {
                    panic!("expected local agent");
                };
                data.transcript.push(
                    rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry::Assistant {
                        text: "done".into(),
                    },
                );
            },
        );

        let tasks = app.tasks.clone();
        sync_foreground_agent_view(&mut app, tasks.as_ref());

        assert_eq!(app.foregrounded_task_id, None);
        assert!(
            reg.snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-1"))
                .unwrap()
                .is_backgrounded
        );
        assert!(!app.local_agent_views.contains_key("agent-1"));
        assert!(app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::System(system)
                if system.subtype == "local_command"
                    && system
                        .content
                        .as_deref()
                        .unwrap_or("")
                        .contains("can no longer receive messages")
        )));
    }
}
