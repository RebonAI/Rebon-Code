use super::super::*;
use crate::message::{
    AssistantMessage, AssistantMessageInner, AssistantRole, AssistantTextBlock,
    AssistantThinkingBlock, AssistantToolUseBlock, SystemLevel, SystemMessage, ToolResultContent,
    UserMessage, UserMessageInner, UserRole, UserTextBlock, UserToolResultBlock,
};
use crate::StreamingToolUse;
use serde_json::json;

pub(crate) fn new_buf(w: u16, h: u16) -> Buffer {
    Buffer::empty(Rect::new(0, 0, w, h))
}

pub(crate) fn row_text(buf: &Buffer, y: u16) -> String {
    let mut s = String::new();
    for x in 0..buf.area().width {
        s.push_str(buf[(x, y)].symbol());
    }
    s.trim_end().to_string()
}

pub(crate) fn assert_text_modifier(
    buf: &Buffer,
    y: u16,
    needle: &str,
    modifier: ratatui::style::Modifier,
    expected: bool,
) {
    let row = row_text(buf, y);
    let byte_x = row
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not found on row {y}: {row:?}"));
    let x = rebon_width::str_width(&row[..byte_x]) as u16;
    assert_eq!(
        buf[(x, y)].style().add_modifier.contains(modifier),
        expected,
        "unexpected modifier {modifier:?} for {needle:?} on row {y}: {row:?}"
    );
}

pub(super) fn user(uuid: &str, text: &str) -> Message {
    Message::User(UserMessage {
        uuid: uuid.into(),
        timestamp: "t".into(),
        message: UserMessageInner {
            role: UserRole::User,
            content: vec![UserContentBlock::Text(UserTextBlock { text: text.into() })],
        },
        is_compact_summary: None,
        is_meta: None,
        is_visible_in_transcript_only: None,
        image_paste_ids: None,
        plan_content: None,
    })
}

pub(super) fn system_info(uuid: &str, text: &str) -> Message {
    Message::System(SystemMessage {
        uuid: uuid.into(),
        timestamp: "t".into(),
        subtype: "info".into(),
        content: Some(text.into()),
        level: Some(SystemLevel::Info),
        is_meta: None,
    })
}

pub(super) fn assistant_text(uuid: &str, text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        uuid: uuid.into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content: vec![AssistantContentBlock::Text(AssistantTextBlock {
                text: text.into(),
            })],
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    })
}

pub(super) fn assistant_thinking(uuid: &str, thinking: &str) -> Message {
    Message::Assistant(AssistantMessage {
        uuid: uuid.into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content: vec![AssistantContentBlock::Thinking(AssistantThinkingBlock {
                thinking: thinking.into(),
                signature: None,
            })],
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    })
}

pub(super) fn assistant_thinking_then_text(uuid: &str, thinking: &str, text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        uuid: uuid.into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content: vec![
                AssistantContentBlock::Thinking(AssistantThinkingBlock {
                    thinking: thinking.into(),
                    signature: None,
                }),
                AssistantContentBlock::Text(AssistantTextBlock { text: text.into() }),
            ],
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    })
}

pub(super) fn normalized_visible_rows(buf: &Buffer) -> Vec<String> {
    (0..buf.area().height)
        .map(|y| row_text(buf, y))
        .filter(|line| !line.trim().is_empty())
        .collect()
}

pub(super) fn all_rows(buf: &Buffer) -> Vec<String> {
    (0..buf.area().height).map(|y| row_text(buf, y)).collect()
}

pub(super) fn trim_blank_boundaries(rows: Vec<String>) -> Vec<String> {
    let start = rows
        .iter()
        .position(|line| !line.trim().is_empty())
        .unwrap_or(rows.len());
    let end = rows
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map(|idx| idx + 1)
        .unwrap_or(start);
    rows[start..end].to_vec()
}

pub(super) fn semantic_rows(buf: &Buffer) -> Vec<String> {
    trim_blank_boundaries(all_rows(buf))
}

pub(super) fn assert_no_adjacent_blank_rows(rows: &[String]) {
    for pair in rows.windows(2) {
        assert!(
            !(pair[0].trim().is_empty() && pair[1].trim().is_empty()),
            "adjacent semantic blank rows found: {rows:?}"
        );
    }
}

pub(super) fn row_y_containing(rows: &[String], needle: &str) -> usize {
    rows.iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} not found in rows: {rows:?}"))
}

pub(super) fn fake_streaming_tool(call_id: &str, tool_name: &str, path: &str) -> StreamingToolUse {
    let mut raw_input = HashMap::new();
    raw_input.insert("path".into(), json!(path));
    StreamingToolUse {
        call_id: call_id.into(),
        tool_name: tool_name.into(),
        kind: committed_tool::tool_kind_from_name(tool_name),
        status: ToolCallStatus::Completed,
        title: Some(path.into()),
        content: None,
        locations: None,
        raw_input: Some(raw_input),
        raw_output: None,
    }
}

pub(super) fn assistant_tool(uuid: &str, id: &str, name: &str, path: &str) -> Message {
    Message::Assistant(AssistantMessage {
        uuid: uuid.into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content: vec![AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                id: id.into(),
                name: name.into(),
                input: json!({ "path": path }),
                tool_call_content: None,
                raw_output: None,
                title: Some(path.into()),
                locations: None,
                status: Some(ToolCallStatus::Completed),
            })],
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    })
}

/// Helper: collect all non-blank text from a buffer into a single string.
pub(crate) fn all_text(buf: &Buffer) -> String {
    (0..buf.area().height)
        .map(|y| row_text(buf, y))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn streaming_tool(
    call_id: &str,
    tool_name: &str,
    kind: ToolKind,
    status: ToolCallStatus,
    raw_input: Vec<(&str, Value)>,
) -> StreamingToolUse {
    StreamingToolUse {
        call_id: call_id.into(),
        tool_name: tool_name.into(),
        kind,
        status,
        title: None,
        content: None,
        locations: None,
        raw_input: Some(
            raw_input
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        ),
        raw_output: None,
    }
}

pub(super) fn assistant_tool_uses(
    uuid: &str,
    tools: Vec<(&str, &str, serde_json::Value)>,
) -> Message {
    Message::Assistant(AssistantMessage {
        uuid: uuid.into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content: tools
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
                .collect(),
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    })
}

pub(super) fn user_tool_results(uuid: &str, results: Vec<(&str, &str, Option<bool>)>) -> Message {
    Message::User(UserMessage {
        uuid: uuid.into(),
        timestamp: "t".into(),
        message: UserMessageInner {
            role: UserRole::User,
            content: results
                .into_iter()
                .map(|(tool_use_id, content, is_error)| {
                    UserContentBlock::ToolResult(UserToolResultBlock {
                        tool_use_id: tool_use_id.into(),
                        content: ToolResultContent::Text(content.into()),
                        is_error,
                    })
                })
                .collect(),
        },
        is_compact_summary: None,
        is_meta: None,
        is_visible_in_transcript_only: None,
        image_paste_ids: None,
        plan_content: None,
    })
}

pub(super) fn assistant_thinking_only(uuid: &str, thinking: &str) -> Message {
    Message::Assistant(AssistantMessage {
        uuid: uuid.into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content: vec![AssistantContentBlock::Thinking(AssistantThinkingBlock {
                thinking: thinking.into(),
                signature: None,
            })],
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    })
}
