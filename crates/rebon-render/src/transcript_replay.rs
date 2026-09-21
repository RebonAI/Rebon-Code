//! Persisted transcript entries in, display rows out — the one producer.
//!
//! This is the pass every surface needs, so it is written once here: recover a
//! malformed line, drop the meta and runtime-context rows the engine injected
//! for the model, strip the `<system-reminder>` and `<additional_context>`
//! bodies out of what the person actually typed, drop the tool calls that
//! render in their own surface, fold each persisted tool result back onto the
//! `tool_use` block it belongs to, and synthesize the plan card an approved
//! `ExitPlanMode` leaves behind.
//!
//! ## The two seams
//!
//! Reading a transcript off disk and knowing what a background agent task is
//! are both jobs for a caller, not for a projection, and `rebon-render`'s
//! dependency list is a contract this must not widen. So:
//!
//! * The input is [`TranscriptLine`], three borrowed fields, and the caller
//!   converts its own entry type into it. No session crate is involved.
//! * Nothing is logged. [`replayed_rows`] hands back a [`ReplayStats`] and
//!   the caller decides whether that is worth a log line.
//! * The `Agent` launch identities a replay walks past are collected by the
//!   caller from the rows it gets back, because a task registry is not a
//!   display concern.

use std::collections::{HashMap, HashSet};

use crate::tool_output::tool_result_update_content;
use crate::transcript_row::{
    AssistantContentBlock, AssistantMessage, AssistantMessageInner, AssistantRole,
    AssistantTextBlock, Message, ToolResultContent, UserContentBlock, UserMessage,
    UserMessageInner, UserRole, UserTextBlock,
};

/// One persisted transcript line, as [`replayed_rows`] needs to see it.
///
/// Deliberately borrowed and deliberately three fields: the caller owns the
/// storage type, and widening this struct is how a session crate would creep
/// into the projection crate.
#[derive(Debug, Clone, Copy)]
pub struct TranscriptLine<'a> {
    /// `type` — `"user"`, `"assistant"`, `"attachment"`, `"system"`, …
    pub entry_type: &'a str,
    /// `uuid` — the row's primary key in the on-disk chain.
    pub uuid: &'a str,
    /// `timestamp` — ISO-8601, absent on malformed or hand-authored lines.
    pub timestamp: Option<&'a str>,
    /// The whole parsed JSON line.
    pub raw: &'a serde_json::Value,
}

/// What a replay walked past, for a caller that wants to log it.
///
/// `committed` counts rows handed back, which is not `total` minus the skips:
/// one assistant entry can also yield an exit-plan card.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplayStats {
    /// Lines offered.
    pub total: usize,
    /// Rows handed back.
    pub committed: usize,
    /// Lines that failed serde and were rebuilt by [`try_recover_message`].
    pub recovered: usize,
    /// User lines dropped for carrying nothing but tool results.
    pub skipped_tool_result: usize,
    /// Lines neither serde nor recovery could read.
    pub skipped_deser: usize,
}

