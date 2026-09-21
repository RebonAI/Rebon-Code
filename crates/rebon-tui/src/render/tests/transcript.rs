use super::super::*;
use super::common::*;
use crate::state::{reducer, Action, SealedPrefixFlushPolicy};
use serde_json::json;

#[test]
fn inline_pending_terminal_tool_cluster_renders_as_bounded_collapsed_live_tail() {
    let theme = RenderTheme::plain();
    let mut state = AppState::new();
    reducer(&mut state, Action::Commit(user("u1", "please inspect")));
    reducer(
        &mut state,
        Action::Commit(assistant_text("a1", "I will check.")),
    );
    state
        .overlay
        .upsert_streaming_tool_use(fake_streaming_tool("t1", "Read", "a.txt"));
    state
        .overlay
        .upsert_streaming_tool_use(fake_streaming_tool("t2", "Grep", "needle"));

    let mut buf = new_buf(80, 10);
    let result = render_transcript(
        &state,
        Rect::new(0, 0, 80, 10),
        &mut buf,
        &theme,
        usize::MAX,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let rows = normalized_visible_rows(&buf);

    assert_eq!(
        state.transcript.len(),
        2,
        "tool cluster must remain pending/live"
    );
    assert_eq!(state.overlay.tool_use_count(), 2);
    assert!(
        rows.iter()
            .any(|row| row.contains("Read") || row.contains("read"))
            && rows
                .iter()
                .any(|row| row.contains("Grep") || row.contains("Search")),
        "collapsed live summary should mention both pending tools: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("Ctrl+O") && row.contains("expand")),
        "live tool cluster should use collapse/expand machinery: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("Read (a.txt)"))
            && !rows.iter().any(|row| row.contains("Grep (needle)")),
        "compact live tail should not expose raw per-tool rows: {rows:?}"
    );
    assert!(
        result.total_lines <= 5,
        "collapsed live tail should stay bounded, got {} lines and rows {rows:?}",
        result.total_lines
    );
}

#[test]
fn inline_group_boundary_keeps_tool_cluster_live_until_following_text_is_sealed() {
    let theme = RenderTheme::plain();
    let mut state = AppState::new();
    state
        .overlay
        .blocks
        .push(crate::streaming::StreamingContentBlock::Text(
            "I will check.".into(),
        ));
    state
        .overlay
        .upsert_streaming_tool_use(fake_streaming_tool("t1", "Read", "a.txt"));
    state
        .overlay
        .upsert_streaming_tool_use(fake_streaming_tool("t2", "Grep", "needle"));

    reducer(
        &mut state,
        Action::FlushSealedPrefix {
            commit_timestamp: "t".into(),
            policy: SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary,
        },
    );
    assert_eq!(state.transcript.len(), 1);
    assert_eq!(state.overlay.tool_use_count(), 2);

    state
        .overlay
        .blocks
        .push(crate::streaming::StreamingContentBlock::Text(
            "Done.".into(),
        ));
    reducer(
        &mut state,
        Action::FlushSealedPrefix {
            commit_timestamp: "t".into(),
            policy: SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary,
        },
    );

    // With the live-tail text boundary, the tool cluster drains
    // immediately instead of waiting for the next tool start.
    assert_eq!(state.transcript.len(), 3);
    assert_eq!(state.overlay.blocks.len(), 1);

    state.overlay.upsert_streaming_tool_use(streaming_tool(
        "t3",
        "Bash",
        rebon_types::ToolKind::Execute,
        rebon_types::ToolCallStatus::InProgress,
        vec![("command", json!("cargo test"))],
    ));
    reducer(
        &mut state,
        Action::FlushSealedPrefix {
            commit_timestamp: "t".into(),
            policy: SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary,
        },
    );

    assert_eq!(state.transcript.len(), 4);
    assert_eq!(state.overlay.tool_use_count(), 1);

    let mut buf = new_buf(80, 10);
    render_transcript(
        &state,
        Rect::new(0, 0, 80, 10),
        &mut buf,
        &theme,
        usize::MAX,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let rows = normalized_visible_rows(&buf);
    assert!(
        rows.iter()
            .any(|row| row.contains("Ctrl+O") && row.contains("expand")),
        "committed tool cluster should still render as a collapsed block: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("Done.")),
        "sealed assistant text should commit with the tool cluster: {rows:?}"
    );
}

#[test]
fn layout_fast_path_rechecks_row_identity_across_synthetic_revision_stores() {
    // Regression: window stores rebuilt per call via `TranscriptStore::from_rows`
    // stamp a constant synthetic transcript revision. Feeding two DIFFERENT
    // stores through one long-lived cache used to satisfy the layout fast path
    // (same revision + same render key, row identity never re-checked) and
    // return the FIRST store's heights for the SECOND store's rows. The inline
    // commit path sized its scrollback insert from exactly that stale
    // measurement, clipping committed content out of scrollback permanently.
    let theme = RenderTheme::plain();
    let mut cache = TranscriptMeasureCache::new();
    let area = Rect::new(0, 0, 80, 200);

    let short_state = AppState {
        transcript: crate::TranscriptStore::from_rows(vec![assistant_text("a-short", "one line")]),
        overlay: StreamingOverlay::default(),
        flush_counter: 0,
    };
    let mut short_buf = new_buf(80, 200);
    let short = render_transcript_cached_with_running_hints(
        &short_state,
        area,
        &mut short_buf,
        &theme,
        usize::MAX,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        TranscriptRenderExtras::empty(),
    );

    let long_text = (0..30)
        .map(|i| format!("line_{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let long_state = AppState {
        transcript: crate::TranscriptStore::from_rows(vec![assistant_text("a-long", &long_text)]),
        overlay: StreamingOverlay::default(),
        flush_counter: 0,
    };
    assert_eq!(
        short_state.transcript.revision(),
        long_state.transcript.revision(),
        "from_rows stores share the synthetic revision — precondition for the regression"
    );

    let mut long_buf = new_buf(80, 200);
    let long = render_transcript_cached_with_running_hints(
        &long_state,
        area,
        &mut long_buf,
        &theme,
        usize::MAX,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        TranscriptRenderExtras::empty(),
    );

    assert!(
        long.total_lines >= 30,
        "second store must measure its own rows instead of reusing the first \
         store's cached layout (short={}, long={})",
        short.total_lines,
        long.total_lines
    );
    let rows = normalized_visible_rows(&long_buf);
    assert!(
        rows.iter().any(|row| row.contains("line_29")),
        "tail of the second store's content must be painted: {rows:?}"
    );
}

#[test]
fn collapsed_run_streaming_and_committed_have_same_compact_rows() {
    let theme = RenderTheme::plain();
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(fake_streaming_tool("t1", "Read", "a.txt"));
    overlay.append_streaming_thinking("first hidden reasoning\nlatest hidden reasoning");
    overlay.append_streaming_text("   \n\n");
    overlay.upsert_streaming_tool_use(fake_streaming_tool("t2", "Grep", "needle"));
    let mut streaming_buf = new_buf(80, 8);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 80, 8),
        &mut streaming_buf,
        &theme,
        ToolOutputVerbosity::Compact,
    );

    let rows = vec![
        assistant_tool("a1", "t1", "Read", "a.txt"),
        assistant_tool("a2", "t2", "Grep", "needle"),
    ];
    let mut committed_buf = new_buf(80, 8);
    render_committed_collapsed_group(
        &rows,
        &[0, 1],
        Rect::new(0, 0, 80, 8),
        &mut committed_buf,
        &theme,
        ToolOutputVerbosity::Compact,
        false,
        true,
    );

    let streaming_rows = semantic_rows(&streaming_buf);
    let committed_rows = semantic_rows(&committed_buf);
    assert_eq!(streaming_rows.len(), 1, "{streaming_rows:?}");
    assert_eq!(streaming_rows, committed_rows);
    assert!(
        streaming_rows[0].contains("Ctrl+O") && streaming_rows[0].contains("expand"),
        "summary row should carry the inline expand hint: {streaming_rows:?}"
    );
}

#[test]
fn render_transcript_inline_sliced_overlay_has_no_leading_blank_row() {
    let theme = RenderTheme::plain();
    let mut state = AppState::new();
    state
        .overlay
        .upsert_streaming_tool_use(fake_streaming_tool("t1", "Read", "a.txt"));
    state
        .overlay
        .append_streaming_thinking("absorbed thought\nlatest absorbed thought");
    state.overlay.append_streaming_text("  \n");
    state
        .overlay
        .upsert_streaming_tool_use(fake_streaming_tool("t2", "Grep", "needle"));
    state.overlay.append_streaming_text("  \n\n");
    state
        .overlay
        .upsert_streaming_tool_use(fake_streaming_tool("t3", "Read", "b.txt"));
    let mut buf = new_buf(80, 8);
    render_transcript(
        &state,
        Rect::new(0, 0, 80, 8),
        &mut buf,
        &theme,
        usize::MAX,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let rows = all_rows(&buf);
    assert_eq!(
        rows[0].trim().is_empty(),
        false,
        "inline overlay must not reserve a leading blank row: {rows:?}"
    );
    assert!(
        rows[0].contains("read") || rows[0].contains("Search") || rows[0].contains("patterns"),
        "first row should be the collapsed multi-tool summary: {rows:?}"
    );
    assert_eq!(
        rows.iter().take_while(|row| row.trim().is_empty()).count(),
        0,
        "no leading semantic blanks expected: {rows:?}"
    );
}

#[test]
fn paint_after_committed_does_not_starve_first_segment_in_one_row_area() {
    // Regression: a 1-row paint area with leading_margin=true used to
    // burn the single available row on the margin gap, leaving every
    // segment unrendered and emitting the BREAK warning observed in
    // `stream_dbg`. The overlay should still paint its first segment
    // jammed against the row above instead of yielding a blank row.
    let theme = RenderTheme::plain();
    let mut overlay = StreamingOverlay::new();
    overlay.append_streaming_text("visible streaming text");
    let mut buf = new_buf(80, 1);
    let mut cache = StreamingOverlayRenderCache::new();
    let used = render_streaming_overlay_with_options(
        &overlay,
        Rect::new(0, 0, 80, 1),
        &mut buf,
        &theme,
        ToolOutputVerbosity::Compact,
        &mut cache,
        StreamingOverlayRenderOptions::paint_after_committed(),
        TranscriptRenderExtras::empty(),
    );
    assert!(
        used >= 1,
        "first segment must paint in a 1-row area even with leading_margin=true (used={used})"
    );
    assert!(
        !row_text(&buf, 0).trim().is_empty(),
        "row 0 should carry overlay content, not the dropped margin: {:?}",
        row_text(&buf, 0)
    );
}

#[test]
fn collapsed_run_expanded_skips_hidden_children_without_double_blank_rows() {
    let theme = RenderTheme::plain();
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(fake_streaming_tool("t1", "Read", "a.txt"));
    overlay.append_streaming_text("   \n");
    overlay.append_streaming_thinking("first visible thought\nlatest visible thought");
    overlay.append_streaming_text("\n\t\n");
    overlay.upsert_streaming_tool_use(fake_streaming_tool("t2", "Read", "b.txt"));
    let mut buf = new_buf(80, 16);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 80, 16),
        &mut buf,
        &theme,
        ToolOutputVerbosity::Normal,
    );
    let rows = semantic_rows(&buf);
    assert_no_adjacent_blank_rows(&rows);
    assert_eq!(
        rows.iter().filter(|row| row.trim().is_empty()).count(),
        2,
        "only the two intentional visible-child separators should remain: {rows:?}"
    );
    let first_tool = row_y_containing(&rows, "Read (a.txt)");
    let thinking = row_y_containing(&rows, "first visible thought");
    let second_tool = row_y_containing(&rows, "Read (b.txt)");
    assert!(
        first_tool < thinking && thinking < second_tool,
        "thinking body should remain ordered around hidden whitespace children: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("latest visible thought")),
        "normal expanded collapsed run should show full thinking body text: {rows:?}"
    );
}

