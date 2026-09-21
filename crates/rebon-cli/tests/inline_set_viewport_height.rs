//! Behavioral coverage for the `Terminal::set_viewport_height` API that
//! we backport into the vendored ratatui fork (see
//! `vendor/ratatui-0.29.0-rebon/`). The vendored crate strips its own
//! dev-dependencies to stay lean, so we exercise the patch from here
//! instead — this is the smallest binary that already pulls the fork.

use ratatui::{
    backend::{Backend, TestBackend},
    layout::{Position, Rect},
    text::Line,
    widgets::{Paragraph, Widget},
    Terminal, TerminalOptions, Viewport,
};

/// Concatenate the symbols of one buffer row, trailing blanks trimmed.
fn row_text(terminal: &Terminal<TestBackend>, y: u16) -> String {
    let buf = terminal.backend().buffer();
    (0..buf.area.width)
        .map(|x| buf.cell((x, y)).expect("cell in bounds").symbol())
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Concatenate the symbols of one scrollback row, trailing blanks trimmed.
fn scrollback_row_text(terminal: &Terminal<TestBackend>, y: u16) -> String {
    let sb = terminal.backend().scrollback();
    (0..sb.area.width)
        .map(|x| sb.cell((x, y)).expect("cell in bounds").symbol())
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn inline_terminal(width: u16, height: u16, inline_h: u16) -> Terminal<TestBackend> {
    let backend = TestBackend::new(width, height);
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(inline_h),
        },
    )
    .expect("inline terminal constructs")
}

#[test]
fn fullscreen_is_noop() {
    let backend = TestBackend::new(10, 10);
    let mut terminal = Terminal::new(backend).expect("fullscreen terminal");
    let before: Rect = terminal.get_frame().area();
    terminal.set_viewport_height(3).expect("noop returns Ok");
    assert_eq!(terminal.get_frame().area(), before);
}

#[test]
fn same_height_short_circuits() {
    let mut terminal = inline_terminal(20, 20, 5);
    let before = terminal.get_frame().area();
    terminal.set_viewport_height(5).expect("same height is Ok");
    assert_eq!(terminal.get_frame().area(), before);
}

#[test]
fn grow_before_bottom_keeps_viewport_top_fixed() {
    let mut terminal = inline_terminal(20, 20, 5);
    let before = terminal.get_frame().area();

    terminal.set_viewport_height(8).expect("grow succeeds");
    let now = terminal.get_frame().area();

    assert_eq!(now.y, before.y);
    assert_eq!(now.height, 8);
    assert_eq!(now.bottom(), before.bottom() + 3);
}

#[test]
fn grow_past_available_below_claims_only_remainder_above() {
    let mut terminal = inline_terminal(20, 20, 6);
    terminal
        .insert_before(12, |buf| {
            Paragraph::new(Line::from("committed output")).render(buf.area, buf);
        })
        .expect("insert committed output");
    let before = terminal.get_frame().area();
    assert_eq!(before.y, 12);
    assert_eq!(before.bottom(), 18);

    terminal
        .set_viewport_height(10)
        .expect("grow to screen bottom succeeds");
    let now = terminal.get_frame().area();

    assert_eq!(now.y, 10);
    assert_eq!(now.height, 10);
    assert_eq!(now.bottom(), 20);
}

#[test]
fn grow_clamps_at_screen_height() {
    let mut terminal = inline_terminal(20, 10, 4);
    terminal
        .set_viewport_height(50)
        .expect("clamp request succeeds");
    assert_eq!(terminal.get_frame().area().height, 10);
}

#[test]
fn shrink_before_bottom_keeps_viewport_top_fixed() {
    let mut terminal = inline_terminal(20, 20, 10);
    let before = terminal.get_frame().area();

    terminal.set_viewport_height(4).expect("shrink succeeds");
    let now = terminal.get_frame().area();

    assert_eq!(now.y, before.y);
    assert_eq!(now.height, 4);
    assert_eq!(now.bottom(), 4);
}

