use rebon_design_system::shortcut_hint::format_shortcut_hint;

use super::ToolOutputVerbosity;

/// Number of body lines to surface under a tool-use header in
/// `Compact` and `Normal` modes before folding the remainder behind a
/// `… +N lines` hint. Shell output is first split into terminal-width
/// visual lines so one huge logical line cannot bypass the cap.
pub(super) const TOOL_PREVIEW_MAX_LINES: usize = 4;

pub(super) fn hard_wrap_text_lines(text: &[String], width: u16) -> Vec<String> {
    use rebon_width::WidthChar;

    let width = width.max(1) as usize;
    let mut lines = Vec::new();
    for logical in text.iter().flat_map(|text| text.split('\n')) {
        let mut current = String::new();
        let mut current_width = 0usize;
        for ch in logical.chars() {
            let ch_width = ch.width().unwrap_or(0);
            if current_width > 0 && current_width.saturating_add(ch_width) > width {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(ch);
            current_width = current_width.saturating_add(ch_width);
        }
        lines.push(current);
    }
    lines
}

fn push_tail_line(
    lines: &mut std::collections::VecDeque<String>,
    current: &mut String,
    limit: usize,
    hidden: &mut bool,
) {
    if limit == 0 {
        current.clear();
        *hidden = true;
        return;
    }
    if lines.len() == limit {
        let mut recycled = lines.pop_front().expect("tail is at capacity");
        std::mem::swap(&mut recycled, current);
        current.clear();
        lines.push_back(recycled);
        *hidden = true;
    } else {
        lines.push_back(std::mem::take(current));
    }
}

fn hard_wrap_text_tail_lines(text: &[String], width: u16, limit: usize) -> (Vec<String>, bool) {
    use rebon_width::WidthChar;

    let width = width.max(1) as usize;
    let mut lines = std::collections::VecDeque::with_capacity(limit);
    let mut current = String::new();
    let mut hidden = false;
    for logical in text.iter().flat_map(|text| text.split('\n')) {
        current.clear();
        let mut current_width = 0usize;
        for ch in logical.chars() {
            let ch_width = ch.width().unwrap_or(0);
            if current_width > 0 && current_width.saturating_add(ch_width) > width {
                push_tail_line(&mut lines, &mut current, limit, &mut hidden);
                current_width = 0;
            }
            current.push(ch);
            current_width = current_width.saturating_add(ch_width);
        }
        push_tail_line(&mut lines, &mut current, limit, &mut hidden);
    }
    (lines.into_iter().collect(), hidden)
}

pub(super) fn visual_preview_lines(tool_name: &str, lines: &[String], width: u16) -> Vec<String> {
    if rebon_render::streaming::is_live_shell_tool(tool_name) || tool_name == "WebFetch" {
        hard_wrap_text_lines(lines, width)
    } else {
        lines.to_vec()
    }
}

pub(super) fn live_shell_preview_lines(lines: &[String], width: u16) -> (Vec<String>, bool) {
    let source_omitted = lines
        .first()
        .is_some_and(|line| line == rebon_render::streaming::LIVE_SHELL_OMITTED_MARKER);
    let source = if source_omitted { &lines[1..] } else { lines };
    let tail_limit = if source_omitted {
        TOOL_PREVIEW_MAX_LINES.saturating_sub(1)
    } else {
        TOOL_PREVIEW_MAX_LINES
    };
    let (mut wrapped, wrap_omitted) = hard_wrap_text_tail_lines(source, width, tail_limit);
    let hidden = source_omitted || wrap_omitted;
    if !hidden {
        return (wrapped, false);
    }

    if !source_omitted && wrapped.len() == TOOL_PREVIEW_MAX_LINES {
        wrapped.remove(0);
    }
    let marker = hard_wrap_text_lines(
        &[rebon_render::streaming::LIVE_SHELL_OMITTED_MARKER.to_string()],
        width,
    )
    .into_iter()
    .next()
    .unwrap_or_else(|| "…".to_string());
    let mut preview = Vec::with_capacity(1 + wrapped.len());
    preview.push(marker);
    preview.extend(wrapped);
    (preview, true)
}

pub(super) fn tool_content_preview_lines(
    all_content: &[String],
    tool_name: &str,
    verbosity: ToolOutputVerbosity,
    content_width: u16,
) -> Vec<String> {
    let preview_content = if matches!(verbosity, ToolOutputVerbosity::Verbose) {
        all_content.to_vec()
    } else {
        visual_preview_lines(tool_name, all_content, content_width)
    };
    let content_limit = match verbosity {
        ToolOutputVerbosity::Compact | ToolOutputVerbosity::Normal => Some(TOOL_PREVIEW_MAX_LINES),
        ToolOutputVerbosity::Verbose => None,
    };
    let visible_content = content_limit.map_or(preview_content.len(), |limit| {
        preview_content.len().min(limit)
    });
    let mut lines = preview_content
        .iter()
        .take(visible_content)
        .cloned()
        .collect::<Vec<_>>();
    if let Some(limit) = content_limit {
        if preview_content.len() > limit {
            let remaining = preview_content.len() - limit;
            let suffix = if matches!(verbosity, ToolOutputVerbosity::Compact) {
                format!(
                    " ({})",
                    format_shortcut_hint("ctrl+o", "expand", false, false).plain_text
                )
            } else {
                String::new()
            };
            lines.push(format!("… +{remaining} lines{suffix}"));
        }
    }
    lines
}

pub(super) fn normalize_tool_body_lines(
    mut lines: Vec<String>,
    preserve_internal_blanks: bool,
) -> Vec<String> {
    // Trim purely-empty trailing lines so a source that ends with a
    // newline does not waste a preview slot on blank space.
    while lines.last().map(|s| s.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }
    if !preserve_internal_blanks {
        lines.retain(|line| !line.trim().is_empty());
    }
    lines
}

pub(super) fn compact_preview_lines(
    all: &[String],
    tool_name: &str,
) -> (Vec<String>, Option<String>) {
    let preview_count = all.len().min(TOOL_PREVIEW_MAX_LINES);
    let preview = if tool_name == "Agent" {
        all.iter().rev().take(preview_count).cloned().collect()
    } else {
        all.iter().take(preview_count).cloned().collect()
    };
    let hint = if all.len() > preview_count {
        let remaining = all.len() - preview_count;
        Some(if tool_name == "Agent" {
            format!("… +{remaining} older lines")
        } else {
            format!(
                "… +{remaining} lines ({})",
                format_shortcut_hint("ctrl+o", "expand", false, false).plain_text
            )
        })
    } else {
        None
    };
    (preview, hint)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ratatui::{buffer::Buffer, layout::Rect};
    use rebon_types::{ContentBlock, ToolCallContent, ToolCallStatus, ToolKind};

    use super::super::{render_streaming_overlay, RenderTheme, ToolOutputVerbosity};
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

    fn trim_blank_boundaries(rows: Vec<String>) -> Vec<String> {
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

    fn all_rows(buf: &Buffer) -> Vec<String> {
        (0..buf.area().height).map(|y| row_text(buf, y)).collect()
    }

    fn semantic_rows(buf: &Buffer) -> Vec<String> {
        trim_blank_boundaries(all_rows(buf))
    }

    fn assert_no_adjacent_blank_rows(rows: &[String]) {
        for pair in rows.windows(2) {
            assert!(
                !(pair[0].trim().is_empty() && pair[1].trim().is_empty()),
                "adjacent semantic blank rows found: {rows:?}"
            );
        }
    }

    fn fake_streaming_tool(call_id: &str, tool_name: &str, path: &str) -> StreamingToolUse {
        let mut raw_input = HashMap::new();
        raw_input.insert("path".into(), serde_json::json!(path));
        StreamingToolUse {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            kind: ToolKind::Other,
            status: ToolCallStatus::Completed,
            title: Some(path.into()),
            content: None,
            locations: None,
            raw_input: Some(raw_input),
            raw_output: None,
        }
    }

    fn live_shell_overlay(tool_name: &str, lines: usize) -> StreamingOverlay {
        let mut tool = fake_streaming_tool("shell-1", tool_name, "ignored");
        tool.kind = ToolKind::Execute;
        tool.status = ToolCallStatus::InProgress;
        tool.title = None;
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(tool);
        for index in 0..lines {
            overlay.update_streaming_tool_use(
                "shell-1",
                Some(ToolCallStatus::InProgress),
                None,
                Some(vec![ToolCallContent::Content(
                    rebon_types::RegularContent {
                        content: ContentBlock::Text(rebon_types::TextContent {
                            text: format!("live-line-{index:02}"),
                            annotations: None,
                        }),
                    },
                )]),
                None,
                None,
            );
        }
        overlay
    }

    #[test]
    fn compact_and_normal_show_live_shell_tail_while_verbose_shows_all() {
        for tool_name in ["Bash", "PowerShell"] {
            let overlay = live_shell_overlay(tool_name, 12);

            for verbosity in [ToolOutputVerbosity::Compact, ToolOutputVerbosity::Normal] {
                let mut buf = new_buf(80, 12);
                render_streaming_overlay(
                    &overlay,
                    Rect::new(0, 0, 80, 12),
                    &mut buf,
                    &RenderTheme::plain(),
                    verbosity,
                );
                let rendered = semantic_rows(&buf).join("\n");
                assert!(rendered.contains("older live output omitted"), "{rendered}");
                assert!(rendered.contains("live-line-11"), "{rendered}");
                assert!(!rendered.contains("live-line-00"), "{rendered}");
            }

            let mut buf = new_buf(80, 24);
            render_streaming_overlay(
                &overlay,
                Rect::new(0, 0, 80, 24),
                &mut buf,
                &RenderTheme::plain(),
                ToolOutputVerbosity::Verbose,
            );
            let rendered = semantic_rows(&buf).join("\n");
            assert!(rendered.contains("live-line-00"), "{rendered}");
            assert!(rendered.contains("live-line-11"), "{rendered}");
            assert!(
                !rendered.contains("older live output omitted"),
                "{rendered}"
            );
        }
    }

    #[test]
    fn verbose_live_shell_keeps_identical_progress_lines() {
        for tool_name in ["Bash", "PowerShell"] {
            let mut overlay = live_shell_overlay(tool_name, 0);
            for _ in 0..2 {
                assert!(overlay.update_streaming_tool_use(
                    "shell-1",
                    Some(ToolCallStatus::InProgress),
                    None,
                    Some(vec![ToolCallContent::Content(
                        rebon_types::RegularContent {
                            content: ContentBlock::Text(rebon_types::TextContent {
                                text: "same-live-line".into(),
                                annotations: None,
                            }),
                        },
                    )]),
                    None,
                    None,
                ));
            }

            let mut buf = new_buf(80, 12);
            render_streaming_overlay(
                &overlay,
                Rect::new(0, 0, 80, 12),
                &mut buf,
                &RenderTheme::plain(),
                ToolOutputVerbosity::Verbose,
            );
            let rendered = semantic_rows(&buf).join("\n");
            assert_eq!(rendered.matches("same-live-line").count(), 2, "{rendered}");
        }
    }

    #[test]
    fn web_fetch_single_line_preview_wraps_before_line_cap() {
        let content = vec!["x".repeat(300)];
        let preview = super::tool_content_preview_lines(
            &content,
            "WebFetch",
            ToolOutputVerbosity::Normal,
            20,
        );

        assert_eq!(preview.len(), super::TOOL_PREVIEW_MAX_LINES + 1);
        assert!(preview[..super::TOOL_PREVIEW_MAX_LINES]
            .iter()
            .all(|line| line.chars().count() <= 20));
        assert_eq!(preview.last().map(String::as_str), Some("… +11 lines"));
    }

    #[test]
    fn streaming_web_fetch_uses_content_preview_for_direct_and_deferred_calls() {
        for (tool_name, raw_input) in [
            (
                "WebFetch",
                HashMap::from([("url".into(), serde_json::json!("https://example.com"))]),
            ),
            (
                "InvokeDeferredTool",
                HashMap::from([
                    ("tool_name".into(), serde_json::json!("WebFetch")),
                    (
                        "arguments".into(),
                        serde_json::json!({"url": "https://example.com"}),
                    ),
                ]),
            ),
        ] {
            let mut tool = fake_streaming_tool("web-fetch-1", tool_name, "ignored");
            tool.title = None;
            tool.raw_input = Some(raw_input);
            tool.raw_output = Some(HashMap::from([
                (
                    "content".into(),
                    serde_json::json!("one\ntwo\nthree\nfour\nfive"),
                ),
                ("status".into(), serde_json::json!(200)),
            ]));

            let mut buf = new_buf(80, 12);
            render_streaming_overlay(
                &StreamingOverlay::from_blocks(vec![StreamingContentBlock::ToolUse(tool)]),
                Rect::new(0, 0, 80, 12),
                &mut buf,
                &RenderTheme::plain(),
                ToolOutputVerbosity::Normal,
            );
            let rendered = semantic_rows(&buf).join("\n");
            assert!(rendered.contains("one"), "{tool_name}: {rendered}");
            assert!(rendered.contains("… +1 lines"), "{tool_name}: {rendered}");
            assert!(
                !rendered.contains("\\\"content\\\""),
                "{tool_name}: {rendered}"
            );
        }
    }

    #[test]
    fn shell_long_single_line_is_capped_by_visual_rows() {
        let theme = RenderTheme::plain();
        let final_output = format!("FINAL-{}-TAIL", "x".repeat(300));
        let live_output = format!("LIVE-{}", "y".repeat(300));

        for verbosity in [ToolOutputVerbosity::Compact, ToolOutputVerbosity::Normal] {
            let mut tool = fake_streaming_tool("t1", "Bash", "ignored");
            tool.title = None;
            tool.content = Some(vec![ToolCallContent::Content(
                rebon_types::RegularContent {
                    content: ContentBlock::Text(rebon_types::TextContent {
                        text: live_output.clone(),
                        annotations: None,
                    }),
                },
            )]);
            tool.raw_output = Some(HashMap::from([
                ("stdout".into(), serde_json::json!(final_output.clone())),
                ("stderr".into(), serde_json::json!("")),
            ]));

            let mut buf = new_buf(40, 16);
            render_streaming_overlay(
                &StreamingOverlay::from_blocks(vec![StreamingContentBlock::ToolUse(tool)]),
                Rect::new(0, 0, 40, 16),
                &mut buf,
                &theme,
                verbosity,
            );

            let rows = semantic_rows(&buf);
            let snap = rows.join("\n");
            assert_eq!(rows.len(), 6, "{verbosity:?}: {rows:?}");
            assert!(snap.contains("FINAL-"), "{verbosity:?}: {snap:?}");
            assert!(!snap.contains("LIVE-"), "{verbosity:?}: {snap:?}");
            assert!(!snap.contains("-TAIL"), "{verbosity:?}: {snap:?}");
            assert!(snap.contains("… +5 lines"), "{verbosity:?}: {snap:?}");
            assert_eq!(
                snap.contains("Ctrl+O to expand"),
                matches!(verbosity, ToolOutputVerbosity::Compact),
                "{verbosity:?}: {snap:?}"
            );
        }
    }

    #[test]
    fn verbose_shell_output_keeps_full_final_stream_without_live_duplicate() {
        let theme = RenderTheme::plain();
        let mut tool = fake_streaming_tool("t1", "Bash", "ignored");
        tool.content = Some(vec![ToolCallContent::Content(
            rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: format!("LIVE-{}", "y".repeat(120)),
                    annotations: None,
                }),
            },
        )]);
        tool.raw_output = Some(HashMap::from([
            (
                "stdout".into(),
                serde_json::json!(format!("FINAL-{}-TAIL", "x".repeat(120))),
            ),
            ("stderr".into(), serde_json::json!("")),
        ]));

        let mut buf = new_buf(40, 12);
        render_streaming_overlay(
            &StreamingOverlay::from_blocks(vec![StreamingContentBlock::ToolUse(tool)]),
            Rect::new(0, 0, 40, 12),
            &mut buf,
            &theme,
            ToolOutputVerbosity::Verbose,
        );

        let snap = semantic_rows(&buf).join("\n");
        assert!(snap.contains("FINAL-"), "{snap:?}");
        assert!(snap.contains("-TAIL"), "{snap:?}");
        assert!(!snap.contains("LIVE-"), "{snap:?}");
        assert!(!snap.contains("Ctrl+O to expand"), "{snap:?}");
    }

    #[test]
    fn compact_tool_preview_filters_blanks_and_uses_single_hint_row() {
        let theme = RenderTheme::plain();
        let mut tool = fake_streaming_tool("t1", "Bash", "ignored");
        tool.content = Some(vec![ToolCallContent::Content(
            rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "one\n\n two \nthree\nfour\nfive\nsix".into(),
                    annotations: None,
                }),
            },
        )]);
        let mut buf = new_buf(80, 12);
        render_streaming_overlay(
            &StreamingOverlay::from_blocks(vec![StreamingContentBlock::ToolUse(tool)]),
            Rect::new(0, 0, 80, 12),
            &mut buf,
            &theme,
            ToolOutputVerbosity::Compact,
        );
        let rows = semantic_rows(&buf);
        assert_eq!(
            rows,
            vec![
                "● Bash (ignored)".to_string(),
                "⎿ one".to_string(),
                "   two".to_string(),
                "  three".to_string(),
                "  four".to_string(),
                "  … +2 lines (Ctrl+O to expand)".to_string(),
            ]
        );
        assert_eq!(
            rows.iter()
                .filter(|r| r.to_lowercase().contains("ctrl+o"))
                .count(),
            1,
            "{rows:?}"
        );
        assert_no_adjacent_blank_rows(&rows);
    }
}
