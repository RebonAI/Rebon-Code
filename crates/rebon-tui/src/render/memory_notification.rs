use serde_json::Value;

use rebon_types::{ContentBlock, ToolCallContent};

/// Extract the memory-update notification emitted by Edit/Write-style tools.
/// Prefer the structured `raw_output.memoryNotification` value, then fall back
/// to text content that carries the same user-facing notification.
pub(super) fn extract_memory_notification(
    tool: &crate::streaming::StreamingToolUse,
) -> Option<String> {
    if let Some(notification) = tool
        .raw_output
        .as_ref()
        .and_then(|m| m.get("memoryNotification"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Some(notification.to_string());
    }

    tool.content.as_ref()?.iter().find_map(|c| match c {
        ToolCallContent::Content(regular) => match &regular.content {
            ContentBlock::Text(text) if text.text.starts_with("Memory updated in ") => {
                Some(text.text.clone())
            }
            _ => None,
        },
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ratatui::{buffer::Buffer, layout::Rect};
    use serde_json::json;

    use rebon_types::{ContentBlock, DiffContent, ToolCallContent, ToolCallStatus, ToolKind};

    use crate::streaming::{StreamingOverlay, StreamingToolUse};

    use super::super::{render_streaming_overlay, RenderTheme, ToolOutputVerbosity};

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

    #[test]
    fn edit_tool_renders_memory_notification_after_diff() {
        let notification = "Memory updated in ./MEMORY.md \u{00B7} /memory to edit";
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(StreamingToolUse {
            call_id: "tool-edit".into(),
            tool_name: "Edit".into(),
            kind: ToolKind::Edit,
            status: ToolCallStatus::Completed,
            title: None,
            content: Some(vec![
                ToolCallContent::Diff(DiffContent {
                    path: "MEMORY.md".into(),
                    old_text: Some("before".into()),
                    new_text: "after".into(),
                }),
                ToolCallContent::Content(rebon_types::RegularContent {
                    content: ContentBlock::Text(rebon_types::TextContent {
                        text: notification.into(),
                        annotations: None,
                    }),
                }),
            ]),
            locations: None,
            raw_input: Some(HashMap::from([("file_path".into(), json!("MEMORY.md"))])),
            raw_output: Some(HashMap::from([
                ("type".into(), json!("update")),
                ("originalFile".into(), json!("before")),
                ("memoryNotification".into(), json!(notification)),
            ])),
        });
        let mut buf = new_buf(80, 8);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 80, 8),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Verbose,
        );
        let snap = all_text(&buf);
        assert!(snap.contains("Memory updated in ./MEMORY.md"), "{snap:?}");
        assert!(snap.contains("/memory to edit"), "{snap:?}");
    }
}