#[test]
fn shrink_after_full_height_keeps_prompt_at_bottom() {
    let mut terminal = inline_terminal(20, 20, 6);

    terminal
        .set_viewport_height(20)
        .expect("temporary full-height surface opens");
    let full_height = terminal.get_frame().area();
    assert_eq!(full_height.y, 0);
    assert_eq!(full_height.bottom(), 20);

    terminal
        .set_viewport_height(4)
        .expect("temporary full-height surface closes");
    let compact = terminal.get_frame().area();

    assert_eq!(compact.height, 4);
    assert_eq!(compact.bottom(), 20);
}

#[test]
fn shrink_before_bottom_clears_rows_released_below_old_viewport() {
    let mut terminal = inline_terminal(20, 20, 10);
    let before = terminal.get_frame().area();
    terminal
        .draw(|frame| {
            Paragraph::new(vec![Line::from("stale prompt box"); 10])
                .render(frame.area(), frame.buffer_mut());
        })
        .expect("draw stale prompt");

    terminal.set_viewport_height(4).expect("shrink succeeds");
    let now = terminal.get_frame().area();

    assert_eq!(before.y, now.y);
    let symbol = terminal
        .backend()
        .buffer()
        .cell((0, now.bottom()))
        .expect("released row exists")
        .symbol();
    assert_eq!(symbol, " ");
}

/// Commit one full-width transcript line into scrollback above the viewport.
fn commit_line(terminal: &mut Terminal<TestBackend>, text: &str) {
    let owned = text.to_string();
    terminal
        .insert_before(1, move |buf| {
            Paragraph::new(Line::from(owned.clone())).render(buf.area, buf);
        })
        .expect("commit transcript line");
}

#[test]
fn shrink_inline_viewport_keeping_top_lifts_bottom_edge_at_screen_bottom() {
    // `shrink_inline_viewport_keeping_top` is the streaming-commit prelude: even
    // when the viewport is glued to the screen bottom, it keeps the TOP row
    // fixed and releases (clearing) rows at the BOTTOM — the opposite of
    // `set_viewport_height`'s bottom-anchored at-bottom shrink.
    let mut terminal = inline_terminal(12, 10, 6);
    for i in 0..4u16 {
        commit_line(&mut terminal, &format!("C{i}"));
    }
    let before = terminal.get_frame().area();
    assert_eq!(before.y, 4, "viewport glued to bottom");
    assert_eq!(before.bottom(), 10);

    terminal
        .draw(|f| {
            Paragraph::new(vec![Line::from("live"); 6]).render(f.area(), f.buffer_mut());
        })
        .expect("draw live tail + prompt");

    terminal
        .shrink_inline_viewport_keeping_top(3)
        .expect("top-anchored shrink succeeds");
    let after = terminal.get_frame().area();

    assert_eq!(
        after.y, before.y,
        "top row stays fixed (NOT bottom-anchored)"
    );
    assert_eq!(after.height, 3);
    assert_eq!(
        after.bottom(),
        7,
        "bottom edge lifted up, freeing rows below"
    );
    // The freed rows below the viewport are blank, and nothing was scrolled
    // into scrollback.
    assert_eq!(row_text(&terminal, 7), "");
    assert_eq!(row_text(&terminal, 8), "");
    assert_eq!(row_text(&terminal, 9), "");
    terminal.backend().assert_scrollback_empty();
}

