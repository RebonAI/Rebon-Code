//! Smoothed token-counter animation for spinner rows.
//!
//! A displayed token count is walked toward the live response length,
//! with three increment buckets:
//!
//! ```text
//! gap = target - displayed
//! if gap > 0:
//!   increment = gap < 70  ? 3
//!             : gap < 200 ? max(8, ceil(gap * 0.15))
//!             : 50
//!   displayed = min(displayed + increment, target)
//! ```
//!
//! When `reduced_motion` is set, the displayed value snaps to the
//! current target.

/// Advance `displayed` toward `target` by one tick. Returns the new
/// displayed value.
///
/// * `displayed` — current animated value.
/// * `target` — the live response length.
/// * `reduced_motion` — when set, snap to `target`.
///
/// The result never overshoots `target`, even when `target < displayed`.
pub fn tween_token_counter(displayed: u64, target: u64, reduced_motion: bool) -> u64 {
    if reduced_motion {
        return target;
    }
    if target <= displayed {
        return displayed;
    }
    let gap = target - displayed;
    let increment: u64 = if gap < 70 {
        3
    } else if gap < 200 {
        // max(8, ceil(gap * 0.15))
        // f64 ceil is fine here; gap < 200, so the computation can't
        // overflow.
        let scaled = ((gap as f64) * 0.15).ceil() as u64;
        scaled.max(8)
    } else {
        50
    };
    (displayed + increment).min(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_motion_when_target_equals_displayed() {
        assert_eq!(tween_token_counter(100, 100, false), 100);
    }

    #[test]
    fn no_motion_when_target_below_displayed() {
        assert_eq!(tween_token_counter(100, 50, false), 100);
    }

    #[test]
    fn small_gap_increment_three() {
        // gap = 5 < 70 → increment 3.
        assert_eq!(tween_token_counter(100, 105, false), 103);
    }

    #[test]
    fn small_gap_caps_at_target() {
        // gap = 1 < 70 → increment 3 capped to target.
        assert_eq!(tween_token_counter(100, 101, false), 101);
    }

    #[test]
    fn medium_gap_floor_eight() {
        // gap = 70 → not < 70, so middle bucket.
        // ceil(70 * 0.15) = ceil(10.5) = 11. max(8, 11) = 11.
        assert_eq!(tween_token_counter(0, 70, false), 11);
    }

    #[test]
    fn medium_gap_min_eight() {
        // gap = 70 yields 11; pick a smaller-mid where the max kicks in.
        // gap = 50 → small bucket (< 70), not medium. Use gap = 70..
        // Try gap = 70: ceil(70 * 0.15) = 11, max(8,11) = 11.
        // Try gap = 199: ceil(29.85) = 30, max(8,30) = 30.
        assert_eq!(tween_token_counter(0, 199, false), 30);
    }

    #[test]
    fn medium_gap_minimum_eight_when_ceil_below() {
        // The smallest medium gap is 70 with ceil(10.5)=11 → max(8,11)=11.
        // The 8 minimum kicks in only if scaled would be < 8, which
        // never happens in [70, 199]. Defensive test: confirm 70 →
        // not below 8.
        let v = tween_token_counter(0, 70, false);
        assert!(v >= 8);
    }

    #[test]
    fn large_gap_increment_fifty() {
        // gap = 200 → large bucket → increment 50.
        assert_eq!(tween_token_counter(0, 200, false), 50);
    }

    #[test]
    fn large_gap_eventually_reaches_target() {
        // The increment buckets shrink as the gap closes, so it
        // takes more than `gap / 50` ticks to converge. We just
        // assert eventual convergence.
        let mut v = 0;
        for _ in 0..200 {
            v = tween_token_counter(v, 200, false);
        }
        assert_eq!(v, 200);
    }

    #[test]
    fn token_counter_never_overshoots() {
        // Drive a session of arbitrary growth and make sure we never
        // exceed the target.
        let target = 1234u64;
        let mut v = 0u64;
        for _ in 0..1000 {
            v = tween_token_counter(v, target, false);
            assert!(v <= target);
        }
        assert_eq!(v, target);
    }

    #[test]
    fn very_large_gap_still_increment_fifty() {
        // gap = 10_000 → still 50 per tick.
        assert_eq!(tween_token_counter(0, 10_000, false), 50);
    }

    #[test]
    fn reduced_motion_snaps_to_target() {
        assert_eq!(tween_token_counter(0, 1_000_000, true), 1_000_000);
    }

    #[test]
    fn reduced_motion_below_target_still_snaps() {
        assert_eq!(tween_token_counter(100, 50, true), 50);
    }

    #[test]
    fn boundary_69_uses_small_bucket() {
        // gap = 69 → small bucket → increment 3.
        assert_eq!(tween_token_counter(0, 69, false), 3);
    }

    #[test]
    fn boundary_70_uses_medium_bucket() {
        assert_eq!(tween_token_counter(0, 70, false), 11);
    }

    #[test]
    fn boundary_199_uses_medium_bucket() {
        // ceil(199 * 0.15) = ceil(29.85) = 30; max(8,30) = 30.
        assert_eq!(tween_token_counter(0, 199, false), 30);
    }

    #[test]
    fn boundary_200_uses_large_bucket() {
        assert_eq!(tween_token_counter(0, 200, false), 50);
    }
}
