use super::super::*;
use super::common::*;
use crate::message::{
    AssistantMessage, AssistantMessageInner, AssistantRedactedThinkingBlock, AssistantRole,
    AssistantTextBlock, AssistantThinkingBlock, AssistantToolUseBlock,
};
use ratatui::style::Color;
use rebon_types::ToolCallLocation;
use serde_json::json;

fn hyperlink_cells(buf: &Buffer, target: &str) -> Vec<(u16, u16)> {
    let area = buf.area();
    let mut cells = Vec::new();
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if buf[(x, y)].hyperlink() == Some(target) {
                cells.push((x, y));
            }
        }
    }
    cells
}

#[test]
fn hidden_info_system_message_consumes_zero_transcript_rows() {
    let mut buf = new_buf(40, 4);
    let used = render_message(
        &system_info("s-provider", "Switched provider"),
        Rect::new(0, 0, 40, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    assert_eq!(used, 0);
    assert_eq!(row_text(&buf, 0), "");
}

#[test]
fn user_prompt_renders_with_prompt_gutter() {
    let mut buf = new_buf(40, 4);
    let used = render_message(
        &user("u1", "please help"),
        Rect::new(0, 0, 40, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    assert!(used > 0);
    let snap = all_text(&buf);
    assert!(snap.contains("❯"), "expected USR gutter: {snap:?}");
    assert!(snap.contains("please help"), "expected content: {snap:?}");
}

#[test]
fn user_prompt_paints_message_background_only_under_content() {
    rebon_design_system::theme::set_active_theme(rebon_design_system::theme::ThemeName::Light);
    let mut buf = new_buf(40, 4);
    render_message(
        &user("u1", "please help"),
        Rect::new(0, 0, 40, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );

    assert_eq!(buf[(0, 0)].bg, Color::Reset);
    assert_eq!(buf[(0, 1)].bg, Color::Rgb(240, 240, 240));
    assert_eq!(buf[(2, 1)].bg, Color::Rgb(240, 240, 240));
    assert_eq!(buf[(39, 1)].bg, Color::Rgb(240, 240, 240));
    assert_eq!(buf[(0, 2)].bg, Color::Reset);
    assert_eq!(buf[(2, 2)].bg, Color::Reset);

    rebon_design_system::theme::set_active_theme(rebon_design_system::theme::ThemeName::Dark);
}

#[test]
fn assistant_text_renders_with_bullet_gutter() {
    let mut buf = new_buf(40, 4);
    let used = render_message(
        &assistant_text("a1", "answer"),
        Rect::new(0, 0, 40, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    assert!(used > 0);
    let snap = all_text(&buf);
    assert!(snap.contains("●"), "expected AST gutter: {snap:?}");
    assert!(snap.contains("answer"), "expected content: {snap:?}");
}

#[test]
fn math_rendering_defaults_off_and_preserves_literal_source() {
    let mut buf = new_buf(48, 6);
    render_message(
        &assistant_text("a1", "Euler: $e^{i\\pi}+1=0$"),
        Rect::new(0, 0, 48, 6),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );

    let snap = all_text(&buf);
    assert!(snap.contains("$e^{i\\pi}+1=0$"), "{snap:?}");
    assert!(!snap.contains('\u{2580}') && !snap.contains('\u{2584}'));
    assert!(!snap.contains('\u{1b}'));
}

#[test]
fn unicode_math_rendering_replaces_source_with_portable_half_blocks() {
    let mut theme = RenderTheme::plain();
    theme.math_display = MathDisplayMode::Unicode;
    let mut buf = new_buf(48, 8);
    let used = render_message(
        &assistant_text("a1", "Euler: $e^{i\\pi}+1=0$"),
        Rect::new(0, 0, 48, 8),
        &mut buf,
        &theme,
        ToolOutputVerbosity::Verbose,
    );

    let snap = all_text(&buf);
    assert!(used > 0, "{snap:?}");
    assert!(!snap.contains("$e^{i\\pi}+1=0$"), "{snap:?}");
    assert!(
        snap.contains('\u{2580}') || snap.contains('\u{2584}') || snap.contains('\u{2588}'),
        "{snap:?}"
    );
}

#[test]
fn native_math_graphics_overlay_is_emitted_only_in_graphics_mode() {
    let mut theme = RenderTheme::plain();
    theme.math_display = MathDisplayMode::Graphics {
        protocol: MathGraphicsProtocol::Kitty,
        cell_size: (8, 16),
    };
    let mut buf = new_buf(48, 8);
    render_message(
        &assistant_text("a1", "Euler: $e^{i\\pi}+1=0$"),
        Rect::new(0, 0, 48, 8),
        &mut buf,
        &theme,
        ToolOutputVerbosity::Verbose,
    );

    assert!(
        buf.content
            .iter()
            .any(|cell| cell.symbol().contains('\u{1b}')),
        "expected a native Kitty protocol cell"
    );
}

#[test]
fn unicode_math_rendering_keeps_unclosed_source_visible() {
    let mut theme = RenderTheme::plain();
    theme.math_display = MathDisplayMode::Unicode;
    let mut buf = new_buf(48, 6);
    render_message(
        &assistant_text("a1", "unfinished $x^2"),
        Rect::new(0, 0, 48, 6),
        &mut buf,
        &theme,
        ToolOutputVerbosity::Verbose,
    );

    assert!(all_text(&buf).contains("unfinished $x^2"));
}

#[test]
fn assistant_markdown_inline_styles_use_accent_without_changing_body_text() {
    let mut buf = new_buf(48, 4);
    render_message(
        &assistant_text("a1", "plain `code` [docs](https://example.com)"),
        Rect::new(0, 0, 48, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );

    let row = row_text(&buf, 1);
    let cell_x = |needle: &str| {
        let byte = row
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?}: {row:?}"));
        rebon_width::str_width(&row[..byte]) as u16
    };
    let plain = buf[(cell_x("plain"), 1)].style();
    let code = buf[(cell_x("code"), 1)].style();
    let docs = buf[(cell_x("docs"), 1)].style();
    assert_ne!(code.fg, plain.fg);
    assert_eq!(code.fg, docs.fg);
    assert!(!plain.add_modifier.contains(Modifier::UNDERLINED));
    assert!(docs.add_modifier.contains(Modifier::UNDERLINED));
}

#[test]
fn committed_markdown_links_use_labels_and_osc8() {
    let mut theme = RenderTheme::plain();
    theme.supports_hyperlinks = true;
    let mut buf = new_buf(48, 5);
    render_message(
        &assistant_text(
            "a1",
            "See [the docs](https://example.com/docs) and https://example.com/bare.",
        ),
        Rect::new(0, 0, 48, 5),
        &mut buf,
        &theme,
        ToolOutputVerbosity::Verbose,
    );

    let rendered = all_text(&buf);
    assert!(rendered.contains("See the docs and https://example.com/bare."));
    assert!(!rendered.contains("(https://example.com/docs)"));
    assert!(!hyperlink_cells(&buf, "https://example.com/docs").is_empty());
    assert!(!hyperlink_cells(&buf, "https://example.com/bare").is_empty());
    assert!(!rendered.contains('\x1b'));
}

#[test]
fn wrapped_markdown_link_patches_each_visible_row() {
    let mut theme = RenderTheme::plain();
    theme.supports_hyperlinks = true;
    let mut buf = new_buf(9, 5);
    render_message(
        &assistant_text("a1", "[abcdefghij](https://example.com/long)"),
        Rect::new(0, 0, 9, 5),
        &mut buf,
        &theme,
        ToolOutputVerbosity::Verbose,
    );

    let cells = hyperlink_cells(&buf, "https://example.com/long");
    let rows = cells
        .iter()
        .map(|(_, y)| *y)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(rows.len(), 2, "wrapped link cells: {cells:?}");
    assert!(!all_text(&buf).contains('\x1b'));
}

#[test]
fn markdown_link_wrapping_handles_cjk_emoji_and_clipping() {
    let mut theme = RenderTheme::plain();
    theme.supports_hyperlinks = true;
    let message = assistant_text("a1", "[中文🙂abc](https://example.com/unicode)");

    let mut full = new_buf(7, 4);
    render_message_inner(
        &message,
        Rect::new(0, 0, 7, 4),
        &mut full,
        &theme,
        ToolOutputVerbosity::Verbose,
        false,
    );
    let rendered = all_text(&full);
    let cells = hyperlink_cells(&full, "https://example.com/unicode");
    let rows = cells
        .iter()
        .map(|(_, y)| *y)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(rows.len() >= 2, "unicode link should wrap: {cells:?}");
    let visible = rendered
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>();
    assert_eq!(visible, "●中文🙂abc");

    let mut clipped = new_buf(7, 1);
    render_message_inner(
        &message,
        Rect::new(0, 0, 7, 1),
        &mut clipped,
        &theme,
        ToolOutputVerbosity::Verbose,
        false,
    );
    let cells = hyperlink_cells(&clipped, "https://example.com/unicode");
    let rows = cells
        .iter()
        .map(|(_, y)| *y)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(rows.len(), 1);
}

#[test]
fn unsafe_markdown_link_target_never_reaches_osc8() {
    let mut theme = RenderTheme::plain();
    theme.supports_hyperlinks = true;
    let mut buf = new_buf(40, 4);
    render_message(
        &assistant_text("a1", "[bad](javascript:alert%281%29)"),
        Rect::new(0, 0, 40, 4),
        &mut buf,
        &theme,
        ToolOutputVerbosity::Verbose,
    );

    let rendered = all_text(&buf);
    assert!(hyperlink_cells(&buf, "javascript:alert%281%29").is_empty());
    assert!(buf.content().iter().all(|cell| cell.hyperlink().is_none()));
    assert!(rendered.contains("bad"));
}

#[test]
fn committed_and_streaming_markdown_links_match() {
    let mut theme = RenderTheme::plain();
    theme.supports_hyperlinks = true;
    let message = assistant_text(
        "a1",
        "Open [docs](https://example.com/docs) or https://example.com/bare",
    );
    let mut committed = new_buf(44, 5);
    let mut streaming = new_buf(44, 5);

    let committed_height = render_message_inner(
        &message,
        Rect::new(0, 0, 44, 5),
        &mut committed,
        &theme,
        ToolOutputVerbosity::Verbose,
        false,
    );
    let streaming_height = render_streaming_message(
        &message,
        Rect::new(0, 0, 44, 5),
        &mut streaming,
        &theme,
        ToolOutputVerbosity::Verbose,
        false,
        StreamingOverlayRenderMode::Paint,
        None,
    );

    assert_eq!(committed_height, streaming_height);
    assert_eq!(all_text(&committed), all_text(&streaming));
    assert_eq!(
        hyperlink_cells(&committed, "https://example.com/docs"),
        hyperlink_cells(&streaming, "https://example.com/docs")
    );
    assert_eq!(
        hyperlink_cells(&committed, "https://example.com/bare"),
        hyperlink_cells(&streaming, "https://example.com/bare")
    );
}

/// Bash stdout wrapper is classified by rebon-render's user text
/// projection and rendered accordingly.
#[test]
fn user_bash_stdout_wrapper_collapses_via_inner_dispatch() {
    let mut buf = new_buf(40, 4);
    let msg = user("u1", "<bash-stdout>lots of output</bash-stdout>");
    let used = render_message(
        &msg,
        Rect::new(0, 0, 40, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    assert!(used > 0);
    let snap = all_text(&buf);
    // The raw wrapper tags must not leak into the output.
    assert!(!snap.contains("<bash-stdout"), "raw tags leaked: {snap:?}");
}

#[test]
fn committed_shell_output_respects_output_verbosity() {
    for name in ["Bash", "PowerShell"] {
        let tool = AssistantToolUseBlock {
            id: format!("{name}-1"),
            name: name.into(),
            input: json!({"command": "emit lines"}),
            tool_call_content: None,
            raw_output: Some(json!({
                "stdout": "one\ntwo\nthree\nfour\nfive\nsix"
            })),
            title: None,
            locations: None,
            status: None,
        };

        assert_eq!(
            collect_tool_block_body_lines(&tool, ToolOutputVerbosity::Compact),
            vec![
                "one",
                "two",
                "three",
                "four",
                "… +2 lines (Ctrl+O to expand)",
            ],
            "{name} compact output"
        );
        assert_eq!(
            collect_tool_block_body_lines(&tool, ToolOutputVerbosity::Normal),
            vec!["one", "two", "three", "four", "… +2 lines"],
            "{name} normal output"
        );
        assert_eq!(
            collect_tool_block_body_lines(&tool, ToolOutputVerbosity::Verbose),
            vec!["one", "two", "three", "four", "five", "six"],
            "{name} verbose output"
        );
    }
}

#[test]
fn committed_web_fetch_renders_content_instead_of_raw_json() {
    let tool = AssistantToolUseBlock {
        id: "web-fetch-1".into(),
        name: "WebFetch".into(),
        input: json!({"url": "https://example.com"}),
        tool_call_content: None,
        raw_output: Some(json!({
            "url": "https://example.com",
            "status": 200,
            "content": "one\ntwo\nthree\nfour\nfive",
            "total_chars": 23,
            "truncated": true,
            "next_offset": 20,
        })),
        title: None,
        locations: None,
        status: None,
    };

    assert_eq!(
        collect_tool_block_body_lines(&tool, ToolOutputVerbosity::Compact),
        vec![
            "one",
            "two",
            "three",
            "four",
            "… +1 lines (Ctrl+O to expand)",
        ]
    );
    assert_eq!(
        collect_tool_block_body_lines(&tool, ToolOutputVerbosity::Normal),
        vec!["one", "two", "three", "four", "… +1 lines"]
    );
    assert_eq!(
        collect_tool_block_body_lines(&tool, ToolOutputVerbosity::Verbose),
        vec!["one", "two", "three", "four", "five"]
    );
}

#[test]
fn committed_compact_preview_caps_all_detail_sources() {
    let tool = AssistantToolUseBlock {
        id: "grep-1".into(),
        name: "Grep".into(),
        input: json!({"pattern": "needle"}),
        tool_call_content: Some(vec![rebon_types::ToolCallContent::Content(
            rebon_types::RegularContent {
                content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: "match one\nmatch two".into(),
                    annotations: None,
                }),
            },
        )]),
        raw_output: Some(json!({"matches": 4})),
        title: None,
        locations: Some(
            ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"]
                .into_iter()
                .map(|path| ToolCallLocation {
                    path: path.into(),
                    line: None,
                })
                .collect(),
        ),
        status: Some(rebon_types::ToolCallStatus::Completed),
    };

    assert_eq!(
        collect_tool_block_body_lines(&tool, ToolOutputVerbosity::Compact),
        vec![
            "match one",
            "match two",
            "src/a.rs",
            "src/b.rs",
            "… +3 lines (Ctrl+O to expand)",
        ]
    );
    assert_eq!(
        collect_tool_block_body_lines(&tool, ToolOutputVerbosity::Normal),
        vec!["match one", "match two"]
    );
    assert_eq!(
        collect_tool_block_body_lines(&tool, ToolOutputVerbosity::Verbose),
        vec![
            "match one",
            "match two",
            "src/a.rs",
            "src/b.rs",
            "src/c.rs",
            "src/d.rs",
            "matches=4",
        ]
    );
}

#[test]
fn mixed_committed_shell_output_uses_compact_preview() {
    let output = "one\ntwo\nthree\nfour\nfive\nsix";
    for name in ["Bash", "PowerShell"] {
        let message = Message::Assistant(AssistantMessage {
            uuid: format!("mixed-{name}"),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![
                    AssistantContentBlock::Text(AssistantTextBlock {
                        text: "Running the command".into(),
                    }),
                    AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                        id: format!("{name}-1"),
                        name: name.into(),
                        input: json!({"command": "emit lines"}),
                        tool_call_content: Some(vec![rebon_types::ToolCallContent::Content(
                            rebon_types::RegularContent {
                                content: rebon_types::ContentBlock::Text(
                                    rebon_types::TextContent {
                                        text: output.into(),
                                        annotations: None,
                                    },
                                ),
                            },
                        )]),
                        raw_output: Some(json!({"stdout": output})),
                        title: None,
                        locations: None,
                        status: Some(rebon_types::ToolCallStatus::Completed),
                    }),
                ],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        });

        let mut compact_buf = new_buf(80, 16);
        render_message(
            &message,
            Rect::new(0, 0, 80, 16),
            &mut compact_buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );
        let compact = all_text(&compact_buf);
        assert!(compact.contains("four"), "{name}: {compact:?}");
        assert!(!compact.contains("five"), "{name}: {compact:?}");
        assert!(!compact.contains("six"), "{name}: {compact:?}");
        assert!(compact.contains("+2 lines"), "{name}: {compact:?}");
        assert!(compact.contains("Ctrl+O to expand"), "{name}: {compact:?}");

        let mut normal_buf = new_buf(80, 16);
        render_message(
            &message,
            Rect::new(0, 0, 80, 16),
            &mut normal_buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Normal,
        );
        let normal = all_text(&normal_buf);
        assert!(normal.contains("four"), "{name}: {normal:?}");
        assert!(!normal.contains("five"), "{name}: {normal:?}");
        assert!(!normal.contains("six"), "{name}: {normal:?}");
        assert!(normal.contains("+2 lines"), "{name}: {normal:?}");
        assert!(!normal.contains("Ctrl+O to expand"), "{name}: {normal:?}");

        let mut verbose_buf = new_buf(80, 16);
        render_message(
            &message,
            Rect::new(0, 0, 80, 16),
            &mut verbose_buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Verbose,
        );
        let verbose = all_text(&verbose_buf);
        assert!(verbose.contains("five"), "{name}: {verbose:?}");
        assert!(verbose.contains("six"), "{name}: {verbose:?}");
        assert!(!verbose.contains("+2 lines"), "{name}: {verbose:?}");
    }
}

#[test]
fn mixed_committed_shell_long_single_line_is_visually_bounded() {
    let final_output = format!("FINAL-{}-TAIL", "x".repeat(300));
    let live_output = format!("LIVE-{}", "y".repeat(300));
    let message = Message::Assistant(AssistantMessage {
        uuid: "mixed-long-bash".into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content: vec![
                AssistantContentBlock::Text(AssistantTextBlock {
                    text: "Running the command".into(),
                }),
                AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                    id: "bash-1".into(),
                    name: "Bash".into(),
                    input: json!({"command": "emit one long line"}),
                    tool_call_content: Some(vec![rebon_types::ToolCallContent::Content(
                        rebon_types::RegularContent {
                            content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                                text: live_output,
                                annotations: None,
                            }),
                        },
                    )]),
                    raw_output: Some(json!({"stdout": final_output, "stderr": ""})),
                    title: None,
                    locations: None,
                    status: Some(rebon_types::ToolCallStatus::Completed),
                }),
            ],
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    });

    for verbosity in [ToolOutputVerbosity::Compact, ToolOutputVerbosity::Normal] {
        let mut buf = new_buf(40, 24);
        let used = render_message(
            &message,
            Rect::new(0, 0, 40, 24),
            &mut buf,
            &RenderTheme::plain(),
            verbosity,
        );
        let snap = all_text(&buf);

        assert!(used < 12, "{verbosity:?} used {used} rows: {snap:?}");
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
fn pure_committed_failed_shell_long_single_line_is_visually_bounded() {
    let long_error = format!("FAIL-{}-TAIL", "x".repeat(300));

    for name in ["Bash", "PowerShell"] {
        let message = Message::Assistant(AssistantMessage {
            uuid: format!("pure-failed-{name}"),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                    id: format!("{name}-1"),
                    name: name.into(),
                    input: json!({"command": "emit one long error"}),
                    tool_call_content: None,
                    raw_output: Some(json!({"stderr": long_error})),
                    title: None,
                    locations: None,
                    status: Some(rebon_types::ToolCallStatus::Failed),
                })],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        });

        for verbosity in [ToolOutputVerbosity::Compact, ToolOutputVerbosity::Normal] {
            let mut buf = new_buf(40, 24);
            let used = render_message(
                &message,
                Rect::new(0, 0, 40, 24),
                &mut buf,
                &RenderTheme::plain(),
                verbosity,
            );
            let snap = all_text(&buf);

            assert!(used <= 7, "{name} {verbosity:?} used {used} rows: {snap:?}");
            assert!(snap.contains("FAIL-"), "{name} {verbosity:?}: {snap:?}");
            assert!(!snap.contains("-TAIL"), "{name} {verbosity:?}: {snap:?}");
            assert!(
                snap.contains("… +5 lines"),
                "{name} {verbosity:?}: {snap:?}"
            );
            assert_eq!(
                snap.matches("Ctrl+O to expand").count(),
                usize::from(matches!(verbosity, ToolOutputVerbosity::Compact)),
                "{name} {verbosity:?}: {snap:?}"
            );
        }
    }
}

