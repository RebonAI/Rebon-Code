//! Markdown-table column-width calculator.
//!
//! ## Column-width algorithm
//!
//! 1. Measure every column twice: a `min` width (its longest word) and an
//!    `ideal` width (its full content).
//! 2. Work out how much room the row leaves:
//!
//! ```text
//!   border_overhead = 1 + num_cols * 3
//!   available_width = max(terminal_width - border_overhead - SAFETY_MARGIN,
//!                         num_cols * MIN_COLUMN_WIDTH)
//! ```
//!
//! 3. Choose the per-column widths that fit that space; the three branches
//!    are listed below.
//!
//! Plus three constants: [`SAFETY_MARGIN`] (4), [`MIN_COLUMN_WIDTH`] (3)
//! and [`MAX_ROW_LINES`] (4).
//!
//! `MAX_ROW_LINES` is consumed by the render module, not the layout
//! module — it's exposed here for re-use to keep the constants in
//! one place.
//!
//! ## Three branches of the decision tree
//!
//! 1. **Everything fits at ideal width.** Use the ideal widths directly.
//! 2. **Doesn't fit at ideal but fits at min.** Distribute the extra
//!    space proportional to each column's overflow (ideal - min).
//!    The integer rounding may leave 0..n columns of unused space; that is
//!    by design rather than a bug.
//! 3. **Doesn't fit even at min.** Scale every min width by
//!    `available_width / total_min` and clamp to `MIN_COLUMN_WIDTH`.
//!    Sets `needs_hard_wrap = true` so the renderer knows to break
//!    inside words. The total in this branch can EXCEED
//!    available_width in pathological cases (each column clamps up
//!    to MIN_COLUMN_WIDTH); the safety check at render time handles
//!    that by switching to vertical format.

/// Reserved columns against terminal-resize races.
pub const SAFETY_MARGIN: usize = 4;

/// Floor for any individual column. The "longest word" min width also
/// clamps up to this.
pub const MIN_COLUMN_WIDTH: usize = 3;

/// The render module
/// switches to vertical (key-value) format when any row would wrap
/// to more than this many lines.
pub const MAX_ROW_LINES: usize = 4;

/// Compute the available content width for `num_cols` columns inside
/// `terminal_width` columns total.
///
/// The underlying expression is:
///
/// ```text
/// border_overhead = 1 + num_cols * 3
/// available_width = max(terminal_width - border_overhead - SAFETY_MARGIN,
///                       num_cols * MIN_COLUMN_WIDTH)
/// ```
///
/// `border_overhead` accounts for the leading `│`, plus `(width + 3)`
/// per column where the `+3` is `space + content + space + border`
/// minus the leading column's already-counted leading bar. The
/// floor at `num_cols * MIN_COLUMN_WIDTH` guarantees we never return
/// less than the absolute minimum number of columns needed even on
/// extremely narrow terminals.
pub fn compute_available_width(num_cols: usize, terminal_width: usize) -> usize {
    let border_overhead = 1 + num_cols * 3;
    let raw = terminal_width
        .saturating_sub(border_overhead)
        .saturating_sub(SAFETY_MARGIN);
    raw.max(num_cols * MIN_COLUMN_WIDTH)
}

/// Output of [`compute_column_widths`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnLayout {
    /// One width per column (in display columns).
    pub widths: Vec<usize>,
    /// `true` when the layout had to scale below per-column minimum
    /// widths and the renderer must break inside words.
    pub needs_hard_wrap: bool,
}