#[test]
fn render_transcript_screen_keeps_exact_gap_between_committed_and_live_overlay() {
    let theme = RenderTheme::plain();
    let mut state = AppState::new();
    reducer(&mut state, Action::Commit(assistant_text("a1", "done")));
    state
        .overlay
        .upsert_streaming_tool_use(fake_streaming_tool("t1", "Read", "a.txt"));
    state.overlay.append_streaming_text("  \n");
    state
        .overlay
        .upsert_streaming_tool_use(fake_streaming_tool("t2", "Grep", "needle"));
    let mut buf = new_buf(80, 12);
    render_transcript(
        &state,
        Rect::new(0, 0, 80, 12),
        &mut buf,
        &theme,
        usize::MAX,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let rows = all_rows(&buf);
    let done = row_y_containing(&rows, "done");
    let live = rows
        .iter()
        .position(|r| r.contains("Ctrl+O") && r.contains("expand"))
        .unwrap_or_else(|| panic!("live collapsed summary missing: {rows:?}"));
    assert_eq!(
        &rows[done + 1..live],
        &[String::new()],
        "full transcript should reserve exactly one blank row before live overlay: {rows:?}"
    );
}

#[test]
fn streaming_text_and_finalized_committed_text_render_same_normalized_rows() {
    let markdown = "# Header\n\n- first item\n- second item\n\n**bold** and *em*";
    let width = 72;
    let height = 24;
    let theme = RenderTheme::plain();

    let mut streaming_state = AppState::new();
    reducer(
        &mut streaming_state,
        Action::SetStreamingText(markdown.to_string()),
    );
    let mut streaming_buf = new_buf(width, height);
    render_transcript(
        &streaming_state,
        Rect::new(0, 0, width, height),
        &mut streaming_buf,
        &theme,
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );

    let mut committed_state = streaming_state.clone();
    reducer(
        &mut committed_state,
        Action::FinalizeTurn {
            commit_uuid: "a-final".into(),
            commit_timestamp: "t".into(),
        },
    );
    let mut committed_buf = new_buf(width, height);
    render_transcript(
        &committed_state,
        Rect::new(0, 0, width, height),
        &mut committed_buf,
        &theme,
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );

    let streaming_rows = normalized_visible_rows(&streaming_buf);
    let committed_rows = normalized_visible_rows(&committed_buf);
    assert_eq!(streaming_rows, committed_rows);
    let joined = streaming_rows.join("\n");
    assert!(joined.contains("Header"), "heading missing: {joined:?}");
    assert!(joined.contains("- first item"), "list missing: {joined:?}");
    assert!(
        joined.contains("bold") && joined.contains("em"),
        "emphasis text missing: {joined:?}"
    );
    assert!(
        !joined.contains("**bold**"),
        "markdown marker leaked: {joined:?}"
    );
}

#[test]
fn sticky_anchor_tracks_previous_user_when_scrolled_inside_assistant_response() {
    let theme = RenderTheme::plain();
    let mut state = AppState::new();
    reducer(
        &mut state,
        Action::Commit(user("u1", "first prompt anchor text")),
    );
    reducer(
        &mut state,
        Action::Commit(assistant_text(
            "a1",
            &(0..24)
                .map(|idx| format!("assistant line {idx}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )),
    );
    let mut buf = new_buf(80, 10);

    let result = render_transcript(
        &state,
        Rect::new(0, 0, 80, 10),
        &mut buf,
        &theme,
        2,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );

    assert_eq!(
        result.sticky_anchor,
        Some(TranscriptStickyAnchor {
            row_index: 0,
            scroll_offset: 0,
        })
    );
}

#[test]
fn streaming_text_cache_handles_incremental_markdown_deltas() {
    let width = 72;
    let height = 24;
    let theme = RenderTheme::plain();
    let mut state = AppState::new();
    let mut cache = TranscriptMeasureCache::new();

    let mut final_rows = Vec::new();
    let mut final_result = TranscriptRenderResult {
        total_lines: 0,
        render_y_end: 0,
        suffix_skip_lines: 0,
        sticky_anchor: None,
    };
    for delta in [
        "# Streaming header\n\n",
        "The assistant is rendering ",
        "**bo",
        "ld** markdown",
        " without source markers.\n\n- first item\n- second item",
    ] {
        state.overlay.append_streaming_text(delta);
        let mut buf = new_buf(width, height);
        final_result = render_transcript_cached(
            &state,
            Rect::new(0, 0, width, height),
            &mut buf,
            &theme,
            0,
            ToolOutputVerbosity::Compact,
            0,
            None,
            &mut cache,
        );
        final_rows = normalized_visible_rows(&buf);
    }

    let mut stable_buf = new_buf(width, height);
    let stable_result = render_transcript_cached(
        &state,
        Rect::new(0, 0, width, height),
        &mut stable_buf,
        &theme,
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
    );
    let stable_rows = normalized_visible_rows(&stable_buf);
    assert_eq!(final_result.total_lines, stable_result.total_lines);
    assert_eq!(final_rows, stable_rows);

    let joined = stable_rows.join("\n");
    assert!(
        joined.contains("Streaming header"),
        "heading missing: {joined:?}"
    );
    assert!(
        joined.contains("bold markdown"),
        "bold text missing: {joined:?}"
    );
    assert!(joined.contains("- first item"), "list missing: {joined:?}");
    assert!(
        !joined.contains("**bold**"),
        "markdown marker leaked after incremental cache reuse: {joined:?}"
    );
}

#[test]
fn streaming_text_cache_isolated_across_tool_separated_segments() {
    let mut overlay = StreamingOverlay::new();
    overlay.append_streaming_text("First segment has **bold-one** markdown.");
    overlay.upsert_streaming_tool_use(streaming_tool(
        "tool-middle",
        "Bash",
        ToolKind::Execute,
        ToolCallStatus::Completed,
        vec![("command", json!("echo middle"))],
    ));
    overlay.append_streaming_text("Second segment has **bold-two** markdown.");

    let mut cache = StreamingOverlayRenderCache::new();
    let mut buf = new_buf(96, 14);
    let used = render_streaming_overlay_cached(
        &overlay,
        Rect::new(0, 0, 96, 14),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
        &mut cache,
        TranscriptRenderExtras::empty(),
    );
    assert!(used > 0);

    let joined = normalized_visible_rows(&buf).join("\n");
    assert!(
        joined.contains("First segment"),
        "first text missing: {joined:?}"
    );
    assert!(
        joined.contains("bold-one"),
        "first markdown body missing: {joined:?}"
    );
    assert!(joined.contains("Bash"), "tool card missing: {joined:?}");
    assert!(
        joined.contains("echo middle"),
        "tool summary missing: {joined:?}"
    );
    assert!(
        joined.contains("Second segment"),
        "second text missing: {joined:?}"
    );
    assert!(
        joined.contains("bold-two"),
        "second markdown body missing: {joined:?}"
    );
    assert!(
        !joined.contains("**bold-one**") && !joined.contains("**bold-two**"),
        "markdown markers leaked across tool-separated text segments: {joined:?}"
    );
}

#[test]
fn streaming_overlay_cache_resets_after_shape_change() {
    let width = 72;
    let height = 16;
    let theme = RenderTheme::plain();
    let mut state = AppState::new();
    let mut cache = TranscriptMeasureCache::new();

    state
        .overlay
        .append_streaming_text("First overlay carries **stale-bold** text.");
    let mut first_buf = new_buf(width, height);
    render_transcript_cached(
        &state,
        Rect::new(0, 0, width, height),
        &mut first_buf,
        &theme,
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
    );
    state.overlay.clear();
    let mut empty_buf = new_buf(width, height);
    render_transcript_cached(
        &state,
        Rect::new(0, 0, width, height),
        &mut empty_buf,
        &theme,
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
    );
    state
        .overlay
        .append_streaming_text("Second overlay carries **fresh-bold** text.");
    let mut second_buf = new_buf(width, height);
    render_transcript_cached(
        &state,
        Rect::new(0, 0, width, height),
        &mut second_buf,
        &theme,
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
    );
    let joined = normalized_visible_rows(&second_buf).join("\n");
    assert!(
        joined.contains("Second overlay"),
        "new overlay missing: {joined:?}"
    );
    assert!(
        joined.contains("fresh-bold"),
        "new markdown body missing: {joined:?}"
    );
    assert!(
        !joined.contains("First overlay") && !joined.contains("stale-bold"),
        "previous overlay content leaked after clear: {joined:?}"
    );
    assert!(
        !joined.contains("**fresh-bold**"),
        "new overlay leaked markdown source markers: {joined:?}"
    );
}

#[test]
fn expanded_committed_tool_group_reports_height_beyond_measure_scratch() {
    let theme = RenderTheme::plain();
    let mut state = AppState::new();
    for i in 0..320 {
        let uuid = format!("a-read-{i}");
        let id = format!("toolu-read-{i}");
        let path = format!("file_{i}.rs");
        reducer(
            &mut state,
            Action::Commit(assistant_tool(&uuid, &id, "Read", &path)),
        );
    }

    let mut buf = new_buf(80, 12);
    let result = render_transcript(
        &state,
        Rect::new(0, 0, 80, 12),
        &mut buf,
        &theme,
        usize::MAX,
        ToolOutputVerbosity::Verbose,
        0,
        None,
    );
    let rows = all_rows(&buf);

    assert!(
        result.total_lines > 500,
        "expanded collapsed group height should not be capped by the scratch buffer: {result:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("file_319.rs")),
        "tail-follow should reach the end of the expanded group: {rows:?}"
    );
}

#[test]
fn expanded_committed_thinking_group_reports_height_beyond_measure_scratch() {
    let theme = RenderTheme::plain();
    let mut state = AppState::new();
    for i in 0..320 {
        reducer(
            &mut state,
            Action::Commit(assistant_thinking(
                &format!("a-thinking-{i}"),
                &format!("step {i}\ndetail {i}"),
            )),
        );
    }
    reducer(
        &mut state,
        Action::Commit(assistant_text("a-final", "final answer")),
    );

    let mut buf = new_buf(80, 12);
    let result = render_transcript(
        &state,
        Rect::new(0, 0, 80, 12),
        &mut buf,
        &theme,
        usize::MAX,
        ToolOutputVerbosity::Normal,
        0,
        None,
    );
    let rows = all_rows(&buf);

    assert!(
        result.total_lines > 620,
        "expanded thinking group height should not be capped by the scratch buffer: {result:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("detail 319")),
        "tail-follow should reach the end of the expanded thinking group: {rows:?}"
    );
}

#[test]
fn expanded_streaming_thinking_group_reports_height_beyond_measure_scratch() {
    let theme = RenderTheme::plain();
    let mut state = AppState::new();
    for i in 0..320 {
        state
            .overlay
            .append_streaming_thinking(&format!("step {i}\ndetail {i}"));
        state.overlay.end_streaming_thinking();
    }

    let mut buf = new_buf(80, 12);
    let result = render_transcript(
        &state,
        Rect::new(0, 0, 80, 12),
        &mut buf,
        &theme,
        usize::MAX,
        ToolOutputVerbosity::Normal,
        0,
        None,
    );
    let rows = all_rows(&buf);

    assert!(
        result.total_lines > 620,
        "expanded streaming thinking height should not be capped by the scratch buffer: {result:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("detail 319")),
        "tail-follow should reach the end of the streaming thinking group: {rows:?}"
    );
}

#[test]
fn streaming_overlay_partial_scroll_matches_full_render_slice() {
    let width = 48;
    let full_height = 40;
    let viewport_height = 6;
    let scroll_offset = 4usize;
    let theme = RenderTheme::plain();
    let mut state = AppState::new();
    state.overlay.append_streaming_text(
        "# Streaming overlay\n\n\
         Intro paragraph with **bold** text before a list.\n\n\
         - first item wraps with enough words to occupy more than one rendered row\n\
         - second item keeps markdown `code` inline\n\
         - third item confirms the scrolled slice has later content\n\n\
         Final paragraph after the list.",
    );

    let mut cache = TranscriptMeasureCache::new();
    let mut full_buf = new_buf(width, full_height);
    let full_result = render_transcript_cached(
        &state,
        Rect::new(0, 0, width, full_height),
        &mut full_buf,
        &theme,
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
    );
    assert!(
        full_result.total_lines >= scroll_offset + viewport_height as usize,
        "fixture must be taller than the partial viewport: {full_result:?}"
    );

    let mut partial_buf = new_buf(width, viewport_height);
    render_transcript_cached(
        &state,
        Rect::new(0, 0, width, viewport_height),
        &mut partial_buf,
        &theme,
        scroll_offset,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
    );

    let full_rows = all_rows(&full_buf);
    let expected = full_rows[scroll_offset..scroll_offset + viewport_height as usize].to_vec();
    let actual = all_rows(&partial_buf);
    assert_eq!(
        trim_blank_boundaries(expected),
        trim_blank_boundaries(actual),
        "partial overlay scratch paint must match the full render slice"
    );
}
