//! `compute_gutter_width` — pure math for the diff line-number column.
//!
//! The column layout is `marker (1) + space + right-aligned line number +
//! space`, sized so the widest line number in the hunk still fits. Nothing but
//! the hunk's line numbers feeds into it, so the result is a stable function of
//! the patch and can be cached alongside the rendered output.
//!
//! Three load-bearing properties:
//!
//! 1. The width depends on the **largest** line number that will be
//!    rendered: the max of the last old-file line and the last new-file line.
//! 2. The minimum largest line number is **1**, not 0 — even an empty hunk
//!    gets a 1-digit column, because of the `max(…, 1)` clamp.
//! 3. The constant `+ 3` accounts for the diff sigil (`+`/`-`/space) and the
//!    two padding spaces flanking the line number.
//!
//! ## Subtraction underflow
//!
//! `start + lines - 1` would underflow a `usize` when both are 0, so the
//! arithmetic saturates: `0_usize.saturating_add(0).saturating_sub(1) == 0`.
//! That value then meets the `max(…, 1)` clamp, so such a hunk still gets the
//! minimum width of 4.

use crate::patch::PatchHunk;

/// Returns the gutter column width for a hunk: digits in the largest
/// line number, plus 3 for the marker and surrounding spaces.
///
/// Saturating arithmetic keeps a hunk with zero start and zero line count
/// from underflowing; see the module-level docs.
pub fn compute_gutter_width(patch: &PatchHunk) -> usize {
    let last_old = patch
        .old_start
        .saturating_add(patch.old_lines)
        .saturating_sub(1);
    let last_new = patch
        .new_start
        .saturating_add(patch.new_lines)
        .saturating_sub(1);
    let max_line_number = last_old.max(last_new).max(1);
    digit_count(max_line_number) + 3
}