#[test]
fn commit_driven_shrink_then_commit_refills_flush_without_scrollback_gap() {
    // Regression for the streaming "bottom flicker + mid-history blank band":
    // when a live tool block finishes and its rows commit, the inline event
    // loop shrinks the viewport FIRST (top-anchored, freeing rows at the
    // bottom) and the following `insert_before` refills exactly those rows and
    // re-anchors the viewport to the screen bottom — with NO over-scroll. The
    // earlier `scroll_down`-on-shrink approach over-scrolled live content into
    // scrollback and manufactured blank rows that the next grow folded into
    // scrollback as a permanent gap; this asserts that never happens.
    let mut terminal = inline_terminal(12, 10, 6);
    for i in 0..4u16 {
        commit_line(&mut terminal, &format!("C{i}"));
    }
    assert_eq!(terminal.get_frame().area().y, 4, "viewport glued to bottom");
    // Live frame: 3 tool rows X0..X2 then a 3-row prompt block fill [4,10).
    terminal
        .draw(|f| {
            let lines = vec![
                Line::from("X0"),
                Line::from("X1"),
                Line::from("X2"),
                Line::from("P0"),
                Line::from("P1"),
                Line::from("P2"),
            ];
            Paragraph::new(lines).render(f.area(), f.buffer_mut());
        })
        .expect("draw live tail + prompt");

    // Event-loop order: shrink BEFORE the commit (the tool's 3 rows leave the
    // live tail, so the tail collapses from 6 to 3), then commit, then redraw.
    terminal
        .shrink_inline_viewport_keeping_top(3)
        .expect("commit-driven shrink");
    commit_line(&mut terminal, "X0");
    commit_line(&mut terminal, "X1");
    commit_line(&mut terminal, "X2");
    terminal
        .draw(|f| {
            let lines = vec![Line::from("P0"), Line::from("P1"), Line::from("P2")];
            Paragraph::new(lines).render(f.area(), f.buffer_mut());
        })
        .expect("redraw prompt into the re-anchored viewport");

    let area = terminal.get_frame().area();
    assert_eq!(area.height, 3);
    assert_eq!(
        area.bottom(),
        10,
        "viewport re-anchored to the screen bottom"
    );
    // Committed content is contiguous and flush against the prompt — no gap.
    for (y, expected) in ["C0", "C1", "C2", "C3", "X0", "X1", "X2"]
        .iter()
        .enumerate()
    {
        assert_eq!(row_text(&terminal, y as u16), *expected);
    }
    assert_eq!(row_text(&terminal, 7), "P0");
    assert_eq!(row_text(&terminal, 8), "P1");
    assert_eq!(row_text(&terminal, 9), "P2");
    // The crux: the shrink+commit did NOT over-scroll anything into scrollback.
    terminal.backend().assert_scrollback_empty();

    // Next cycle: a new tool starts, the viewport grows back to 6. The grow
    // scrolls C0..C2 into scrollback — and they must be CONTIGUOUS (no blank
    // band), proving the mid-history gap is gone.
    terminal
        .set_viewport_height(6)
        .expect("grow back for next tool");
    assert_eq!(
        terminal.backend().scrollback().area.height,
        3,
        "exactly the 3 displaced committed rows scrolled into scrollback"
    );
    assert_eq!(scrollback_row_text(&terminal, 0), "C0");
    assert_eq!(scrollback_row_text(&terminal, 1), "C1");
    assert_eq!(
        scrollback_row_text(&terminal, 2),
        "C2",
        "scrolled rows are contiguous — no manufactured blank band"
    );
}