#[test]
fn mixed_committed_edit_respects_output_verbosity() {
    let new_text = (1..=20)
        .map(|line| format!("added line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let message = Message::Assistant(AssistantMessage {
        uuid: "mixed-edit".into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content: vec![
                AssistantContentBlock::Text(AssistantTextBlock {
                    text: "Updating the file".into(),
                }),
                AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                    id: "edit-1".into(),
                    name: "Edit".into(),
                    input: json!({"file_path": "src/app.rs"}),
                    tool_call_content: Some(vec![rebon_types::ToolCallContent::Diff(
                        rebon_types::DiffContent {
                            path: "src/app.rs".into(),
                            old_text: None,
                            new_text,
                        },
                    )]),
                    raw_output: Some(json!({"type": "update"})),
                    title: None,
                    locations: None,
                    status: Some(rebon_types::ToolCallStatus::Completed),
                }),
            ],
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    });

    let mut compact_buf = new_buf(100, 40);
    let compact_used = render_message(
        &message,
        Rect::new(0, 0, 100, 40),
        &mut compact_buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let compact = all_text(&compact_buf);
    assert!(compact.contains("Updating the file"), "{compact:?}");
    assert!(compact.contains("Edit (src/app.rs)"), "{compact:?}");
    assert!(!compact.contains("added line 20"), "{compact:?}");
    assert!(compact_used < 10, "compact edit used {compact_used} rows");

    let mut normal_buf = new_buf(100, 40);
    render_message(
        &message,
        Rect::new(0, 0, 100, 40),
        &mut normal_buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Normal,
    );
    let normal = all_text(&normal_buf);
    assert!(normal.contains("added line 20"), "{normal:?}");

    let mut verbose_buf = new_buf(100, 40);
    render_message(
        &message,
        Rect::new(0, 0, 100, 40),
        &mut verbose_buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    let verbose = all_text(&verbose_buf);
    assert!(verbose.contains("added line 20"), "{verbose:?}");
}

///: unbalanced `<tick>` must render as a plain prompt,
/// not disappear as a tick.
#[test]
fn unbalanced_tick_open_tag_renders_as_prompt_not_hidden() {
    let mut buf = new_buf(40, 4);
    let msg = user("u1", "<tick>");
    let used = render_message(
        &msg,
        Rect::new(0, 0, 40, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    assert!(used > 0, "unbalanced <tick> must NOT render as zero-height");
}

/// Balanced `<tick>...</tick>` pair with non-empty content has its
/// body hidden by the rebon-render user text projection; the prompt
/// gutter still takes its rows.
#[test]
fn balanced_tick_pair_hides_its_content() {
    let mut buf = new_buf(40, 4);
    let msg = user("u1", "<tick>1</tick>");
    let used = render_message(
        &msg,
        Rect::new(0, 0, 40, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    assert_eq!(used, 2, "prompt gutter still occupies its rows");
    let snap = all_text(&buf);
    assert_eq!(row_text(&buf, 1), "❯", "{snap:?}");
    assert!(!snap.contains("tick"), "{snap:?}");
    assert!(!snap.contains('1'), "{snap:?}");
}

/// NO_CONTENT_MESSAGE user text has its body hidden; the prompt
/// gutter still takes its rows.
#[test]
fn no_content_user_text_body_is_hidden() {
    let mut buf = new_buf(40, 4);
    let msg = user("u1", crate::user_text::NO_CONTENT_MESSAGE);
    let used = render_message(
        &msg,
        Rect::new(0, 0, 40, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    assert_eq!(used, 2, "prompt gutter still occupies its rows");
    let snap = all_text(&buf);
    assert_eq!(row_text(&buf, 1), "❯", "{snap:?}");
}

/// Compact summary branch renders something visible.
#[test]
fn is_compact_summary_branch_fires_on_flag() {
    let mut msg = user("u1", "anything");
    if let Message::User(u) = &mut msg {
        u.is_compact_summary = Some(true);
    }
    let mut buf = new_buf(40, 4);
    let used = render_message(
        &msg,
        Rect::new(0, 0, 40, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    assert!(used > 0, "compact summary must render something");
}

/// Assistant thinking block renders the `thinking` field content.
#[test]
fn assistant_thinking_block_renders_thinking_field() {
    let msg = Message::Assistant(AssistantMessage {
        uuid: "a1".into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content: vec![AssistantContentBlock::Thinking(AssistantThinkingBlock {
                thinking: "reasoning step".into(),
                signature: None,
            })],
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    });
    let mut buf = new_buf(40, 6);
    render_message(
        &msg,
        Rect::new(0, 0, 40, 6),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    let snap = all_text(&buf);
    assert!(
        snap.contains("reasoning step"),
        "expected thinking body: {snap:?}"
    );
}

/// Thinking rows render only their title in compact mode.
#[test]
fn committed_thinking_compact_renders_title_only() {
    use crate::state::{reducer, Action};

    let mut state = AppState::new();
    reducer(
        &mut state,
        Action::Commit(assistant_thinking(
            "a1",
            "**Planning worker tasks**\nsecond thought\nthird thought",
        )),
    );
    let mut buf = new_buf(80, 8);
    let mut cache = TranscriptMeasureCache::new();
    render_transcript_cached_with_running_hints(
        &state,
        Rect::new(0, 0, 80, 8),
        &mut buf,
        &RenderTheme::plain(),
        usize::MAX,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        TranscriptRenderExtras {
            render_thinking_only_rows: true,
            ..TranscriptRenderExtras::empty()
        },
    );
    let snap = all_text(&buf);
    assert!(
        snap.contains("Planning worker tasks"),
        "title must be visible: {snap:?}"
    );
    assert!(
        !snap.contains("second thought") && !snap.contains("third thought"),
        "only the first thinking line should be visible: {snap:?}"
    );
    assert!(snap.contains("Ctrl+O to expand"), "{snap:?}");
}

#[test]
fn committed_thinking_verbose_renders_title_only() {
    let msg = Message::Assistant(AssistantMessage {
        uuid: "a1".into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content: vec![AssistantContentBlock::Thinking(AssistantThinkingBlock {
                thinking: "first thought\nsecond thought".into(),
                signature: None,
            })],
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    });
    let mut buf = new_buf(80, 8);
    render_message(
        &msg,
        Rect::new(0, 0, 80, 8),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    let snap = all_text(&buf);
    assert!(snap.contains("first thought"), "{snap:?}");
    assert!(snap.contains("second thought"), "{snap:?}");
    assert!(!snap.contains("Thinking"), "{snap:?}");
    assert!(!snap.contains("Ctrl+O to expand"), "{snap:?}");
}

/// Streaming overlay thinking renders only its title.
#[test]
fn streaming_thinking_compact_renders_header_line() {
    let mut overlay = StreamingOverlay::new();
    overlay
        .append_streaming_thinking("**Planning worker tasks**\nweighing option B\nchose option C");
    let mut buf = new_buf(80, 6);
    let used = render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 80, 6),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    assert!(used > 0);
    let snap = (0..6)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        snap.contains("Planning worker tasks"),
        "title must appear: {snap:?}"
    );
    assert!(
        !snap.contains("weighing option B") && !snap.contains("chose option C"),
        "streaming thinking body must be hidden: {snap:?}"
    );
    assert_eq!(
        snap.matches('·').count(),
        1,
        "streaming thinking should render only the thinking gutter, not a nested label: {snap:?}"
    );
    assert!(snap.contains("Ctrl+O to expand"), "{snap:?}");
}

#[test]
fn streaming_thinking_group_stacks_titles_and_expands_bodies() {
    let mut overlay = StreamingOverlay::new();
    overlay.append_streaming_thinking("**Planning files**\ninspect the repository");
    overlay.end_streaming_thinking();
    overlay.append_streaming_thinking("**Planning dependencies**\nstart task one");
    overlay.end_streaming_thinking();
    overlay.append_streaming_thinking("**Planning safe inspection**\navoid user changes");

    let mut compact_buf = new_buf(100, 10);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 10),
        &mut compact_buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let compact = all_text(&compact_buf);
    assert!(compact.contains("Reasoning (3 steps)"), "{compact:?}");
    assert!(compact.contains("├ Planning files"), "{compact:?}");
    assert!(compact.contains("├ Planning dependencies"), "{compact:?}");
    assert!(
        compact.contains("└ Planning safe inspection"),
        "{compact:?}"
    );
    assert!(!compact.contains("inspect the repository"), "{compact:?}");
    assert!(!compact.contains("TaskCreate"), "{compact:?}");
    assert!(!compact.contains("TaskUpdate"), "{compact:?}");
    assert_eq!(
        compact.matches("Ctrl+O to expand").count(),
        1,
        "{compact:?}"
    );

    let mut normal_buf = new_buf(100, 16);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 16),
        &mut normal_buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Normal,
    );
    let normal = all_text(&normal_buf);
    assert!(normal.contains("Reasoning (3 steps)"), "{normal:?}");
    assert!(normal.contains("inspect the repository"), "{normal:?}");
    assert!(normal.contains("start task one"), "{normal:?}");
    assert!(normal.contains("avoid user changes"), "{normal:?}");
    assert!(!normal.contains("Ctrl+O to expand"), "{normal:?}");
}

#[test]
fn streaming_hidden_tool_keeps_thinking_in_one_reasoning_group() {
    let mut overlay = StreamingOverlay::new();
    overlay.append_streaming_thinking("Updating task progress status");
    overlay.end_streaming_thinking();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "task-update-1",
        "TaskUpdate",
        ToolKind::Other,
        ToolCallStatus::Completed,
        vec![("taskId", json!("2")), ("status", json!("completed"))],
    ));
    overlay.append_streaming_thinking("Deploying docs and landing site");
    overlay.end_streaming_thinking();

    let mut buf = new_buf(100, 10);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 10),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = all_text(&buf);

    assert!(snap.contains("Reasoning (2 steps)"), "{snap:?}");
    assert!(snap.contains("├ Updating task progress status"), "{snap:?}");
    assert!(
        snap.contains("└ Deploying docs and landing site"),
        "{snap:?}"
    );
    assert_eq!(snap.matches("Ctrl+O to expand").count(), 1, "{snap:?}");
    assert!(!snap.contains("TaskUpdate"), "{snap:?}");
}

#[test]
fn streaming_reasoning_groups_only_adjacent_thinking_between_text_and_tools() {
    let mut overlay = StreamingOverlay::new();
    overlay.append_streaming_text("agent text before reasoning");
    overlay.append_streaming_thinking("**Inspecting first path**\nfirst detail");
    overlay.end_streaming_thinking();
    overlay.append_streaming_thinking("**Inspecting second path**\nsecond detail");
    overlay.end_streaming_thinking();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "grep-1",
        "Grep",
        ToolKind::Search,
        ToolCallStatus::Completed,
        vec![("pattern", json!("first"))],
    ));
    overlay.upsert_streaming_tool_use(streaming_tool(
        "grep-2",
        "Grep",
        ToolKind::Search,
        ToolCallStatus::Completed,
        vec![("pattern", json!("second"))],
    ));

    let mut buf = new_buf(100, 12);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 12),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let rows = (0..12).map(|y| row_text(&buf, y)).collect::<Vec<_>>();
    let text_row = rows
        .iter()
        .position(|row| row.contains("agent text before reasoning"))
        .unwrap();
    let header_row = rows
        .iter()
        .position(|row| row.contains("Reasoning (2 steps)"))
        .unwrap();
    let reasoning_end_row = rows
        .iter()
        .position(|row| row.contains("└ Inspecting second path"))
        .unwrap();
    let tool_row = rows
        .iter()
        .position(|row| row.contains("2 patterns"))
        .unwrap();

    assert!(text_row < header_row, "{rows:?}");
    assert_eq!(tool_row, reasoning_end_row + 2, "{rows:?}");
    assert!(rows[reasoning_end_row + 1].trim().is_empty(), "{rows:?}");
}

