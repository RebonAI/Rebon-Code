//! This crate's end of transcript replay.
//!
//! The pass that decides what a persisted entry looks like on screen moved
//! to [`rebon_render::transcript_replay`], so that `serve` and the
//! desktop app read the rows the terminal reads instead of each folding the
//! transcript its own way. What is left here is the part that is not a
//! display concern: reading the authoritative file for a mirrored session,
//! converting this crate's storage entries into the projection's borrowed
//! input, logging what the walk skipped, and collecting the
//! `BackgroundAgentTaskRef` identities an async Agent launch leaves in a
//! tool result — a worker keeps that map so it can answer about the tasks a
//! resumed session started.
//!
//! Committing the rows to a screen is further out still, and
//! `tui::runner::transcript_replay` is that shell.

use std::collections::HashMap;

pub enum MirroredTranscriptRead {
    Unchanged,
    Loaded {
        file_stat: Option<(u64, Option<std::time::SystemTime>)>,
        title: Option<String>,
        entries: Vec<rebon_session::TranscriptEntry>,
    },
}

/// Stat and load the authoritative transcript for a mirrored session.
///
/// The stat is intentionally taken before the read: a concurrent append then
/// makes the next refresh look changed, never the reverse. Registry eviction,
/// load and residency release stay together in the session owner.
pub fn read_mirrored_transcript(
    state: &rebon_acp::ServerState,
    session_id: &str,
    cwd: &str,
    previous_stat: Option<(u64, Option<std::time::SystemTime>)>,
    transcript_loaded: bool,
    force: bool,
) -> anyhow::Result<MirroredTranscriptRead> {
    let projects_root = rebon_harness::projects_root();
    let transcript_path = rebon_session::transcript_file_path(&projects_root, cwd, session_id);
    let file_stat = std::fs::metadata(&transcript_path)
        .ok()
        .map(|meta| (meta.len(), meta.modified().ok()));
    if !force && file_stat.is_some() && file_stat == previous_stat && transcript_loaded {
        return Ok(MirroredTranscriptRead::Unchanged);
    }

    drop(state.evict_session(session_id));
    let record = state
        .load_session(&projects_root, session_id, cwd, Some(cwd), Vec::new())
        .map_err(|err| anyhow::anyhow!("failed to load mirrored transcript: {err:?}"))?;
    let title = record.title;
    let entries = record.loaded_transcript;
    // The returned entries are owned. The duplicate registry residency is no
    // longer needed; a future engine cache miss rematerializes the JSONL.
    state.release_transcript_residency(session_id);
    Ok(MirroredTranscriptRead::Loaded {
        file_stat,
        title,
        entries,
    })
}

/// Registry identities returned by an async Agent launch result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundAgentTaskRef {
    pub task_id: Option<String>,
    pub agent_id: Option<String>,
}

impl BackgroundAgentTaskRef {
    pub(crate) fn from_raw_output(raw_output: &serde_json::Value) -> Option<Self> {
        let output = raw_output.as_object()?;
        if output.get("status").and_then(serde_json::Value::as_str) != Some("async_launched") {
            return None;
        }
        Self::from_ids(
            output
                .get("task_id")
                .or_else(|| output.get("taskId"))
                .and_then(serde_json::Value::as_str),
            output
                .get("agent_id")
                .or_else(|| output.get("agentId"))
                .and_then(serde_json::Value::as_str),
        )
    }

