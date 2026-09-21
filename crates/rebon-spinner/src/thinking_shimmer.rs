//! Thinking-shimmer color computation for spinner rows.
//!
//! Two grey colors and a 2-second sine glow:
//!
//! ```text
//! THINKING_INACTIVE         = 153, 153, 153
//! THINKING_INACTIVE_SHIMMER = 185, 185, 185
//! THINKING_DELAY_MS         = 3000
//! THINKING_GLOW_PERIOD_S    = 2
//!
//! elapsed_sec = (time - THINKING_DELAY_MS) / 1000
//! opacity     = 0                                            if time < THINKING_DELAY_MS
//!             = (sin(elapsed_sec * 2*PI / THINKING_GLOW_PERIOD_S) + 1) / 2 otherwise
//! ```

use crate::color::{interpolate_color, RgbColor};

/// Mid-grey resting color used by the thinking shimmer.
pub const THINKING_INACTIVE: RgbColor = RgbColor::new(153, 153, 153);

/// Light-grey peak shimmer color.
pub const THINKING_INACTIVE_SHIMMER: RgbColor = RgbColor::new(185, 185, 185);

/// Delay in milliseconds before the shimmer animation starts.
pub const THINKING_DELAY_MS: u64 = 3000;

/// Period of the sine glow in seconds.
pub const THINKING_GLOW_PERIOD_S: f64 = 2.0;

/// Compute the thinking-shimmer color at the given monotonic
/// `time_ms`.
///
/// * Before `THINKING_DELAY_MS` → returns the inactive color (opacity
/// 0).
/// * After → linear interpolation between `THINKING_INACTIVE` and
/// `THINKING_INACTIVE_SHIMMER` driven by a 2-second sine wave.
pub fn thinking_shimmer_color(time_ms: u64) -> RgbColor {
    if time_ms < THINKING_DELAY_MS {
        return THINKING_INACTIVE;
    }
    let elapsed_sec = ((time_ms - THINKING_DELAY_MS) as f64) / 1000.0;
    let opacity =
        ((elapsed_sec * std::f64::consts::PI * 2.0 / THINKING_GLOW_PERIOD_S).sin() + 1.0) / 2.0;
    interpolate_color(THINKING_INACTIVE, THINKING_INACTIVE_SHIMMER, opacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn before_delay_is_inactive() {
        assert_eq!(thinking_shimmer_color(0), THINKING_INACTIVE);
        assert_eq!(thinking_shimmer_color(2999), THINKING_INACTIVE);
    }

    #[test]
    fn at_delay_starts_at_midpoint_then_rises() {
        // sin(0) = 0; (0+1)/2 = 0.5; midpoint between 153 and 185.
        let c = thinking_shimmer_color(THINKING_DELAY_MS);
        // (153 + 185) / 2 = 169.
        assert_eq!(c, RgbColor::new(169, 169, 169));
    }

    #[test]
    fn at_quarter_period_is_peak() {
        // 0.5s into the period → sin(0.5 PI) = 1 → opacity 1 → shimmer.
        let c = thinking_shimmer_color(THINKING_DELAY_MS + 500);
        assert_eq!(c, THINKING_INACTIVE_SHIMMER);
    }

    #[test]
    fn at_half_period_returns_to_midpoint() {
        // 1s into the period → sin(PI) = 0 → opacity 0.5 → midpoint.
        let c = thinking_shimmer_color(THINKING_DELAY_MS + 1000);
        assert_eq!(c, RgbColor::new(169, 169, 169));
    }

    #[test]
    fn at_three_quarters_period_is_inactive() {
        // 1.5s into the period → sin(1.5 PI) = -1 → opacity 0 →
        // inactive.
        let c = thinking_shimmer_color(THINKING_DELAY_MS + 1500);
        assert_eq!(c, THINKING_INACTIVE);
    }

    #[test]
    fn full_period_repeats_midpoint() {
        let a = thinking_shimmer_color(THINKING_DELAY_MS);
        let b = thinking_shimmer_color(THINKING_DELAY_MS + 2000);
        assert_eq!(a, b);
    }
}