#[test]
fn streaming_thinking_compact_disappears_after_later_assistant_text() {
    let mut overlay = StreamingOverlay::new();
    overlay.append_streaming_thinking("**Latest thought**\nold thought");
    overlay.append_streaming_text("assistant answer after thinking");

    let mut buf = new_buf(100, 8);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 8),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..8)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        snap.contains("Latest thought") && snap.contains("assistant answer after thinking"),
        "live compact thinking title should remain visible before later text: {snap:?}"
    );
    assert!(
        !snap.contains("old thought"),
        "thinking body should stay hidden: {snap:?}"
    );
}

#[test]
fn streaming_thinking_hint_reuses_trailing_tool_hint() {
    let mut overlay = StreamingOverlay::new();
    overlay
        .append_streaming_thinking("**Calculating title row**\nthis way titles appear correctly");
    overlay.end_streaming_thinking();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t1",
        "Bash",
        ToolKind::Execute,
        ToolCallStatus::Completed,
        vec![("command", json!("cargo test"))],
    ));

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

    assert!(snap.contains("Calculating title row"), "{snap:?}");
    assert!(snap.contains("Bash"), "{snap:?}");
    assert_eq!(
        snap.matches("Ctrl+O to expand").count(),
        1,
        "thinking title should carry the compact expansion hint: {snap:?}"
    );
}

