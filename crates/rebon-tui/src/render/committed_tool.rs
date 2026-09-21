use std::collections::HashMap;

use rebon_types::ToolCallStatus;

use crate::message::AssistantToolUseBlock;
use crate::streaming::StreamingToolUse;

use super::TRANSCRIPT_HIDDEN_TOOLS;

pub(super) fn committed_tool_has_streaming_render_state(tu: &AssistantToolUseBlock) -> bool {
    tu.status.is_some()
        || tu.tool_call_content.is_some()
        || tu.raw_output.is_some()
        || tu.title.is_some()
        || tu.locations.is_some()
}

pub(super) fn committed_tool_to_streaming(tu: &AssistantToolUseBlock) -> Option<StreamingToolUse> {
    committed_tool_to_streaming_with_terminal_status(tu, false)
}

pub(super) fn committed_tool_to_streaming_with_terminal_status(
    tu: &AssistantToolUseBlock,
    force_terminal_status: bool,
) -> Option<StreamingToolUse> {
    if !committed_tool_has_streaming_render_state(tu) {
        return None;
    }
    if TRANSCRIPT_HIDDEN_TOOLS
        .iter()
        .any(|&hidden| hidden == tu.name)
    {
        return None;
    }
    let status = if force_terminal_status
        && matches!(
            tu.status,
            Some(ToolCallStatus::Pending | ToolCallStatus::InProgress)
        ) {
        ToolCallStatus::Completed
    } else {
        tu.status.unwrap_or(ToolCallStatus::Completed)
    };
    Some(StreamingToolUse {
        call_id: tu.id.clone(),
        tool_name: tu.name.clone(),
        kind: tool_kind_from_name(&tu.name),
        status,
        title: tu.title.clone(),
        content: tu.tool_call_content.clone(),
        locations: tu.locations.clone(),
        raw_input: tu.input.as_object().map(|map| {
            map.iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<HashMap<_, _>>()
        }),
        raw_output: tu.raw_output.as_ref().and_then(|value| {
            value.as_object().map(|map| {
                map.iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<HashMap<_, _>>()
            })
        }),
    })
}

// Canonical tool-name → ToolKind now lives in the shared `rebon-render`
// (the GPUI app uses the same mapping). Re-exported so `tool_kind_from_name(…)`
// resolves here for committed_tool_to_streaming and the other render siblings.
// NB: the shared mapping is the UNION (adds NotebookRead/BashOutput/WebSearch),
// so a few names that were `Other` now classify more specifically.
pub(super) use rebon_render::kind::tool_kind_from_name;

#[cfg(test)]
mod tests {
    use ratatui::{buffer::Buffer, layout::Rect};
    use serde_json::json;

    use super::super::{render_transcript, RenderTheme, ToolOutputVerbosity};
    use crate::message::{
        AssistantContentBlock, AssistantMessage, AssistantMessageInner, AssistantRole,
        AssistantToolUseBlock, Message, ToolResultContent, UserContentBlock, UserMessage,
        UserMessageInner, UserRole, UserToolResultBlock,
    };
    use crate::state::{reducer, Action, AppState};

    fn new_buf(w: u16, h: u16) -> Buffer {
        Buffer::empty(Rect::new(0, 0, w, h))
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        let mut s = String::new();
        for x in 0..buf.area().width {
            s.push_str(buf[(x, y)].symbol());
        }
        s.trim_end().to_string()
    }

    fn all_text(buf: &Buffer) -> String {
        (0..buf.area().height)
            .map(|y| row_text(buf, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assistant_tool_uses(uuid: &str, tools: Vec<(&str, &str, serde_json::Value)>) -> Message {
        let content = tools
            .into_iter()
            .map(|(id, name, input)| {
                AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                    id: id.into(),
                    name: name.into(),
                    input,
                    tool_call_content: None,
                    raw_output: None,
                    title: None,
                    locations: None,
                    status: None,
                })
            })
            .collect();
        Message::Assistant(AssistantMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content,
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })
    }

    fn user_tool_results(uuid: &str, results: Vec<(&str, &str, Option<bool>)>) -> Message {
        let content = results
            .into_iter()
            .map(|(tool_use_id, body, is_error)| {
                UserContentBlock::ToolResult(UserToolResultBlock {
                    tool_use_id: tool_use_id.into(),
                    content: ToolResultContent::Text(body.into()),
                    is_error,
                })
            })
            .collect();
        Message::User(UserMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: UserMessageInner {
                role: UserRole::User,
                content,
            },
            is_compact_summary: None,
            is_meta: None,
            is_visible_in_transcript_only: None,
            image_paste_ids: None,
            plan_content: None,
        })
    }

    #[test]
    fn committed_single_tool_use_still_renders_per_tool_card() {
        // A lone tool_use (count < 2) must keep its per-tool card —
        // collapse is opt-in at `>= 2` total tool_uses.
        let mut s = AppState::new();
        reducer(
            &mut s,
            Action::Commit(assistant_tool_uses(
                "a1",
                vec![("toolu_1", "Read", json!({ "file_path": "Cargo.toml" }))],
            )),
        );
        reducer(
            &mut s,
            Action::Commit(user_tool_results("u1", vec![("toolu_1", "body", None)])),
        );

        let mut buf = new_buf(80, 8);
        render_transcript(
            &s,
            Rect::new(0, 0, 80, 8),
            &mut buf,
            &RenderTheme::plain(),
            0,
            ToolOutputVerbosity::Compact,
            0,
            None,
        );
        let snap = all_text(&buf);

        assert!(
            snap.contains("Read"),
            "single Read tool_use must survive un-collapsed: {snap:?}"
        );
        assert!(
            !snap.contains("read 1 file"),
            "single tool use should not hit collapse path: {snap:?}"
        );
    }
}
