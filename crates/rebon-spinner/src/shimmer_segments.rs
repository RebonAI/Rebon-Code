//! Three-segment shimmer overlay for spinner messages.
//!
//! The function walks each grapheme of the text, accumulates each
//! grapheme's width into a column position, and sorts each grapheme into
//! one of three buckets:
//!
//! * `before` — graphemes whose right edge is at or before
//! `clamped_start = max(0, glimmer_index - 1)`.
//! * `shimmer` — graphemes whose left edge is at or before
//! `glimmer_index + 1`.
//! * `after` — graphemes whose left edge is past `glimmer_index + 1`.
//!
//! When the shimmer window is fully off-screen (left of column 0 or
//! right of `message_width`), the function short-circuits and returns
//! the entire text in `before`.
//!
//! Segmentation and display-width measurement happen in the caller,
//! which passes a slice of `(grapheme, width)` pairs.

/// A pre-segmented grapheme + its visual width in display columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphemeWidth<'a> {
    /// The grapheme (likely 1 char, possibly more for combining
    /// sequences).
    pub grapheme: &'a str,
    /// Display width in columns.
    pub width: usize,
}

/// The result of [`compute_shimmer_segments`]. The three strings
/// concatenate back to the whole text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShimmerSegments {
    /// The portion of the text before the shimmer window.
    pub before: String,
    /// The portion of the text inside the shimmer window. Empty when
    /// the shimmer is off-screen.
    pub shimmer: String,
    /// The portion of the text after the shimmer window. Empty when
    /// the shimmer is off-screen.
    pub after: String,
}

