use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use rebon_message_tui::{diff_lines_to_text, MessagesRenderTheme};
use rebon_render::fold_long_diff_runs;
use rebon_types::{DiffContent, ToolCallContent};

use super::{clear_buffer_area, GUTTER};

// The pure diff-marker algorithm (line-level LCS + file-context hunks) now lives
// in the framework-agnostic `rebon-render` crate so the GPUI app renders
// byte-for-byte identical Edit diffs. Re-export the names the render/ siblings
// import via `super::`.
pub(super) use rebon_render::{build_context_diff, diff_line_counts, diff_summary_from_counts};

/// Extract the first `DiffContent` from a streaming tool use's content,
/// if present. Edit tools send their result as `ToolCallContent::Diff`.
pub(super) fn extract_diff_content(
    tool: &crate::streaming::StreamingToolUse,
) -> Option<&DiffContent> {
    tool.content.as_ref()?.iter().find_map(|c| match c {
        ToolCallContent::Diff(d) => Some(d),
        _ => None,
    })
}

pub(super) fn measure_diff_block_height(
    diff_lines: &[String],
    start_line: usize,
    area_width: u16,
    fold_long_runs: bool,
) -> u16 {
    if area_width == 0 || diff_lines.is_empty() {
        return 0;
    }

    let width = area_width.saturating_sub(GUTTER) as usize;
    let wrap_fn = |s: &str, _w: usize| -> Vec<String> { vec![s.to_string()] };
    let word_diff_fn = rebon_render::calculate_word_diff;
    let options = rebon_render::FormatOptions {
        width,
        dim: false,
        wrap: &wrap_fn,
        word_diff: &word_diff_fn,
    };
    let rendered = rebon_render::format_diff_lines(diff_lines, start_line, &options);
    let rendered = if fold_long_runs {
        fold_long_diff_runs(rendered, width)
    } else {
        rendered
    };
    let theme = MessagesRenderTheme::default_styled();
    let text = diff_lines_to_text(&rendered, &theme);
    text.lines.len().min(u16::MAX as usize) as u16
}

