use super::*;
use ratatui::style::Color;
use rebon_render::{
    AttachmentFileDisplay, AttachmentFileKind, AttachmentInput, AttachmentRelevantMemoryEntry,
};

fn make_buf(width: u16, height: u16) -> Buffer {
    Buffer::empty(Rect::new(0, 0, width, height))
}

fn line_at(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf[(x, y)].symbol())
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn assert_text_modifier(buf: &Buffer, y: u16, needle: &str, modifier: Modifier, expected: bool) {
    let row = line_at(buf, y);
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

#[test]
fn teammate_completion_uses_status_gutter() {
    let widget = UserTextBodyWidget::from_raw(
        "<teammate-message teammate_id=\"fix-d1-transport\">{\"type\":\"task_completed\",\"taskId\":\"7\",\"taskSubject\":\"Fix transport\"}</teammate-message>",
        false,
        false,
        None,
        None,
        false,
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    let mut buf = make_buf(60, 2);
    widget.render_to_buffer(Rect::new(0, 0, 60, 2), &mut buf);

    assert_eq!(line_at(&buf, 0), "● Teammate @fix-d1-transport finished");
}

#[test]
fn teammate_message_uses_message_gutter_and_visible_body() {
    let widget = UserTextBodyWidget::from_raw(
        "<teammate-message teammate_id=\"fix-d1-transport\" summary=\"done\">D1 complete\nHandoff ready.</teammate-message>",
        false,
        false,
        None,
        None,
        false,
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    let mut buf = make_buf(60, 5);
    widget.render_to_buffer(Rect::new(0, 0, 60, 5), &mut buf);

    assert_eq!(line_at(&buf, 0), "› Message from @fix-d1-transport");
    assert_eq!(line_at(&buf, 1), "");
    assert_eq!(line_at(&buf, 2), "  D1 complete");
    assert_eq!(line_at(&buf, 3), "  Handoff ready.");
}

#[test]
fn assistant_tool_use_body_widget_bolds_tool_name_and_dims_summary() {
    let detail_color = Color::Rgb(96, 96, 96);
    let theme = MessagesRenderTheme {
        dim: Style::new().fg(detail_color),
        ..MessagesRenderTheme::plain()
    };
    let widget = AssistantToolUseBodyWidget::from_raw(
        Some("toolu-1"),
        Some("Bash"),
        Some("cargo test"),
        &[],
        false,
        theme,
        Style::new(),
    );
    let mut buf = make_buf(40, 2);
    widget.render_to_buffer(Rect::new(0, 0, 40, 2), &mut buf);

    assert!(line_at(&buf, 0).contains("Bash (cargo test)"));
    let tool_x = (0..buf.area.width)
        .find(|&x| buf[(x, 0)].symbol() == "B")
        .expect("Bash tool name");
    let summary_x = (0..buf.area.width)
        .find(|&x| buf[(x, 0)].symbol() == "(")
        .expect("tool summary");
    assert_text_modifier(&buf, 0, "Bash", Modifier::BOLD, true);
    assert_text_modifier(&buf, 0, "(", Modifier::BOLD, false);
    assert_ne!(buf[(tool_x, 0)].style().fg, Some(detail_color));
    assert_eq!(buf[(summary_x, 0)].style().fg, Some(detail_color));
    assert_eq!(buf[(summary_x + 1, 0)].style().fg, Some(detail_color));
}

#[test]
fn attachment_body_widget_paints_gutter_and_line_rows() {
    let widget = AttachmentBodyWidget::from_input(
        &AttachmentInput::File(AttachmentFileDisplay {
            display_path: "src/lib.rs".into(),
            kind: AttachmentFileKind::Text {
                num_lines: 42,
                truncated: false,
            },
        }),
        false,
        false,
        true,
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    // Width 40 → gutter 4 + content 36. Line is 26 chars → fits in one row.
    assert_eq!(widget.height(40), 1);
    let mut buf = make_buf(40, 2);
    widget.render_to_buffer(Rect::new(0, 0, 40, 2), &mut buf);
    assert!(line_at(&buf, 0).starts_with("● "));
    assert!(line_at(&buf, 0).contains("Read src/lib.rs (42 lines)"));
}

#[test]
fn attachment_body_widget_uses_directory_gutter_for_listed_directory() {
    let widget = AttachmentBodyWidget::from_input(
        &AttachmentInput::Directory {
            display_path: "src".into(),
        },
        false,
        false,
        true,
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    let mut buf = make_buf(40, 2);
    widget.render_to_buffer(Rect::new(0, 0, 40, 2), &mut buf);
    assert!(line_at(&buf, 0).starts_with("⎿ "));
    assert!(line_at(&buf, 0).contains("Listed directory src"));
}

#[test]
fn attachment_body_widget_renders_relevant_memories_entries_with_indent() {
    let widget = AttachmentBodyWidget::from_input(
        &AttachmentInput::RelevantMemories {
            memories: vec![
                AttachmentRelevantMemoryEntry {
                    path: "/tmp/a.md".into(),
                    content: "remember me".into(),
                },
                AttachmentRelevantMemoryEntry {
                    path: "/tmp/b.md".into(),
                    content: "and me".into(),
                },
            ],
        },
        false,
        true,
        true,
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    // Margin top + summary row + two transcript-block entries (header + body each).
    assert!(widget.height(40) >= 6);
    let mut buf = make_buf(40, 10);
    widget.render_to_buffer(Rect::new(0, 0, 40, 10), &mut buf);
    let rendered: Vec<String> = (0..10).map(|y| line_at(&buf, y)).collect();
    assert!(rendered
        .iter()
        .any(|line| line.contains("Recalled 2 memories")));
    assert!(rendered.iter().any(|line| line.contains("a.md")));
    assert!(rendered.iter().any(|line| line.contains("remember me")));
    assert!(rendered.iter().any(|line| line.contains("b.md")));
}

#[test]
fn assistant_text_body_widget_paints_single_gutter_with_margin() {
    let widget = AssistantTextBodyWidget::from_raw(
        "hello\nworld",
        true,
        false,
        true,
        MessagesRenderTheme::plain(),
        Style::new(),
        38,
    );
    assert!(widget.height(40) >= 3);
    let mut buf = make_buf(40, 4);
    widget.render_to_buffer(Rect::new(0, 0, 40, 4), &mut buf);
    let rendered: Vec<String> = (0..4).map(|y| line_at(&buf, y)).collect();
    // add_margin=true → row 0 is blank, gutter+content at row 1+
    assert!(rendered.iter().any(|line| line.starts_with("●")));
    assert!(rendered.iter().any(|line| line.contains("hello")));
    assert!(rendered.iter().any(|line| line.contains("world")));
}

#[test]
fn assistant_text_body_widget_continuation_paints_no_dot_but_keeps_alignment() {
    // show_gutter_dot=false is the stream-continuation shape: the body
    // must stay in the content column (aligned with the dotted half
    // committed above it) while the gutter itself stays blank.
    let dotted = AssistantTextBodyWidget::from_raw(
        "continued text",
        false,
        false,
        true,
        MessagesRenderTheme::plain(),
        Style::new(),
        38,
    );
    let continuation = AssistantTextBodyWidget::from_raw(
        "continued text",
        false,
        false,
        false,
        MessagesRenderTheme::plain(),
        Style::new(),
        38,
    );

    let mut dotted_buf = make_buf(40, 2);
    dotted.render_to_buffer(Rect::new(0, 0, 40, 2), &mut dotted_buf);
    let mut cont_buf = make_buf(40, 2);
    continuation.render_to_buffer(Rect::new(0, 0, 40, 2), &mut cont_buf);

    let dotted_line = line_at(&dotted_buf, 0);
    let cont_line = line_at(&cont_buf, 0);
    // The dot occupies gutter column 0; the body starts at display
    // column 2 (GUTTER_WIDTH) in both variants — `starts_with` pins the
    // display columns since every prefix char here is width 1.
    assert!(
        dotted_line.starts_with("● continued text"),
        "{dotted_line:?}"
    );
    assert!(
        cont_line.starts_with("  continued text"),
        "continuation body must keep the dotted variant's column: {cont_line:?}"
    );
    assert!(!cont_line.contains('●'), "{cont_line:?}");
}

#[test]
fn assistant_text_body_widget_renders_markdown_headings_and_lists() {
    // Before this wiring, H1/H2/list/emphasis all rendered as flat
    // text via Span::styled(line, theme.text). Now the projection
    // routes through render_markdown_blocks, so the heading gets
    // its own bolded line and list bullets get a "• " prefix.
    let widget = AssistantTextBodyWidget::from_raw(
        "# Header\n\n- first\n- second\n\n**bold** and *em*",
        false,
        false,
        true,
        MessagesRenderTheme::plain(),
        Style::new(),
        38,
    );
    let mut buf = make_buf(60, 10);
    widget.render_to_buffer(Rect::new(0, 0, 60, 10), &mut buf);
    let rendered: Vec<String> = (0..10).map(|y| line_at(&buf, y)).collect();
    assert!(
        rendered.iter().any(|line| line.contains("Header")),
        "heading text missing: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|line| line.contains("- first")),
        "list bullet not rendered: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|line| line.contains("- second")),
        "second list bullet not rendered: {rendered:?}"
    );
    // Emphasis markers are consumed by the lexer — the span text
    // contains "bold" and "em" without the surrounding delimiters.
    let joined: String = rendered.join(" ");
    assert!(joined.contains("bold"), "bold run missing: {joined}");
    assert!(joined.contains("em"), "em run missing: {joined}");
    assert!(
        !joined.contains("**bold**"),
        "raw `**` leaked through: {joined}"
    );
}

#[test]
fn assistant_text_body_widget_streaming_advances_across_deltas() {
    let mut renderer = crate::streaming_markdown::StreamingMarkdownRenderer::new();

    // Delta 1: single complete paragraph — boundary can't advance
    // yet (only one block, which is "still streaming").
    let w1 = AssistantTextBodyWidget::from_streaming(
        "first paragraph",
        false,
        false,
        MessagesRenderTheme::plain(),
        Style::new(),
        38,
        &mut renderer,
    );
    let mut buf = make_buf(40, 3);
    w1.render_to_buffer(Rect::new(0, 0, 40, 3), &mut buf);
    let rows: Vec<String> = (0..3).map(|y| line_at(&buf, y)).collect();
    assert!(rows.iter().any(|l| l.contains("first paragraph")));

    // Delta 2: blank line committed, second block starting —
    // stable prefix should advance.
    let _ = AssistantTextBodyWidget::from_streaming(
        "first paragraph\n\nsecond",
        false,
        false,
        MessagesRenderTheme::plain(),
        Style::new(),
        38,
        &mut renderer,
    );
    assert!(
        !renderer.stable_prefix().is_empty(),
        "stable prefix should have committed first block"
    );

    // Delta 3: appending inside the in-flight block — prefix must
    // not retreat.
    let before = renderer.stable_prefix().to_string();
    let _ = AssistantTextBodyWidget::from_streaming(
        "first paragraph\n\nsecond longer",
        false,
        false,
        MessagesRenderTheme::plain(),
        Style::new(),
        38,
        &mut renderer,
    );
    assert_eq!(
        renderer.stable_prefix(),
        before,
        "prefix retreated mid-block"
    );
}

#[test]
fn assistant_text_body_widget_collects_merged_streaming_hyperlinks() {
    let mut renderer = crate::streaming_markdown::StreamingMarkdownRenderer::new();
    let widget = AssistantTextBodyWidget::from_streaming(
        "[stable](https://stable.test)\n\nhttps://unstable.test",
        false,
        false,
        MessagesRenderTheme::plain(),
        Style::new(),
        38,
        &mut renderer,
    );
    let mut buf = make_buf(40, 4);
    let mut layers = Vec::new();
    MessageBodyKind::AssistantText(widget).render_to_buffer_with_hyperlinks(
        Rect::new(0, 0, 40, 4),
        &mut buf,
        &mut layers,
    );

    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].area, Rect::new(GUTTER_WIDTH, 0, 38, 3));
    assert_eq!(layers[0].hyperlinks.len(), 2);
    assert_eq!(layers[0].hyperlinks[0].target, "https://stable.test");
    assert_eq!(layers[0].hyperlinks[0].line, 0);
    assert_eq!(layers[0].hyperlinks[1].target, "https://unstable.test");
    assert_eq!(layers[0].hyperlinks[1].line, 2);
}

#[test]
fn assistant_text_widget_sidecar_carries_links_and_formula_assets() {
    let widget = AssistantTextBodyWidget::from_raw_with_options(
        "[docs](https://example.test) and $x^2$",
        false,
        false,
        true,
        MessagesRenderTheme::plain(),
        Style::new(),
        38,
        crate::MarkdownRenderOptions::math(),
    );
    let height = widget.height(40);
    let mut buf = make_buf(40, height.max(1));
    let mut layers = Vec::new();
    MessageBodyKind::AssistantText(widget).render_to_buffer_with_hyperlinks(
        Rect::new(0, 0, 40, height.max(1)),
        &mut buf,
        &mut layers,
    );

    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].hyperlinks.len(), 1);
    assert_eq!(layers[0].formulas.len(), 1);
    assert_eq!(layers[0].formulas[0].source, "$x^2$");
    assert!(!layers[0].formulas[0].asset.bitmap.rgba.is_empty());
    for range in &layers[0].formulas[0].ranges {
        let line = layers[0].text.lines[range.line]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(range.start_byte < range.end_byte);
        assert!(range.end_byte <= line.len());
        assert!(line.is_char_boundary(range.start_byte));
        assert!(line.is_char_boundary(range.end_byte));
        assert!(!line[range.start_byte..range.end_byte].is_empty());
    }
}

#[test]
fn markdown_accent_is_limited_to_assistant_text_child_theme() {
    let parent = crate::MessageRenderTheme {
        assistant_text: Style::new().fg(Color::White),
        markdown_accent: Style::new().fg(Color::Magenta),
        ..crate::MessageRenderTheme::plain()
    };
    let tool_theme = child_theme_for(&parent);
    let markdown_theme = assistant_text_child_theme_for(&parent);

    assert_eq!(markdown_theme.accent, parent.markdown_accent);
    assert_eq!(tool_theme.accent.fg, Some(Color::White));
    assert!(tool_theme.accent.add_modifier.contains(Modifier::BOLD));
    assert_ne!(tool_theme.accent, parent.markdown_accent);
}

#[test]
fn assistant_text_body_widget_streaming_resets_on_non_markdown_branch() {
    // If the projection resolves to a non-markdown branch mid-stream
    // (e.g. rate-limit or response), we must drop the cached prefix
    // so future markdown deltas start fresh.
    let mut renderer = crate::streaming_markdown::StreamingMarkdownRenderer::new();
    let _ = AssistantTextBodyWidget::from_streaming(
        "first\n\nsecond typing",
        false,
        false,
        MessagesRenderTheme::plain(),
        Style::new(),
        38,
        &mut renderer,
    );
    assert!(!renderer.stable_prefix().is_empty());
    let _ = AssistantTextBodyWidget::from_streaming(
        "You've hit your limit",
        false,
        false,
        MessagesRenderTheme::plain(),
        Style::new(),
        38,
        &mut renderer,
    );
    assert_eq!(renderer.stable_prefix(), "");
}

#[test]
fn assistant_thinking_body_widget_paints_compact_title_and_expanded_body() {
    let collapsed = AssistantThinkingBodyWidget::from_raw(
        "deep thought\nfull body",
        false,
        false,
        false,
        false,
        true,
        true,
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    let mut buf = make_buf(80, 3);
    collapsed.render_to_buffer(Rect::new(0, 0, 80, 3), &mut buf);
    let rendered: Vec<String> = (0..3).map(|y| line_at(&buf, y)).collect();
    assert!(rendered[0].starts_with("· "));
    assert!(rendered[0].contains("deep thought"));
    assert!(rendered[0].contains("Ctrl+O to expand"));
    assert!(!rendered.iter().any(|line| line.contains("full body")));

    let expanded = AssistantThinkingBodyWidget::from_raw(
        "**step 1**\nstep 2",
        false,
        true,
        true,
        false,
        false,
        false,
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    let mut buf = make_buf(40, 5);
    expanded.render_to_buffer(Rect::new(0, 0, 40, 5), &mut buf);
    let rendered: Vec<String> = (0..5).map(|y| line_at(&buf, y)).collect();
    assert!(rendered.iter().any(|line| line.contains("step 1")));
    assert!(rendered.iter().any(|line| line.contains("step 2")));
    assert!(!rendered.iter().any(|line| line.contains("Thinking")));
    assert!(!rendered
        .iter()
        .any(|line| line.contains("Ctrl+O to expand")));
}

#[test]
fn compact_thinking_preview_renders_markdown_title_only() {
    let widget = AssistantThinkingBodyWidget::from_raw(
        "**Planning worker tasks**\nsecond line",
        false,
        true,
        false,
        false,
        true,
        true,
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    let mut buf = make_buf(80, 3);
    widget.render_to_buffer(Rect::new(0, 0, 80, 3), &mut buf);
    let rendered: Vec<String> = (0..3).map(|y| line_at(&buf, y)).collect();

    assert!(rendered[0].starts_with("· "));
    assert!(rendered[0].contains("Planning worker tasks"));
    assert!(!rendered.iter().any(|line| line.contains("second line")));
    assert!(rendered
        .iter()
        .any(|line| line.contains("Ctrl+O to expand")));
}

#[test]
fn user_tool_result_body_widget_paints_success_summary_and_result_block() {
    let widget = UserToolResultBodyWidget::from_raw(
        rebon_render::UserToolResultInput {
            tool_use_id: "toolu-1".into(),
            content: "line 1\nline 2".into(),
            is_error: false,
            tool_exists: true,
            tool_has_custom_reject_renderer: false,
            tool_has_custom_error_renderer: false,
            renders_as_assistant_text: false,
            input_summary: None,
            verbose: false,
            is_transcript_mode: false,
            width: "80".into(),
            classifier_rule: None,
            yolo_reason: None,
            classifier_denial: false,
        },
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    let height = widget.height(50);
    assert!(height >= 5, "height should be at least 5, got {height}");
    let mut buf = make_buf(50, height + 1);
    widget.render_to_buffer(Rect::new(0, 0, 50, height + 1), &mut buf);
    let rendered: Vec<String> = (0..buf.area.height).map(|y| line_at(&buf, y)).collect();
    assert!(rendered[0].starts_with("↳ "));
    assert!(rendered
        .iter()
        .any(|line| line.contains("[tool result toolu-1]")));
    assert!(rendered.iter().any(|line| line.contains("Result")));
    assert!(rendered.iter().any(|line| line.contains("line 1")));
    assert!(rendered.iter().any(|line| line.contains("2 lines")));
}

#[test]
fn user_tool_result_body_widget_paints_bash_json_with_stdout_and_stderr() {
    let raw = r#"{
            "stdout": "hello world\nline two",
            "stderr": "danger",
            "interrupted": false,
            "exitCode": 1,
            "timedOut": false,
            "command": "true"
        }"#;
    let widget = UserToolResultBodyWidget::from_raw(
        rebon_render::UserToolResultInput {
            tool_use_id: "toolu-bash".into(),
            content: raw.into(),
            is_error: false,
            tool_exists: true,
            tool_has_custom_reject_renderer: false,
            tool_has_custom_error_renderer: false,
            renders_as_assistant_text: false,
            input_summary: None,
            verbose: true,
            is_transcript_mode: false,
            width: "80".into(),
            classifier_rule: None,
            yolo_reason: None,
            classifier_denial: false,
        },
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    let height = widget.height(60);
    let mut buf = make_buf(60, height + 1);
    widget.render_to_buffer(Rect::new(0, 0, 60, height + 1), &mut buf);
    let rendered: Vec<String> = (0..buf.area.height).map(|y| line_at(&buf, y)).collect();
    assert!(
        rendered.iter().any(|line| line.contains("Bash Output")),
        "expected Bash Output title, got: {rendered:?}"
    );
    assert!(
        !rendered.iter().any(|line| line.contains("\"stdout\"")),
        "raw JSON should not leak through: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|line| line.contains("hello world")),
        "stdout first line missing: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|line| line.contains("line two")),
        "stdout second line missing: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|line| line.contains("danger")),
        "stderr body missing: {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("stdout") && line.contains("stderr")),
        "subtitle with stdout/stderr counts missing: {rendered:?}"
    );
}

#[test]
fn user_tool_result_body_widget_paints_bash_empty_output_fallback() {
    let raw = r#"{
            "stdout": "",
            "stderr": "",
            "interrupted": false,
            "exitCode": 0,
            "timedOut": false,
            "command": "true",
            "noOutputExpected": true
        }"#;
    let widget = UserToolResultBodyWidget::from_raw(
        rebon_render::UserToolResultInput {
            tool_use_id: "toolu-bash-done".into(),
            content: raw.into(),
            is_error: false,
            tool_exists: true,
            tool_has_custom_reject_renderer: false,
            tool_has_custom_error_renderer: false,
            renders_as_assistant_text: false,
            input_summary: None,
            verbose: false,
            is_transcript_mode: false,
            width: "80".into(),
            classifier_rule: None,
            yolo_reason: None,
            classifier_denial: false,
        },
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    let height = widget.height(40);
    let mut buf = make_buf(40, height + 1);
    widget.render_to_buffer(Rect::new(0, 0, 40, height + 1), &mut buf);
    let rendered: Vec<String> = (0..buf.area.height).map(|y| line_at(&buf, y)).collect();
    assert!(
        rendered.iter().any(|line| line.contains("Bash Output")),
        "expected Bash Output title, got: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|line| line.contains("Done")),
        "noOutputExpected should render Done fallback, got: {rendered:?}"
    );
}

#[test]
fn user_tool_result_body_widget_falls_back_to_generic_result_for_non_bash_json() {
    let widget = UserToolResultBodyWidget::from_raw(
        rebon_render::UserToolResultInput {
            tool_use_id: "toolu-generic".into(),
            content: "plain text result".into(),
            is_error: false,
            tool_exists: true,
            tool_has_custom_reject_renderer: false,
            tool_has_custom_error_renderer: false,
            renders_as_assistant_text: false,
            input_summary: None,
            verbose: false,
            is_transcript_mode: false,
            width: "80".into(),
            classifier_rule: None,
            yolo_reason: None,
            classifier_denial: false,
        },
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    let mut buf = make_buf(40, 6);
    widget.render_to_buffer(Rect::new(0, 0, 40, 6), &mut buf);
    let rendered: Vec<String> = (0..6).map(|y| line_at(&buf, y)).collect();
    assert!(
        rendered.iter().any(|line| line.contains("Result")),
        "non-bash body should still use generic Result block: {rendered:?}"
    );
    assert!(
        !rendered.iter().any(|line| line.contains("Bash Output")),
        "non-bash body must not use Bash Output title: {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("plain text result")),
        "generic body content missing: {rendered:?}"
    );
}

#[test]
fn user_tool_result_body_widget_paints_error_block() {
    let widget = UserToolResultBodyWidget::from_raw(
        rebon_render::UserToolResultInput {
            tool_use_id: "toolu-1".into(),
            content: "boom".into(),
            is_error: true,
            tool_exists: true,
            tool_has_custom_reject_renderer: false,
            tool_has_custom_error_renderer: false,
            renders_as_assistant_text: false,
            input_summary: None,
            verbose: false,
            is_transcript_mode: false,
            width: "80".into(),
            classifier_rule: None,
            yolo_reason: None,
            classifier_denial: false,
        },
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    let mut buf = make_buf(50, 6);
    widget.render_to_buffer(Rect::new(0, 0, 50, 6), &mut buf);
    let rendered: Vec<String> = (0..6).map(|y| line_at(&buf, y)).collect();
    assert!(rendered[0].starts_with("↳ "));
    assert!(rendered.iter().any(|line| line.contains("Tool Error")));
    assert!(rendered.iter().any(|line| line.contains("boom")));
}

#[test]
fn message_body_kind_dispatches_height_and_paint_per_variant() {
    let assistant_text = MessageBodyKind::AssistantText(AssistantTextBodyWidget::from_raw(
        "hello",
        false,
        false,
        true,
        MessagesRenderTheme::plain(),
        Style::new(),
        38,
    ));
    assert_eq!(assistant_text.height(40), 1);

    let attachment = MessageBodyKind::Attachment(AttachmentBodyWidget::from_input(
        &AttachmentInput::Directory {
            display_path: "src".into(),
        },
        false,
        false,
        false,
        MessagesRenderTheme::plain(),
        Style::new(),
    ));
    assert_eq!(attachment.height(40), 1);

    let text = MessageBodyKind::Text(Text::from(Line::from(Span::raw("hello"))));
    assert_eq!(text.height(20), 1);

    let tool_result = MessageBodyKind::UserToolResult(UserToolResultBodyWidget::from_raw(
        rebon_render::UserToolResultInput {
            tool_use_id: "toolu-1".into(),
            content: "ok".into(),
            is_error: false,
            tool_exists: true,
            tool_has_custom_reject_renderer: false,
            tool_has_custom_error_renderer: false,
            renders_as_assistant_text: false,
            input_summary: None,
            verbose: false,
            is_transcript_mode: false,
            width: "80".into(),
            classifier_rule: None,
            yolo_reason: None,
            classifier_denial: false,
        },
        MessagesRenderTheme::plain(),
        Style::new(),
    ));
    assert!(tool_result.height(40) >= 3);
}

#[test]
fn file_edit_body_widget_paints_inline_diff() {
    use rebon_render::file_edit::{FileEditUpdatedProjection, StructuredPatchHunk};

    let projection = FileEditUpdatedProjection::Detailed {
        summary: "Edit (src/lib.rs) · Added 2 lines, removed 1 line".to_string(),
        diff_width: 40,
        fold_long_runs: false,
        file_path: "src/lib.rs".to_string(),
        first_line: None,
        file_content: None,
        structured_patch: vec![StructuredPatchHunk {
            lines: vec![
                " context".to_string(),
                "+new line a".to_string(),
                "+new line b".to_string(),
                "-old line".to_string(),
            ],
        }],
    };
    let widget =
        FileEditBodyWidget::from_updated(projection, MessagesRenderTheme::plain(), Style::new());
    let height = widget.height(60);
    // Summary (1) + 4 diff lines = at least 5.
    assert!(height >= 5, "height should be at least 5, got {height}");

    let mut buf = make_buf(60, height + 1);
    widget.render_to_buffer(Rect::new(0, 0, 60, height + 1), &mut buf);
    let rendered: Vec<String> = (0..buf.area.height).map(|y| line_at(&buf, y)).collect();
    assert!(rendered[0].starts_with("●"));
    assert!(rendered
        .iter()
        .any(|line| line.contains("Edit (src/lib.rs)")
            && line.contains("Added 2 lines, removed 1 line")));
    // Inline diff lines: context, added, and removed content visible.
    assert!(
        rendered.iter().any(|line| line.contains("context")),
        "expected context line in inline diff, got {rendered:?}"
    );
    assert!(
        rendered.iter().any(|line| line.contains("new line a")),
        "expected added line in inline diff, got {rendered:?}"
    );
    assert!(
        rendered.iter().any(|line| line.contains("old line")),
        "expected removed line in inline diff, got {rendered:?}"
    );
}

#[test]
fn file_edit_body_widget_folds_long_added_and_removed_runs() {
    use rebon_render::file_edit::{FileEditUpdatedProjection, StructuredPatchHunk};

    let mut lines = (1..=30)
        .map(|line| format!("-old {line}"))
        .collect::<Vec<_>>();
    lines.extend((1..=30).map(|line| format!("+new {line}")));
    let projection = FileEditUpdatedProjection::Detailed {
        summary: "Edit (src/lib.rs) · Added 30 lines, removed 30 lines".to_string(),
        diff_width: 68,
        fold_long_runs: true,
        file_path: "src/lib.rs".to_string(),
        first_line: None,
        file_content: None,
        structured_patch: vec![StructuredPatchHunk { lines }],
    };
    let widget =
        FileEditBodyWidget::from_updated(projection, MessagesRenderTheme::plain(), Style::new());
    let height = widget.height(80);
    let mut buf = make_buf(80, height + 1);
    widget.render_to_buffer(Rect::new(0, 0, 80, height + 1), &mut buf);
    let rendered = (0..buf.area.height)
        .map(|y| line_at(&buf, y))
        .collect::<Vec<_>>();

    assert_eq!(
        rendered
            .iter()
            .filter(|line| line.contains("(10 lines hidden)"))
            .count(),
        2,
        "{rendered:?}"
    );
    assert!(!rendered.iter().any(|line| line.contains("old 11")));
    assert!(!rendered.iter().any(|line| line.contains("new 11")));
    assert!(rendered.iter().any(|line| line.contains("old 21")));
    assert!(rendered.iter().any(|line| line.contains("new 21")));
}

#[test]
fn file_edit_body_widget_verbose_projection_keeps_long_run_expanded() {
    use rebon_render::file_edit::{FileEditUpdatedProjection, StructuredPatchHunk};

    let projection = FileEditUpdatedProjection::Detailed {
        summary: "Edit (src/lib.rs) · Added 24 lines".to_string(),
        diff_width: 68,
        fold_long_runs: false,
        file_path: "src/lib.rs".to_string(),
        first_line: None,
        file_content: None,
        structured_patch: vec![StructuredPatchHunk {
            lines: (1..=24).map(|line| format!("+added {line}")).collect(),
        }],
    };
    let widget =
        FileEditBodyWidget::from_updated(projection, MessagesRenderTheme::plain(), Style::new());
    let height = widget.height(80);
    let mut buf = make_buf(80, height + 1);
    widget.render_to_buffer(Rect::new(0, 0, 80, height + 1), &mut buf);
    let rendered = (0..buf.area.height)
        .map(|y| line_at(&buf, y))
        .collect::<Vec<_>>();

    assert!(!rendered.iter().any(|line| line.contains("lines hidden")));
    assert!(rendered.iter().any(|line| line.contains("added 11")));
    assert!(rendered.iter().any(|line| line.contains("added 24")));
}

#[test]
fn diff_lines_to_text_colors_changed_rows_from_gutter_through_padding() {
    let ds = theme::get_active_theme();
    let cases = [
        (LineColor::Added, parse_theme_color(ds.diffAdded)),
        (LineColor::Removed, parse_theme_color(ds.diffRemoved)),
    ];

    for (line_color, expected_bg) in cases {
        let rendered = RenderedLine {
            gutter: "658 +".to_string(),
            content: vec![DiffSegment {
                text: "changed".to_string(),
                word_color: WordColor::None,
            }],
            padding: "      ".to_string(),
            line_color,
            dim: false,
        };
        let text = diff_lines_to_text(&[rendered], &MessagesRenderTheme::plain());
        let spans = &text.lines[0].spans;

        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content.as_ref(), "658 +");
        assert_eq!(spans[2].content.as_ref(), "      ");
        assert!(spans.iter().all(|span| span.style.bg == Some(expected_bg)));
    }
}

#[test]
fn diff_lines_to_text_uses_theme_dim_for_regular_context_rows() {
    let rendered = RenderedLine {
        gutter: "42  ".to_string(),
        content: vec![DiffSegment {
            text: "unchanged context".to_string(),
            word_color: WordColor::None,
        }],
        padding: String::new(),
        line_color: LineColor::None,
        dim: true,
    };
    let theme = MessagesRenderTheme {
        dim: Style::new().fg(Color::Blue),
        ..MessagesRenderTheme::plain()
    };
    let text = diff_lines_to_text(&[rendered], &theme);

    assert_eq!(text.lines[0].spans[0].style.fg, Some(Color::Blue));
    assert_eq!(text.lines[0].spans[1].style.fg, Some(Color::Blue));
}

#[test]
fn diff_lines_to_text_does_not_misclassify_separator_like_source_lines() {
    let rendered = ["...", "──── (5 lines hidden) ────"]
        .into_iter()
        .enumerate()
        .map(|(index, source)| RenderedLine {
            gutter: format!("{}  ", index + 42),
            content: vec![DiffSegment {
                text: source.to_string(),
                word_color: WordColor::None,
            }],
            padding: String::new(),
            line_color: LineColor::None,
            dim: true,
        })
        .collect::<Vec<_>>();
    let theme = MessagesRenderTheme {
        dim: Style::new().fg(Color::Blue),
        ..MessagesRenderTheme::plain()
    };
    let text = diff_lines_to_text(&rendered, &theme);

    for line in &text.lines {
        assert!(line
            .spans
            .iter()
            .all(|span| span.style.fg == Some(Color::Blue)));
    }
}

#[test]
fn diff_lines_to_text_uses_gray_for_synthetic_hunk_separator() {
    use rebon_render::file_edit::StructuredPatchHunk;

    let rendered = render_inline_diff(
        &[
            StructuredPatchHunk {
                lines: vec!["+first".to_string()],
            },
            StructuredPatchHunk {
                lines: vec!["+second".to_string()],
            },
        ],
        68,
    );
    let separator = rendered
        .iter()
        .position(|line| line.gutter.is_empty() && line.content[0].text == "...")
        .expect("synthetic hunk separator");
    let text = diff_lines_to_text(&rendered, &MessagesRenderTheme::plain());

    assert!(text.lines[separator]
        .spans
        .iter()
        .filter(|span| !span.content.is_empty())
        .all(|span| span.style.fg == Some(Color::Gray)));
}

#[test]
fn diff_lines_to_text_uses_dimmed_background_across_the_changed_row() {
    let ds = theme::get_active_theme();
    let rendered = RenderedLine {
        gutter: "42 -".to_string(),
        content: vec![DiffSegment {
            text: "removed".to_string(),
            word_color: WordColor::None,
        }],
        padding: "   ".to_string(),
        line_color: LineColor::RemovedDimmed,
        dim: true,
    };
    let text = diff_lines_to_text(&[rendered], &MessagesRenderTheme::default_styled());
    let expected_bg = parse_theme_color(ds.diffRemovedDimmed);

    assert!(text.lines[0]
        .spans
        .iter()
        .all(|span| span.style.bg == Some(expected_bg)));
}

#[test]
fn folded_diff_separators_keep_added_and_removed_backgrounds() {
    use rebon_render::file_edit::StructuredPatchHunk;

    let mut lines = (1..=30)
        .map(|line| format!("-old {line}"))
        .collect::<Vec<_>>();
    lines.extend((1..=30).map(|line| format!("+new {line}")));
    let rendered = render_inline_diff_for_display(&[StructuredPatchHunk { lines }], 68, true);
    let text = diff_lines_to_text(&rendered, &MessagesRenderTheme::plain());
    let hidden_lines = text
        .lines
        .iter()
        .filter(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("lines hidden"))
        })
        .collect::<Vec<_>>();
    let ds = theme::get_active_theme();
    let expected_backgrounds = [
        parse_theme_color(ds.diffRemoved),
        parse_theme_color(ds.diffAdded),
    ];

    assert_eq!(hidden_lines.len(), expected_backgrounds.len());
    for (line, expected_bg) in hidden_lines.into_iter().zip(expected_backgrounds) {
        assert!(line
            .spans
            .iter()
            .all(|span| span.style.bg == Some(expected_bg)));
        assert!(line
            .spans
            .iter()
            .filter(|span| !span.content.is_empty())
            .all(|span| span.style.fg == Some(Color::Gray)));
    }
}

#[test]
fn diff_lines_to_text_keeps_word_highlight_inside_full_row_background() {
    let ds = theme::get_active_theme();
    let rendered = RenderedLine {
        gutter: "7 +".to_string(),
        content: vec![
            DiffSegment {
                text: "same ".to_string(),
                word_color: WordColor::None,
            },
            DiffSegment {
                text: "new".to_string(),
                word_color: WordColor::AddedWord,
            },
        ],
        padding: "  ".to_string(),
        line_color: LineColor::Added,
        dim: false,
    };
    let text = diff_lines_to_text(&[rendered], &MessagesRenderTheme::plain());
    let spans = &text.lines[0].spans;
    let line_bg = Some(parse_theme_color(ds.diffAdded));

    assert_eq!(spans[0].style.bg, line_bg);
    assert_eq!(spans[1].style.bg, line_bg);
    assert_eq!(spans[2].style.bg, Some(parse_theme_color(ds.diffAddedWord)));
    assert_eq!(spans[3].style.bg, line_bg);
}

#[test]
fn file_edit_body_widget_paints_write_preview_with_hidden_lines_footer() {
    use rebon_render::file_edit::FileEditRejectedProjection;

    let projection = FileEditRejectedProjection::WritePreview {
        summary: "User rejected write to new.txt".to_string(),
        preview: "line 1\nline 2\nline 3".to_string(),
        preview_width: 40,
        hidden_line_count: 5,
        file_path: "new.txt".to_string(),
    };
    let widget =
        FileEditBodyWidget::from_rejected(projection, MessagesRenderTheme::plain(), Style::new());
    let height = widget.height(50);
    assert!(height >= 6, "height should be at least 6, got {height}");

    let mut buf = make_buf(50, height + 1);
    widget.render_to_buffer(Rect::new(0, 0, 50, height + 1), &mut buf);
    let rendered: Vec<String> = (0..buf.area.height).map(|y| line_at(&buf, y)).collect();
    assert!(rendered[0].starts_with("●"));
    assert!(rendered
        .iter()
        .any(|line| line.contains("User rejected write to new.txt")));
    assert!(rendered.iter().any(|line| line.contains("line 1")));
    assert!(rendered.iter().any(|line| line.contains("line 3")));
    assert!(rendered.iter().any(|line| line.contains("... +5 lines")));
}

#[test]
fn file_edit_body_widget_paints_notebook_rejected_with_preview_block() {
    use rebon_render::file_edit::NotebookEditRejectedProjection;

    let projection = NotebookEditRejectedProjection {
        summary: "User rejected replace cell in demo.ipynb".to_string(),
        preview: Some("print(1)".to_string()),
        preview_file_path: Some("demo.py".to_string()),
    };
    let widget = FileEditBodyWidget::from_notebook_rejected(
        projection,
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    let height = widget.height(50);
    let mut buf = make_buf(50, height + 1);
    widget.render_to_buffer(Rect::new(0, 0, 50, height + 1), &mut buf);
    let rendered: Vec<String> = (0..buf.area.height).map(|y| line_at(&buf, y)).collect();
    assert!(rendered[0].starts_with("●"));
    assert!(rendered
        .iter()
        .any(|line| line.contains("User rejected replace cell")));
    assert!(rendered.iter().any(|line| line.contains("print(1)")));
}

#[test]
fn fallback_body_widget_paints_bordered_error_and_footer() {
    let projection = FallbackToolUseErrorProjection {
        error_text: "Error: bad input".to_string(),
        hidden_line_count: 3,
        footer: Some("... +3 lines (Ctrl+O to see all)".to_string()),
    };
    let widget = FallbackBodyWidget::new(projection, MessagesRenderTheme::plain(), Style::new());
    let height = widget.height(50);
    // 2 border lines + 1 content + 1 footer.
    assert!(height >= 4, "height should be at least 4, got {height}");

    let mut buf = make_buf(50, height + 1);
    widget.render_to_buffer(Rect::new(0, 0, 50, height + 1), &mut buf);
    let rendered: Vec<String> = (0..buf.area.height).map(|y| line_at(&buf, y)).collect();
    assert!(rendered[0].starts_with("● "));
    assert!(rendered.iter().any(|line| line.contains("Tool Use Error")));
    assert!(rendered
        .iter()
        .any(|line| line.contains("Error: bad input")));
    assert!(rendered.iter().any(|line| line.contains("... +3 lines")));
}

fn system_margin_widget(
    message: rebon_render::SystemTextProjectionInput,
    add_margin: bool,
    verbose: bool,
) -> SystemTextBodyWidget {
    let projection =
        rebon_render::project_system_text_message(&rebon_render::SystemTextMessageInput {
            message,
            add_margin,
            verbose,
            is_transcript_mode: true,
            background: None,
            terminal_columns: Some(80),
        });
    SystemTextBodyWidget::from_projection(projection, MessagesRenderTheme::plain(), Style::new())
}

#[test]
fn system_margin_variants_share_height_and_clipped_paint() {
    use rebon_render::{
        SystemMemorySavedInput, SystemTextProjectionInput as Input, SystemTurnDurationInput,
    };

    let messages = [
        Input::TurnDuration(SystemTurnDurationInput {
            duration_ms: 383_000,
            budget_tokens: Some(42),
            budget_limit: Some(100),
            budget_nudges: 1,
            show_turn_duration: true,
            completion_verb: "Worked".into(),
            background_task_summary: Some("2 tasks".into()),
            tick_glyph: String::new(),
        }),
        Input::MemorySaved(SystemMemorySavedInput {
            written_paths: vec!["notes/one.txt".into(), "notes/two.txt".into()],
            verb: None,
            team_count: 0,
        }),
        Input::BridgeStatus {
            url: "https://example.com/session".into(),
            upgrade_nudge: Some("Upgrade available".into()),
        },
        Input::PermissionRetry {
            commands: vec!["cargo test".into(), "cargo check".into()],
        },
        Input::AwaySummary {
            content: "Completed the background work".into(),
        },
        Input::AgentsKilled,
        Input::ScheduledTaskFire {
            content: "Scheduled reminder fired".into(),
        },
        Input::Thinking {
            content: "Planning the next action".into(),
            internal_build: true,
        },
        Input::Generic {
            subtype: "turn_duration".into(),
            level: "info".into(),
            content: Some("Worked 6m 23s".into()),
        },
    ];
    for message in messages {
        for verbose in [false, true] {
            for width in [18, 80] {
                let baseline = system_margin_widget(message.clone(), false, verbose);
                let body_height = baseline.height(width);
                let mut body = make_buf(width + 4, body_height + 4);
                baseline.render_to_buffer(Rect::new(2, 1, width, body_height), &mut body);
                assert!(
                    !line_at(&body, body_height).trim().is_empty(),
                    "{message:?}"
                );
                for add_margin in [false, true] {
                    let widget = system_margin_widget(message.clone(), add_margin, verbose);
                    let margin = u16::from(add_margin);
                    let height = body_height + margin;
                    assert_eq!(
                        widget.height(width),
                        height,
                        "{message:?}, {add_margin}, {verbose}"
                    );
                    let mut full = make_buf(width + 4, height + 4);
                    widget
                        .clone()
                        .render_to_buffer(Rect::new(2, 1, width, height), &mut full);
                    if add_margin {
                        assert!(line_at(&full, 1).is_empty(), "{message:?}");
                    }
                    for y in 1..=body_height {
                        for x in 0..body.area.width {
                            assert_eq!(full[(x, y + margin)], body[(x, y)], "{message:?}");
                        }
                    }
                    for clip in 0..=height + 1 {
                        let mut actual = make_buf(width + 4, height + 4);
                        let area = Rect::new(2, 1, width, clip);
                        widget.clone().render_to_buffer(area, &mut actual);
                        for y in 0..actual.area.height {
                            for x in 0..actual.area.width {
                                if area.contains((x, y).into()) {
                                    assert_eq!(
                                        actual[(x, y)],
                                        full[(x, y)],
                                        "{message:?}, {add_margin}, {verbose}, {width}, {clip}"
                                    );
                                } else {
                                    assert_eq!(actual[(x, y)].symbol(), " ");
                                }
                            }
                        }
                    }
                    let mut empty = make_buf(4, 4);
                    widget.render_to_buffer(Rect::new(2, 1, 0, 3), &mut empty);
                    assert_eq!(empty, make_buf(4, 4));
                }
            }
        }
    }
}

#[test]
fn system_margin_does_not_change_api_error_or_stop_hook_spacing() {
    use rebon_render::{SystemStopHookSummaryInput, SystemTextProjectionInput as Input};

    for verbose in [false, true] {
        for hook_label in [None, Some("Checks".into())] {
            let messages = [
                Input::Generic {
                    subtype: "api_error".into(),
                    level: "error".into(),
                    content: Some("model stream error: boom".into()),
                },
                Input::StopHookSummary(SystemStopHookSummaryInput {
                    hook_count: 1,
                    hook_infos: vec![],
                    hook_errors: vec!["check failed".into()],
                    prevented_continuation: true,
                    stop_reason: Some("policy".into()),
                    hook_label,
                    total_duration_ms: None,
                    internal_build: false,
                    hook_timing_display_threshold_ms: 0,
                    terminal_columns: 80,
                }),
            ];
            for message in messages {
                let baseline = system_margin_widget(message.clone(), false, verbose);
                let with_margin = system_margin_widget(message.clone(), true, verbose);
                let height = baseline.height(80);
                assert_eq!(with_margin.height(80), height, "{message:?}");
                let mut expected = make_buf(80, height + 1);
                baseline.render_to_buffer(expected.area, &mut expected);
                let mut actual = make_buf(80, height + 1);
                with_margin.render_to_buffer(actual.area, &mut actual);
                assert!(!line_at(&actual, 0).trim().is_empty(), "{message:?}");
                assert_eq!(actual, expected, "{message:?}");
            }
        }
    }
}

#[test]
fn system_margin_empty_and_hidden_projections_keep_existing_height() {
    use rebon_render::{SystemApiErrorInput, SystemTextProjectionInput as Input};

    for add_margin in [false, true] {
        for verbose in [false, true] {
            let messages = [
                Input::Generic {
                    subtype: "informational".into(),
                    level: "warning".into(),
                    content: None,
                },
                Input::Thinking {
                    content: "hidden".into(),
                    internal_build: false,
                },
                Input::ApiError(SystemApiErrorInput {
                    retry_attempt: 1,
                    formatted_error: "retrying".into(),
                    retry_in_ms: 1000,
                    max_retries: 5,
                    verbose,
                    countdown_ms: 0,
                    api_timeout_ms: None,
                }),
            ];
            for message in messages {
                let widget = system_margin_widget(message, add_margin, verbose);
                let (rows, block) = system_text_layout(&widget.projection, &widget.theme);
                assert!(rows.is_empty());
                assert!(block.is_none());
                assert_eq!(widget.height(80), 1);
                let mut buf = make_buf(80, 3);
                widget.render_to_buffer(buf.area, &mut buf);
                assert_eq!(buf, make_buf(80, 3));
            }
        }
    }
}

#[test]
fn system_text_body_widget_paints_bordered_stop_hook_block_with_status() {
    let display = rebon_render::StopHookSummaryDisplay::Default {
        margin_top: 0,
        background: None,
        marker: rebon_render::SystemVisualMarker::BlackCircle,
        summary: "Ran 2 stop hooks".to_string(),
        detail_lines: vec![],
        prevented_line: Some("\u{23BF}  Stopped by policy".to_string()),
        error_lines: vec!["\u{23BF}  Stop hook error: boom".to_string()],
        show_expand_hint: false,
        width: 40,
    };
    let widget = SystemTextBodyWidget::from_stop_hook_summary(
        display,
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    let height = widget.height(50);
    // summary + bordered block (2 borders + 2 body rows)
    assert!(height >= 5, "height should be at least 5, got {height}");

    let mut buf = make_buf(50, height + 1);
    widget.render_to_buffer(Rect::new(0, 0, 50, height + 1), &mut buf);
    let rendered: Vec<String> = (0..buf.area.height).map(|y| line_at(&buf, y)).collect();
    assert!(rendered[0].starts_with("Ran 2 stop hooks"));
    assert!(rendered
        .iter()
        .any(|line| line.contains("Ran 2 stop hooks")));
    assert!(rendered.iter().any(|line| line.contains("Stop Hooks")));
    assert!(rendered
        .iter()
        .any(|line| line.contains("Stopped by policy")));
    assert!(rendered
        .iter()
        .any(|line| line.contains("Stop hook error: boom")));
}

#[test]
fn system_text_body_widget_labeled_branch_hides_block_when_no_transcript_lines() {
    let display = rebon_render::StopHookSummaryDisplay::Labeled {
        summary: "Ran 3 PreToolUse hooks".to_string(),
        transcript_lines: vec![],
    };
    let widget = SystemTextBodyWidget::from_stop_hook_summary(
        display,
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    // No block — just the summary row.
    assert_eq!(widget.height(40), 1);
}

#[test]
fn user_text_plan_body_widget_paints_plan_block_with_line_count() {
    let widget = UserTextPlanBodyWidget::new(
        "# Goal\n- Step 1\n- Step 2\n- Step 3".to_string(),
        MessagesRenderTheme::plain(),
        Style::new(),
    );
    let height = widget.height(50);
    // Two rules + 4 content rows = 6.
    assert!(height >= 6, "height should be at least 6, got {height}");

    let mut buf = make_buf(50, height + 1);
    widget.render_to_buffer(Rect::new(0, 0, 50, height + 1), &mut buf);
    let rendered: Vec<String> = (0..buf.area.height).map(|y| line_at(&buf, y)).collect();
    // The opening rule carries the title and runs above the gutter glyph.
    assert!(
        rendered[0].starts_with("── Plan to implement ─"),
        "{rendered:?}"
    );
    assert!(rendered[1].starts_with("● "), "{rendered:?}");
    assert!(rendered.iter().any(|line| line.contains("# Goal")));
    assert!(rendered.iter().any(|line| line.contains("- Step 2")));
    // The closing rule reports the line count.
    assert!(
        rendered.iter().any(|line| line.contains("4 lines")),
        "expected 4 lines in the closing rule, got {rendered:?}"
    );
    // No side borders anywhere in the block.
    assert!(
        !rendered.iter().any(|line| line.contains('│')),
        "{rendered:?}"
    );
}
