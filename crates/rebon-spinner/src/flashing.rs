//! Flashing-character color interpolation for spinner messages.
//!
//! A sine wave produces a "flash opacity" between 0 and 1, which linearly
//! interpolates between the message color and the shimmer color. When the theme
//! palette does not resolve both to RGB (ANSI themes), the interpolation falls
//! back to a binary switch at opacity > 0.5.
//!
//! ```text
//! flash_opacity = 0                                 if reduced_motion or not a tool use
//!               = (sin(time_ms / 1000 * PI) + 1) / 2 otherwise
//! ```

use crate::color::{interpolate_color, RgbColor};

/// Returns `(sin(time_s * PI) + 1) / 2`, a value in `[0, 1]` that
/// oscillates with a 2-second period.
///
/// Returns `0.0` when `reduced_motion` is set, or when the row is not a
/// tool-use row.
pub fn flash_opacity_at(time_ms: u64, is_tool_use: bool, reduced_motion: bool) -> f64 {
    if reduced_motion || !is_tool_use {
        return 0.0;
    }
    let time_s = (time_ms as f64) / 1000.0;
    ((time_s * std::f64::consts::PI).sin() + 1.0) / 2.0
}

/// The discriminated result of [`flashing_char_color`].
#[derive(Debug, Clone, PartialEq)]
pub enum FlashingResult {
    /// Both theme keys resolved to RGB. Use this interpolated color.
    Interpolated(RgbColor),
    /// Theme didn't resolve. Fallback to either the shimmer key
    /// (when opacity > 0.5) or the message key.
    BinaryFallback {
        /// True when the flash opacity is above 0.5, so the shimmer key is used.
        use_shimmer: bool,
    },
}

/// Pick the color for one flashing character.
///
/// * Both `message_color` and `shimmer_color` resolve, giving
/// [`FlashingResult::Interpolated`] over
/// `interpolate_color(message, shimmer, flash_opacity)`.
/// * Either is `None`, giving [`FlashingResult::BinaryFallback`]: use the
/// shimmer color when opacity > 0.5, strictly.
pub fn flashing_char_color(
    flash_opacity: f64,
    message_color: Option<RgbColor>,
    shimmer_color: Option<RgbColor>,
) -> FlashingResult {
    if let (Some(m), Some(s)) = (message_color, shimmer_color) {
        return FlashingResult::Interpolated(interpolate_color(m, s, flash_opacity));
    }
    FlashingResult::BinaryFallback {
        use_shimmer: flash_opacity > 0.5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn flash_opacity_zero_when_not_tool_use() {
        assert_eq!(flash_opacity_at(500, false, false), 0.0);
    }

    #[test]
    fn flash_opacity_zero_when_reduced_motion() {
        assert_eq!(flash_opacity_at(500, true, true), 0.0);
    }

    #[test]
    fn flash_opacity_at_t_zero_is_half() {
        // sin(0) = 0; (0 + 1) / 2 = 0.5.
        assert!(approx(flash_opacity_at(0, true, false), 0.5));
    }

    #[test]
    fn flash_opacity_at_half_second_is_one() {
        // sin(0.5 * PI) = 1; (1 + 1) / 2 = 1.
        assert!(approx(flash_opacity_at(500, true, false), 1.0));
    }

    #[test]
    fn flash_opacity_at_one_second_is_half() {
        // sin(PI) = 0; (0 + 1) / 2 = 0.5.
        assert!(approx(flash_opacity_at(1000, true, false), 0.5));
    }

    #[test]
    fn flash_opacity_at_1_5_seconds_is_zero() {
        // sin(1.5 * PI) = -1; (-1 + 1) / 2 = 0.
        assert!(approx(flash_opacity_at(1500, true, false), 0.0));
    }

    #[test]
    fn flash_opacity_at_two_seconds_repeats_zero() {
        assert!(approx(
            flash_opacity_at(0, true, false),
            flash_opacity_at(2000, true, false)
        ));
    }

    #[test]
    fn flashing_color_with_themes_interpolates() {
        let m = RgbColor::new(0, 0, 0);
        let s = RgbColor::new(255, 255, 255);
        match flashing_char_color(0.5, Some(m), Some(s)) {
            FlashingResult::Interpolated(c) => {
                assert_eq!(c, RgbColor::new(128, 128, 128));
            }
            _ => panic!("expected interpolated"),
        }
    }

    #[test]
    fn flashing_color_no_message_falls_back() {
        match flashing_char_color(0.7, None, Some(RgbColor::new(255, 255, 255))) {
            FlashingResult::BinaryFallback { use_shimmer } => assert!(use_shimmer),
            _ => panic!("expected fallback"),
        }
    }

    #[test]
    fn flashing_color_no_shimmer_falls_back() {
        match flashing_char_color(0.3, Some(RgbColor::new(0, 0, 0)), None) {
            FlashingResult::BinaryFallback { use_shimmer } => assert!(!use_shimmer),
            _ => panic!("expected fallback"),
        }
    }

    #[test]
    fn flashing_color_threshold_inclusive_below() {
        // exactly 0.5 is not > 0.5, so use_shimmer = false.
        match flashing_char_color(0.5, None, None) {
            FlashingResult::BinaryFallback { use_shimmer } => assert!(!use_shimmer),
            _ => panic!("expected fallback"),
        }
    }
}
