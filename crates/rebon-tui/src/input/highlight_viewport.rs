//! Cursor-aware highlight filter + viewport remap.
//!
//! There are TWO orthogonal filters:
//!
//! 1. **Cursor filter** — drop highlights that currently contain the
//!    cursor (to avoid drawing the highlight over the visible cursor
//!    glyph). A highlight survives iff `dim_color == true` OR
//!    `cursor_offset < start` OR `cursor_offset >= end`. Only active
//!    when `show_cursor == true` AND at least one highlight exists.
//! 2. **Viewport filter + remap** — when the renderer has scrolled
//!    (`viewport_char_offset > 0`), drop highlights that fall
//!    entirely outside the visible window and remap the survivors
//!    into viewport-local coordinates.
//!
//! The remap step preserves every additional field on each highlight
//! via a `Clone` + field mutation on [`TextHighlight`], which owns
//! all the fields a highlight needs.
//!
//! `TextHighlight` is defined locally here; only the fields the
//! filter actually reads are surfaced, and the full shape with its
//! colour model is the renderer's responsibility.

/// A text highlight span with the fields the filter reads.
/// `start` and `end` are half-open character offsets into the
/// already-rendered value (the same coordinate space as
/// `cursor_offset`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextHighlight {
    /// Inclusive start character offset.
    pub start: usize,
    /// Exclusive end character offset.
    pub end: usize,
    /// If true this is a "dim" highlight and should **not** be
    /// filtered out by the cursor-overlap check: a dim highlight
    /// survives no matter where `cursor_offset` sits.
    pub dim_color: bool,
}

/// Inputs for [`filter_and_remap_highlights`].
#[derive(Debug, Clone)]
pub struct HighlightViewportInput<'a> {
    /// The user's highlights (possibly empty).
    pub highlights: &'a [TextHighlight],
    /// Whether cursor filtering should run. When false the cursor
    /// check is skipped entirely, whatever `cursor_offset` holds.
    pub show_cursor: bool,
    /// Current cursor character offset. Only read when
    /// `show_cursor == true`.
    pub cursor_offset: usize,
    /// Left edge of the visible viewport. When `> 0` the viewport
    /// filter + remap runs.
    pub viewport_char_offset: usize,
    /// Right edge of the visible viewport.
    pub viewport_char_end: usize,
}

