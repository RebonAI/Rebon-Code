use ratatui::style::Style;
use rebon_types::ToolCallStatus;

use super::RenderTheme;

pub(super) fn tool_status_style(theme: &RenderTheme, status: ToolCallStatus) -> Style {
    match status {
        ToolCallStatus::Pending => theme.streaming,
        ToolCallStatus::InProgress => theme.system_warning,
        ToolCallStatus::Completed => theme.assistant_prefix,
        ToolCallStatus::Failed => theme.system_error,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier, Style},
    };
    use rebon_types::{ContentBlock, ToolCallContent, ToolCallStatus};
    use serde_json::json;

    use super::super::{render_streaming_overlay, ToolOutputVerbosity};
    use super::*;
    use crate::streaming::{StreamingContentBlock, StreamingOverlay, StreamingToolUse};

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

    fn semantic_rows(buf: &Buffer) -> Vec<String> {
        let rows = all_rows(buf);
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

    fn row_style_at_text(buf: &Buffer, y: u16, needle: &str) -> Style {
        let row = row_text(buf, y);
        let x = row
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not found on row {y}: {row:?}"));
        buf[(x as u16, y)].style()
    }

    fn streaming_tool_with_status(
        call_id: &str,
        tool_name: &str,
        path: &str,
        status: ToolCallStatus,
    ) -> StreamingToolUse {
        let mut raw_input = HashMap::new();
        raw_input.insert("path".into(), json!(path));
        StreamingToolUse {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            kind: super::super::committed_tool::tool_kind_from_name(tool_name),
            status,
            title: Some(path.into()),
            content: None,
            locations: None,
            raw_input: Some(raw_input),
            raw_output: None,
        }
    }

    #[test]
    fn compact_failed_shell_deduplicates_output_and_hides_raw_metadata() {
        let mut theme = RenderTheme::plain();
        theme.system_error = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
        theme.tool_result_error = Style::new().fg(Color::Red);
        let mut tool = streaming_tool_with_status("t1", "Bash", "ignored", ToolCallStatus::Failed);
        tool.content = Some(vec![ToolCallContent::Content(
            rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "command failed\npermission denied".into(),
                    annotations: None,
                }),
            },
        )]);
        let mut raw_output = HashMap::new();
        raw_output.insert("stderr".into(), json!("permission denied"));
        tool.raw_output = Some(raw_output);
        let mut buf = new_buf(80, 8);
        render_streaming_overlay(
            &StreamingOverlay::from_blocks(vec![StreamingContentBlock::ToolUse(tool)]),
            Rect::new(0, 0, 80, 8),
            &mut buf,
            &theme,
            ToolOutputVerbosity::Compact,
        );
        let rows = semantic_rows(&buf);
        assert_eq!(
            rows,
            vec![
                "● Bash (ignored)".to_string(),
                "⎿ command failed".to_string(),
                "  permission denied".to_string(),
                "  Ctrl+O to expand".to_string(),
            ]
        );
        assert_eq!(buf[(0, 0)].style().fg, Some(Color::Red));
        assert_eq!(
            row_style_at_text(&buf, 1, "command failed").fg,
            Some(Color::Red)
        );
    }

    #[test]
    fn failed_shell_long_single_line_respects_visual_preview_limit() {
        let long_error = format!("FAIL-{}-TAIL", "x".repeat(300));

        for tool_name in ["Bash", "PowerShell"] {
            for verbosity in [ToolOutputVerbosity::Compact, ToolOutputVerbosity::Normal] {
                let mut tool =
                    streaming_tool_with_status("t1", tool_name, "ignored", ToolCallStatus::Failed);
                tool.raw_output = Some(HashMap::from([(
                    "stderr".into(),
                    json!(long_error.clone()),
                )]));

                let mut buf = new_buf(40, 16);
                render_streaming_overlay(
                    &StreamingOverlay::from_blocks(vec![StreamingContentBlock::ToolUse(tool)]),
                    Rect::new(0, 0, 40, 16),
                    &mut buf,
                    &RenderTheme::plain(),
                    verbosity,
                );

                let rows = semantic_rows(&buf);
                let snap = rows.join("\n");
                assert_eq!(rows.len(), 6, "{tool_name} {verbosity:?}: {rows:?}");
                assert!(
                    snap.contains("FAIL-"),
                    "{tool_name} {verbosity:?}: {snap:?}"
                );
                assert!(
                    !snap.contains("-TAIL"),
                    "{tool_name} {verbosity:?}: {snap:?}"
                );
                assert!(
                    snap.contains("… +5 lines"),
                    "{tool_name} {verbosity:?}: {snap:?}"
                );
                assert_eq!(
                    snap.matches("Ctrl+O to expand").count(),
                    usize::from(matches!(verbosity, ToolOutputVerbosity::Compact)),
                    "{tool_name} {verbosity:?}: {snap:?}"
                );
            }

            let mut tool =
                streaming_tool_with_status("t1", tool_name, "ignored", ToolCallStatus::Failed);
            tool.raw_output = Some(HashMap::from([(
                "stderr".into(),
                json!(long_error.clone()),
            )]));
            let mut buf = new_buf(40, 16);
            render_streaming_overlay(
                &StreamingOverlay::from_blocks(vec![StreamingContentBlock::ToolUse(tool)]),
                Rect::new(0, 0, 40, 16),
                &mut buf,
                &RenderTheme::plain(),
                ToolOutputVerbosity::Verbose,
            );
            let snap = semantic_rows(&buf).join("\n");
            assert!(snap.contains("-TAIL"), "{tool_name}: {snap:?}");
            assert!(!snap.contains("… +"), "{tool_name}: {snap:?}");
        }
    }

    #[test]
    fn compact_bash_output_is_deduplicated_and_hides_raw_metadata() {
        let mut tool =
            streaming_tool_with_status("t1", "Bash", "test command", ToolCallStatus::Completed);
        tool.content = Some(vec![
            ToolCallContent::Content(rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "exit=101".into(),
                    annotations: None,
                }),
            }),
            ToolCallContent::Content(rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "exit=101".into(),
                    annotations: None,
                }),
            }),
        ]);
        let mut raw_output = HashMap::new();
        raw_output.insert("stdout".into(), json!("exit=101"));
        raw_output.insert("exitCode".into(), json!(101));
        raw_output.insert("command".into(), json!("cargo test"));
        tool.raw_output = Some(raw_output);

        let mut buf = new_buf(80, 6);
        render_streaming_overlay(
            &StreamingOverlay::from_blocks(vec![StreamingContentBlock::ToolUse(tool)]),
            Rect::new(0, 0, 80, 6),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );

        let rows = semantic_rows(&buf);
        assert_eq!(
            rows.iter().filter(|row| row.contains("exit=101")).count(),
            1
        );
        let snap = rows.join("\n");
        assert!(!snap.contains("exitCode"), "{snap:?}");
        assert!(!snap.contains("stdout="), "{snap:?}");
        assert!(!snap.contains("command="), "{snap:?}");
    }

    #[test]
    fn failed_edit_tool_hides_detailed_error() {
        let mut tool = streaming_tool_with_status("t1", "Edit", "short.ts", ToolCallStatus::Failed);
        tool.raw_input
            .as_mut()
            .unwrap()
            .insert("file_path".into(), json!("short.ts"));
        tool.content = Some(vec![ToolCallContent::Content(
            rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "invalid input for tool `Edit`: `old_string` was not found in C:\\project\\src\\utils\\config.ts".into(),
                    annotations: None,
                }),
            },
        )]);
        let mut raw_output = HashMap::new();
        raw_output.insert("error".into(), json!("formatter changed the file"));
        tool.raw_output = Some(raw_output);
        let mut buf = new_buf(80, 6);
        render_streaming_overlay(
            &StreamingOverlay::from_blocks(vec![StreamingContentBlock::ToolUse(tool)]),
            Rect::new(0, 0, 80, 6),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );
        let rows = semantic_rows(&buf);
        assert_eq!(rows, vec!["● Edit(short.ts)", "  Edit failed"]);
        let snap = rows.join("\n");
        assert!(!snap.contains("old_string"), "{snap:?}");
        assert!(!snap.contains("formatter"), "{snap:?}");
    }
}