/// Convert `DiffContent` into a `StructuredPatchHunk` with `+`/`-`
/// prefixed lines, then render through the `rebon-render`
/// pipeline (`format_diff_lines` → `diff_lines_to_text`).
/// Returns the number of vertical lines consumed.
///
/// `start_line` is the 1-based line number of the first context/change
/// line (defaults to 1 when not provided by callers that lack file
/// position info).
pub(super) fn render_diff_block_at(
    diff_lines: &[String],
    start_line: usize,
    area: Rect,
    buf: &mut Buffer,
    fold_long_runs: bool,
) -> u16 {
    if area.height == 0 || area.width == 0 || diff_lines.is_empty() {
        return 0;
    }

    let width = area.width.saturating_sub(GUTTER) as usize;
    let wrap_fn = |s: &str, _w: usize| -> Vec<String> { vec![s.to_string()] };
    let word_diff_fn = rebon_render::calculate_word_diff;
    let options = rebon_render::FormatOptions {
        width,
        dim: false,
        wrap: &wrap_fn,
        word_diff: &word_diff_fn,
    };
    let rendered = rebon_render::format_diff_lines(diff_lines, start_line, &options);
    let rendered = if fold_long_runs {
        fold_long_diff_runs(rendered, width)
    } else {
        rendered
    };
    let theme = MessagesRenderTheme::default_styled();
    let text = diff_lines_to_text(&rendered, &theme);

    let height = measure_diff_block_height(diff_lines, start_line, area.width, fold_long_runs)
        .min(area.height);
    if height == 0 {
        return 0;
    }
    let sub = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height,
    };
    clear_buffer_area(buf, sub);
    // Indent diff lines to align with the tool header body.
    let indented_lines: Vec<Line<'static>> = text
        .lines
        .into_iter()
        .map(|mut line| {
            line.spans.insert(0, Span::raw("  "));
            line
        })
        .collect();
    Paragraph::new(indented_lines)
        .wrap(Wrap { trim: false })
        .render(sub, buf);
    height
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
    use serde_json::json;

    use super::super::{render_streaming_overlay, RenderTheme, ToolOutputVerbosity};
    use crate::streaming::{StreamingOverlay, StreamingToolUse};
    use rebon_types::{DiffContent, ToolCallContent, ToolCallStatus, ToolKind};

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

    fn assert_text_bold(buf: &Buffer, y: u16, needle: &str, expected: bool) {
        let row = row_text(buf, y);
        let x = row
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not found on row {y}: {row:?}"));
        assert_eq!(
            buf[(x as u16, y)]
                .style()
                .add_modifier
                .contains(Modifier::BOLD),
            expected,
            "unexpected bold style for {needle:?} on row {y}: {row:?}"
        );
    }

    fn normalized_visible_rows(buf: &Buffer) -> Vec<String> {
        (0..buf.area().height)
            .map(|y| row_text(buf, y))
            .filter(|line| !line.trim().is_empty())
            .collect()
    }

    fn render_completed_edit(
        old_text: Option<String>,
        new_text: String,
        verbosity: ToolOutputVerbosity,
    ) -> Vec<String> {
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(StreamingToolUse {
            call_id: "tool-edit-long".into(),
            tool_name: "Edit".into(),
            kind: ToolKind::Edit,
            status: ToolCallStatus::Completed,
            title: None,
            content: Some(vec![ToolCallContent::Diff(DiffContent {
                path: "src/long.rs".into(),
                old_text,
                new_text,
            })]),
            locations: None,
            raw_input: Some(HashMap::from([("file_path".into(), json!("src/long.rs"))])),
            raw_output: Some(HashMap::from([("type".into(), json!("update"))])),
        });

        let mut buf = new_buf(100, 120);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 100, 120),
            &mut buf,
            &RenderTheme::plain(),
            verbosity,
        );
        (0..buf.area().height).map(|y| row_text(&buf, y)).collect()
    }

    fn render_sparse_multi_edit(verbosity: ToolOutputVerbosity) -> Vec<String> {
        let old_lines = (1..=80)
            .map(|line| format!("unchanged {line}"))
            .collect::<Vec<_>>();
        let mut new_lines = old_lines.clone();
        new_lines[0] = "changed at start".into();
        new_lines[79] = "changed at end".into();
        let old_text = old_lines.join("\n");
        let new_text = new_lines.join("\n");

        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(StreamingToolUse {
            call_id: "tool-multi-edit-sparse".into(),
            tool_name: "MultiEdit".into(),
            kind: ToolKind::Edit,
            status: ToolCallStatus::Completed,
            title: None,
            content: Some(vec![ToolCallContent::Diff(DiffContent {
                path: "src/sparse.rs".into(),
                old_text: Some(old_text.clone()),
                new_text,
            })]),
            locations: None,
            raw_input: Some(HashMap::from([(
                "file_path".into(),
                json!("src/sparse.rs"),
            )])),
            raw_output: Some(HashMap::from([
                ("type".into(), json!("update")),
                ("editCount".into(), json!(2)),
                ("originalFile".into(), json!(old_text)),
            ])),
        });

        let mut buf = new_buf(100, 120);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 100, 120),
            &mut buf,
            &RenderTheme::plain(),
            verbosity,
        );
        (0..buf.area().height).map(|y| row_text(&buf, y)).collect()
    }

    #[test]
    fn in_progress_edit_without_diff_renders_editing_without_expand_hint() {
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(StreamingToolUse {
            call_id: "tool-edit".into(),
            tool_name: "Edit".into(),
            kind: ToolKind::Edit,
            status: ToolCallStatus::InProgress,
            title: None,
            content: None,
            locations: None,
            raw_input: Some(HashMap::from([(
                "file_path".into(),
                json!("crates/rebon-tui/src/render.rs"),
            )])),
            raw_output: None,
        });

        let mut buf = new_buf(80, 6);
        let used = render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 80, 6),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );
        let snap = normalized_visible_rows(&buf).join("\n");

        assert!(used > 0);
        assert!(snap.contains("⠋ Editing"), "{snap:?}");
        assert!(!snap.contains("● Edit("), "{snap:?}");
        assert!(!snap.contains("Ctrl+O to expand"), "{snap:?}");
    }

    #[test]
    fn pending_edit_without_diff_renders_editing_without_expand_hint() {
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(StreamingToolUse {
            call_id: "tool-edit".into(),
            tool_name: "Edit".into(),
            kind: ToolKind::Edit,
            status: ToolCallStatus::Pending,
            title: None,
            content: None,
            locations: None,
            raw_input: Some(HashMap::from([("file_path".into(), json!("src/lib.rs"))])),
            raw_output: None,
        });

        let mut buf = new_buf(80, 6);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 80, 6),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );
        let snap = normalized_visible_rows(&buf).join("\n");

        assert!(snap.contains("⠋ Editing"), "{snap:?}");
        assert!(!snap.contains("● Edit("), "{snap:?}");
        assert!(!snap.contains("Ctrl+O to expand"), "{snap:?}");
    }

    #[test]
    fn compact_edit_preview_keeps_small_context_hunk_expanded() {
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(StreamingToolUse {
            call_id: "tool-edit".into(),
            tool_name: "Edit".into(),
            kind: ToolKind::Edit,
            status: ToolCallStatus::Completed,
            title: None,
            content: Some(vec![ToolCallContent::Diff(DiffContent {
                path: "src/lib.rs".into(),
                old_text: Some("old one\nold two".into()),
                new_text: "new one\nnew two\nnew three".into(),
            })]),
            locations: None,
            raw_input: Some(HashMap::from([("file_path".into(), json!("src/lib.rs"))])),
            raw_output: Some(HashMap::from([
                ("type".into(), json!("update")),
                (
                    "originalFile".into(),
                    json!("before1\nbefore2\nbefore3\nold one\nold two\nafter1\nafter2\nafter3"),
                ),
            ])),
        });

        let mut buf = new_buf(80, 18);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 80, 18),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );
        let snap = normalized_visible_rows(&buf).join("\n");

        assert!(snap.contains("Added 3 lines, removed 2 lines"), "{snap:?}");
        assert!(snap.contains("after3"), "{snap:?}");
        assert!(!snap.contains("… +"), "{snap:?}");
    }

    #[test]
    fn normal_edit_folds_long_added_run_like_user_prompt() {
        let new_text = (1..=30)
            .map(|line| format!("added {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = render_completed_edit(None, new_text, ToolOutputVerbosity::Normal);
        let snap = rows.join("\n");

        assert!(snap.contains("──── (10 lines hidden) ─"), "{snap:?}");
        assert!(snap.contains("added 10"), "{snap:?}");
        assert!(!snap.contains("added 11"), "{snap:?}");
        assert!(!snap.contains("added 20"), "{snap:?}");
        assert!(snap.contains("added 21"), "{snap:?}");
        assert!(snap.contains("added 30"), "{snap:?}");
    }

    #[test]
    fn normal_edit_folds_long_removed_run_like_user_prompt() {
        let old_text = (1..=30)
            .map(|line| format!("removed {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows =
            render_completed_edit(Some(old_text), String::new(), ToolOutputVerbosity::Normal);
        let snap = rows.join("\n");

        assert!(snap.contains("──── (10 lines hidden) ─"), "{snap:?}");
        assert!(snap.contains("removed 10"), "{snap:?}");
        assert!(!snap.contains("removed 11"), "{snap:?}");
        assert!(snap.contains("removed 21"), "{snap:?}");
        assert!(snap.contains("removed 30"), "{snap:?}");
    }

    #[test]
    fn verbose_edit_keeps_long_change_run_fully_expanded() {
        let new_text = (1..=24)
            .map(|line| format!("added {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = render_completed_edit(None, new_text, ToolOutputVerbosity::Verbose);
        let snap = rows.join("\n");

        assert!(!snap.contains("lines hidden"), "{snap:?}");
        assert!(snap.contains("added 11"), "{snap:?}");
        assert!(snap.contains("added 24"), "{snap:?}");
    }

    #[test]
    fn compact_multi_edit_folds_long_unchanged_span_and_keeps_all_changes() {
        let snap = render_sparse_multi_edit(ToolOutputVerbosity::Compact).join("\n");

        assert!(snap.contains("Added 2 lines, removed 2 lines"), "{snap:?}");
        assert!(snap.contains("changed at start"), "{snap:?}");
        assert!(snap.contains("changed at end"), "{snap:?}");
        assert!(snap.contains("──── (58 lines hidden) ─"), "{snap:?}");
        assert!(snap.contains("unchanged 11"), "{snap:?}");
        assert!(!snap.contains("unchanged 40"), "{snap:?}");
        assert!(snap.contains("unchanged 70"), "{snap:?}");
        assert!(!snap.contains("… +"), "{snap:?}");
    }

    #[test]
    fn verbose_multi_edit_expands_long_unchanged_span() {
        let snap = render_sparse_multi_edit(ToolOutputVerbosity::Verbose).join("\n");

        assert!(!snap.contains("lines hidden"), "{snap:?}");
        assert!(snap.contains("unchanged 40"), "{snap:?}");
        assert!(snap.contains("changed at start"), "{snap:?}");
        assert!(snap.contains("changed at end"), "{snap:?}");
    }

    #[test]
    fn inline_edit_preview_keeps_full_line_and_highlights_changed_word() {
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(StreamingToolUse {
            call_id: "tool-edit".into(),
            tool_name: "Edit".into(),
            kind: ToolKind::Edit,
            status: ToolCallStatus::Completed,
            title: None,
            content: Some(vec![ToolCallContent::Diff(DiffContent {
                path: "src/agents.rs".into(),
                old_text: Some("claude-code-guide".into()),
                new_text: "rebon-code-guide".into(),
            })]),
            locations: None,
            raw_input: Some(HashMap::from([(
                "file_path".into(),
                json!("src/agents.rs"),
            )])),
            raw_output: Some(HashMap::from([
                ("type".into(), json!("update")),
                (
                    "originalFile".into(),
                    json!("pub const BUILTINS: &[(&str, &str)] = &[\n    (\"claude-code-guide\", \"文档问答（含 Web）\"),\n];"),
                ),
            ])),
        });

        let mut buf = new_buf(100, 10);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 100, 10),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );

        let removed_row = (0..buf.area().height)
            .find(|&y| row_text(&buf, y).contains("claude-code-guide"))
            .expect("removed full line should be rendered");
        let added_row = (0..buf.area().height)
            .find(|&y| row_text(&buf, y).contains("rebon-code-guide"))
            .expect("added full line should be rendered");
        let removed_text = row_text(&buf, removed_row);
        let changed_x = removed_text.find("claude").expect("changed word") as u16;
        let common_x = removed_text.find("-code-guide").expect("common suffix") as u16;

        assert!(
            removed_text.contains("(\"claude-code-guide\", \""),
            "{removed_text:?}"
        );
        assert!(removed_text.ends_with("\"),"), "{removed_text:?}");
        let added_text = row_text(&buf, added_row);
        assert!(
            added_text.contains("(\"rebon-code-guide\", \""),
            "{added_text:?}"
        );
        assert!(added_text.ends_with("\"),"), "{added_text:?}");
        assert_ne!(
            buf[(changed_x, removed_row)].style().bg,
            buf[(common_x, removed_row)].style().bg,
            "changed word should use the inline removal highlight"
        );
    }

    #[test]
    fn completed_edit_with_diff_keeps_update_header() {
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(StreamingToolUse {
            call_id: "tool-edit".into(),
            tool_name: "Edit".into(),
            kind: ToolKind::Edit,
            status: ToolCallStatus::Completed,
            title: None,
            content: Some(vec![ToolCallContent::Diff(DiffContent {
                path: "src/lib.rs".into(),
                old_text: Some("old".into()),
                new_text: "new".into(),
            })]),
            locations: None,
            raw_input: Some(HashMap::from([("file_path".into(), json!("src/lib.rs"))])),
            raw_output: Some(HashMap::from([
                ("type".into(), json!("update")),
                ("originalFile".into(), json!("old")),
            ])),
        });

        let mut buf = new_buf(80, 8);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 80, 8),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );
        let snap = normalized_visible_rows(&buf).join("\n");

        assert!(snap.contains("● Update(src/lib.rs)"), "{snap:?}");
        assert_text_bold(&buf, 0, "Update", true);
        assert_text_bold(&buf, 0, "(", false);
        assert!(!snap.contains("● Editing"), "{snap:?}");
    }
}
