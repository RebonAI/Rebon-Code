//! Usage tab — pure number formatting and the data shapes the tab reads.
//!
//! [`format_cost`] renders a dollar amount as a display string,
//! [`SubscriptionType`] says which plan's numbers are being shown, and
//! [`RateLimit`] is one rate-limit window. Nothing here paints.
//!
//! ## Fixed rules
//!
//! 1. **A cost above 0.5 is rounded to 100ths and shown with 2 decimal
//!    places; at or below 0.5 the caller's decimal-place count is
//!    used.** The comparison is `> 0.5`, NOT `>= 0.5`.
//! 2. **Rounding is half away from zero** — `1.235` renders `$1.24`,
//!    and a negative under the threshold keeps its sign
//!    (`-0.1` at 4dp is `$-0.1000`).
//! 3. **A utilization of `0` is real data.** Only `None` hides a rate
//!    limit row; `Some(0.0)` still renders.

/// The cost above which [`format_cost`] switches the display
/// to 2dp.
pub const FORMAT_COST_HIGH_THRESHOLD: f64 = 0.5;

/// The subscription plan a session is on — `Pro`, `Max`, `Team`,
/// `Enterprise`, or [`SubscriptionType::Unknown`] when no plan was
/// reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionType {
    /// The Pro plan.
    Pro,
    /// The Max plan.
    Max,
    /// A team seat.
    Team,
    /// An enterprise seat.
    Enterprise,
    /// No plan reported — not the same as "no plan".
    Unknown,
}

/// Render a cost as a `$` string.
///
/// Above [`FORMAT_COST_HIGH_THRESHOLD`] the amount is rounded to 100ths
/// first and then formatted with exactly 2 decimal places. At or below
/// the threshold it is formatted with `max_decimal_places` decimal
/// places, with no pre-rounding.
///
/// Rounding is half away from zero, so `1.235` renders as `$1.24`.
pub fn format_cost(cost: f64, max_decimal_places: u32) -> String {
    if cost > FORMAT_COST_HIGH_THRESHOLD {
        // Round to 100ths first, then format with 2dp.
        let rounded = (cost * 100.0).round() / 100.0;
        format!("${:.2}", rounded)
    } else {
        // Format with the caller's decimal-place count.
        format!("${:.*}", max_decimal_places as usize, cost)
    }
}

/// Convenience form of [`format_cost`] with 4 decimal places.
pub fn format_cost_default(cost: f64) -> String {
    format_cost(cost, 4)
}

/// One rate-limit window as the usage tab reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimit {
    /// `None` → no data was reported, hide the row.
    pub utilization: Option<f64>,
    /// When the window resets, in epoch seconds; `None` hides the reset text.
    pub resets_at_epoch_seconds: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- format_cost ----

    #[test]
    fn format_cost_default_below_threshold_uses_4dp() {
        assert_eq!(format_cost_default(0.0), "$0.0000");
        assert_eq!(format_cost_default(0.123456), "$0.1235");
        assert_eq!(format_cost_default(0.5), "$0.5000"); // exactly 0.5 → NOT > 0.5
    }

    #[test]
    fn format_cost_above_threshold_uses_2dp() {
        // 0.51 → > 0.5 → 2dp
        assert_eq!(format_cost_default(0.51), "$0.51");
        assert_eq!(format_cost_default(1.234), "$1.23");
        assert_eq!(format_cost_default(1.235), "$1.24"); // round half away
        assert_eq!(format_cost_default(99.999), "$100.00");
    }

    #[test]
    fn format_cost_zero() {
        assert_eq!(format_cost_default(0.0), "$0.0000");
        assert_eq!(format_cost(0.0, 2), "$0.00");
    }

    #[test]
    fn format_cost_max_decimal_places_argument() {
        assert_eq!(format_cost(0.001, 2), "$0.00");
        assert_eq!(format_cost(0.001, 4), "$0.0010");
        assert_eq!(format_cost(0.001, 6), "$0.001000");
    }

    #[test]
    fn format_cost_threshold_boundary() {
        // > 0.5, NOT >= 0.5 — the check is strictly greater-than.
        assert_eq!(format_cost_default(0.5000001), "$0.50");
        assert_eq!(format_cost_default(0.5), "$0.5000");
    }

    #[test]
    fn format_cost_negative() {
        // Negatives aren't special-cased; Rust matches.
        assert_eq!(format_cost_default(-0.1), "$-0.1000");
    }

    #[test]
    fn format_cost_high_threshold_constant_pinned() {
        assert_eq!(FORMAT_COST_HIGH_THRESHOLD, 0.5);
    }

    // ---- cost-formatting cases ----

    #[test]
    fn format_cost_table() {
        let cases: &[(f64, u32, &str)] = &[
            (0.0, 4, "$0.0000"),
            (0.5, 4, "$0.5000"),
            (0.50000001, 4, "$0.50"),
            (0.51, 4, "$0.51"),
            (1.0, 4, "$1.00"),
            (10.0, 4, "$10.00"),
            (100.0, 4, "$100.00"),
            (1234.5678, 4, "$1234.57"),
            (0.0001, 4, "$0.0001"),
            (0.0001, 2, "$0.00"),
            (0.0, 2, "$0.00"),
        ];
        for (cost, dp, expected) in cases {
            assert_eq!(format_cost(*cost, *dp), *expected, "case={cost} dp={dp}");
        }
    }
}