    pub fn from_raw_output_map(
        raw_output: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Option<Self> {
        if raw_output.get("status").and_then(serde_json::Value::as_str) != Some("async_launched") {
            return None;
        }
        Self::from_ids(
            raw_output
                .get("task_id")
                .or_else(|| raw_output.get("taskId"))
                .and_then(serde_json::Value::as_str),
            raw_output
                .get("agent_id")
                .or_else(|| raw_output.get("agentId"))
                .and_then(serde_json::Value::as_str),
        )
    }

    pub fn from_background_task(task: &rebon_session_host::BackgroundTaskDescriptor) -> Self {
        Self {
            task_id: Some(task.task_id.clone()),
            agent_id: task.agent_id.clone().or_else(|| Some(task.task_id.clone())),
        }
    }

    fn from_ids(task_id: Option<&str>, agent_id: Option<&str>) -> Option<Self> {
        let task_id = task_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let agent_id = agent_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        (task_id.is_some() || agent_id.is_some()).then_some(Self { task_id, agent_id })
    }
}

/// The rows a transcript replays to, with nothing committed anywhere.
///
/// The pass itself is [`rebon_render::transcript_replay::replayed_rows`],
/// which every surface reads — the terminal through this shell, `serve`
/// through `/api/history`. What stays here is the part that is not a
/// display concern: turning this crate's storage type into the projection's
/// borrowed input, logging what the walk skipped, and collecting the
/// registry identities an async `Agent` launch left behind in a tool result.
pub fn replayed_messages(
    entries: Vec<rebon_session::TranscriptEntry>,
) -> (
    Vec<rebon_render::transcript_row::Message>,
    HashMap<String, BackgroundAgentTaskRef>,
) {
    let lines = transcript_lines(&entries);
    let (messages, stats) = rebon_render::transcript_replay::replayed_rows(&lines);
    tracing::info!(
        total = stats.total,
        committed = stats.committed,
        recovered = stats.recovered,
        skipped_tool_result = stats.skipped_tool_result,
        skipped_deser = stats.skipped_deser,
        "replay: transcript entries processed"
    );
    let mut agent_tasks = HashMap::new();
    for message in &messages {
        collect_async_agent_tasks(message, &mut agent_tasks);
    }
    (messages, agent_tasks)
}

/// This crate's persisted entries, borrowed in the shape the projection
/// takes. The projection deliberately does not know `rebon-session`.
///
/// `pub(crate)` because `serve` converts the same way for `/api/history`.
/// One converter, so the terminal and the page cannot disagree about what a
/// stored entry is before the projection even sees it.
pub fn transcript_lines(
    entries: &[rebon_session::TranscriptEntry],
) -> Vec<rebon_render::transcript_replay::TranscriptLine<'_>> {
    entries
        .iter()
        .map(|entry| rebon_render::transcript_replay::TranscriptLine {
            entry_type: &entry.entry_type,
            uuid: &entry.uuid,
            timestamp: entry.timestamp.as_deref(),
            raw: &entry.raw,
        })
        .collect()
}

/// Tool call ids the engine recorded as auto-mode allowed on the
/// display-only `autoModeAllowed` sidecar of user tool-result entries, with
/// the part of the gate that allowed each.
///
/// Purely additive: a transcript written before the sidecar existed (or by a
/// session that never ran in auto mode) simply yields nothing and replays as
/// it did before. One written before the sidecar carried a source yields
/// `Unspecified`.
pub fn collect_auto_mode_allowed_ids(
    entries: &[rebon_session::TranscriptEntry],
) -> HashMap<String, rebon_types::AutoModeAllowSource> {
    rebon_render::transcript_replay::collect_auto_mode_allowed_ids(&transcript_lines(entries))
}

