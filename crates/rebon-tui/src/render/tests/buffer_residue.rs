use super::super::*;
use super::common::*;
use crate::state::reducer;
use crate::StreamingToolUse;
use rebon_types::DiffContent;
use serde_json::json;

/// Pre-fill a buffer with a recognisable dirty sentinel so we can
/// later assert the renderer overwrote (or cleared) every cell it
/// claims as its own.
fn fill_dirty(buf: &mut Buffer) {
    let area = *buf.area();
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
                cell.set_symbol("█");
            }
        }
    }
}

fn assert_area_has_no_dirty_cells(buf: &Buffer, area: Rect) {
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            let cell = buf.cell((x, y)).expect("cell inside test buffer");
            assert_ne!(cell.symbol(), "█", "dirty glyph remained at ({x}, {y})");
        }
    }
}

fn buffer_contains(buf: &Buffer, needle: &str) -> bool {
    (0..buf.area().height)
        .map(|y| row_text(buf, y))
        .any(|line| line.contains(needle))
}

/// Regression for the Ctrl+O Normal→Compact ghost residue.
///
/// In Normal verbosity each streaming Read tool card paints a
/// preview of its body. After the user presses Ctrl+O to collapse
/// back to Compact the renderer paints a single aggregated
/// `Searching … reading … (ctrl+o to expand)` summary line, which
/// is much shorter than the per-tool cards Normal had drawn.
///
/// `render_transcript_cached_with_running_hints` is supposed to
/// reset every cell in the transcript area at the start of each
/// frame so no body fragment from the Normal pass survives in
/// rows the Compact pass no longer touches.
#[test]
fn ctrl_o_toggle_clears_normal_mode_body_residue() {
    let width = 80u16;
    let height = 30u16;
    let area = Rect::new(0, 0, width, height);
    let theme = RenderTheme::plain();

    let body_a = (0..6)
        .map(|i| format!("//! line {i} from src/a.rs body"))
        .collect::<Vec<_>>()
        .join("\n");
    let body_b = (0..6)
        .map(|i| format!("//! line {i} from src/b.rs body"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut state = AppState::new();
    state.overlay.upsert_streaming_tool_use(StreamingToolUse {
        call_id: "tool-a".into(),
        tool_name: "Read".into(),
        kind: ToolKind::Read,
        status: ToolCallStatus::Completed,
        title: None,
        content: Some(vec![ToolCallContent::Content(
            rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: body_a.clone(),
                    annotations: None,
                }),
            },
        )]),
        locations: None,
        raw_input: Some(HashMap::from([("file_path".into(), json!("src/a.rs"))])),
        raw_output: None,
    });
    state.overlay.upsert_streaming_tool_use(StreamingToolUse {
        call_id: "tool-b".into(),
        tool_name: "Read".into(),
        kind: ToolKind::Read,
        status: ToolCallStatus::Completed,
        title: None,
        content: Some(vec![ToolCallContent::Content(
            rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: body_b.clone(),
                    annotations: None,
                }),
            },
        )]),
        locations: None,
        raw_input: Some(HashMap::from([("file_path".into(), json!("src/b.rs"))])),
        raw_output: None,
    });

    let mut cache = TranscriptMeasureCache::new();

    // Frame 1 — Normal verbosity paints the bodies of both tools
    // (Compact would collapse them into a single summary line).
    let mut buf = new_buf(width, height);
    render_transcript_cached(
        &state,
        area,
        &mut buf,
        &theme,
        0,
        ToolOutputVerbosity::Normal,
        0,
        None,
        &mut cache,
    );
    assert!(
        buffer_contains(&buf, "//! line 0 from src/a.rs"),
        "Normal mode should paint tool-a body: {:?}",
        all_rows(&buf)
    );
    assert!(
        buffer_contains(&buf, "//! line 0 from src/b.rs"),
        "Normal mode should paint tool-b body: {:?}",
        all_rows(&buf)
    );

    // Frame 2 — Compact verbosity reuses the *same* buffer. The
    // top-level clear inside `render_transcript_cached_with_running_hints`
    // must wipe the previous body so no `//!` fragments survive.
    render_transcript_cached(
        &state,
        area,
        &mut buf,
        &theme,
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
    );
    let rows_after = all_rows(&buf);
    assert!(
        !buffer_contains(&buf, "//!"),
        "Ctrl+O collapse must clear Normal-mode body residue: {rows_after:?}"
    );
    assert!(
        !buffer_contains(&buf, "from src/a.rs body"),
        "tool-a body line survived collapse: {rows_after:?}"
    );
    assert!(
        !buffer_contains(&buf, "from src/b.rs body"),
        "tool-b body line survived collapse: {rows_after:?}"
    );
}