/// Run the cursor filter + viewport filter + viewport remap.
/// Returns a freshly built `Vec<TextHighlight>` on every call.
pub fn filter_and_remap_highlights(input: &HighlightViewportInput<'_>) -> Vec<TextHighlight> {
    // Stage 1: cursor filter, guarded by `show_cursor && !highlights.is_empty()`.
    // An empty highlights slice is treated as a no-op pass-through.
    let cursor_filtered: Vec<TextHighlight> = if input.show_cursor && !input.highlights.is_empty() {
        input
            .highlights
            .iter()
            .filter(|h| {
                h.dim_color || input.cursor_offset < h.start || input.cursor_offset >= h.end
            })
            .cloned()
            .collect()
    } else {
        input.highlights.to_vec()
    };

    // Stage 2: viewport filter + remap, guarded by `viewport_char_offset > 0`.
    if input.viewport_char_offset == 0 {
        return cursor_filtered;
    }

    cursor_filtered
        .into_iter()
        .filter(|h| h.end > input.viewport_char_offset && h.start < input.viewport_char_end)
        .map(|mut h| {
            h.start = h.start.saturating_sub(input.viewport_char_offset);
            // The filter above guarantees `h.end > viewport_char_offset`,
            // so this subtraction never underflows.
            h.end -= input.viewport_char_offset;
            h
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(start: usize, end: usize, dim: bool) -> TextHighlight {
        TextHighlight {
            start,
            end,
            dim_color: dim,
        }
    }

    fn base_input<'a>(highlights: &'a [TextHighlight]) -> HighlightViewportInput<'a> {
        HighlightViewportInput {
            highlights,
            show_cursor: false,
            cursor_offset: 0,
            viewport_char_offset: 0,
            viewport_char_end: 1000,
        }
    }

    // ------- cursor filter ------------------------------------------------

    #[test]
    fn empty_highlights_returns_empty() {
        let highlights: Vec<TextHighlight> = vec![];
        let p = base_input(&highlights);
        assert_eq!(filter_and_remap_highlights(&p), vec![]);
    }

    #[test]
    fn no_show_cursor_passes_highlights_through() {
        let highlights = vec![h(0, 5, false), h(10, 20, false)];
        let p = base_input(&highlights);
        assert_eq!(filter_and_remap_highlights(&p), highlights);
    }

    #[test]
    fn cursor_before_highlight_keeps_it() {
        let highlights = vec![h(10, 20, false)];
        let mut p = base_input(&highlights);
        p.show_cursor = true;
        p.cursor_offset = 5;
        assert_eq!(filter_and_remap_highlights(&p), highlights);
    }

    #[test]
    fn cursor_inside_highlight_drops_it() {
        let highlights = vec![h(10, 20, false)];
        let mut p = base_input(&highlights);
        p.show_cursor = true;
        p.cursor_offset = 15;
        assert_eq!(filter_and_remap_highlights(&p), vec![]);
    }

    #[test]
    fn cursor_at_start_of_highlight_drops_it() {
        // `cursor_offset < start` — equality counts as inside.
        let highlights = vec![h(10, 20, false)];
        let mut p = base_input(&highlights);
        p.show_cursor = true;
        p.cursor_offset = 10;
        assert_eq!(filter_and_remap_highlights(&p), vec![]);
    }

    #[test]
    fn cursor_at_end_of_highlight_keeps_it() {
        // `cursor_offset >= end` — end is the half-open exclusive edge.
        let highlights = vec![h(10, 20, false)];
        let mut p = base_input(&highlights);
        p.show_cursor = true;
        p.cursor_offset = 20;
        assert_eq!(filter_and_remap_highlights(&p), highlights);
    }

    #[test]
    fn cursor_just_before_end_drops_highlight() {
        let highlights = vec![h(10, 20, false)];
        let mut p = base_input(&highlights);
        p.show_cursor = true;
        p.cursor_offset = 19;
        assert_eq!(filter_and_remap_highlights(&p), vec![]);
    }

    #[test]
    fn dim_highlight_survives_cursor_overlap() {
        let highlights = vec![h(10, 20, true)];
        let mut p = base_input(&highlights);
        p.show_cursor = true;
        p.cursor_offset = 15;
        assert_eq!(filter_and_remap_highlights(&p), highlights);
    }

    #[test]
    fn cursor_filter_drops_only_overlapping_highlights() {
        let highlights = vec![
            h(0, 5, false),   // before cursor — kept
            h(10, 20, false), // contains cursor — dropped
            h(25, 30, false), // after cursor — kept
            h(12, 18, true),  // dim, contains cursor — kept
        ];
        let mut p = base_input(&highlights);
        p.show_cursor = true;
        p.cursor_offset = 15;
        let got = filter_and_remap_highlights(&p);
        assert_eq!(got, vec![h(0, 5, false), h(25, 30, false), h(12, 18, true)]);
    }

    // ------- viewport filter + remap --------------------------------------

    #[test]
    fn viewport_zero_offset_skips_remap() {
        let highlights = vec![h(0, 5, false), h(10, 20, false)];
        let mut p = base_input(&highlights);
        p.viewport_char_offset = 0;
        p.viewport_char_end = 100;
        assert_eq!(filter_and_remap_highlights(&p), highlights);
    }

    #[test]
    fn viewport_drops_highlights_entirely_to_the_left() {
        let highlights = vec![h(0, 5, false)]; // end=5 ≤ offset=10
        let mut p = base_input(&highlights);
        p.viewport_char_offset = 10;
        p.viewport_char_end = 50;
        assert_eq!(filter_and_remap_highlights(&p), vec![]);
    }

    #[test]
    fn viewport_drops_highlights_entirely_to_the_right() {
        let highlights = vec![h(100, 110, false)]; // start=100 ≥ end=50
        let mut p = base_input(&highlights);
        p.viewport_char_offset = 10;
        p.viewport_char_end = 50;
        assert_eq!(filter_and_remap_highlights(&p), vec![]);
    }

    #[test]
    fn viewport_keeps_highlight_straddling_left_edge() {
        // h=[5, 15), viewport=[10, 50)
        // end(15) > offset(10) ✓, start(5) < end(50) ✓ — kept
        // remapped: start = max(0, 5-10) = 0, end = 15-10 = 5
        let highlights = vec![h(5, 15, false)];
        let mut p = base_input(&highlights);
        p.viewport_char_offset = 10;
        p.viewport_char_end = 50;
        assert_eq!(filter_and_remap_highlights(&p), vec![h(0, 5, false)]);
    }

    #[test]
    fn viewport_keeps_highlight_straddling_right_edge() {
        // h=[45, 60), viewport=[10, 50)
        // end(60) > offset(10) ✓, start(45) < end(50) ✓ — kept
        // remapped: start=45-10=35, end=60-10=50
        let highlights = vec![h(45, 60, false)];
        let mut p = base_input(&highlights);
        p.viewport_char_offset = 10;
        p.viewport_char_end = 50;
        assert_eq!(filter_and_remap_highlights(&p), vec![h(35, 50, false)]);
    }

    #[test]
    fn viewport_remaps_fully_contained_highlight() {
        let highlights = vec![h(20, 30, false)];
        let mut p = base_input(&highlights);
        p.viewport_char_offset = 10;
        p.viewport_char_end = 50;
        assert_eq!(filter_and_remap_highlights(&p), vec![h(10, 20, false)]);
    }

    #[test]
    fn viewport_preserves_dim_color_flag_on_remap() {
        let highlights = vec![h(20, 30, true)];
        let mut p = base_input(&highlights);
        p.viewport_char_offset = 10;
        p.viewport_char_end = 50;
        assert_eq!(filter_and_remap_highlights(&p), vec![h(10, 20, true)]);
    }

    #[test]
    fn viewport_remap_with_multiple_highlights() {
        let highlights = vec![
            h(0, 3, false),   // entirely left — dropped
            h(8, 12, false),  // straddles left — clipped
            h(20, 25, false), // inside — remapped
            h(48, 52, false), // straddles right — clipped
            h(60, 65, false), // entirely right — dropped
        ];
        let mut p = base_input(&highlights);
        p.viewport_char_offset = 10;
        p.viewport_char_end = 50;
        let got = filter_and_remap_highlights(&p);
        assert_eq!(
            got,
            vec![h(0, 2, false), h(10, 15, false), h(38, 42, false)]
        );
    }

    #[test]
    fn viewport_exact_edge_end_equal_offset_is_dropped() {
        // `end > viewport_char_offset` — equality excluded
        let highlights = vec![h(0, 10, false)];
        let mut p = base_input(&highlights);
        p.viewport_char_offset = 10;
        p.viewport_char_end = 50;
        assert_eq!(filter_and_remap_highlights(&p), vec![]);
    }

    #[test]
    fn viewport_exact_edge_start_equal_end_is_dropped() {
        // `start < viewport_char_end` — equality excluded
        let highlights = vec![h(50, 60, false)];
        let mut p = base_input(&highlights);
        p.viewport_char_offset = 10;
        p.viewport_char_end = 50;
        assert_eq!(filter_and_remap_highlights(&p), vec![]);
    }

    // ------- cursor filter + viewport remap composed ---------------------

    #[test]
    fn cursor_filter_runs_before_viewport_remap() {
        // Cursor at 15 — drops h=[10,20), leaving h=[5,15).
        // Viewport [10,50) — keeps h=[5,15), remapped to [0,5).
        let highlights = vec![h(5, 15, false), h(10, 20, false)];
        let mut p = base_input(&highlights);
        p.show_cursor = true;
        p.cursor_offset = 15;
        p.viewport_char_offset = 10;
        p.viewport_char_end = 50;
        assert_eq!(filter_and_remap_highlights(&p), vec![h(0, 5, false)]);
    }

    #[test]
    fn dim_color_preserved_through_both_stages() {
        let highlights = vec![h(12, 18, true)];
        let mut p = base_input(&highlights);
        p.show_cursor = true;
        p.cursor_offset = 15;
        p.viewport_char_offset = 10;
        p.viewport_char_end = 50;
        assert_eq!(filter_and_remap_highlights(&p), vec![h(2, 8, true)]);
    }
}