#[test]
fn assistant_redacted_thinking_shows_placeholder() {
    let msg = Message::Assistant(AssistantMessage {
        uuid: "a1".into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content: vec![AssistantContentBlock::RedactedThinking(
                AssistantRedactedThinkingBlock {
                    data: Some("opaque".into()),
                },
            )],
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    });
    let mut buf = new_buf(40, 4);
    render_message(
        &msg,
        Rect::new(0, 0, 40, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    let snap = all_text(&buf);
    // The opaque `data` must NOT leak into output.
    assert!(!snap.contains("opaque"), "redacted data leaked: {snap:?}");
}

#[test]
fn unknown_assistant_content_block_is_filtered() {
    // Deserialize a server_tool_use block, which lands in Other.
    // to_message_row filters out Other blocks, so the message
    // has an empty content array and renders as minimal.
    let msg: Message = serde_json::from_value(json!({
        "type": "assistant",
        "uuid": "a1",
        "timestamp": "t",
        "message": {
            "role": "assistant",
            "content": [
                { "type": "server_tool_use", "id": "x", "name": "Y", "input": {} }
            ]
        }
    }))
    .unwrap();
    let mut buf = new_buf(40, 4);
    let used = render_message(
        &msg,
        Rect::new(0, 0, 40, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    // Unknown blocks are filtered out by to_message_row, so nothing
    // from the server_tool_use block reaches the buffer.
    let snap = all_text(&buf);
    assert!(!snap.contains("server_tool_use"), "{snap:?}");
    assert!(!snap.contains('Y'), "{snap:?}");
    assert!(snap.trim().is_empty(), "{snap:?}");
    assert_eq!(used, 1, "the filtered message keeps only its spacing row");
}

#[test]
fn system_local_command_renders_content() {
    let msg: Message = serde_json::from_value(json!({
        "type": "system",
        "uuid": "s1",
        "timestamp": "t",
        "subtype": "local_command",
        "content": "!pwd\n⎿ Path\n  ----\n  C:\\project",
        "level": "info"
    }))
    .unwrap();
    let mut buf = new_buf(80, 6);
    render_message(
        &msg,
        Rect::new(0, 0, 80, 6),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    assert_eq!(
        normalized_visible_rows(&buf),
        vec![
            "❯ !pwd".to_string(),
            "⎿ Path".to_string(),
            "  ----".to_string(),
            "  C:\\project".to_string(),
        ]
    );
}

#[test]
fn system_microcompact_boundary_is_zero_height() {
    let msg: Message = serde_json::from_value(json!({
        "type": "system",
        "uuid": "s1",
        "timestamp": "t",
        "subtype": "microcompact_boundary"
    }))
    .unwrap();
    let mut buf = new_buf(40, 4);
    let used = render_message(
        &msg,
        Rect::new(0, 0, 40, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    // rebon-render hides microcompact_boundary; the fullscreen
    // gate may cause a 1-line stub instead of true zero-height.
    assert!(
        used <= 1,
        "microcompact should be hidden or minimal: {used}"
    );
}

#[test]
fn system_api_error_renders_content() {
    let msg: Message = serde_json::from_value(json!({
        "type": "system",
        "uuid": "s1",
        "timestamp": "t",
        "subtype": "api_error",
        "content": "overloaded",
        "level": "error"
    }))
    .unwrap();
    let mut buf = new_buf(40, 4);
    render_message(
        &msg,
        Rect::new(0, 0, 40, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    let snap = all_text(&buf);
    assert!(snap.contains("overloaded"), "expected content: {snap:?}");
}

#[test]
fn render_transcript_paints_rows_then_streaming_overlay() {
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(&mut s, Action::Commit(user("u1", "question?")));
    reducer(&mut s, Action::Commit(assistant_text("a1", "answer!")));
    reducer(&mut s, Action::SetStreamingText("in flight".into()));
    let mut buf = new_buf(40, 10);
    render_transcript(
        &s,
        Rect::new(0, 0, 40, 10),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Verbose,
        0,
        None,
    );
    let snap = all_text(&buf);
    let user_pos = snap.find("question?").expect("user content missing");
    let asst_pos = snap.find("answer!").expect("assistant content missing");
    let stream_pos = snap.find("in flight").expect("streaming content missing");
    assert!(user_pos < asst_pos, "user before assistant");
    assert!(asst_pos < stream_pos, "assistant before streaming");
}

#[test]
fn render_transcript_hides_divider_after_it_scrolls_off_top() {
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(&mut s, Action::Commit(assistant_text("a1", "alpha")));

    let mut buf = new_buf(40, 1);
    render_transcript(
        &s,
        Rect::new(0, 0, 40, 1),
        &mut buf,
        &RenderTheme::plain(),
        1,
        ToolOutputVerbosity::Verbose,
        0,
        Some(0),
    );

    let snap = all_text(&buf);
    assert!(
        !snap.contains(" new "),
        "divider should scroll away: {snap:?}"
    );
    assert!(
        snap.contains("alpha"),
        "message should remain visible: {snap:?}"
    );
}

#[test]
fn committed_background_shell_launch_renders_confirmation() {
    for name in ["Bash", "PowerShell"] {
        let tool = AssistantToolUseBlock {
            id: format!("background-{name}"),
            name: name.into(),
            input: json!({"command": "cargo test", "run_in_background": true}),
            tool_call_content: None,
            raw_output: Some(json!({
                "shellId": "sh_1",
                "tool": name,
                "command": "cargo test",
                "status": "running",
                "startedAtMs": 42,
                "completedAtMs": null,
                "timeoutMs": null,
                "exitCode": null
            })),
            title: None,
            locations: None,
            status: Some(ToolCallStatus::Completed),
        };

        assert_eq!(
            collect_tool_block_body_lines(&tool, ToolOutputVerbosity::Compact),
            vec!["Background shell launched"]
        );
    }
}

#[test]
fn committed_shell_output_block_renders_status_and_output_text() {
    let tool = AssistantToolUseBlock {
        id: "shell-output-1".into(),
        name: "ShellOutput".into(),
        input: json!({"shellId": "sh_1", "wait": true, "timeout": 30000}),
        tool_call_content: None,
        raw_output: Some(json!({
            "shellId": "sh_1",
            "status": "exited",
            "completed": true,
            "exitCode": 0,
            "nextCursor": 2,
            "output": "first\nsecond\n"
        })),
        title: None,
        locations: None,
        status: None,
    };

    assert_eq!(
        collect_tool_block_body_lines(&tool, ToolOutputVerbosity::Compact),
        vec!["exited (code 0)", "first", "second"]
    );
}

#[test]
fn committed_shell_stop_block_renders_single_status_line() {
    let tool = AssistantToolUseBlock {
        id: "shell-stop-1".into(),
        name: "ShellStop".into(),
        input: json!({"shellId": "sh_1"}),
        tool_call_content: None,
        raw_output: Some(json!({
            "shellId": "sh_1",
            "status": "stopping",
            "stopRequested": true,
            "alreadyRequested": false,
            "alreadyCompleted": false
        })),
        title: None,
        locations: None,
        status: None,
    };

    assert_eq!(
        collect_tool_block_body_lines(&tool, ToolOutputVerbosity::Compact),
        vec!["stopping"]
    );
}

// ------------------------------------------------------------------
// Committed Code Mode (`run_code`) bodies
//
// A committed assistant row carrying prose *and* a `run_code` call never
// reaches the streaming card: it renders through the message projection,
// which collects the tool body with `collect_tool_block_body_lines_with_width`.
// Code Mode progress events are execution activity, not output, so that body
// must show the shared completion summary plus the final output — and never
// replay the progress stream or the JSON of its structured payloads.
// ------------------------------------------------------------------

const RUN_CODE_PROGRAM: &str = "const a = await tools.Read({file_path: 'a'});\n\
console.log('PROGRAM-SOURCE-MARKER');";

fn text_tool_content(text: &str) -> rebon_types::ToolCallContent {
    rebon_types::ToolCallContent::Content(rebon_types::RegularContent {
        content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: text.into(),
            annotations: None,
        }),
    })
}

fn code_progress_content(
    kind: &str,
    message: &str,
    payload: Value,
) -> rebon_types::ToolCallContent {
    rebon_render::tool_output::tool_progress_update_content(
        &rebon_tools_core::ToolProgressUpdate::new(kind)
            .with_message(message)
            .with_payload(payload),
    )
    .expect("progress text")
    .remove(0)
}

/// Two nested calls (Read, Bash) as the engine emits them: every start and
/// finish is a progress event with its own structured payload.
fn code_mode_progress_events() -> Vec<rebon_types::ToolCallContent> {
    vec![
        code_progress_content(
            "code_mode/program",
            "Running JavaScript (2 lines)",
            json!({"language": "javascript"}),
        ),
        code_progress_content(
            "code_mode/dispatch-start",
            "→ Read (a)",
            json!({"seq": 0, "tool": "Read"}),
        ),
        code_progress_content(
            "code_mode/dispatch",
            "← Read succeeded",
            json!({"seq": 0, "tool": "Read", "isError": false}),
        ),
        code_progress_content(
            "code_mode/dispatch-start",
            "→ Bash (check)",
            json!({"seq": 1, "tool": "Bash"}),
        ),
        code_progress_content(
            "code_mode/dispatch",
            "← Bash succeeded",
            json!({"seq": 1, "tool": "Bash", "isError": false}),
        ),
    ]
}

/// Every string a leaked progress event would put on screen: its prose and
/// the JSON of its payload.
const PROGRESS_LEAK_MARKERS: [&str; 9] = [
    "Running JavaScript (2 lines)",
    "→ Read (a)",
    "← Read succeeded",
    "→ Bash (check)",
    "← Bash succeeded",
    r#"{"seq":0,"tool":"Read"}"#,
    r#"{"seq":1,"tool":"Bash"}"#,
    r#""seq":0"#,
    r#""seq":1"#,
];

/// A committed assistant row with prose before a `run_code` call — the shape
/// that bypasses the streaming card. `raw_output` mirrors what the engine
/// leaves behind: the last progress payload, or the terminal error.
fn committed_run_code_message(
    status: ToolCallStatus,
    tool_call_content: Vec<rebon_types::ToolCallContent>,
    raw_output: Option<Value>,
) -> Message {
    Message::Assistant(AssistantMessage {
        uuid: "a-mixed-run-code".into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content: vec![
                AssistantContentBlock::Text(AssistantTextBlock {
                    text: "Running a sequence".into(),
                }),
                AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                    id: "code-1".into(),
                    name: "run_code".into(),
                    input: json!({
                        "description": "Inspect and check files",
                        "code": RUN_CODE_PROGRAM,
                    }),
                    tool_call_content: Some(tool_call_content),
                    raw_output,
                    title: None,
                    locations: None,
                    status: Some(status),
                }),
            ],
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    })
}

fn render_committed_run_code(
    status: ToolCallStatus,
    tool_call_content: Vec<rebon_types::ToolCallContent>,
    raw_output: Option<Value>,
    verbosity: ToolOutputVerbosity,
) -> String {
    let message = committed_run_code_message(status, tool_call_content, raw_output);
    let mut buf = new_buf(100, 30);
    render_message(
        &message,
        Rect::new(0, 0, 100, 30),
        &mut buf,
        &RenderTheme::plain(),
        verbosity,
    );
    all_text(&buf)
}

fn assert_no_progress_leak(snap: &str, verbosity: ToolOutputVerbosity) {
    for marker in PROGRESS_LEAK_MARKERS {
        assert!(
            !snap.contains(marker),
            "{verbosity:?}: Code Mode progress leaked into the committed body: \
             {marker:?}\n{snap}"
        );
    }
}

/// The program body is what Ctrl+O reveals while streaming; a committed row
/// has no such expansion, so it only appears in explicit Verbose.
fn assert_program_body(snap: &str, verbosity: ToolOutputVerbosity) {
    let verbose = verbosity == ToolOutputVerbosity::Verbose;
    assert_eq!(
        snap.contains("JavaScript:"),
        verbose,
        "{verbosity:?}: program label\n{snap}"
    );
    assert_eq!(
        snap.contains("PROGRAM-SOURCE-MARKER"),
        verbose,
        "{verbosity:?}: program body\n{snap}"
    );
}

#[test]
fn committed_run_code_body_shows_the_summary_and_never_the_progress_stream() {
    let final_output = "final console output\nreturned value";

    for verbosity in [
        ToolOutputVerbosity::Compact,
        ToolOutputVerbosity::Normal,
        ToolOutputVerbosity::Verbose,
    ] {
        for status in [ToolCallStatus::Completed, ToolCallStatus::Failed] {
            for shape in ["progress events", "final output only"] {
                let with_progress = shape == "progress events";
                let mut content = if with_progress {
                    code_mode_progress_events()
                } else {
                    Vec::new()
                };
                // A failed call still carries the last progress payload when the
                // program threw before reporting an error of its own.
                let raw_output = if with_progress {
                    Some(json!({"seq": 1, "tool": "Bash", "isError": false}))
                } else {
                    None
                };
                content.push(text_tool_content(if status == ToolCallStatus::Failed {
                    "program failed: boom"
                } else {
                    final_output
                }));

                let snap = render_committed_run_code(status, content, raw_output, verbosity);
                let case = format!("{status:?} / {shape} / {verbosity:?}");

                assert_no_progress_leak(&snap, verbosity);
                assert_program_body(&snap, verbosity);

                if status == ToolCallStatus::Failed {
                    assert!(
                        snap.contains("program failed: boom"),
                        "{case}: the failure reason is missing\n{snap}"
                    );
                    assert!(
                        !snap.contains("Completed"),
                        "{case}: a failed call must not claim completion\n{snap}"
                    );
                } else {
                    assert!(
                        snap.contains("Completed"),
                        "{case}: the completion summary is missing\n{snap}"
                    );
                    assert!(
                        snap.contains("final console output") && snap.contains("returned value"),
                        "{case}: the final output is missing\n{snap}"
                    );
                    if with_progress {
                        assert!(
                            snap.contains("Completed · 2 calls")
                                && snap.contains("Read ×1")
                                && snap.contains("Bash ×1"),
                            "{case}: the summary must count the nested calls\n{snap}"
                        );
                    } else {
                        assert!(
                            !snap.contains("Completed · "),
                            "{case}: no dispatch events means no call counts\n{snap}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn committed_run_code_completed_without_final_output_says_so() {
    for verbosity in [
        ToolOutputVerbosity::Compact,
        ToolOutputVerbosity::Normal,
        ToolOutputVerbosity::Verbose,
    ] {
        // Only progress events: the sequence ran, and nothing was returned.
        let snap = render_committed_run_code(
            ToolCallStatus::Completed,
            code_mode_progress_events(),
            None,
            verbosity,
        );
        assert!(
            snap.contains("Completed · 2 calls"),
            "{verbosity:?}: the summary still reports what ran\n{snap}"
        );
        assert!(
            snap.contains("No final output received"),
            "{verbosity:?}: a completed call with no output must say so, as the \
             streaming card does\n{snap}"
        );
        assert_no_progress_leak(&snap, verbosity);
    }
}

#[test]
fn committed_run_code_completed_preview_uses_the_shared_line_budget() {
    let mut content = code_mode_progress_events();
    content.extend((1..=6).map(|n| text_tool_content(&format!("output line {n}"))));

    let compact = render_committed_run_code(
        ToolCallStatus::Completed,
        content.clone(),
        None,
        ToolOutputVerbosity::Compact,
    );
    assert!(compact.contains("Completed · 2 calls"), "{compact}");
    assert!(
        compact.contains("output line 1") && compact.contains("output line 3"),
        "{compact}"
    );
    assert!(
        !compact.contains("output line 4"),
        "the shared four-line preview budget must bound the body\n{compact}"
    );
    assert!(compact.contains("… +3 lines"), "{compact}");
    assert_program_body(&compact, ToolOutputVerbosity::Compact);

    let verbose = render_committed_run_code(
        ToolCallStatus::Completed,
        content,
        None,
        ToolOutputVerbosity::Verbose,
    );
    for n in 1..=6 {
        assert!(
            verbose.contains(&format!("output line {n}")),
            "Verbose must not truncate the final output\n{verbose}"
        );
    }
    assert!(!verbose.contains("… +"), "{verbose}");
    assert_program_body(&verbose, ToolOutputVerbosity::Verbose);
}

#[test]
fn committed_run_code_failure_prefers_the_reported_error_over_the_last_block() {
    let mut content = code_mode_progress_events();
    content.push(text_tool_content("boom from the content block"));

    let reported = render_committed_run_code(
        ToolCallStatus::Failed,
        content.clone(),
        Some(json!({"error": "boom from the error payload"})),
        ToolOutputVerbosity::Normal,
    );
    assert!(
        reported.contains("boom from the error payload"),
        "{reported}"
    );
    assert!(
        !reported.contains("boom from the content block"),
        "{reported}"
    );
    assert!(!reported.contains("Completed"), "{reported}");
    assert_no_progress_leak(&reported, ToolOutputVerbosity::Normal);

    let fallback = render_committed_run_code(
        ToolCallStatus::Failed,
        content,
        None,
        ToolOutputVerbosity::Normal,
    );
    assert!(
        fallback.contains("boom from the content block"),
        "without an error payload the last content block is the reason\n{fallback}"
    );
    assert_no_progress_leak(&fallback, ToolOutputVerbosity::Normal);
}
