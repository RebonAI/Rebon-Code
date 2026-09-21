// Tab expansion moved to `rebon-render::text`; no in-crate caller
// remains (the body/agent renderers that used it moved to the shared core too),
// so this file now only carries the ratatui rendering regression tests below.

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ratatui::{buffer::Buffer, layout::Rect};
    use serde_json::json;

    use rebon_types::{ContentBlock, ToolCallContent, ToolCallStatus, ToolKind};

    use crate::message::{
        AssistantContentBlock, AssistantMessage, AssistantMessageInner, AssistantRole,
        AssistantToolUseBlock, Message,
    };
    use crate::streaming::{StreamingOverlay, StreamingToolUse};

    use super::super::{
        render_message, render_streaming_overlay, RenderTheme, ToolOutputVerbosity,
    };

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

    fn all_rows(buf: &Buffer) -> Vec<String> {
        (0..buf.area().height).map(|y| row_text(buf, y)).collect()
    }

    fn assistant_tool_uses(uuid: &str, tools: Vec<(&str, &str, serde_json::Value)>) -> Message {
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

    #[test]
    fn streaming_bash_multiline_command_has_no_blank_gap_before_output() {
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(StreamingToolUse {
            call_id: "tool-bash".into(),
            tool_name: "Bash".into(),
            kind: ToolKind::Execute,
            status: ToolCallStatus::Completed,
            title: None,
            content: Some(vec![ToolCallContent::Content(
                rebon_types::RegularContent {
                    content: ContentBlock::Text(rebon_types::TextContent {
                        text: "first output\nsecond output".into(),
                        annotations: None,
                    }),
                },
            )]),
            locations: None,
            raw_input: Some(HashMap::from([(
                "command".into(),
                json!("python -c \"one\n  two\nthree\""),
            )])),
            raw_output: None,
        });
        let mut buf = new_buf(60, 10);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 60, 10),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );

        let rows = all_rows(&buf);
        let body_idx = rows
            .iter()
            .position(|row| row.contains('⎿'))
            .expect("body gutter should render");
        let blank_header_rows: Vec<_> = rows[..body_idx]
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, row)| row.trim().is_empty())
            .collect();
        assert!(
            blank_header_rows.is_empty(),
            "blank gap before bash output: {rows:?}"
        );
        assert!(
            rows[body_idx].contains("first output"),
            "body should start immediately after header: {rows:?}"
        );
    }

    #[test]
    fn committed_bash_multiline_command_header_has_no_internal_blank_rows() {
        let msg = assistant_tool_uses(
            "a-bash",
            vec![(
                "tool-bash",
                "Bash",
                json!({ "command": "cargo fmt --manifest-path Cargo.toml &&\n  cargo test -p rebon-cli file_index &&\n  cargo test -p rebon-dialog project_suggestions" }),
            )],
        );
        let mut buf = new_buf(70, 8);
        let used = render_message(
            &msg,
            Rect::new(0, 0, 70, 8),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );

        let rows = all_rows(&buf);
        let rendered_rows = &rows[..used as usize];
        assert_eq!(
            used, 2,
            "multiline bash command should consume only margin + header: {rows:?}"
        );
        assert!(
            rendered_rows.iter().any(|row| row.contains("Bash")),
            "bash header should render: {rows:?}"
        );
        assert!(
            rendered_rows
                .iter()
                .skip(1)
                .all(|row| !row.trim().is_empty()),
            "multiline bash command produced blank header rows: {rows:?}"
        );
    }
}