/// Regression for the committed-transcript counterpart of the
/// Ctrl+O ghost: a chain of completed Read/Grep tool uses collapses
/// in Compact mode but expands to per-tool cards in Normal. When
/// the user pops back to Compact, the previous expanded body must
/// not leak into the rows the new collapsed summary no longer
/// touches.
#[test]
fn ctrl_o_committed_collapse_clears_normal_mode_residue() {
    use crate::state::{reducer, Action};

    let width = 100u16;
    let height = 30u16;
    let area = Rect::new(0, 0, width, height);
    let theme = RenderTheme::plain();

    let mut state = AppState::new();
    reducer(&mut state, Action::Commit(user("u0", "scan files")));
    reducer(
        &mut state,
        Action::Commit(assistant_tool_uses(
            "a1",
            vec![
                ("toolu_1", "Grep", json!({ "pattern": "old-pattern" })),
                ("toolu_2", "Read", json!({ "file_path": "old.rs" })),
            ],
        )),
    );
    let body = (0..8)
        .map(|i| format!("//! committed line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    reducer(
        &mut state,
        Action::Commit(user_tool_results(
            "u1",
            vec![
                ("toolu_1", "matches for old-pattern", None),
                ("toolu_2", body.as_str(), None),
            ],
        )),
    );

    let mut cache = TranscriptMeasureCache::new();
    let mut buf = new_buf(width, height);

    // Frame 1 — Normal verbosity expands the chain, so each tool
    // result body is painted.
    render_transcript_cached(
        &state,
        area,
        &mut buf,
        &theme,
        0,
        ToolOutputVerbosity::Normal,
        0,
        None,
        &mut cache,
    );
    assert!(
        buffer_contains(&buf, "committed line 0"),
        "Normal mode should paint committed body: {:?}",
        all_rows(&buf)
    );

    // Frame 2 — Compact verbosity collapses the chain. The top
    // level clear must wipe every line the expanded view occupied.
    render_transcript_cached(
        &state,
        area,
        &mut buf,
        &theme,
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
    );
    let rows_after = all_rows(&buf);
    assert!(
        !buffer_contains(&buf, "committed line"),
        "Ctrl+O committed-collapse must clear Normal-mode residue: {rows_after:?}"
    );
    assert!(
        !buffer_contains(&buf, "//!"),
        "leftover doc-comment glyph survived collapse: {rows_after:?}"
    );
}

/// Regression for the user-reported screenshot: a long chain of
/// streaming tool uses (5 search + 12 read) collapses to a single
/// `Searching … reading … (ctrl+o to expand)` summary in Compact.
/// In Normal verbosity each of the 17 tools paints its own card
/// with body preview, producing tall content. After Ctrl+O flips
/// back to Compact the Normal-mode body fragments must NOT survive
/// in the rows the new short summary no longer touches. The
/// preceding assistant text uses CJK wide characters because that
/// is what the user reported the bug against — wide-char paths
/// have an extra `invalidated` mechanism in the ratatui diff that
/// could in principle leave continuation residue.
#[test]
fn ctrl_o_many_streaming_tools_with_cjk_clears_normal_residue() {
    let width = 100u16;
    let height = 40u16;
    let area = Rect::new(0, 0, width, height);
    let theme = RenderTheme::plain();

    let mut state = AppState::new();
    // Mirror the Chinese assistant text from the screenshot. Using
    // a long CJK paragraph maximises the number of wide-char cells
    // we paint into the buffer before the streaming overlay.
    let cjk_assistant = "我已经检查了你的代码库，发现以下几个问题需要解决。\
                          首先，渲染管线在 Ctrl+O 切换时存在残留字符的问题。";
    reducer(
        &mut state,
        crate::state::Action::Commit(assistant_text("a-cjk", cjk_assistant)),
    );

    // 5 streaming Grep tools, each with several body lines.
    for i in 0..5 {
        let pattern = format!("pattern-{i}");
        let body = (0..3)
            .map(|j| format!("//! grep hit {i}.{j} → src/lib_{i}.rs:{}", 10 + j))
            .collect::<Vec<_>>()
            .join("\n");
        state.overlay.upsert_streaming_tool_use(StreamingToolUse {
            call_id: format!("grep-{i}"),
            tool_name: "Grep".into(),
            kind: ToolKind::Search,
            status: ToolCallStatus::Completed,
            title: None,
            content: Some(vec![ToolCallContent::Content(
                rebon_types::RegularContent {
                    content: ContentBlock::Text(rebon_types::TextContent {
                        text: body,
                        annotations: None,
                    }),
                },
            )]),
            locations: None,
            raw_input: Some(HashMap::from([("pattern".into(), json!(pattern))])),
            raw_output: None,
        });
    }
    // 12 streaming Read tools, each with 4-5 body lines so Normal
    // verbosity paints a tall card per tool.
    for i in 0..12 {
        let path = format!("src/file_{i}.rs");
        let body = (0..4)
            .map(|j| format!("//! file {i} line {j} body content using identifiers"))
            .collect::<Vec<_>>()
            .join("\n");
        state.overlay.upsert_streaming_tool_use(StreamingToolUse {
            call_id: format!("read-{i}"),
            tool_name: "Read".into(),
            kind: ToolKind::Read,
            status: ToolCallStatus::Completed,
            title: None,
            content: Some(vec![ToolCallContent::Content(
                rebon_types::RegularContent {
                    content: ContentBlock::Text(rebon_types::TextContent {
                        text: body,
                        annotations: None,
                    }),
                },
            )]),
            locations: None,
            raw_input: Some(HashMap::from([("file_path".into(), json!(path))])),
            raw_output: None,
        });
    }

    let mut cache = TranscriptMeasureCache::new();
    let mut buf = new_buf(width, height);

    // Frame 1 — Normal verbosity expands every tool body. Total
    // content is way taller than the viewport so render must
    // clip / scroll to fit. We deliberately use scroll_offset=0
    // so the TOP of the long content is what's visible — that
    // way the Compact frame's short summary lands at the same
    // top rows the bodies just occupied, making any residue
    // collide with the new short content rather than being hidden
    // off-screen.
    render_transcript_cached(
        &state,
        area,
        &mut buf,
        &theme,
        0,
        ToolOutputVerbosity::Normal,
        0,
        None,
        &mut cache,
    );
    assert!(
        buffer_contains(&buf, "//! file") || buffer_contains(&buf, "//! grep hit"),
        "Normal mode must paint at least one expanded tool body: {:?}",
        all_rows(&buf)
    );

    // Frame 2 — Compact: 17 tools collapse into a single summary
    // line "Searching for 5 patterns, reading 12 files…". Every
    // cell the Normal-mode bodies occupied must be cleared.
    render_transcript_cached(
        &state,
        area,
        &mut buf,
        &theme,
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
    );
    let rows_after = all_rows(&buf);
    assert!(
        !buffer_contains(&buf, "//!"),
        "Ctrl+O Normal→Compact left `//!` body residue: {rows_after:?}"
    );
    assert!(
        !buffer_contains(&buf, "body content"),
        "Ctrl+O Normal→Compact left `body content` residue: {rows_after:?}"
    );
    assert!(
        !buffer_contains(&buf, "grep hit"),
        "Ctrl+O Normal→Compact left `grep hit` residue: {rows_after:?}"
    );
    assert!(
        !buffer_contains(&buf, "using identifiers"),
        "Ctrl+O Normal→Compact left `using identifiers` residue: {rows_after:?}"
    );
    // Sanity: the collapsed summary itself must be present.
    assert!(
        buffer_contains(&buf, "(Ctrl+O to expand)")
            || buffer_contains(&buf, "Searching")
            || buffer_contains(&buf, "Reading"),
        "Compact summary line missing after collapse: {rows_after:?}"
    );
}

/// End-to-end regression for the user-reported Read-widget residue.
///
/// The buffer-only `ctrl_o_*` tests above exercise
/// `render_transcript_cached`, but they paint into the same buffer
/// twice without going through `Buffer::diff` — the residue gets
/// overwritten by the top-level clear regardless of whether the
/// diff layer would actually have flushed those cells. This test
/// drives the full `Terminal::draw` pipeline (paint → diff → swap →
/// backend.draw) against a `TestBackend`, whose internal buffer
/// reflects only what the diff actually emitted to the simulated
/// terminal screen. If a cell painted in frame 1 is left untouched
/// by frame 2's diff, the residue survives in the backend buffer
/// even though the back buffer itself was cleared correctly.
#[test]
fn ctrl_o_through_diff_pipeline_clears_read_widget_residue() {
    use crate::state::{reducer, Action};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let width = 100u16;
    let height = 40u16;
    let theme = RenderTheme::plain();

    // 12 completed Read tools with bodies that resemble Rust
    // source-line content the user reported residue from. The
    // collapse threshold is `>= 2` so 12 reads always fold into a
    // single Compact summary, while Verbose paints a per-tool card
    // for each one.
    let mut state = AppState::new();
    reducer(&mut state, Action::Commit(user("u0", "look at files")));

    let tool_specs: Vec<(String, String, String)> = (0..12usize)
        .map(|i| {
            let id = format!("toolu_{i}");
            let path = format!("src/file_{i}.rs");
            let body = (0..4)
                .map(|j| format!("//! file {i} line {j} body content using identifiers"))
                .collect::<Vec<_>>()
                .join("\n");
            (id, path, body)
        })
        .collect();

    let assistant_specs: Vec<(&str, &str, serde_json::Value)> = tool_specs
        .iter()
        .map(|(id, path, _)| (id.as_str(), "Read", json!({ "file_path": path })))
        .collect();
    reducer(
        &mut state,
        Action::Commit(assistant_tool_uses("a1", assistant_specs)),
    );

    let result_specs: Vec<(&str, &str, Option<bool>)> = tool_specs
        .iter()
        .map(|(id, _, body)| (id.as_str(), body.as_str(), None))
        .collect();
    reducer(
        &mut state,
        Action::Commit(user_tool_results("u1", result_specs)),
    );

    let mut cache = TranscriptMeasureCache::new();
    let area = Rect::new(0, 0, width, height);

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("create test terminal");

    // Frame 1: Normal verbosity paints per-tool cards. Each Read
    // gets a header + body block, which collectively fills more
    // than the viewport's height. The diff sees an initially-empty
    // simulated screen and emits a write for every painted cell.
    // Normal (not Verbose) is the verbosity the user actually
    // toggles into via Ctrl+O — see runner/mod.rs:2107-2112,
    // which cycles `Compact ↔ Normal` for that key.
    terminal
        .draw(|frame| {
            render_transcript_cached_with_running_hints(
                &state,
                area,
                frame.buffer_mut(),
                &theme,
                0,
                ToolOutputVerbosity::Normal,
                0,
                None,
                &mut cache,
                false,
                TranscriptRenderExtras::empty(),
            );
        })
        .expect("frame 1 draw");

    // Sanity: the simulated terminal screen now shows the bodies.
    let frame1_screen: Vec<String> = (0..height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect();
    assert!(
        frame1_screen.iter().any(|line| line.contains("//!")),
        "frame 1 must paint Read body content to TestBackend: {frame1_screen:?}"
    );

    // Frame 2: Compact verbosity collapses the chain to one summary
    // line. Every cell that frame 1 had painted with `//!` body
    // content must be reset to a space on the simulated screen.
    terminal
        .draw(|frame| {
            render_transcript_cached_with_running_hints(
                &state,
                area,
                frame.buffer_mut(),
                &theme,
                0,
                ToolOutputVerbosity::Compact,
                0,
                None,
                &mut cache,
                false,
                TranscriptRenderExtras::empty(),
            );
        })
        .expect("frame 2 draw");

    let frame2_screen: Vec<String> = (0..height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect();
    assert!(
        !frame2_screen.iter().any(|line| line.contains("//!")),
        "Ctrl+O Normal→Compact left `//!` residue on TestBackend screen: {frame2_screen:?}"
    );
    assert!(
        !frame2_screen
            .iter()
            .any(|line| line.contains("body content")),
        "Ctrl+O Normal→Compact left `body content` residue: {frame2_screen:?}"
    );
    assert!(
        !frame2_screen
            .iter()
            .any(|line| line.contains("using identifiers")),
        "Ctrl+O Normal→Compact left `using identifiers` residue: {frame2_screen:?}"
    );
}

/// Targeted regression for the Read-tool body residue path. The
/// original v94d9e54 fix landed `clear_buffer_area` inside
/// `render_gutter_lines` and `render_wrapped` after users reported
/// stale Read body glyphs surviving a Verbose→Compact transition;
/// it was reverted, then the same residue resurfaced. This test
/// pins the invariant at the paint level: even when the caller
/// hands us a buffer pre-filled with a dirty sentinel, painting a
/// Read tool card via `render_streaming_tool_use` must reset every
/// cell in the painted band.
///
/// We pre-fill the whole buffer with `█` cells, then render a Read
/// tool whose body wraps over multiple rows. The painted region
/// (header + wrapped body) must contain no `█` glyph afterward.
/// Without the fix this test would catch the "//!" / "body content"
/// residue users see after pressing Ctrl+O twice on a Read-heavy
/// transcript.
#[test]
fn render_streaming_read_tool_clears_dirty_cells_before_painting_body() {
    let width = 60u16;
    let height = 10u16;
    let area = Rect::new(0, 0, width, height);
    let theme = RenderTheme::plain();

    let body = "//! crate-level doc\n\
                pub fn alpha() {}\n\
                pub fn beta_with_a_long_name_that_wraps_past_the_gutter() {}";
    let tool = StreamingToolUse {
        call_id: "toolu_read".into(),
        tool_name: "Read".into(),
        kind: ToolKind::Read,
        title: Some("Read src/lib.rs".into()),
        raw_input: Some({
            let mut m = HashMap::new();
            m.insert("file_path".into(), serde_json::json!("src/lib.rs"));
            m
        }),
        content: Some(vec![ToolCallContent::Content(
            rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: body.into(),
                    annotations: None,
                }),
            },
        )]),
        raw_output: None,
        locations: None,
        status: ToolCallStatus::Completed,
    };

    let mut buf = new_buf(width, height);
    fill_dirty(&mut buf);

    let used = render_streaming_tool_use(
        &tool,
        area,
        &mut buf,
        &theme,
        ToolOutputVerbosity::Verbose,
        TranscriptRenderExtras::empty(),
    );

    assert!(used > 0, "Read tool render must consume rows");
    assert_area_has_no_dirty_cells(&buf, Rect::new(area.x, area.y, area.width, used));

    let painted: Vec<String> = (0..used).map(|y| row_text(&buf, y)).collect();
    let joined = painted.join("\n");
    assert!(joined.contains("Read"), "header missing: {painted:?}");
    assert!(joined.contains("//!"), "body missing: {painted:?}");
}