#[test]
fn transient_picker_shrink_reuses_space_below_without_repeated_scrollback() {
    let mut terminal = inline_terminal(12, 10, 3);
    for i in 0..7u16 {
        commit_line(&mut terminal, &format!("C{i}"));
    }
    assert_eq!(terminal.get_frame().area().y, 7, "viewport glued to bottom");

    terminal
        .set_viewport_height(6)
        .expect("picker opens and grows viewport");
    let open = terminal.get_frame().area();
    assert_eq!(open.y, 4);
    assert_eq!(open.bottom(), 10);
    assert_eq!(terminal.backend().scrollback().area.height, 3);
    assert_eq!(scrollback_row_text(&terminal, 0), "C0");
    assert_eq!(scrollback_row_text(&terminal, 1), "C1");
    assert_eq!(scrollback_row_text(&terminal, 2), "C2");

    terminal
        .shrink_inline_viewport_keeping_top(3)
        .expect("picker closes with top-anchored shrink");
    let closed = terminal.get_frame().area();
    assert_eq!(closed.y, open.y, "closing picker must not push input down");
    assert_eq!(closed.bottom(), 7, "released rows stay below for reuse");
    assert_eq!(row_text(&terminal, 7), "");
    assert_eq!(row_text(&terminal, 8), "");
    assert_eq!(row_text(&terminal, 9), "");

    terminal
        .set_viewport_height(6)
        .expect("picker reopens into the released rows");
    let reopened = terminal.get_frame().area();
    assert_eq!(reopened.y, open.y);
    assert_eq!(reopened.bottom(), 10);
    assert_eq!(
        terminal.backend().scrollback().area.height,
        3,
        "reopening should grow into the reusable rows, not scroll more blank/history rows"
    );
}

#[test]
fn commit_prelude_shrink_does_not_blank_rows_before_insert_before() {
    let mut terminal = inline_terminal(12, 10, 6);
    for i in 0..4u16 {
        commit_line(&mut terminal, &format!("C{i}"));
    }
    terminal
        .draw(|f| {
            let lines = vec![
                Line::from("X0"),
                Line::from("X1"),
                Line::from("X2"),
                Line::from("P0"),
                Line::from("P1"),
                Line::from("P2"),
            ];
            Paragraph::new(lines).render(f.area(), f.buffer_mut());
        })
        .expect("draw live tail + prompt");

    terminal
        .shrink_inline_viewport_keeping_top_for_insert_before(3, 3)
        .expect("commit prelude shrink succeeds");

    assert_eq!(terminal.get_frame().area().height, 3);
    assert_eq!(terminal.get_frame().area().bottom(), 7);
    assert_eq!(row_text(&terminal, 7), "P0");
    assert_eq!(row_text(&terminal, 8), "P1");
    assert_eq!(row_text(&terminal, 9), "P2");
    terminal.backend().assert_scrollback_empty();
}

#[test]
fn commit_prelude_shrink_clears_released_rows_not_reused_by_insert_before() {
    let mut terminal = inline_terminal(12, 10, 6);
    for i in 0..4u16 {
        commit_line(&mut terminal, &format!("C{i}"));
    }
    terminal
        .draw(|f| {
            let lines = vec![
                Line::from("L0"),
                Line::from("L1"),
                Line::from("L2"),
                Line::from("OLD0"),
                Line::from("OLD1"),
                Line::from("OLD2"),
            ];
            Paragraph::new(lines).render(f.area(), f.buffer_mut());
        })
        .expect("draw old tall prompt");

    terminal
        .shrink_inline_viewport_keeping_top_for_insert_before(3, 1)
        .expect("commit prelude shrink succeeds");

    assert_eq!(terminal.get_frame().area().height, 3);
    assert_eq!(terminal.get_frame().area().bottom(), 7);
    assert_eq!(row_text(&terminal, 7), "OLD0");
    assert_eq!(row_text(&terminal, 8), "");
    assert_eq!(row_text(&terminal, 9), "");
}

#[test]
fn insert_before_does_not_blank_live_viewport_before_redraw() {
    let mut terminal = inline_terminal(12, 10, 3);
    for i in 0..7u16 {
        commit_line(&mut terminal, &format!("C{i}"));
    }
    assert_eq!(terminal.get_frame().area().y, 7, "viewport glued to bottom");
    terminal
        .draw(|f| {
            let lines = vec![Line::from("P0"), Line::from("P1"), Line::from("P2")];
            Paragraph::new(lines).render(f.area(), f.buffer_mut());
        })
        .expect("draw live viewport");

    commit_line(&mut terminal, "X");

    assert_eq!(terminal.get_frame().area().bottom(), 10);
    assert_eq!(row_text(&terminal, 6), "X");
    assert_eq!(
        row_text(&terminal, 8),
        "P2",
        "insert_before must not physically clear the viewport before the next redraw"
    );
}

