use std::collections::HashMap;

use serde_json::Value;

fn is_image_output_shape(type_value: Option<&Value>, file_value: Option<&Value>) -> bool {
    if type_value.and_then(Value::as_str) != Some("image") {
        return false;
    }
    file_value
        .and_then(Value::as_object)
        .and_then(|file| file.get("type"))
        .and_then(Value::as_str)
        .map(|media_type| media_type.starts_with("image/"))
        .unwrap_or(false)
}

pub(super) fn is_read_image_raw_output(
    tool_name: &str,
    raw_output: &HashMap<String, Value>,
) -> bool {
    tool_name == "Read" && is_image_output_shape(raw_output.get("type"), raw_output.get("file"))
}

pub(super) fn is_read_image_object_output(
    tool_name: &str,
    map: &serde_json::Map<String, Value>,
) -> bool {
    tool_name == "Read" && is_image_output_shape(map.get("type"), map.get("file"))
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::HashMap;

    use ratatui::layout::Rect;
    use rebon_types::{ContentBlock, ToolCallContent, ToolCallLocation, ToolCallStatus, ToolKind};
    use serde_json::json;

    use super::super::{
        collect_tool_block_body_lines, render_streaming_overlay, RenderTheme, ToolOutputVerbosity,
    };
    use crate::message::AssistantToolUseBlock;
    use crate::render::tests::{new_buf, row_text};
    use crate::streaming::{StreamingOverlay, StreamingToolUse};

    #[test]
    fn compact_mode_read_image_shows_single_path_line() {
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(StreamingToolUse {
            call_id: "tool-1".into(),
            tool_name: "Read".into(),
            kind: ToolKind::Read,
            status: ToolCallStatus::Completed,
            title: None,
            content: Some(vec![ToolCallContent::Content(
                rebon_types::RegularContent {
                    content: ContentBlock::Text(rebon_types::TextContent {
                        text: "Image file: /workspace/pixel.jpg".into(),
                        annotations: None,
                    }),
                },
            )]),
            locations: Some(vec![ToolCallLocation {
                path: "/workspace/pixel.jpg".into(),
                line: None,
            }]),
            raw_input: Some(HashMap::from([(
                "file_path".into(),
                json!("/workspace/pixel.jpg"),
            )])),
            raw_output: Some(HashMap::from([
                ("type".into(), json!("image")),
                (
                    "file".into(),
                    json!({
                        "filePath": "/workspace/pixel.jpg",
                        "type": "image/jpeg"
                    }),
                ),
            ])),
        });

        let mut buf = new_buf(120, 8);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 120, 8),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );
        let snap = (0..8)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(snap.contains("Read (/workspace/pixel.jpg)"), "{snap:?}");
        assert!(
            snap.contains("Image file: /workspace/pixel.jpg"),
            "{snap:?}"
        );
        assert_eq!(snap.matches("/workspace/pixel.jpg").count(), 2, "{snap:?}");
        assert!(!snap.contains("file="), "{snap:?}");
        assert!(!snap.contains("type=image"), "{snap:?}");
    }

    #[test]
    fn verbose_committed_read_image_shows_single_path_line() {
        let tu = AssistantToolUseBlock {
            id: "toolu_img".into(),
            name: "Read".into(),
            input: json!({ "file_path": "/workspace/pixel.jpg" }),
            tool_call_content: Some(vec![ToolCallContent::Content(
                rebon_types::RegularContent {
                    content: ContentBlock::Text(rebon_types::TextContent {
                        text: "Image file: /workspace/pixel.jpg".into(),
                        annotations: None,
                    }),
                },
            )]),
            raw_output: Some(json!({
                "type": "image",
                "file": {
                    "filePath": "/workspace/pixel.jpg",
                    "type": "image/jpeg"
                }
            })),
            title: None,
            locations: Some(vec![ToolCallLocation {
                path: "/workspace/pixel.jpg".into(),
                line: None,
            }]),
            status: Some(ToolCallStatus::Completed),
        };
        let lines = collect_tool_block_body_lines(&tu, ToolOutputVerbosity::Verbose);

        assert_eq!(lines, vec!["Image file: /workspace/pixel.jpg"]);
    }
}