#[test]
fn render_streaming_edit_tool_clears_dirty_cells_before_painting_diff() {
    let width = 80u16;
    let height = 12u16;
    let area = Rect::new(0, 0, width, height);
    let theme = RenderTheme::plain();
    let original = "const setup = () => {\n    const removed = true;\n    return removed;\n};\n";
    let tool = StreamingToolUse {
        call_id: "toolu_edit".into(),
        tool_name: "Edit".into(),
        kind: ToolKind::Edit,
        title: None,
        raw_input: Some(HashMap::from([("file_path".into(), json!("src/app.ts"))])),
        content: Some(vec![ToolCallContent::Diff(DiffContent {
            path: "src/app.ts".into(),
            old_text: Some("    const removed = true;".into()),
            new_text: "    const added = false;".into(),
        })]),
        raw_output: Some(HashMap::from([
            ("type".into(), json!("update")),
            ("originalFile".into(), json!(original)),
        ])),
        locations: None,
        status: ToolCallStatus::Completed,
    };

    let mut buf = new_buf(width, height);
    fill_dirty(&mut buf);

    let used = render_streaming_tool_use(
        &tool,
        area,
        &mut buf,
        &theme,
        ToolOutputVerbosity::Compact,
        TranscriptRenderExtras::empty(),
    );

    assert!(used > 0, "Edit tool render must consume rows");
    assert_area_has_no_dirty_cells(&buf, Rect::new(area.x, area.y, area.width, used));

    let painted: Vec<String> = (0..used).map(|y| row_text(&buf, y)).collect();
    let joined = painted.join("\n");
    assert!(
        joined.contains("Update(src/app.ts)"),
        "header missing: {painted:?}"
    );
    assert!(
        joined.contains("Added 1 line, removed 1 line"),
        "summary missing: {painted:?}"
    );
    assert!(joined.contains("const added"), "diff missing: {painted:?}");
}

