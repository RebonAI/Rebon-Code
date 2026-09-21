/// The nine fill levels of a progress bar, in ascending order:
/// `[' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█']`. Index 0 is empty space
/// and index 8 is a full block.
///
/// The order is load-bearing: a partial fill picks its glyph by scaling the
/// remainder up to this table's length.
pub const BLOCK_GLYPHS: &[&str; 9] = &[" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];

/// Segment breakdown for one progress bar, produced by
/// [`progress_bar_segments`]. A renderer may draw the pieces separately or
/// use the pre-joined `rendered` string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressBarSegments {
    /// How many full blocks (`█`) to draw.
    pub whole_count: u32,
    /// The partial block, or `None` when the bar is entirely full
    /// (`whole >= width`). When present it is one of [`BLOCK_GLYPHS`].
    pub middle: Option<&'static str>,
    /// How many spaces follow the partial block.
    pub empty_count: u32,
    /// The bar as one string: full blocks, then the partial block, then the
    /// spaces.
    pub rendered: String,
}

/// Lay out one progress bar.
///
/// * `ratio` — fill fraction in `0.0..=1.0`; anything outside is clamped.
/// * `width` — total width of the bar in characters.
///
/// The full, partial and empty counts together account for exactly `width`
/// characters whenever `width` is non-zero, and `rendered` already reflects
/// that. A `width` of 0 yields no segments at all.
pub fn progress_bar_segments(ratio: f64, width: u32) -> ProgressBarSegments {
    // Clamp the ratio to [0, 1].
    let ratio = ratio.clamp(0.0, 1.0);

    // Whole block count: floor(ratio * width).
    let whole = (ratio * width as f64).floor() as u32;
    // Cap defensive: in pathological floating-point cases the floor
    // could exceed width by 1; cap to width.
    let whole = whole.min(width);

    let mut rendered = String::new();
    for _ in 0..whole {
        rendered.push_str(BLOCK_GLYPHS[BLOCK_GLYPHS.len() - 1]); // █
    }

    let mut middle: Option<&'static str> = None;
    let mut empty: u32 = 0;

    if whole < width {
        // remainder = ratio * width - whole
        let remainder = ratio * width as f64 - whole as f64;
        // middle = floor(remainder * BLOCKS.length)
        // BLOCKS.length is 9; index 0..=8.
        let middle_index = (remainder * BLOCK_GLYPHS.len() as f64).floor() as usize;
        // Clamp to valid index range. Index 8 can appear (when
        // remainder is exactly 1.0, e.g. ratio*width is exactly an
        // integer + 1, which can't happen because we already handled
        // whole, but if width=0 and ratio=1 we'd need defensive
        // bounds).
        let middle_index = middle_index.min(BLOCK_GLYPHS.len() - 1);
        middle = Some(BLOCK_GLYPHS[middle_index]);
        rendered.push_str(BLOCK_GLYPHS[middle_index]);

        // empty = width - whole - 1
        let after_middle = width.saturating_sub(whole).saturating_sub(1);
        if after_middle > 0 {
            empty = after_middle;
            for _ in 0..empty {
                rendered.push_str(BLOCK_GLYPHS[0]); // " "
            }
        }
    }

    ProgressBarSegments {
        whole_count: whole,
        middle,
        empty_count: empty,
        rendered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_glyphs_pinned_order() {
        // Exact order matters because the index is computed by
        // `floor(remainder * BLOCKS.length)`.
        assert_eq!(BLOCK_GLYPHS, &[" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"]);
    }

    #[test]
    fn block_glyphs_count_is_nine() {
        assert_eq!(BLOCK_GLYPHS.len(), 9);
    }

    #[test]
    fn ratio_zero_is_all_empty() {
        let s = progress_bar_segments(0.0, 10);
        assert_eq!(s.whole_count, 0);
        assert_eq!(s.middle, Some(" ")); // remainder is 0, middle index 0
        assert_eq!(s.empty_count, 9);
        assert_eq!(s.rendered, "          "); // 10 spaces
    }

    #[test]
    fn ratio_one_is_all_full() {
        let s = progress_bar_segments(1.0, 10);
        assert_eq!(s.whole_count, 10);
        assert_eq!(s.middle, None);
        assert_eq!(s.empty_count, 0);
        assert_eq!(s.rendered, "██████████");
    }

    #[test]
    fn ratio_half_width_ten_yields_five_full_plus_middle() {
        let s = progress_bar_segments(0.5, 10);
        assert_eq!(s.whole_count, 5);
        // remainder = 0.5 * 10 - 5 = 0; middle index = floor(0*9) = 0
        assert_eq!(s.middle, Some(" "));
        assert_eq!(s.empty_count, 4);
    }

    #[test]
    fn ratio_clamped_below_zero() {
        let s = progress_bar_segments(-0.5, 10);
        assert_eq!(s.whole_count, 0);
        assert_eq!(s.empty_count, 9);
    }

    #[test]
    fn ratio_clamped_above_one() {
        let s = progress_bar_segments(2.0, 10);
        assert_eq!(s.whole_count, 10);
        assert_eq!(s.middle, None);
        assert_eq!(s.empty_count, 0);
    }

    #[test]
    fn width_zero_yields_empty_string() {
        let s = progress_bar_segments(0.5, 0);
        assert_eq!(s.whole_count, 0);
        assert_eq!(s.middle, None); // whole < width is false (0 < 0 = false)
        assert_eq!(s.empty_count, 0);
        assert_eq!(s.rendered, "");
    }

    #[test]
    fn width_one_zero_ratio() {
        let s = progress_bar_segments(0.0, 1);
        assert_eq!(s.whole_count, 0);
        assert_eq!(s.middle, Some(" "));
        assert_eq!(s.empty_count, 0);
        assert_eq!(s.rendered, " ");
    }

    #[test]
    fn width_one_full_ratio() {
        let s = progress_bar_segments(1.0, 1);
        assert_eq!(s.whole_count, 1);
        assert_eq!(s.middle, None);
        assert_eq!(s.empty_count, 0);
        assert_eq!(s.rendered, "█");
    }

    #[test]
    fn width_one_half_ratio_picks_a_sub_block() {
        let s = progress_bar_segments(0.5, 1);
        assert_eq!(s.whole_count, 0);
        // remainder = 0.5*1 - 0 = 0.5; index = floor(0.5*9) = 4
        assert_eq!(s.middle, Some("▌"));
        assert_eq!(s.empty_count, 0);
        assert_eq!(s.rendered, "▌");
    }

    #[test]
    fn width_eight_eighths() {
        // ratio just above 0 with width 1 should pick smaller sub-blocks
        for (ratio, expected_idx) in [
            (0.0, 0usize), // index 0 -> " "
            (0.13, 1),     // 0.13*9=1.17 -> 1
            (0.25, 2),     // 0.25*9=2.25 -> 2
            (0.4, 3),      // 0.4*9=3.6 -> 3
            (0.5, 4),      // 0.5*9=4.5 -> 4
            (0.65, 5),     // 0.65*9=5.85 -> 5
            (0.7, 6),      // 0.7*9=6.3 -> 6
            (0.85, 7),     // 0.85*9=7.65 -> 7
            (0.99, 8),     // 0.99*9=8.91 -> 8
        ] {
            let s = progress_bar_segments(ratio, 1);
            assert_eq!(
                s.middle,
                Some(BLOCK_GLYPHS[expected_idx]),
                "ratio {ratio} expected index {expected_idx}"
            );
        }
    }

    #[test]
    fn quarter_width_twenty() {
        let s = progress_bar_segments(0.25, 20);
        assert_eq!(s.whole_count, 5);
        // remainder = 0.25*20 - 5 = 0; middle = " "
        assert_eq!(s.middle, Some(" "));
        assert_eq!(s.empty_count, 14);
        assert_eq!(s.rendered.chars().count(), 20);
    }

    #[test]
    fn rendered_length_equals_width_when_width_positive() {
        for w in 1..=30 {
            for r10 in 0..=10 {
                let r = r10 as f64 / 10.0;
                let s = progress_bar_segments(r, w);
                assert_eq!(
                    s.rendered.chars().count(),
                    w as usize,
                    "ratio {r} width {w}"
                );
            }
        }
    }

    #[test]
    fn very_large_width() {
        let s = progress_bar_segments(0.5, 1000);
        assert_eq!(s.whole_count, 500);
        assert_eq!(s.empty_count, 499);
        assert_eq!(s.rendered.chars().count(), 1000);
    }

    #[test]
    fn middle_index_clamped_when_remainder_is_one() {
        // Defensive: catch the off-by-one if a future refactor lets
        // the middle index exceed BLOCK_GLYPHS.len() - 1.
        let s = progress_bar_segments(0.99999999, 1);
        assert!(s.middle.is_some());
        assert_ne!(s.middle, Some(""));
    }
}
