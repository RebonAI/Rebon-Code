//! Elapsed-time math for spinner rows.
//!
//! Elapsed time is derived from four tracked values:
//!
//! * loading start — wall clock when the current turn began.
//! * total paused — accumulated paused milliseconds.
//! * pause start — wall clock when the current pause began, absent
//! when not paused.
//! * turn start — earliest derived start seen during a swarm
//! (anchors elapsed time across leader-task resets).
//!
//! And combines them into:
//!
//! ```text
//! elapsed = (pause start ?? now) - loading start - total paused
//! ```

/// Threshold (in ms) past which the "use /clear to start fresh" tip
/// is shown: `1_800_000`, i.e. 30 minutes.
pub const CLEAR_TIP_THRESHOLD_MS: u64 = 1_800_000;

/// Threshold (in ms) past which the "use /btw" tip is shown:
/// `30_000`, i.e. 30 seconds.
pub const BTW_TIP_THRESHOLD_MS: u64 = 30_000;

/// Compute the current elapsed time in ms for spinner status display.
///
/// `pause_start` is `Some(t)` while paused, `None` otherwise. Every
/// subtraction saturates, so the result is never a wrapped-around huge value.
pub fn current_elapsed_snapshot(
    now_ms: u64,
    loading_start_ms: u64,
    total_paused_ms: u64,
    pause_start_ms: Option<u64>,
) -> u64 {
    let basis = pause_start_ms.unwrap_or(now_ms);
    basis
        .saturating_sub(loading_start_ms)
        .saturating_sub(total_paused_ms)
}

/// Compute the effective elapsed time, anchoring to the earliest
/// turn-start seen during a swarm.
///
/// * If `has_running_teammates` is `false`, returns `elapsed_ms`
/// unchanged.
/// * If `true`, returns `max(elapsed_ms, now_ms - turn_start_ms)`.
pub fn effective_elapsed_ms(
    elapsed_ms: u64,
    now_ms: u64,
    turn_start_ms: u64,
    has_running_teammates: bool,
) -> u64 {
    if !has_running_teammates {
        return elapsed_ms;
    }
    let from_turn_start = now_ms.saturating_sub(turn_start_ms);
    elapsed_ms.max(from_turn_start)
}

/// Whether the "use /clear to start fresh" tip should be shown.
///
/// Returns `true` when tips are enabled and `elapsed_snapshot >
/// CLEAR_TIP_THRESHOLD_MS`. The comparison is strict.
pub fn should_show_clear_tip(elapsed_snapshot_ms: u64, tips_enabled: bool) -> bool {
    tips_enabled && elapsed_snapshot_ms > CLEAR_TIP_THRESHOLD_MS
}

/// Whether the "use /btw" tip should be shown.
///
/// Returns `true` when tips are enabled, `elapsed_snapshot >
/// BTW_TIP_THRESHOLD_MS`, and the global config reports no `/btw` use
/// yet (`btw_use_count == 0`).
pub fn should_show_btw_tip(
    elapsed_snapshot_ms: u64,
    tips_enabled: bool,
    btw_use_count: u64,
) -> bool {
    tips_enabled && elapsed_snapshot_ms > BTW_TIP_THRESHOLD_MS && btw_use_count == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_no_pause_is_now_minus_start() {
        // now=10000, start=2000, paused=500 → 10000 - 2000 - 500 = 7500.
        assert_eq!(current_elapsed_snapshot(10000, 2000, 500, None), 7500);
    }

    #[test]
    fn elapsed_with_pause_uses_pause_start() {
        // pause started at 6000 → 6000 - 2000 - 500 = 3500.
        assert_eq!(current_elapsed_snapshot(10000, 2000, 500, Some(6000)), 3500);
    }

    #[test]
    fn elapsed_no_paused_time() {
        assert_eq!(current_elapsed_snapshot(10000, 2000, 0, None), 8000);
    }

    #[test]
    fn elapsed_now_equals_start_is_zero() {
        assert_eq!(current_elapsed_snapshot(2000, 2000, 0, None), 0);
    }

    #[test]
    fn elapsed_clamps_at_zero_for_pre_start_now() {
        // This shouldn't happen in practice but the saturating_sub
        // keeps the function total.
        assert_eq!(current_elapsed_snapshot(1000, 2000, 0, None), 0);
    }

    #[test]
    fn effective_no_teammates_returns_elapsed() {
        assert_eq!(effective_elapsed_ms(5000, 10000, 1000, false), 5000);
    }

    #[test]
    fn effective_teammates_anchors_to_turn_start() {
        // turn started at 1000, now=10000 → from-turn = 9000.
        // elapsed = 5000. max(5000, 9000) = 9000.
        assert_eq!(effective_elapsed_ms(5000, 10000, 1000, true), 9000);
    }

    #[test]
    fn effective_teammates_keeps_elapsed_when_larger() {
        // elapsed = 10000, from-turn = 9000. max = 10000.
        assert_eq!(effective_elapsed_ms(10000, 10000, 1000, true), 10000);
    }

    #[test]
    fn show_clear_tip_disabled_by_default() {
        assert!(!should_show_clear_tip(2_000_000, false));
    }

    #[test]
    fn show_clear_tip_at_threshold_no() {
        // strict >, not >=
        assert!(!should_show_clear_tip(CLEAR_TIP_THRESHOLD_MS, true));
    }

    #[test]
    fn show_clear_tip_above_threshold_yes() {
        assert!(should_show_clear_tip(CLEAR_TIP_THRESHOLD_MS + 1, true));
    }

    #[test]
    fn show_btw_tip_disabled_by_default() {
        assert!(!should_show_btw_tip(60_000, false, 0));
    }

    #[test]
    fn show_btw_tip_count_blocks() {
        // Already used /btw → don't show the tip.
        assert!(!should_show_btw_tip(60_000, true, 1));
    }

    #[test]
    fn show_btw_tip_above_threshold_yes() {
        assert!(should_show_btw_tip(BTW_TIP_THRESHOLD_MS + 1, true, 0));
    }

    #[test]
    fn show_btw_tip_at_threshold_no() {
        assert!(!should_show_btw_tip(BTW_TIP_THRESHOLD_MS, true, 0));
    }

    #[test]
    fn thresholds_are_pinned() {
        assert_eq!(CLEAR_TIP_THRESHOLD_MS, 1_800_000);
        assert_eq!(BTW_TIP_THRESHOLD_MS, 30_000);
    }
}