/// The registry identity an async `Agent` launch leaves in a tool result.
///
/// Read off the produced rows rather than inside the projection: which task
/// a row started is a fact about this process's registry, not about how the
/// row is drawn, so `rebon-render` has no business knowing it.
fn collect_async_agent_tasks(
    message: &rebon_render::transcript_row::Message,
    tasks: &mut HashMap<String, BackgroundAgentTaskRef>,
) {
    let rebon_render::transcript_row::Message::Assistant(assistant) = message else {
        return;
    };
    for block in &assistant.message.content {
        let rebon_render::transcript_row::AssistantContentBlock::ToolUse(tool) = block else {
            continue;
        };
        let Some(raw_output) = tool.raw_output.as_ref() else {
            continue;
        };
        if let Some(task_ref) = BackgroundAgentTaskRef::from_raw_output(raw_output) {
            tasks.insert(tool.id.clone(), task_ref);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The helpers these exercise now live in the projection crate. The tests
    // stay here because they also pin `replayed_messages`, this crate's
    // entry point, end to end.
    use rebon_render::transcript_replay::{
        strip_hidden_tool_uses, strip_replayed_user_system_reminders,
    };
    use serde_json::json;

    #[test]
    fn strip_hidden_tool_uses_preserves_thinking_only_survivor() {
        let msg = rebon_render::transcript_row::Message::Assistant(
            rebon_render::transcript_row::AssistantMessage {
                uuid: "a-thinking-hidden".into(),
                timestamp: "2026-04-18T00:00:00.000Z".into(),
                message: rebon_render::transcript_row::AssistantMessageInner {
                    role: rebon_render::transcript_row::AssistantRole::Assistant,
                    content: vec![
                        rebon_render::transcript_row::AssistantContentBlock::Thinking(
                            rebon_render::transcript_row::AssistantThinkingBlock {
                                thinking: "Reason before a task update".into(),
                                signature: None,
                            },
                        ),
                        rebon_render::transcript_row::AssistantContentBlock::ToolUse(
                            rebon_render::transcript_row::AssistantToolUseBlock {
                                id: "tu-hidden".into(),
                                name: "TaskUpdate".into(),
                                input: json!({"taskId": "1", "status": "in_progress"}),
                                tool_call_content: None,
                                raw_output: None,
                                title: None,
                                locations: None,
                                status: None,
                            },
                        ),
                    ],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            },
        );

        let stripped = strip_hidden_tool_uses(msg).expect("thinking-only survivor should remain");
        let rebon_render::transcript_row::Message::Assistant(asst) = stripped else {
            panic!("expected assistant message after stripping");
        };
        assert_eq!(asst.message.content.len(), 1);
        match &asst.message.content[0] {
            rebon_render::transcript_row::AssistantContentBlock::Thinking(block) => {
                assert_eq!(block.thinking, "Reason before a task update");
            }
            other => panic!("expected thinking block, got {other:?}"),
        }
    }

    #[test]
    fn replay_skips_meta_user_messages_in_both_top_level_and_nested_shapes() {
        let legacy_nested_meta = rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u-attach-legacy".into(),
            parent_uuid: Some("a-1".into()),
            timestamp: Some("2026-04-17T10:00:00.000Z".into()),
            raw: json!({
                "type": "user",
                "uuid": "u-attach-legacy",
                "parentUuid": "a-1",
                "timestamp": "2026-04-17T10:00:00.000Z",
                "message": {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "<system-reminder>\nLegacy meta body\n</system-reminder>"}
                    ],
                    "isMeta": true,
                },
                "iteration": 3
            }),
        };

        let current_top_meta = rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u-attach-current".into(),
            parent_uuid: Some("a-2".into()),
            timestamp: Some("2026-04-17T10:00:01.000Z".into()),
            raw: json!({
                "type": "user",
                "uuid": "u-attach-current",
                "parentUuid": "a-2",
                "timestamp": "2026-04-17T10:00:01.000Z",
                "message": {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "<system-reminder>\nCurrent meta body\n</system-reminder>"}
                    ],
                },
                "isMeta": true,
                "iteration": 4
            }),
        };

        let flagged_runtime_context = rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u-runtime-flagged".into(),
            parent_uuid: Some("a-3".into()),
            timestamp: Some("2026-04-17T10:00:01.500Z".into()),
            raw: json!({
                "type": "user",
                "uuid": "u-runtime-flagged",
                "parentUuid": "a-3",
                "timestamp": "2026-04-17T10:00:01.500Z",
                "message": {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "<system-reminder>\n<runtime_context>\ngitStatus: noisy\n</runtime_context>\n</system-reminder>"}
                    ],
                },
                "runtimeContext": true,
                "iteration": 5
            }),
        };

        let legacy_runtime_context = rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u-runtime-legacy".into(),
            parent_uuid: Some("a-4".into()),
            timestamp: Some("2026-04-17T10:00:01.750Z".into()),
            raw: json!({
                "type": "user",
                "uuid": "u-runtime-legacy",
                "parentUuid": "a-4",
                "timestamp": "2026-04-17T10:00:01.750Z",
                "message": {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "<system-reminder>\n<runtime_context>\nlegacy gitStatus\n</runtime_context>\n</system-reminder>"}
                    ],
                },
                "iteration": 6
            }),
        };

        let real_user_turn = rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u-human-1".into(),
            parent_uuid: None,
            timestamp: Some("2026-04-17T10:00:02.000Z".into()),
            raw: json!({
                "type": "user",
                "uuid": "u-human-1",
                "timestamp": "2026-04-17T10:00:02.000Z",
                "message": {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "hello agent"}
                    ],
                }
            }),
        };

        let (rows, _) = replayed_messages(vec![
            legacy_nested_meta,
            current_top_meta,
            flagged_runtime_context,
            legacy_runtime_context,
            real_user_turn,
        ]);

        assert_eq!(
            rows.len(),
            1,
            "both meta shapes must be suppressed from the replayed transcript, got: {rows:?}"
        );
        match &rows[0] {
            rebon_render::transcript_row::Message::User(u) => {
                assert_eq!(u.uuid, "u-human-1");
                if let Some(rebon_render::transcript_row::UserContentBlock::Text(t)) =
                    u.message.content.first()
                {
                    assert_eq!(t.text, "hello agent");
                } else {
                    panic!("expected text block for surviving user message");
                }
            }
            other => panic!("expected Message::User, got {other:?}"),
        }
    }

    #[test]
    fn replay_strips_hook_additional_context_from_user_prompt() {
        let hooked_user_turn = rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u-hooked".into(),
            parent_uuid: None,
            timestamp: Some("2026-08-29T00:00:00.000Z".into()),
            raw: json!({
                "type": "user",
                "uuid": "u-hooked",
                "timestamp": "2026-08-29T00:00:00.000Z",
                "message": {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "what is the code word?\n\n<additional_context>\nThe secret code word is PINEAPPLE.\n</additional_context>"}
                    ],
                }
            }),
        };

        let (rows, _) = replayed_messages(vec![hooked_user_turn]);

        assert_eq!(rows.len(), 1, "{rows:?}");
        match &rows[0] {
            rebon_render::transcript_row::Message::User(u) => {
                if let Some(rebon_render::transcript_row::UserContentBlock::Text(t)) =
                    u.message.content.first()
                {
                    assert_eq!(
                        t.text, "what is the code word?",
                        "the hook's context is the model's, not the person's"
                    );
                } else {
                    panic!("expected text block");
                }
            }
            other => panic!("expected Message::User, got {other:?}"),
        }
    }

    #[test]
    fn replay_strips_model_only_system_reminder_from_user_prompt() {
        let wrapped_user_turn = rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u-ultrawork".into(),
            parent_uuid: None,
            timestamp: Some("2026-07-29T00:00:00.000Z".into()),
            raw: json!({
                "type": "user",
                "uuid": "u-ultrawork",
                "timestamp": "2026-07-29T00:00:00.000Z",
                "message": {
                    "role": "user",
                    "content": "<system-reminder>\nUse the Workflow tool.\n</system-reminder>\n\n/ultrawork 实现完整动效"
                }
            }),
        };

        let (rows, _) = replayed_messages(vec![wrapped_user_turn]);

        assert_eq!(rows.len(), 1);
        let rebon_render::transcript_row::Message::User(user) = &rows[0] else {
            panic!("expected replayed user message");
        };
        let Some(rebon_render::transcript_row::UserContentBlock::Text(text)) =
            user.message.content.first()
        else {
            panic!("expected replayed user text");
        };
        assert_eq!(text.text, "/ultrawork 实现完整动效");
    }

    #[test]
    fn replay_strips_multiple_leading_reminders_and_preserves_images() {
        let message = serde_json::from_value(json!({
            "type": "user",
            "uuid": "u-mixed",
            "timestamp": "2026-07-29T00:00:00.000Z",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": " \n<system-reminder>one</system-reminder>\n<system-reminder>two</system-reminder>\n\n/ceo ship it"
                    },
                    {
                        "type": "image",
                        "source": {"type": "base64", "media_type": "image/png", "data": "abc"}
                    }
                ]
            }
        }))
        .expect("valid user message");

        let Some(rebon_render::transcript_row::Message::User(user)) =
            strip_replayed_user_system_reminders(message)
        else {
            panic!("user message should remain visible");
        };
        assert_eq!(user.message.content.len(), 2);
        let rebon_render::transcript_row::UserContentBlock::Text(text) = &user.message.content[0]
        else {
            panic!("expected text block");
        };
        assert_eq!(text.text, "/ceo ship it");
        assert!(matches!(
            user.message.content[1],
            rebon_render::transcript_row::UserContentBlock::Image(_)
        ));
    }

    #[test]
    fn replay_drops_reminder_only_text_without_trimming_plain_user_text() {
        let reminder_only = serde_json::from_value(json!({
            "type": "user",
            "uuid": "u-hidden",
            "timestamp": "2026-07-29T00:00:00.000Z",
            "message": {
                "role": "user",
                "content": [
                    {"type": "text", "text": "<system-reminder>internal only</system-reminder>"}
                ]
            }
        }))
        .expect("valid reminder message");
        assert!(strip_replayed_user_system_reminders(reminder_only).is_none());

        let plain = serde_json::from_value(json!({
            "type": "user",
            "uuid": "u-plain",
            "timestamp": "2026-07-29T00:00:00.000Z",
            "message": {
                "role": "user",
                "content": [
                    {"type": "text", "text": "  preserve indentation"}
                ]
            }
        }))
        .expect("valid plain message");
        let Some(rebon_render::transcript_row::Message::User(user)) =
            strip_replayed_user_system_reminders(plain)
        else {
            panic!("plain user message should remain visible");
        };
        let rebon_render::transcript_row::UserContentBlock::Text(text) = &user.message.content[0]
        else {
            panic!("expected plain text block");
        };
        assert_eq!(text.text, "  preserve indentation");
    }

    #[test]
    fn replay_preserves_thinking_when_hidden_tool_use_is_stripped() {
        let assistant = rebon_session::TranscriptEntry {
            entry_type: "assistant".into(),
            uuid: "a-thinking-hidden".into(),
            parent_uuid: Some("root".into()),
            timestamp: Some("2026-04-18T00:00:00.000Z".into()),
            raw: json!({
                "type": "assistant",
                "uuid": "a-thinking-hidden",
                "parentUuid": "root",
                "timestamp": "2026-04-18T00:00:00.000Z",
                "message": {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "thinking",
                            "thinking": "Need to update task status before replying.",
                        },
                        {
                            "type": "tool_use",
                            "id": "tu-hidden",
                            "name": "TaskUpdate",
                            "input": {"taskId": "1", "status": "completed"},
                        }
                    ],
                }
            }),
        };

        let (rows, _) = replayed_messages(vec![assistant]);

        assert_eq!(
            rows.len(),
            1,
            "thinking row should survive replay: {rows:?}"
        );
        match &rows[0] {
            rebon_render::transcript_row::Message::Assistant(asst) => {
                assert_eq!(asst.message.content.len(), 1);
                match &asst.message.content[0] {
                    rebon_render::transcript_row::AssistantContentBlock::Thinking(block) => {
                        assert_eq!(
                            block.thinking,
                            "Need to update task status before replying."
                        );
                    }
                    other => panic!("expected thinking block, got {other:?}"),
                }
            }
            other => panic!("expected assistant message, got {other:?}"),
        }
    }

    fn assistant_tool_use_entry(
        uuid: &str,
        parent: &str,
        tool_use_id: &str,
        tool_name: &str,
    ) -> rebon_session::TranscriptEntry {
        rebon_session::TranscriptEntry {
            entry_type: "assistant".into(),
            uuid: uuid.into(),
            parent_uuid: Some(parent.into()),
            timestamp: Some("2026-04-18T00:00:00.000Z".into()),
            raw: json!({
                "type": "assistant",
                "uuid": uuid,
                "parentUuid": parent,
                "timestamp": "2026-04-18T00:00:00.000Z",
                "message": {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": tool_use_id,
                            "name": tool_name,
                            "input": {},
                        }
                    ],
                }
            }),
        }
    }

    fn user_tool_result_entry(
        uuid: &str,
        parent: &str,
        results: Vec<(&str, serde_json::Value)>,
        outputs_by_id: Option<Vec<(&str, serde_json::Value)>>,
    ) -> rebon_session::TranscriptEntry {
        let content: Vec<serde_json::Value> = results
            .into_iter()
            .map(|(id, content)| {
                json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": content,
                })
            })
            .collect();
        let mut raw = json!({
            "type": "user",
            "uuid": uuid,
            "parentUuid": parent,
            "timestamp": "2026-04-18T00:00:01.000Z",
            "message": {
                "role": "user",
                "content": content,
            }
        });
        if let Some(outputs) = outputs_by_id {
            let mut map = serde_json::Map::new();
            for (id, value) in outputs {
                map.insert(id.into(), value);
            }
            raw.as_object_mut()
                .unwrap()
                .insert("toolUseResults".into(), serde_json::Value::Object(map));
        }
        rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: uuid.into(),
            parent_uuid: Some(parent.into()),
            timestamp: Some("2026-04-18T00:00:01.000Z".into()),
            raw,
        }
    }

    fn tool_use_block<'a>(
        rows: &'a [rebon_render::transcript_row::Message],
        tool_use_id: &str,
    ) -> &'a rebon_render::transcript_row::AssistantToolUseBlock {
        for row in rows {
            if let rebon_render::transcript_row::Message::Assistant(a) = row {
                for block in &a.message.content {
                    if let rebon_render::transcript_row::AssistantContentBlock::ToolUse(tu) = block
                    {
                        if tu.id == tool_use_id {
                            return tu;
                        }
                    }
                }
            }
        }
        panic!("tool_use block {tool_use_id} not found in replayed rows");
    }

    #[test]
    fn replay_restores_async_agent_tool_task_mapping() {
        let assistant = assistant_tool_use_entry("a-agent", "root", "tu-agent", "Agent");
        let raw_output = json!({
            "status": "async_launched",
            "task_id": "agent-task-1",
            "agent_id": "agent-task-1",
            "agent_type": "Explore"
        });
        let tool_result = user_tool_result_entry(
            "u-agent-result",
            "a-agent",
            vec![("tu-agent", json!("launched"))],
            Some(vec![("tu-agent", raw_output)]),
        );
        let (rows, mappings) = replayed_messages(vec![assistant, tool_result]);

        assert_eq!(
            mappings
                .get("tu-agent")
                .and_then(|task| task.task_id.as_deref()),
            Some("agent-task-1")
        );
        assert_eq!(
            tool_use_block(&rows, "tu-agent")
                .raw_output
                .as_ref()
                .and_then(|output| output.get("status"))
                .and_then(serde_json::Value::as_str),
            Some("async_launched")
        );
    }

    #[test]
    fn replay_enriches_tool_use_with_diff() {
        let raw_output = json!({
            "filePath": "/tmp/a.rs",
            "oldString": "before",
            "newString": "after",
            "type": "update",
        });
        let entries = vec![
            assistant_tool_use_entry("a-edit", "root", "tu-edit", "Edit"),
            user_tool_result_entry(
                "u-edit-result",
                "a-edit",
                vec![("tu-edit", json!("ok"))],
                Some(vec![("tu-edit", raw_output.clone())]),
            ),
        ];

        let (rows, _) = replayed_messages(entries);

        let tu = tool_use_block(&rows, "tu-edit");
        assert_eq!(tu.status, Some(rebon_types::ToolCallStatus::Completed));
        let content = tu
            .tool_call_content
            .as_ref()
            .expect("tool_call_content must be populated from toolUseResults");
        assert_eq!(content.len(), 1);
        match &content[0] {
            rebon_types::ToolCallContent::Diff(d) => {
                assert_eq!(d.path, "/tmp/a.rs");
                assert_eq!(d.old_text.as_deref(), Some("before"));
                assert_eq!(d.new_text, "after");
            }
            other => panic!("expected ToolCallContent::Diff, got {other:?}"),
        }
        assert_eq!(
            tu.raw_output.as_ref(),
            Some(&raw_output),
            "raw_output should be reattached verbatim"
        );
    }

    #[test]
    fn replay_restores_failed_tool_status_and_error_content() {
        let assistant = assistant_tool_use_entry("a-fail", "root", "tu-fail", "Bash");
        let mut tool_result = user_tool_result_entry(
            "u-fail-result",
            "a-fail",
            vec![("tu-fail", json!("permission denied"))],
            None,
        );
        tool_result.raw["message"]["content"][0]["is_error"] = json!(true);

        let (rows, _) = replayed_messages(vec![assistant, tool_result]);

        let tu = tool_use_block(&rows, "tu-fail");
        assert_eq!(tu.status, Some(rebon_types::ToolCallStatus::Failed));
        assert!(tu.raw_output.is_none());
        let content = tu
            .tool_call_content
            .as_ref()
            .expect("failed tool content should be restored");
        match &content[0] {
            rebon_types::ToolCallContent::Content(content) => match &content.content {
                rebon_types::ContentBlock::Text(text) => {
                    assert_eq!(text.text, "permission denied");
                }
                other => panic!("expected text content, got {other:?}"),
            },
            other => panic!("expected regular content, got {other:?}"),
        }
    }

    #[test]
    fn replay_prefers_persisted_display_message_for_failed_tool() {
        let assistant =
            assistant_tool_use_entry("a-fail-short", "root", "tu-fail-short", "SendMessage");
        let mut tool_result = user_tool_result_entry(
            "u-fail-short-result",
            "a-fail-short",
            vec![(
                "tu-fail-short",
                json!("Agent expired; spawn a fresh worker with the follow-up instructions."),
            )],
            None,
        );
        tool_result.raw["message"]["content"][0]["is_error"] = json!(true);
        tool_result.raw["toolErrorPresentations"] = json!({
            "tu-fail-short": {
                "code": "agent_closed",
                "displayMessage": "Agent is no longer available."
            }
        });

        let (rows, _) = replayed_messages(vec![assistant, tool_result]);

        let tu = tool_use_block(&rows, "tu-fail-short");
        assert_eq!(tu.status, Some(rebon_types::ToolCallStatus::Failed));
        assert!(tu.raw_output.is_none());
        let content = tu.tool_call_content.as_ref().unwrap();
        let rebon_types::ToolCallContent::Content(content) = &content[0] else {
            panic!("expected regular content");
        };
        let rebon_types::ContentBlock::Text(text) = &content.content else {
            panic!("expected text content");
        };
        assert_eq!(text.text, "Agent is no longer available.");
    }

    #[test]
    fn replay_restores_failed_tool_status_from_camel_case_array_content() {
        let assistant = assistant_tool_use_entry("a-fail-array", "root", "tu-fail-array", "Bash");
        let mut tool_result = user_tool_result_entry(
            "u-fail-array-result",
            "a-fail-array",
            vec![("tu-fail-array", json!("placeholder"))],
            None,
        );
        let block = tool_result.raw["message"]["content"][0]
            .as_object_mut()
            .expect("tool result block must be an object");
        let tool_use_id = block
            .remove("tool_use_id")
            .expect("tool result should contain tool_use_id");
        block.insert("toolUseId".into(), tool_use_id);
        block.insert("isError".into(), json!(true));
        block.insert(
            "content".into(),
            json!([
                {"type": "text", "text": "permission denied"},
                {"type": "text", "text": "retry later"}
            ]),
        );

        let (rows, _) = replayed_messages(vec![assistant, tool_result]);

        let tu = tool_use_block(&rows, "tu-fail-array");
        assert_eq!(tu.status, Some(rebon_types::ToolCallStatus::Failed));
        assert!(tu.raw_output.is_none());
        let content = tu
            .tool_call_content
            .as_ref()
            .expect("failed tool content should be restored");
        match &content[0] {
            rebon_types::ToolCallContent::Content(content) => match &content.content {
                rebon_types::ContentBlock::Text(text) => {
                    assert_eq!(text.text, "permission denied\nretry later");
                }
                other => panic!("expected text content, got {other:?}"),
            },
            other => panic!("expected regular content, got {other:?}"),
        }
    }

    #[test]
    fn replay_is_backward_compatible_without_tool_use_results() {
        let entries = vec![
            assistant_tool_use_entry("a-legacy", "root", "tu-legacy", "Write"),
            user_tool_result_entry(
                "u-legacy-result",
                "a-legacy",
                vec![("tu-legacy", json!("persisted"))],
                None,
            ),
        ];

        let (rows, _) = replayed_messages(entries);

        let tu = tool_use_block(&rows, "tu-legacy");
        assert_eq!(
            tu.status,
            Some(rebon_types::ToolCallStatus::Completed),
            "the persisted tool_result establishes the terminal state"
        );
        assert!(
            tu.tool_call_content.is_none(),
            "no toolUseResults: tool_call_content stays None"
        );
        assert!(
            tu.raw_output.is_none(),
            "no toolUseResults: raw_output stays None"
        );
    }

    #[test]
    fn replay_handles_multiple_tool_uses_in_one_iteration() {
        let assistant_batch = rebon_session::TranscriptEntry {
            entry_type: "assistant".into(),
            uuid: "a-batch".into(),
            parent_uuid: Some("root".into()),
            timestamp: Some("2026-04-18T00:00:00.000Z".into()),
            raw: json!({
                "type": "assistant",
                "uuid": "a-batch",
                "parentUuid": "root",
                "timestamp": "2026-04-18T00:00:00.000Z",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "tu-one", "name": "Edit", "input": {}},
                        {"type": "tool_use", "id": "tu-two", "name": "Write", "input": {}},
                    ],
                }
            }),
        };
        let raw_one = json!({
            "filePath": "/tmp/one.rs",
            "oldString": "a",
            "newString": "A",
        });
        let raw_two = json!({
            "filePath": "/tmp/two.rs",
            "oldString": "",
            "newString": "fresh",
        });
        let user_batch = user_tool_result_entry(
            "u-batch-result",
            "a-batch",
            vec![("tu-one", json!("edit-ok")), ("tu-two", json!("write-ok"))],
            Some(vec![
                ("tu-one", raw_one.clone()),
                ("tu-two", raw_two.clone()),
            ]),
        );

        let (rows, _) = replayed_messages(vec![assistant_batch, user_batch]);

        let first = tool_use_block(&rows, "tu-one");
        let second = tool_use_block(&rows, "tu-two");

        match first
            .tool_call_content
            .as_ref()
            .expect("first tool_use must be enriched")
            .first()
            .expect("first must have content")
        {
            rebon_types::ToolCallContent::Diff(d) => {
                assert_eq!(d.path, "/tmp/one.rs");
                assert_eq!(d.old_text.as_deref(), Some("a"));
                assert_eq!(d.new_text, "A");
            }
            other => panic!("expected first to be Diff, got {other:?}"),
        }
        match second
            .tool_call_content
            .as_ref()
            .expect("second tool_use must be enriched")
            .first()
            .expect("second must have content")
        {
            rebon_types::ToolCallContent::Diff(d) => {
                assert_eq!(d.path, "/tmp/two.rs");
                assert!(
                    d.old_text.is_none(),
                    "empty oldString should normalise to None for Write-create diff preview"
                );
                assert_eq!(d.new_text, "fresh");
            }
            other => panic!("expected second to be Diff (write create), got {other:?}"),
        }
        assert_eq!(first.raw_output.as_ref(), Some(&raw_one));
        assert_eq!(second.raw_output.as_ref(), Some(&raw_two));
    }

    #[test]
    fn replay_partial_tool_use_results_enriches_only_matching_ids() {
        let assistant_batch = rebon_session::TranscriptEntry {
            entry_type: "assistant".into(),
            uuid: "a-partial".into(),
            parent_uuid: Some("root".into()),
            timestamp: Some("2026-04-18T00:00:00.000Z".into()),
            raw: json!({
                "type": "assistant",
                "uuid": "a-partial",
                "parentUuid": "root",
                "timestamp": "2026-04-18T00:00:00.000Z",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "tu-ok", "name": "Edit", "input": {}},
                        {"type": "tool_use", "id": "tu-missing", "name": "Edit", "input": {}},
                    ],
                }
            }),
        };
        let user_batch = user_tool_result_entry(
            "u-partial-result",
            "a-partial",
            vec![
                ("tu-ok", json!("ok")),
                ("tu-missing", json!("Error: blew up")),
            ],
            Some(vec![(
                "tu-ok",
                json!({
                    "filePath": "/tmp/ok.rs",
                    "oldString": "x",
                    "newString": "y",
                }),
            )]),
        );

        let (rows, _) = replayed_messages(vec![assistant_batch, user_batch]);

        let ok = tool_use_block(&rows, "tu-ok");
        let missing = tool_use_block(&rows, "tu-missing");
        assert!(
            ok.tool_call_content.is_some(),
            "matching id should be enriched"
        );
        assert!(
            missing.tool_call_content.is_none(),
            "unmatched id should stay unenriched, not crash"
        );
    }
}