#[test]
fn inline_resize_clears_live_region_without_clearing_committed_rows() {
    let mut terminal = inline_terminal(12, 10, 3);
    for i in 0..7u16 {
        commit_line(&mut terminal, &format!("C{i}"));
    }
    assert_eq!(terminal.get_frame().area().y, 7, "viewport glued to bottom");
    terminal
        .draw(|f| {
            let lines = vec![Line::from("OLD0"), Line::from("OLD1"), Line::from("OLD2")];
            Paragraph::new(lines).render(f.area(), f.buffer_mut());
        })
        .expect("draw old live viewport");
    assert_eq!(row_text(&terminal, 6), "C6");
    assert_eq!(row_text(&terminal, 7), "OLD0");
    assert_eq!(row_text(&terminal, 9), "OLD2");

    terminal
        .set_cursor_position(Position::new(0, 9))
        .expect("seed cursor at old viewport bottom");
    terminal.backend_mut().resize(12, 12);
    let previous_scrollback_height = terminal.backend().scrollback().area.height;
    terminal
        .resize(Rect::new(0, 0, 12, 12))
        .expect("resize terminal height");

    assert_eq!(
        terminal.backend().scrollback().area.height,
        previous_scrollback_height
    );

    assert_eq!(terminal.get_frame().area(), Rect::new(0, 7, 12, 3));
    assert_eq!(row_text(&terminal, 6), "C6");
    assert_eq!(row_text(&terminal, 7), "");
    assert_eq!(row_text(&terminal, 8), "");
    assert_eq!(row_text(&terminal, 9), "");
    assert_eq!(row_text(&terminal, 10), "");
    assert_eq!(row_text(&terminal, 11), "");

    terminal
        .draw(|f| {
            let lines = vec![Line::from("NEW0"), Line::from("NEW1"), Line::from("NEW2")];
            Paragraph::new(lines).render(f.area(), f.buffer_mut());
        })
        .expect("redraw live viewport after resize");
    assert_eq!(row_text(&terminal, 6), "C6");
    assert_eq!(row_text(&terminal, 7), "NEW0");
    assert_eq!(row_text(&terminal, 8), "NEW1");
    assert_eq!(row_text(&terminal, 9), "NEW2");
}

/// The inline event loop must settle geometry with `autoresize()` BEFORE it
/// reconciles the viewport height with `set_viewport_height` on a resize
/// frame. With that order, growing the viewport after the screen grew lands
/// in the freshly-available rows below: committed history is untouched,
/// nothing is churned into scrollback, and the old live region is cleared
/// (no ghost). Mirrors the post-fix ordering in `event_loop_entry.rs`.
#[test]
fn inline_resize_autoresize_before_reconcile_keeps_committed_rows() {
    let mut terminal = inline_terminal(12, 10, 3);
    for i in 0..7u16 {
        commit_line(&mut terminal, &format!("C{i}"));
    }
    assert_eq!(terminal.get_frame().area().y, 7, "viewport glued to bottom");
    terminal
        .draw(|f| {
            let lines = vec![Line::from("OLD0"), Line::from("OLD1"), Line::from("OLD2")];
            Paragraph::new(lines).render(f.area(), f.buffer_mut());
        })
        .expect("draw old live viewport");
    terminal
        .set_cursor_position(Position::new(0, 9))
        .expect("seed cursor at old viewport bottom");
    let scrollback_before = terminal.backend().scrollback().area.height;

    // The OS grows the terminal: the backend already reports the new size
    // while the Terminal's `last_known_area` is still the old geometry.
    terminal.backend_mut().resize(12, 14);

    // 1. Settle geometry first (the fix). `resize()` repositions against the
    //    real cursor and clears the old live region once.
    terminal.autoresize().expect("autoresize settles geometry");
    assert_eq!(
        terminal.inline_viewport_height(),
        Some(3),
        "a grow does not clamp the inline height"
    );
    // 2. Reconcile the desired height on now-correct geometry: a 3->5 grow.
    terminal
        .set_viewport_height(5)
        .expect("grow viewport to fit");

    assert_eq!(
        terminal.backend().scrollback().area.height,
        scrollback_before,
        "growing into new screen space must not churn committed rows into scrollback"
    );
    for i in 0..7u16 {
        assert_eq!(
            row_text(&terminal, i),
            format!("C{i}"),
            "committed row {i} intact"
        );
    }
    // Old live region (rows 7..=9) cleared by `resize()` — no ghost.
    assert_eq!(row_text(&terminal, 8), "", "old live row cleared");
    assert_eq!(row_text(&terminal, 9), "", "old live row cleared");
    assert_eq!(terminal.get_frame().area(), Rect::new(0, 7, 12, 5));

    terminal
        .draw(|f| {
            let lines = (0..5)
                .map(|i| Line::from(format!("NEW{i}")))
                .collect::<Vec<_>>();
            Paragraph::new(lines).render(f.area(), f.buffer_mut());
        })
        .expect("redraw live viewport after resize");
    for i in 0..5u16 {
        assert_eq!(row_text(&terminal, 7 + i), format!("NEW{i}"));
    }
    for i in 0..7u16 {
        assert_eq!(
            row_text(&terminal, i),
            format!("C{i}"),
            "committed row {i} still intact after redraw"
        );
    }
}