/// The rows a transcript replays to, with nothing committed anywhere.
///
/// Everything that decides what a persisted entry looks like on screen
/// happens here; committing the result to a screen is the caller's business,
/// and a headless caller (`/context`, `/rewind`, a worker, a service) reads
/// the rows directly.
pub fn replayed_rows(entries: &[TranscriptLine<'_>]) -> (Vec<Message>, ReplayStats) {
    let raw_outputs_by_id = collect_tool_use_results(entries);
    let failed_tool_results = collect_failed_tool_results(entries);
    let tool_error_presentations = collect_tool_error_presentations(entries);
    let completed_tool_use_ids = collect_tool_result_ids(entries);

    let mut stats = ReplayStats {
        total: entries.len(),
        ..ReplayStats::default()
    };
    let mut messages = Vec::with_capacity(entries.len());

    for entry in entries {
        let msg = match serde_json::from_value::<Message>(entry.raw.clone()) {
            Ok(m) => m,
            Err(_) => match try_recover_message(entry) {
                Some(m) => {
                    stats.recovered += 1;
                    m
                }
                None => {
                    stats.skipped_deser += 1;
                    continue;
                }
            },
        };
        let exit_plan_cards = exit_plan_mode_cards(&msg, &completed_tool_use_ids);
        // Skip user messages that only carry tool results.
        if msg.tool_result_id().is_some() {
            stats.skipped_tool_result += 1;
            continue;
        }
        // Skip meta user messages (engine-injected attachments like
        // plan_mode_exit, date_change, etc.). These are internal
        // signals the model saw during the live session but should
        // not be visible on replay.
        //
        // Also check `entry.raw.message.isMeta` — historical
        // transcripts (and one earlier engine revision) wrote the
        // flag nested inside the `message` sub-object, where the
        // `UserMessage` struct can't see it. Those transcripts
        // would otherwise leak `<system-reminder>` bodies and
        // `[Request interrupted by user]` into the resumed rows.
        if let Message::User(ref u) = msg {
            let meta_top = u.is_meta == Some(true);
            let meta_nested = entry
                .raw
                .get("message")
                .and_then(|m| m.get("isMeta"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let runtime_context = entry
                .raw
                .get("runtimeContext")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || user_message_is_runtime_context(u);
            if meta_top || meta_nested || runtime_context {
                continue;
            }
        }
        let msg = strip_replayed_user_system_reminders(msg);
        // Strip the tool calls that render in their own surface.
        let msg = msg.and_then(strip_hidden_tool_uses);
        if let Some(mut msg) = msg {
            enrich_tool_use_blocks(
                &mut msg,
                &raw_outputs_by_id,
                &failed_tool_results,
                &tool_error_presentations,
                &completed_tool_use_ids,
            );
            messages.push(msg);
            stats.committed += 1;
        }
        for card in exit_plan_cards {
            messages.push(card);
            stats.committed += 1;
        }
    }
    (messages, stats)
}

pub fn user_message_is_runtime_context(user: &UserMessage) -> bool {
    if user.message.content.len() != 1 {
        return false;
    }
    let Some(UserContentBlock::Text(text)) = user.message.content.first() else {
        return false;
    };
    let trimmed = text.text.trim();
    trimmed.starts_with("<system-reminder>")
        && trimmed.contains("<runtime_context>")
        && trimmed.ends_with("</system-reminder>")
}

/// Take the harness's injected text back out of what the person typed.
///
/// One rule, [`crate::strip_injected_wrappers`], applied to every text block
/// rather than only to one that opens with a reminder. Returns `None` if
/// nothing the person wrote is left.
pub fn strip_replayed_user_system_reminders(msg: Message) -> Option<Message> {
    match msg {
        Message::User(mut user) => {
            for block in &mut user.message.content {
                let UserContentBlock::Text(text) = block else {
                    continue;
                };
                text.text = crate::strip_injected_wrappers(&text.text);
            }
            user.message.content.retain(
                |block| !matches!(block, UserContentBlock::Text(text) if text.text.is_empty()),
            );
            (!user.message.content.is_empty()).then_some(Message::User(user))
        }
        other => Some(other),
    }
}

/// Index every tool output the engine persisted on user tool-result
/// entries by `tool_use_id`. Keys correspond to `tool_use` block ids
/// on assistant entries earlier in the chain, so replay can rehydrate
/// those blocks' `tool_call_content` / `raw_output` fields.
///
/// Missing or malformed `toolUseResults` values are tolerated — the
/// enrichment step is strictly additive, so a transcript recorded
/// before this field existed still replays as it did previously.
pub fn collect_tool_use_results(
    entries: &[TranscriptLine<'_>],
) -> HashMap<String, serde_json::Value> {
    let mut out = HashMap::new();
    for entry in entries {
        let Some(map) = entry
            .raw
            .get("toolUseResults")
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        for (id, value) in map {
            out.insert(id.clone(), value.clone());
        }
    }
    out
}

/// Tool call ids the engine recorded as auto-mode allowed on the
/// display-only `autoModeAllowed` sidecar of user tool-result entries, with
/// the part of the gate that allowed each.
///
/// Purely additive, exactly like `collect_tool_use_results`: a transcript
/// written before the sidecar existed (or by a session that never ran in auto
/// mode) simply yields nothing and replays as it did before. One written
/// before the sidecar carried a source yields `Unspecified`.
pub fn collect_auto_mode_allowed_ids(
    entries: &[TranscriptLine<'_>],
) -> HashMap<String, rebon_types::AutoModeAllowSource> {
    entries
        .iter()
        .filter_map(|entry| entry.raw.get("autoModeAllowed"))
        .flat_map(crate::parse_auto_mode_allowed_sidecar)
        .map(|(id, source)| (id.to_owned(), source))
        .collect()
}

pub fn collect_tool_result_ids(entries: &[TranscriptLine<'_>]) -> HashSet<String> {
    entries
        .iter()
        .filter_map(|entry| entry.raw.get("message"))
        .filter_map(|message| message.get("content"))
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .filter(|block| {
            block.get("type").and_then(serde_json::Value::as_str) == Some("tool_result")
        })
        .filter_map(|block| {
            block
                .get("tool_use_id")
                .or_else(|| block.get("toolUseId"))
                .and_then(serde_json::Value::as_str)
        })
        .map(str::to_owned)
        .collect()
}

pub fn collect_failed_tool_results(entries: &[TranscriptLine<'_>]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for block in entries
        .iter()
        .filter_map(|entry| entry.raw.get("message"))
        .filter_map(|message| message.get("content"))
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .filter(|block| {
            block.get("type").and_then(serde_json::Value::as_str) == Some("tool_result")
                && block
                    .get("is_error")
                    .or_else(|| block.get("isError"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
        })
    {
        let Some(id) = block
            .get("tool_use_id")
            .or_else(|| block.get("toolUseId"))
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let content = block
            .get("content")
            .cloned()
            .and_then(|content| serde_json::from_value::<ToolResultContent>(content).ok())
            .map(|content| content.as_display_string())
            .unwrap_or_default();
        out.insert(id.to_owned(), content);
    }
    out
}

pub fn collect_tool_error_presentations(entries: &[TranscriptLine<'_>]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for entry in entries {
        let Some(map) = entry
            .raw
            .get("toolErrorPresentations")
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        for (id, presentation) in map {
            if let Some(display_message) = presentation
                .get("displayMessage")
                .and_then(serde_json::Value::as_str)
                .filter(|message| !message.is_empty())
            {
                out.insert(id.clone(), display_message.to_string());
            }
        }
    }
    out
}

fn exit_plan_mode_cards(
    message: &Message,
    completed_tool_use_ids: &HashSet<String>,
) -> Vec<Message> {
    let Message::Assistant(assistant) = message else {
        return Vec::new();
    };
    assistant
        .message
        .content
        .iter()
        .filter_map(|block| {
            let AssistantContentBlock::ToolUse(tool_use) = block else {
                return None;
            };
            if tool_use.name != "ExitPlanMode" || !completed_tool_use_ids.contains(&tool_use.id) {
                return None;
            }
            let plan = tool_use
                .input
                .get("plan")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|plan| !plan.is_empty())?;
            Some(Message::User(UserMessage {
                uuid: format!("u-plan-{}", tool_use.id),
                timestamp: assistant.timestamp.clone(),
                message: UserMessageInner {
                    role: UserRole::User,
                    content: vec![UserContentBlock::Text(UserTextBlock {
                        text: format!("Plan:\n\n{plan}"),
                    })],
                },
                is_compact_summary: None,
                is_meta: None,
                is_visible_in_transcript_only: Some(true),
                image_paste_ids: None,
                plan_content: Some(plan.to_string()),
            }))
        })
        .collect()
}

/// Restore terminal tool state on assistant tool-use blocks from the
/// persisted user tool-result entries.
pub fn enrich_tool_use_blocks(
    msg: &mut Message,
    raw_outputs_by_id: &HashMap<String, serde_json::Value>,
    failed_tool_results: &HashMap<String, String>,
    tool_error_presentations: &HashMap<String, String>,
    completed_tool_use_ids: &HashSet<String>,
) {
    if raw_outputs_by_id.is_empty()
        && failed_tool_results.is_empty()
        && tool_error_presentations.is_empty()
        && completed_tool_use_ids.is_empty()
    {
        return;
    }
    let Message::Assistant(assistant) = msg else {
        return;
    };
    for block in assistant.message.content.iter_mut() {
        let AssistantContentBlock::ToolUse(tu) = block else {
            continue;
        };
        if let Some(error) = failed_tool_results.get(&tu.id) {
            tu.status = Some(rebon_types::ToolCallStatus::Failed);
            if let Some(raw) = raw_outputs_by_id.get(&tu.id) {
                if let Some(content) = tool_result_update_content(raw) {
                    tu.tool_call_content = Some(content);
                    tu.raw_output = Some(raw.clone());
                    continue;
                }
            }
            let display_error = tool_error_presentations.get(&tu.id).unwrap_or(error);
            tu.tool_call_content = (!display_error.trim().is_empty()).then(|| {
                vec![rebon_types::ToolCallContent::Content(
                    rebon_types::RegularContent {
                        content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                            text: display_error.clone(),
                            annotations: None,
                        }),
                    },
                )]
            });
            tu.raw_output = None;
            continue;
        }
        if completed_tool_use_ids.contains(&tu.id) {
            tu.status = Some(rebon_types::ToolCallStatus::Completed);
        }
        let Some(raw) = raw_outputs_by_id.get(&tu.id) else {
            continue;
        };
        tu.status = Some(rebon_types::ToolCallStatus::Completed);
        tu.tool_call_content = tool_result_update_content(raw);
        tu.raw_output = Some(raw.clone());
    }
}

/// Try to recover a message from a transcript entry that failed
/// standard deserialization. Handles:
/// - User messages with `"content": "text"` (string instead of array)
/// - Assistant messages with `"content": "text"` (same issue)
pub fn try_recover_message(entry: &TranscriptLine<'_>) -> Option<Message> {
    let obj = entry.raw.as_object()?;
    let msg_obj = obj.get("message")?.as_object()?;
    // Try string content first — this is the most common failure mode.
    let text = msg_obj.get("content")?.as_str()?;
    if text.trim().is_empty() {
        return None;
    }
    let uuid = entry.uuid.to_string();
    let timestamp = entry.timestamp.unwrap_or_default().to_string();

    match entry.entry_type {
        "user" => Some(Message::User(UserMessage {
            uuid,
            timestamp,
            message: UserMessageInner {
                role: UserRole::User,
                content: vec![UserContentBlock::Text(UserTextBlock {
                    text: text.to_string(),
                })],
            },
            is_compact_summary: None,
            is_meta: None,
            is_visible_in_transcript_only: None,
            image_paste_ids: None,
            plan_content: None,
        })),
        "assistant" => Some(Message::Assistant(AssistantMessage {
            uuid,
            timestamp,
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![AssistantContentBlock::Text(AssistantTextBlock {
                    text: text.to_string(),
                })],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })),
        _ => None,
    }
}

/// Remove tool_use blocks for tools that render in dedicated UI
/// surfaces (task list, plan panel, etc.) rather than inline in the
/// transcript. Returns `None` if the message becomes empty after
/// stripping. Non-empty `Thinking`/`RedactedThinking` blocks are
/// preserved even when they are the only survivors; otherwise replay
/// loses reasoning rows that preceded hidden tool calls.
pub fn strip_hidden_tool_uses(msg: Message) -> Option<Message> {
    use {AssistantContentBlock, Message};
    match msg {
        Message::Assistant(mut asst) => {
            asst.message.content.retain(|block| {
                !matches!(block, AssistantContentBlock::ToolUse(tu) if is_hidden_tool_name(&tu.name))
            });
            if asst.message.content.is_empty() {
                None
            } else {
                Some(Message::Assistant(asst))
            }
        }
        other => Some(other),
    }
}

/// Tools that should never appear inline in the committed transcript.
///
/// A replay has to gate on the same list the renderer does:
/// [`crate::hidden::TRANSCRIPT_HIDDEN_TOOLS`]. A private copy of the list here
/// drifted — it named ten tools where the shared list names thirteen, and the
/// three it missed (`TeamCreate`, `TeamDelete`, `SyntheticOutput`) are
/// coordinator-internal and never user-facing, so a live turn hid them while a
/// resumed one showed them.
fn is_hidden_tool_name(name: &str) -> bool {
    crate::hidden::is_transcript_hidden_tool(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The fixture transcript the golden below is pinned against: one user
    /// turn whose text the harness padded with a reminder, an assistant turn
    /// that thinks and then calls two tools (one of them coordinator-internal
    /// and never user-facing), the batched tool-result row the engine wrote
    /// for them, and a runtime-context row the model saw and a person must
    /// not.
    fn fixture() -> Vec<serde_json::Value> {
        vec![
            json!({
                "type": "user",
                "uuid": "u-1",
                "timestamp": "2026-09-06T00:00:00.000Z",
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "text",
                        "text": "<system-reminder>be brief</system-reminder>\nread the file",
                    }],
                },
            }),
            json!({
                "type": "user",
                "uuid": "u-runtime",
                "timestamp": "2026-09-06T00:00:01.000Z",
                "runtimeContext": true,
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "text",
                        "text": "<system-reminder><runtime_context>cwd=/tmp</runtime_context></system-reminder>",
                    }],
                },
            }),
            json!({
                "type": "assistant",
                "uuid": "a-1",
                "timestamp": "2026-09-06T00:00:02.000Z",
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "thinking", "thinking": "which file" },
                        { "type": "tool_use", "id": "t-read", "name": "Read", "input": { "file_path": "a.txt" } },
                        { "type": "tool_use", "id": "t-team", "name": "TeamCreate", "input": {} },
                    ],
                },
            }),
            json!({
                "type": "user",
                "uuid": "u-2",
                "timestamp": "2026-09-06T00:00:03.000Z",
                "message": {
                    "role": "user",
                    "content": [
                        { "type": "tool_result", "tool_use_id": "t-read", "content": "hello" },
                        { "type": "tool_result", "tool_use_id": "t-team", "content": "ok" },
                    ],
                },
            }),
        ]
    }

    fn rows_of(raw: &[serde_json::Value]) -> (Vec<Message>, ReplayStats) {
        let lines: Vec<TranscriptLine<'_>> = raw
            .iter()
            .map(|value| TranscriptLine {
                entry_type: value["type"].as_str().unwrap(),
                uuid: value["uuid"].as_str().unwrap(),
                timestamp: value["timestamp"].as_str(),
                raw: value,
            })
            .collect();
        replayed_rows(&lines)
    }

    /// The one producer, pinned byte for byte. Every surface reads these
    /// rows, so a change here changes all of them at once and has to be
    /// deliberate.
    #[test]
    fn the_row_stream_for_a_fixture_transcript_is_byte_exact() {
        let raw = fixture();
        let (rows, stats) = rows_of(&raw);
        let actual = serde_json::to_string_pretty(&rows).expect("rows serialize");
        let expected = include_str!("../tests/golden/replayed_rows.json").trim_end();
        assert_eq!(actual, expected);
        assert_eq!(
            stats,
            ReplayStats {
                total: 4,
                committed: 2,
                recovered: 0,
                skipped_tool_result: 1,
                skipped_deser: 0,
            }
        );
    }

    /// A resumed session used to show `TeamCreate` / `TeamDelete` /
    /// `SyntheticOutput` rows that the live turn had hidden, because replay
    /// carried its own ten-name copy of the hidden-tool list while the
    /// renderer gated on a thirteen-name one. Both read
    /// `TRANSCRIPT_HIDDEN_TOOLS` now.
    #[test]
    fn replay_hides_every_tool_the_live_renderer_hides() {
        for name in crate::hidden::TRANSCRIPT_HIDDEN_TOOLS {
            let raw = vec![json!({
                "type": "assistant",
                "uuid": "a-only-hidden",
                "timestamp": "2026-09-06T00:00:00.000Z",
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "tool_use", "id": "t-1", "name": name, "input": {} },
                    ],
                },
            })];
            let (rows, _) = rows_of(&raw);
            assert!(rows.is_empty(), "{name} should leave no row on replay");
        }
    }

    /// The reminder the harness appended is not what the person typed, but
    /// the rest of the row is — stripping must keep it.
    #[test]
    fn an_injected_reminder_is_removed_without_losing_the_users_own_words() {
        let raw = fixture();
        let (rows, _) = rows_of(&raw);
        let Message::User(user) = &rows[0] else {
            panic!("first row is the user turn");
        };
        let UserContentBlock::Text(text) = &user.message.content[0] else {
            panic!("user turn is text");
        };
        assert_eq!(text.text.trim(), "read the file");
        assert!(!text.text.contains("system-reminder"));
    }
}