/// Number of decimal digits in `n`. `digit_count(0) == 1`.
fn digit_count(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let mut n = n;
    let mut digits = 0;
    while n > 0 {
        digits += 1;
        n /= 10;
    }
    digits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(old_start: usize, old_lines: usize, new_start: usize, new_lines: usize) -> PatchHunk {
        PatchHunk::new(old_start, old_lines, new_start, new_lines, vec![])
    }

    // -----------------------------------------------------------------
    // digit_count helper
    // -----------------------------------------------------------------

    #[test]
    fn digit_count_handles_zero_as_one_digit() {
        assert_eq!(digit_count(0), 1);
    }

    #[test]
    fn digit_count_single_digit() {
        for n in 1..=9 {
            assert_eq!(digit_count(n), 1, "digit_count({n}) should be 1");
        }
    }

    #[test]
    fn digit_count_two_digits() {
        assert_eq!(digit_count(10), 2);
        assert_eq!(digit_count(99), 2);
    }

    #[test]
    fn digit_count_three_and_four_digits() {
        assert_eq!(digit_count(100), 3);
        assert_eq!(digit_count(999), 3);
        assert_eq!(digit_count(1000), 4);
        assert_eq!(digit_count(9999), 4);
    }

    #[test]
    fn digit_count_five_digits() {
        assert_eq!(digit_count(10_000), 5);
        assert_eq!(digit_count(99_999), 5);
    }

    // -----------------------------------------------------------------
    // compute_gutter_width — field-by-field
    // -----------------------------------------------------------------

    #[test]
    fn single_line_hunk_at_line_one_gives_minimum_width() {
        // old_start=1 old_lines=1 → last_old=1
        // max(1,1,1) = 1, digits(1) = 1, +3 = 4
        let g = compute_gutter_width(&hunk(1, 1, 1, 1));
        assert_eq!(g, 4);
    }

    #[test]
    fn empty_hunk_at_line_one_clamps_to_minimum() {
        // old_start=1 old_lines=0 → 1+0-1 = 0
        // max(0,0,1) = 1, digits(1) = 1, +3 = 4
        let g = compute_gutter_width(&hunk(1, 0, 1, 0));
        assert_eq!(g, 4);
    }

    #[test]
    fn zero_zero_hunk_clamps_to_minimum() {
        // saturating: 0+0-1 saturates to 0
        // max(0,0,1) = 1, digits(1) = 1, +3 = 4
        let g = compute_gutter_width(&hunk(0, 0, 0, 0));
        assert_eq!(g, 4);
    }

    #[test]
    fn nine_line_hunk_uses_one_digit() {
        // old_start=1 old_lines=9 → last_old=9, digits(9)=1, +3 = 4
        let g = compute_gutter_width(&hunk(1, 9, 1, 9));
        assert_eq!(g, 4);
    }

    #[test]
    fn ten_line_hunk_uses_two_digits() {
        // old_start=1 old_lines=10 → last_old=10, digits(10)=2, +3 = 5
        let g = compute_gutter_width(&hunk(1, 10, 1, 10));
        assert_eq!(g, 5);
    }

    #[test]
    fn hunk_at_line_99_with_one_line_uses_two_digits() {
        // last_old = 99+1-1 = 99, digits=2, +3 = 5
        let g = compute_gutter_width(&hunk(99, 1, 99, 1));
        assert_eq!(g, 5);
    }

    #[test]
    fn hunk_at_line_100_uses_three_digits() {
        let g = compute_gutter_width(&hunk(100, 1, 100, 1));
        assert_eq!(g, 6);
    }

    #[test]
    fn hunk_at_line_9999_uses_four_digits() {
        let g = compute_gutter_width(&hunk(9999, 1, 9999, 1));
        assert_eq!(g, 7);
    }

    #[test]
    fn hunk_at_line_10000_uses_five_digits() {
        let g = compute_gutter_width(&hunk(10_000, 1, 10_000, 1));
        assert_eq!(g, 8);
    }

    #[test]
    fn new_side_can_be_larger_than_old_side() {
        // old_start=1 old_lines=1 → last_old=1
        // new_start=1 new_lines=200 → last_new=200, digits=3, +3=6
        let g = compute_gutter_width(&hunk(1, 1, 1, 200));
        assert_eq!(g, 6);
    }

    #[test]
    fn old_side_can_be_larger_than_new_side() {
        // last_old=200 (digits=3), last_new=1
        // max(200, 1, 1) = 200
        let g = compute_gutter_width(&hunk(1, 200, 1, 1));
        assert_eq!(g, 6);
    }

    #[test]
    fn far_apart_hunks_take_max_correctly() {
        // last_old = 50+1-1 = 50, last_new = 1000+1-1 = 1000
        // max=1000, digits=4, +3=7
        let g = compute_gutter_width(&hunk(50, 1, 1000, 1));
        assert_eq!(g, 7);
    }

    // -----------------------------------------------------------------
    // Exhaustive table — kept human-readable for code review
    // -----------------------------------------------------------------

    #[test]
    fn gutter_width_table() {
        // (old_start, old_lines, new_start, new_lines, expected)
        // Each row is hand-computed against:
        //   digit_count(max(old_start+old_lines-1, new_start+new_lines-1, 1)) + 3
        let cases: &[(usize, usize, usize, usize, usize)] = &[
            (1, 1, 1, 1, 4),           // single line, min width
            (1, 0, 1, 0, 4),           // empty hunk, clamp at 1
            (0, 0, 0, 0, 4),           // saturated empty, clamp at 1
            (1, 9, 1, 9, 4),           // last line = 9 → 1 digit
            (1, 10, 1, 10, 5),         // last line = 10 → 2 digits
            (1, 99, 1, 99, 5),         // last line = 99 → 2 digits
            (1, 100, 1, 100, 6),       // last line = 100 → 3 digits
            (1, 999, 1, 999, 6),       // last line = 999 → 3 digits
            (1, 1000, 1, 1000, 7),     // last line = 1000 → 4 digits
            (1, 9999, 1, 9999, 7),     // last line = 9999 → 4 digits
            (1, 10_000, 1, 10_000, 8), // last line = 10000 → 5 digits
            (5, 3, 5, 3, 4),           // last line = 7 → 1 digit
            (98, 1, 98, 1, 5),         // 98 → 2 digits
            (99, 1, 99, 1, 5),         // 99 → 2 digits
            (100, 1, 100, 1, 6),       // 100 → 3 digits
            (1, 1, 1, 200, 6),         // new side larger
            (1, 200, 1, 1, 6),         // old side larger
            (50, 1, 1000, 1, 7),       // far-apart hunks
            // Saturating-arithmetic guard: old_start=0 old_lines=5 → last_old = 4
            (0, 5, 1, 5, 4),
        ];
        for (os, ol, ns, nl, expected) in cases.iter().copied() {
            let actual = compute_gutter_width(&hunk(os, ol, ns, nl));
            assert_eq!(
                actual, expected,
                "compute_gutter_width(oldStart={os}, oldLines={ol}, newStart={ns}, newLines={nl}) → expected {expected}, got {actual}",
            );
        }
    }
}