/// Compute the before / shimmer / after message segments.
///
/// `message_width` is the total visual width of the text (cached by
/// the caller; we take it as input rather than re-summing).
///
/// `glimmer_index` is the centre of the 3-cell-wide shimmer window.
pub fn compute_shimmer_segments(
    graphemes: &[GraphemeWidth<'_>],
    message_width: i64,
    glimmer_index: i64,
) -> ShimmerSegments {
    let shimmer_start = glimmer_index - 1;
    let shimmer_end = glimmer_index + 1;

    // Off-screen: return the whole text in `before`.
    if shimmer_start >= message_width || shimmer_end < 0 {
        let mut before = String::new();
        for g in graphemes {
            before.push_str(g.grapheme);
        }
        return ShimmerSegments {
            before,
            shimmer: String::new(),
            after: String::new(),
        };
    }

    let clamped_start = shimmer_start.max(0);
    let mut col_pos: i64 = 0;
    let mut before = String::new();
    let mut shimmer = String::new();
    let mut after = String::new();
    for g in graphemes {
        let w = g.width as i64;
        if col_pos + w <= clamped_start {
            before.push_str(g.grapheme);
        } else if col_pos > shimmer_end {
            after.push_str(g.grapheme);
        } else {
            shimmer.push_str(g.grapheme);
        }
        col_pos += w;
    }
    ShimmerSegments {
        before,
        shimmer,
        after,
    }
}

/// Convenience wrapper for the glimmer-message call site. Identical to
/// [`compute_shimmer_segments`], kept as a named entry point so the
/// call site reads more naturally.
pub fn glimmer_message_segments<'a>(
    graphemes: &'a [GraphemeWidth<'a>],
    message_width: i64,
    glimmer_index: i64,
) -> ShimmerSegments {
    compute_shimmer_segments(graphemes, message_width, glimmer_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ascii(s: &str) -> Vec<GraphemeWidth<'_>> {
        // Each ASCII char is one grapheme of width 1. The lifetimes
        // require us to build crates of &str into the source.
        let mut out = Vec::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            // ASCII fast path — every byte is a char boundary.
            out.push(GraphemeWidth {
                grapheme: &s[i..i + 1],
                width: 1,
            });
            i += 1;
        }
        out
    }

    #[test]
    fn off_screen_left_returns_all_before() {
        let g = ascii("hello");
        let s = compute_shimmer_segments(&g, 5, -100);
        assert_eq!(s.before, "hello");
        assert_eq!(s.shimmer, "");
        assert_eq!(s.after, "");
    }

    #[test]
    fn off_screen_right_returns_all_before() {
        let g = ascii("hello");
        let s = compute_shimmer_segments(&g, 5, 100);
        assert_eq!(s.before, "hello");
        assert_eq!(s.shimmer, "");
        assert_eq!(s.after, "");
    }

    #[test]
    fn shimmer_in_middle() {
        // Text "hello", glimmer at column 2 → shimmer window [1, 3].
        // 'h' col 0..1 → before (col_pos+w=1 <= clamped_start=1)
        // 'e' col 1..2 → shimmer (col_pos=1 not > 3 yet)
        // 'l' col 2..3 → shimmer
        // 'l' col 3..4 → shimmer (col_pos=3 not > 3)
        // 'o' col 4..5 → after (col_pos=4 > 3)
        let g = ascii("hello");
        let s = compute_shimmer_segments(&g, 5, 2);
        assert_eq!(s.before, "h");
        assert_eq!(s.shimmer, "ell");
        assert_eq!(s.after, "o");
    }

    #[test]
    fn shimmer_at_left_edge_clamps() {
        // glimmer_index = 0 → window [-1, 1] → clamped_start = 0.
        // 'h' col 0..1 → not <= 0 (1 not <= 0), and col_pos=0 not > 1 → shimmer.
        // 'e' col 1..2 → col_pos=1, not > 1 → shimmer.
        // 'l' col 2..3 → col_pos=2 > 1 → after.
        let g = ascii("hello");
        let s = compute_shimmer_segments(&g, 5, 0);
        assert_eq!(s.before, "");
        assert_eq!(s.shimmer, "he");
        assert_eq!(s.after, "llo");
    }

    #[test]
    fn shimmer_at_right_edge() {
        // glimmer_index = 4 (last char), window [3, 5].
        // 'h' col 0..1 → 1 <= 3 → before
        // 'e' col 1..2 → 2 <= 3 → before
        // 'l' col 2..3 → 3 <= 3 → before
        // 'l' col 3..4 → 4 > 3 → not before; col_pos=3 not > 5 → shimmer.
        // 'o' col 4..5 → col_pos=4 not > 5 → shimmer.
        let g = ascii("hello");
        let s = compute_shimmer_segments(&g, 5, 4);
        assert_eq!(s.before, "hel");
        assert_eq!(s.shimmer, "lo");
        assert_eq!(s.after, "");
    }

    #[test]
    fn empty_text_off_screen() {
        let g: Vec<GraphemeWidth<'_>> = Vec::new();
        let s = compute_shimmer_segments(&g, 0, 0);
        // shimmer_start = -1, shimmer_end = 1. message_width = 0.
        // -1 < 0, 1 >= 0 → not off-screen left.
        // Wait: shimmer_start (-1) >= message_width (0)? No.
        // shimmer_end (1) < 0? No. So we enter the loop with no
        // graphemes; all three buckets are empty.
        assert_eq!(s.before, "");
        assert_eq!(s.shimmer, "");
        assert_eq!(s.after, "");
    }

    #[test]
    fn shimmer_just_past_left_edge_off_screen() {
        // shimmer_end = -1 → off-screen left.
        let g = ascii("hello");
        let s = compute_shimmer_segments(&g, 5, -2);
        assert_eq!(s.before, "hello");
        assert_eq!(s.shimmer, "");
    }

    #[test]
    fn shimmer_just_past_right_edge_off_screen() {
        // shimmer_start = 5 >= message_width = 5 → off-screen right.
        let g = ascii("hello");
        let s = compute_shimmer_segments(&g, 5, 6);
        assert_eq!(s.before, "hello");
        assert_eq!(s.shimmer, "");
    }

    #[test]
    fn round_trip_concatenation() {
        let g = ascii("Hello, world!");
        for gi in -2..15 {
            let s = compute_shimmer_segments(&g, 13, gi);
            let joined = format!("{}{}{}", s.before, s.shimmer, s.after);
            assert_eq!(joined, "Hello, world!", "glimmer_index {gi}");
        }
    }

    #[test]
    fn cjk_double_width_grapheme_in_shimmer() {
        // Single CJK char of width 2.
        let g = vec![GraphemeWidth {
            grapheme: "中",
            width: 2,
        }];
        // glimmer at col 0 → window [-1, 1]. col_pos=0+2=2 > 1, so
        // grapheme starts at col_pos=0, not > 1 → shimmer.
        let s = compute_shimmer_segments(&g, 2, 0);
        assert_eq!(s.shimmer, "中");
    }

    #[test]
    fn convenience_wrapper_matches_core() {
        let g = ascii("test");
        let a = compute_shimmer_segments(&g, 4, 1);
        let b = glimmer_message_segments(&g, 4, 1);
        assert_eq!(a, b);
    }
}