#[test]
fn compact_edit_tool_caps_large_diff_preview() {
    let width = 80u16;
    let height = 20u16;
    let area = Rect::new(0, 0, width, height);
    let theme = RenderTheme::plain();
    let new_text = (0..20)
        .map(|i| format!("const added_{i} = {i};"))
        .collect::<Vec<_>>()
        .join("\n");
    let tool = StreamingToolUse {
        call_id: "toolu_edit_large".into(),
        tool_name: "Edit".into(),
        kind: ToolKind::Edit,
        title: None,
        raw_input: Some(HashMap::from([("file_path".into(), json!("src/app.ts"))])),
        content: Some(vec![ToolCallContent::Diff(DiffContent {
            path: "src/app.ts".into(),
            old_text: None,
            new_text,
        })]),
        raw_output: Some(HashMap::from([("type".into(), json!("update"))])),
        locations: None,
        status: ToolCallStatus::Completed,
    };

    let mut buf = new_buf(width, height);
    let used = render_streaming_tool_use(
        &tool,
        area,
        &mut buf,
        &theme,
        ToolOutputVerbosity::Compact,
        TranscriptRenderExtras::empty(),
    );

    assert_eq!(used, 7);
    let painted: Vec<String> = (0..used).map(|y| row_text(&buf, y)).collect();
    let joined = painted.join("\n");
    assert!(joined.contains("Added 20 lines"));
    assert!(joined.contains("… +16 lines"));
}

/// Regression for cells outside the painted region: even when the
/// caller hands us a buffer the previous frame had filled with
/// arbitrary glyphs (a shrinking tool card, a closing dialog), the
/// transcript area's top-level clear must blank every cell it
/// claims so the ratatui diff can reset the terminal.
#[test]
fn render_transcript_clears_dirty_buffer_outside_painted_content() {
    use crate::state::{reducer, Action};

    let width = 80u16;
    let height = 12u16;
    let area = Rect::new(0, 0, width, height);
    let theme = RenderTheme::plain();

    let mut state = AppState::new();
    reducer(&mut state, Action::Commit(user("u0", "hi")));
    reducer(
        &mut state,
        Action::Commit(assistant_text("a0", "short reply")),
    );

    let mut buf = new_buf(width, height);
    fill_dirty(&mut buf);

    let mut cache = TranscriptMeasureCache::new();
    render_transcript_cached(
        &state,
        area,
        &mut buf,
        &theme,
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
    );

    for y in 0..height {
        for x in 0..width {
            let cell = buf.cell((x, y)).expect("cell inside test buffer");
            assert_ne!(
                cell.symbol(),
                "█",
                "dirty sentinel survived render_transcript at ({x}, {y})"
            );
        }
    }
}
