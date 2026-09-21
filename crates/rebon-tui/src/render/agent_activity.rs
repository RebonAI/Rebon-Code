use std::collections::HashMap;

use serde_json::Value;

pub(super) fn extract_agent_activity_lines(
    raw_output: &HashMap<String, Value>,
) -> Option<Vec<String>> {
    let lines: Vec<String> = raw_output
        .get("activity_lines")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_str)
        .flat_map(|line| line.split('\n'))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();

    (!lines.is_empty()).then_some(lines)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ratatui::{buffer::Buffer, layout::Rect};
    use rebon_types::{ToolCallStatus, ToolKind};
    use serde_json::{json, Value};

    use super::super::{render_streaming_overlay, RenderTheme, ToolOutputVerbosity};
    use crate::streaming::{StreamingOverlay, StreamingToolUse};

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

    fn streaming_tool(
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

    fn streaming_tool_with_raw_output(
        mut tool: StreamingToolUse,
        raw_output: Vec<(&str, Value)>,
    ) -> StreamingToolUse {
        let mut merged = raw_output
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect::<HashMap<_, _>>();
        if let Some(existing) = tool.raw_output.take() {
            for (key, value) in existing {
                merged.entry(key).or_insert(value);
            }
        }
        tool.raw_output = Some(merged);
        tool
    }

    #[test]
    fn compact_agent_raw_activity_lines_render_as_semantic_paths() {
        let mut overlay = StreamingOverlay::new();
        let long_path = "/workspace/crates/rebon-cli/src/main.rs";
        overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
            streaming_tool(
                "agent-1",
                "Agent",
                ToolKind::Other,
                ToolCallStatus::InProgress,
                vec![
                    ("subagent_type", json!("Explore")),
                    ("description", json!("测试 agent 创建")),
                    ("prompt", json!("测试 agent 创建")),
                ],
            ),
            vec![("activity_lines", json!([long_path]))],
        ));

        let mut buf = new_buf(80, 6);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 80, 6),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );
        let snap = all_rows(&buf).join("\n");

        assert!(snap.contains("Explore:"), "{snap:?}");
        assert!(snap.contains(long_path), "{snap:?}");
        assert!(!snap.contains("activity_lines="), "{snap:?}");
    }

    #[test]
    fn failed_agent_raw_activity_lines_render_as_semantic_paths() {
        let mut overlay = StreamingOverlay::new();
        let long_path = "/workspace/crates/rebon-tui/src/render.rs";
        overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
            streaming_tool(
                "agent-1",
                "Agent",
                ToolKind::Other,
                ToolCallStatus::Failed,
                vec![
                    ("subagent_type", json!("Explore")),
                    ("description", json!("inspect renderer failure")),
                    ("prompt", json!("inspect renderer failure")),
                ],
            ),
            vec![
                ("activity_lines", json!([long_path])),
                ("error", json!("agent failed")),
            ],
        ));

        let mut buf = new_buf(100, 8);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 100, 8),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );
        let snap = all_rows(&buf).join("\n");

        assert!(snap.contains("Explore:"), "{snap:?}");
        assert!(snap.contains(long_path), "{snap:?}");
        assert!(!snap.contains("activity_lines="), "{snap:?}");
        assert!(snap.contains("error=agent failed"), "{snap:?}");
    }

    #[test]
    fn compact_agent_raw_activity_lines_keep_newest_preview_and_older_hint() {
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
            streaming_tool(
                "agent-1",
                "Agent",
                ToolKind::Other,
                ToolCallStatus::Completed,
                vec![
                    ("subagent_type", json!("Explore")),
                    ("description", json!("ACP code")),
                    ("prompt", json!("ACP code")),
                ],
            ),
            vec![(
                "activity_lines",
                json!([
                    "/workspace/crates/rebon-tui/src/first.rs",
                    "/workspace/crates/rebon-tui/src/second.rs",
                    "/workspace/crates/rebon-tui/src/third.rs",
                    "/workspace/crates/rebon-tui/src/fourth.rs",
                    "/workspace/crates/rebon-tui/src/latest.rs"
                ]),
            )],
        ));

        let mut buf = new_buf(100, 8);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 100, 8),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );
        let snap = all_rows(&buf).join("\n");

        assert!(snap.contains("latest.rs"), "{snap:?}");
        assert!(snap.contains("fourth.rs"), "{snap:?}");
        assert!(!snap.contains("first.rs"), "{snap:?}");
        assert!(snap.contains("… +1 older lines"), "{snap:?}");
        assert!(!snap.contains("activity_lines="), "{snap:?}");
    }

    #[test]
    fn narrow_compact_agent_raw_activity_line_does_not_render_json_field() {
        let mut overlay = StreamingOverlay::new();
        let long_path = "/workspace/crates/rebon-cli/src/main.rs";
        overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
            streaming_tool(
                "agent-1",
                "Agent",
                ToolKind::Other,
                ToolCallStatus::Completed,
                vec![
                    ("subagent_type", json!("Explore")),
                    ("description", json!("narrow path")),
                    ("prompt", json!("narrow path")),
                ],
            ),
            vec![("activity_lines", json!([long_path]))],
        ));

        let mut buf = new_buf(32, 6);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 32, 6),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );
        let snap = all_rows(&buf).join("\n");

        assert!(snap.contains("/workspace/crates/rebon-cli/sr"), "{snap:?}");
        assert!(snap.contains("c/main.rs"), "{snap:?}");
        assert!(!snap.contains("activity_lines="), "{snap:?}");
    }
}