/// Characterizes the resize ghost when the height reconciliation runs on the
/// STALE pre-resize geometry — the bug the autoresize-first order fixes.
/// `set_viewport_height` reads the old (smaller) `last_known_area`, believes
/// the viewport is glued to the bottom, and mis-grows it UPWARD over the
/// committed rows; the trailing `resize()` then derives its clear span from
/// that displaced `previous_viewport_area` and wipes them, leaving a blank
/// band in committed history (on a real terminal: the duplicated prompt/notice
/// ghost). Kept as the rationale for ordering `autoresize()` first; if
/// `resize()` is ever made fully order-independent, revisit this test.
#[test]
fn inline_resize_reconcile_before_autoresize_corrupts_committed_rows() {
    let mut terminal = inline_terminal(12, 10, 3);
    for i in 0..7u16 {
        commit_line(&mut terminal, &format!("C{i}"));
    }
    terminal
        .draw(|f| {
            Paragraph::new(vec![
                Line::from("OLD0"),
                Line::from("OLD1"),
                Line::from("OLD2"),
            ])
            .render(f.area(), f.buffer_mut());
        })
        .expect("draw old live viewport");
    terminal
        .set_cursor_position(Position::new(0, 9))
        .expect("seed cursor");
    assert_eq!(row_text(&terminal, 5), "C5");
    assert_eq!(row_text(&terminal, 6), "C6");

    terminal.backend_mut().resize(12, 14);

    // BUGGY ORDER: reconcile height before settling geometry, against the
    // stale `last_known_area`. Grow believes there is no room below, so it
    // scrolls/moves the viewport up over the committed rows.
    terminal
        .set_viewport_height(5)
        .expect("stale-geometry grow");
    terminal
        .autoresize()
        .expect("autoresize repositions second");

    // The committed rows the stale move climbed over were cleared.
    assert_ne!(
        row_text(&terminal, 6),
        "C6",
        "stale-order reconcile corrupts committed rows (regression rationale)"
    );
}

#[test]
fn shrink_does_not_clear_rows_above_old_viewport() {
    let mut terminal = inline_terminal(20, 20, 10);
    terminal
        .insert_before(1, |buf| {
            Paragraph::new(Line::from("committed output")).render(buf.area, buf);
        })
        .expect("insert committed output");
    let before = terminal.get_frame().area();
    let row_above_viewport = before.y.saturating_sub(1);

    terminal.set_viewport_height(4).expect("shrink succeeds");

    let symbol = terminal
        .backend()
        .buffer()
        .cell((0, row_above_viewport))
        .expect("row above viewport exists")
        .symbol();
    assert_eq!(symbol, "c");
}