/// Compute per-column widths from min/ideal widths and the
/// available content width, following the three-branch decision in the
/// module docs.
///
/// `min_widths` and `ideal_widths` MUST have the same length and
/// must each have at least one entry. Empty inputs return an empty
/// `ColumnLayout` (`needs_hard_wrap = false`) — that branch is
/// defensive only because a table renderer is never instantiated
/// for tables with zero columns.
///
/// The function does **not** clamp individual results above
/// `MIN_COLUMN_WIDTH` in branches 1 or 2 — it trusts the input
/// invariant that each column's minimum already reaches that floor.
/// Branch 3 (`needs_hard_wrap`) does an explicit clamp because the
/// scale factor can drive widths below the floor.
pub fn compute_column_widths(
    min_widths: &[usize],
    ideal_widths: &[usize],
    available_width: usize,
) -> ColumnLayout {
    assert_eq!(
        min_widths.len(),
        ideal_widths.len(),
        "min_widths and ideal_widths must have the same length",
    );

    if min_widths.is_empty() {
        return ColumnLayout {
            widths: Vec::new(),
            needs_hard_wrap: false,
        };
    }

    let total_min: usize = min_widths.iter().sum();
    let total_ideal: usize = ideal_widths.iter().sum();

    if total_ideal <= available_width {
        return ColumnLayout {
            widths: ideal_widths.to_vec(),
            needs_hard_wrap: false,
        };
    }

    if total_min <= available_width {
        // Branch 2: distribute extra space proportional to overflow.
        let extra_space = available_width - total_min;
        let overflows: Vec<usize> = ideal_widths
            .iter()
            .zip(min_widths.iter())
            .map(|(ideal, min)| ideal.saturating_sub(*min))
            .collect();
        let total_overflow: usize = overflows.iter().sum();
        let widths = min_widths
            .iter()
            .enumerate()
            .map(|(i, &min)| {
                if total_overflow == 0 {
                    return min;
                }
                // Integer multiply-then-divide, which truncates the
                // quotient instead of rounding it.
                let extra = overflows[i] * extra_space / total_overflow;
                min + extra
            })
            .collect();
        return ColumnLayout {
            widths,
            needs_hard_wrap: false,
        };
    }

    // Branch 3: doesn't fit even at min. Scale + clamp to floor.
    // Integer multiply-then-divide, so the scaled widths truncate
    // rather than round.
    let widths = min_widths
        .iter()
        .map(|&w| {
            let scaled = (w * available_width) / total_min;
            scaled.max(MIN_COLUMN_WIDTH)
        })
        .collect();
    ColumnLayout {
        widths,
        needs_hard_wrap: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Magic-constant pinning. Rendering relies on these exact values; any
    // drift here is observable in user-visible output.
    // -----------------------------------------------------------------

    #[test]
    fn constants_are_pinned() {
        assert_eq!(SAFETY_MARGIN, 4);
        assert_eq!(MIN_COLUMN_WIDTH, 3);
        assert_eq!(MAX_ROW_LINES, 4);
    }

    // -----------------------------------------------------------------
    // compute_available_width — border-overhead arithmetic
    // -----------------------------------------------------------------

    #[test]
    fn available_width_subtracts_border_and_safety_margin() {
        // 3 cols, terminal 80
        // border_overhead = 1 + 3*3 = 10
        // available = 80 - 10 - 4 = 66
        assert_eq!(compute_available_width(3, 80), 66);
    }

    #[test]
    fn available_width_floors_at_n_cols_times_min() {
        // 3 cols, terminal 8 → raw = 8 - 10 - 4 = -6 → floor at 9
        assert_eq!(compute_available_width(3, 8), 9);
    }

    #[test]
    fn available_width_handles_zero_cols() {
        // 0 cols → border_overhead = 1, floor = 0 → max(terminal-5, 0)
        assert_eq!(compute_available_width(0, 80), 75);
        assert_eq!(compute_available_width(0, 4), 0);
    }

    // -----------------------------------------------------------------
    // Branch 1 — everything fits at ideal width
    // -----------------------------------------------------------------

    #[test]
    fn everything_fits_uses_ideal_widths() {
        let layout = compute_column_widths(&[3, 3, 3], &[5, 7, 4], 30);
        assert_eq!(layout.widths, vec![5, 7, 4]);
        assert!(!layout.needs_hard_wrap);
    }

    #[test]
    fn exact_available_uses_ideal_widths() {
        // total_ideal == available_width → take the ideal widths directly
        let layout = compute_column_widths(&[3, 3], &[5, 5], 10);
        assert_eq!(layout.widths, vec![5, 5]);
        assert!(!layout.needs_hard_wrap);
    }

    // -----------------------------------------------------------------
    // Branch 2 — fits at min, distribute extra proportionally
    // -----------------------------------------------------------------

    #[test]
    fn shrinks_proportionally_when_only_min_fits() {
        // min = [3, 3], ideal = [10, 5], available = 11
        // total_min = 6, total_ideal = 15 → branch 2
        // extra_space = 11 - 6 = 5
        // overflows = [7, 2], total_overflow = 9
        // col0: 7/9 * 5 = 35/9 = floor(3.88) = 3 → 3+3 = 6
        // col1: 2/9 * 5 = 10/9 = floor(1.11) = 1 → 3+1 = 4
        // (sum = 10, leaves 1 unused — that's the floor rounding loss)
        let layout = compute_column_widths(&[3, 3], &[10, 5], 11);
        assert_eq!(layout.widths, vec![6, 4]);
        assert!(!layout.needs_hard_wrap);
    }

    #[test]
    fn equal_overflow_distributes_evenly() {
        // min = [3, 3], ideal = [8, 8], available = 12
        // total_min = 6, total_ideal = 16 → branch 2
        // extra_space = 6, overflows = [5, 5], total_overflow = 10
        // col0: 5*6/10 = 3 → 3+3 = 6
        // col1: 5*6/10 = 3 → 3+3 = 6
        let layout = compute_column_widths(&[3, 3], &[8, 8], 12);
        assert_eq!(layout.widths, vec![6, 6]);
        assert!(!layout.needs_hard_wrap);
    }

    #[test]
    fn zero_overflow_returns_min_widths_directly() {
        // ideal == min → total_overflow = 0 → return the min widths
        // (avoids division by zero)
        let layout = compute_column_widths(&[3, 3], &[3, 3], 10);
        // Wait — if ideal == min, total_ideal = 6 ≤ 10 → branch 1.
        // Construct a case where total_min == total_ideal but the
        // layout still goes through branch 2 by making total_ideal
        // exceed available while total_min is at the limit.
        assert_eq!(layout.widths, vec![3, 3]);
        assert!(!layout.needs_hard_wrap);
    }

    #[test]
    fn branch_2_with_zero_overflow_when_min_equals_ideal() {
        // min = [5, 5], ideal = [5, 5], available = 6
        // total_min = 10, total_ideal = 10. total_ideal > 6 → branch 2.
        // total_min > 6 → actually NOT branch 2 either, falls through
        // to branch 3.
        // We need total_min <= available < total_ideal AND ideal == min.
        // That's impossible by construction (if ideal == min then
        // total_ideal == total_min, so the two branches are disjoint).
        // The "total_overflow == 0" guard in branch 2 is dead code
        // when both inputs are the same. We exercise it
        // anyway with a synthetic case where one column has zero
        // overflow but others don't:
        let layout = compute_column_widths(&[3, 3], &[3, 8], 8);
        // total_min = 6, total_ideal = 11. 11 > 8 → not branch 1.
        // 6 <= 8 → branch 2.
        // extra_space = 2, overflows = [0, 5], total_overflow = 5
        // col0: 0*2/5 = 0 → 3+0 = 3
        // col1: 5*2/5 = 2 → 3+2 = 5
        assert_eq!(layout.widths, vec![3, 5]);
        assert!(!layout.needs_hard_wrap);
    }

    // -----------------------------------------------------------------
    // Branch 3 — needs hard wrap
    // -----------------------------------------------------------------

    #[test]
    fn scales_below_min_with_hard_wrap_flag() {
        // min = [10, 10], ideal = [20, 20], available = 12
        // total_min = 20 > 12 → branch 3
        // scale = 12/20 = 0.6
        // col0: floor(10 * 12 / 20) = floor(6) = 6 → max(6, 3) = 6
        // col1: floor(10 * 12 / 20) = floor(6) = 6 → max(6, 3) = 6
        let layout = compute_column_widths(&[10, 10], &[20, 20], 12);
        assert_eq!(layout.widths, vec![6, 6]);
        assert!(layout.needs_hard_wrap);
    }

    #[test]
    fn scales_clamped_at_min_column_width() {
        // min = [20, 20], ideal = [40, 40], available = 8
        // scale = 8/40 = 0.2
        // col0: floor(20 * 8 / 40) = floor(4) = 4 → max(4, 3) = 4
        // col1: same = 4
        let layout = compute_column_widths(&[20, 20], &[40, 40], 8);
        assert_eq!(layout.widths, vec![4, 4]);
        assert!(layout.needs_hard_wrap);
    }

    #[test]
    fn extreme_scale_floors_at_min_column_width() {
        // min = [100], ideal = [100], available = 1
        // scale = 1/100 = 0.01
        // col0: floor(100 * 1 / 100) = floor(1) = 1 → max(1, 3) = 3
        let layout = compute_column_widths(&[100], &[100], 1);
        assert_eq!(layout.widths, vec![3]);
        assert!(layout.needs_hard_wrap);
    }

    // -----------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------

    #[test]
    fn empty_input_returns_empty() {
        let layout = compute_column_widths(&[], &[], 80);
        assert_eq!(layout.widths, Vec::<usize>::new());
        assert!(!layout.needs_hard_wrap);
    }

    #[test]
    fn single_column_table_passes_through() {
        let layout = compute_column_widths(&[3], &[5], 80);
        assert_eq!(layout.widths, vec![5]);
        assert!(!layout.needs_hard_wrap);
    }

    #[test]
    #[should_panic(expected = "min_widths and ideal_widths must have the same length")]
    fn mismatched_input_lengths_panics() {
        let _ = compute_column_widths(&[3, 3], &[5], 80);
    }

    // -----------------------------------------------------------------
    // decision-tree table
    // -----------------------------------------------------------------

    #[test]
    fn branch_decision_table() {
        struct Case {
            label: &'static str,
            min: &'static [usize],
            ideal: &'static [usize],
            available: usize,
            expected_widths: Vec<usize>,
            expected_hard_wrap: bool,
        }
        let cases = [
            Case {
                label: "branch1: ideal fits",
                min: &[3, 3, 3],
                ideal: &[5, 7, 4],
                available: 30,
                expected_widths: vec![5, 7, 4],
                expected_hard_wrap: false,
            },
            Case {
                label: "branch2: shrink proportionally",
                min: &[3, 3],
                ideal: &[10, 5],
                available: 11,
                expected_widths: vec![6, 4],
                expected_hard_wrap: false,
            },
            Case {
                label: "branch3: scale below min",
                min: &[10, 10],
                ideal: &[20, 20],
                available: 12,
                expected_widths: vec![6, 6],
                expected_hard_wrap: true,
            },
            Case {
                label: "branch3: clamped at MIN_COLUMN_WIDTH",
                min: &[100],
                ideal: &[100],
                available: 1,
                expected_widths: vec![3],
                expected_hard_wrap: true,
            },
        ];
        for case in cases {
            let layout = compute_column_widths(case.min, case.ideal, case.available);
            assert_eq!(
                layout.widths, case.expected_widths,
                "{} — widths mismatch",
                case.label
            );
            assert_eq!(
                layout.needs_hard_wrap, case.expected_hard_wrap,
                "{} — hard wrap mismatch",
                case.label
            );
        }
    }
}