/// `resize()` derives the inline viewport's new top from
/// `last_known_cursor_pos - viewport_top` (`offset_in_previous_viewport`).
/// Rebon places the prompt caret out-of-band (a raw `MoveTo` to stdout) and
/// calls `note_cursor_position` so the tracked cursor matches it. With that
/// sync the offset reflects the caret's real position, the viewport is placed
/// back at the committed boundary, and the startup banner above survives.
#[test]
fn inline_resize_caret_sync_preserves_committed_rows() {
    let mut terminal = inline_terminal(12, 12, 5);
    for i in 0..7u16 {
        commit_line(&mut terminal, &format!("C{i}"));
    }
    assert_eq!(terminal.get_frame().area(), Rect::new(0, 7, 12, 5));
    terminal
        .draw(|f| {
            let lines = (0..5)
                .map(|i| Line::from(format!("OLD{i}")))
                .collect::<Vec<_>>();
            Paragraph::new(lines).render(f.area(), f.buffer_mut());
        })
        .expect("draw old live viewport");
    for i in 0..7u16 {
        assert_eq!(row_text(&terminal, i), format!("C{i}"));
    }

    // Frame end: the buffer diff left the tracked cursor at the viewport bottom
    // (row 11), but the caret was placed out-of-band on the input line (row 8).
    // `note_cursor_position` realigns the tracked cursor to the real caret.
    terminal
        .set_cursor_position(Position::new(0, 11))
        .expect("tracked cursor at last diff cell");
    terminal
        .backend_mut()
        .set_cursor_position(Position::new(0, 8))
        .expect("real caret on the input line");
    terminal.note_cursor_position(Position::new(0, 8));

    // OS grows the terminal; settle geometry as the event loop does.
    let scrollback_before = terminal.backend().scrollback().area.height;
    terminal.backend_mut().resize(12, 14);
    terminal.autoresize().expect("autoresize settles geometry");

    assert_eq!(
        terminal.get_frame().area().y,
        7,
        "viewport returns to the committed boundary, not above it"
    );
    assert_eq!(
        terminal.backend().scrollback().area.height,
        scrollback_before,
        "a grow must not churn committed rows into scrollback"
    );
    for i in 0..7u16 {
        assert_eq!(
            row_text(&terminal, i),
            format!("C{i}"),
            "committed row {i} must survive the resize"
        );
    }
}

/// Characterizes the banner-eating when the tracked cursor is NOT realigned to
/// the out-of-band caret. The tracked cursor stays at the last buffer-diff cell
/// (the box bottom-border / footer, a couple rows below the caret), inflating
/// `offset_in_previous_viewport` so the viewport is placed too high and the
/// clear wipes committed rows above it. Kept as the rationale for the
/// `note_cursor_position` sync; if `resize()` is ever made offset-independent,
/// revisit this test.
#[test]
fn inline_resize_without_caret_sync_eats_committed_rows() {
    let mut terminal = inline_terminal(12, 12, 5);
    for i in 0..7u16 {
        commit_line(&mut terminal, &format!("C{i}"));
    }
    terminal
        .draw(|f| {
            let lines = (0..5)
                .map(|i| Line::from(format!("OLD{i}")))
                .collect::<Vec<_>>();
            Paragraph::new(lines).render(f.area(), f.buffer_mut());
        })
        .expect("draw old live viewport");

    // Tracked cursor stuck at the last diff cell (row 11); caret really on the
    // input line (row 8). NO `note_cursor_position` — the desync the fix removes.
    terminal
        .set_cursor_position(Position::new(0, 11))
        .expect("tracked cursor at last diff cell");
    terminal
        .backend_mut()
        .set_cursor_position(Position::new(0, 8))
        .expect("real caret on the input line");

    terminal.backend_mut().resize(12, 14);
    terminal.autoresize().expect("autoresize settles geometry");

    // Offset inflated to 4 → next top computed at 4 → committed rows 4..=6 wiped.
    assert_ne!(
        row_text(&terminal, 6),
        "C6",
        "without the caret sync the resize eats committed banner rows (rationale)"
    );
}

/// After the inline full-repaint path physically wipes the screen + scrollback
/// and homes the cursor, `reseed_inline_viewport_after_clear` must re-anchor the
/// viewport to a single top row at the new geometry — without scrolling, even
/// when the previous inline height exceeded the new (shorter) screen.
#[test]
fn reseed_after_clear_seeds_single_top_row_without_scrolling() {
    let mut terminal = inline_terminal(20, 12, 8);
    for i in 0..6u16 {
        commit_line(&mut terminal, &format!("C{i}"));
    }

    // Emulate the caller's wipe: the screen shrinks under the old inline height
    // (8 > 5) and the cursor is homed. `reseed` reads only cursor + size, never
    // buffer contents, so the test need not clear the buffers.
    terminal.backend_mut().resize(20, 5);
    terminal
        .set_cursor_position(Position::ORIGIN)
        .expect("home cursor");
    let scrollback_before = terminal.backend().scrollback().area.height;

    terminal
        .reseed_inline_viewport_after_clear()
        .expect("reseed succeeds");

    assert_eq!(
        terminal.get_frame().area(),
        Rect::new(0, 0, 20, 1),
        "viewport re-seeded to a single top row at the new geometry"
    );
    assert_eq!(
        terminal.backend().scrollback().area.height,
        scrollback_before,
        "single-row seed keeps append_lines at zero — nothing scrolls"
    );
}

/// `reseed_inline_viewport_after_clear` adopts the NEW terminal width and
/// settles `last_known_area`, so the `autoresize()` inside the trailing draw is
/// a no-op rather than a second, conflicting reposition.
#[test]
fn reseed_after_clear_adopts_new_width_and_settles_autoresize() {
    let mut terminal = inline_terminal(20, 10, 6);
    commit_line(&mut terminal, "C0");

    // Width grows 20 -> 34 (the reflow-trigger case), then wipe + home.
    terminal.backend_mut().resize(34, 10);
    terminal
        .set_cursor_position(Position::ORIGIN)
        .expect("home cursor");

    terminal
        .reseed_inline_viewport_after_clear()
        .expect("reseed succeeds");
    assert_eq!(
        terminal.get_frame().area(),
        Rect::new(0, 0, 34, 1),
        "viewport seeded at the new width"
    );

    terminal.autoresize().expect("autoresize");
    assert_eq!(
        terminal.get_frame().area(),
        Rect::new(0, 0, 34, 1),
        "autoresize is a no-op once reseed settles last_known_area"
    );
}

/// After reseeding, the rebuild grows the viewport from the single seed row up
/// to its real height. On the freshly cleared screen that grow must be
/// top-anchored (no scrolling) and at the new width — mirroring the inline
/// startup-banner prepare step that follows the wipe.
#[test]
fn reseed_then_grow_rebuilds_top_anchored_at_new_width() {
    let mut terminal = inline_terminal(20, 10, 6);
    for i in 0..3u16 {
        commit_line(&mut terminal, &format!("C{i}"));
    }

    terminal.backend_mut().resize(30, 10);
    terminal
        .set_cursor_position(Position::ORIGIN)
        .expect("home cursor");
    terminal
        .reseed_inline_viewport_after_clear()
        .expect("reseed succeeds");

    terminal
        .set_viewport_height(8)
        .expect("grow into the rebuilt viewport");
    let area = terminal.get_frame().area();
    assert_eq!(area.y, 0, "grow is top-anchored on the cleared screen");
    assert_eq!(area.height, 8);
    assert_eq!(area.width, 30, "rebuild runs at the new width");
}
