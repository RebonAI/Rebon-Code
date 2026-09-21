//! The animated asterisk the landing screen and the header draw.
//!
//! Moved here from the logo crate, which was a crate of its own for this one
//! reducer plus two mascot modules nothing ever called. A reducer that only
//! the terminal's own header renders belongs next to the header.
//!
//! Note the third `hue_to_rgb` in the tree: `crate::input::voice_cursor` has
//! one for the waveform cursor. Same conversion, different callers, and
//! neither crate depends on the other -- worth folding together if a third
//! caller ever appears.

/// RGB color helper used by animation projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

impl RgbColor {
    /// Creates a new color.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// Length of one sweep in milliseconds.
pub const SWEEP_DURATION_MS: u64 = 1_500;
/// Number of sweeps in the animation.
pub const SWEEP_COUNT: u64 = 2;
/// Total animation length: `SWEEP_DURATION_MS * SWEEP_COUNT`.
pub const TOTAL_ANIMATION_MS: u64 = SWEEP_DURATION_MS * SWEEP_COUNT;
/// Settled grey — red, green and blue all 153.
pub const SETTLED_GREY: RgbColor = RgbColor::new(153, 153, 153);

/// Computed frame for animated asterisk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsteriskFrame {
    /// Whether animation has reached settled state.
    pub done: bool,
    /// Current glyph color.
    pub color: RgbColor,
}

/// Stateless projection of the asterisk at a given elapsed time.
///
/// `elapsed_ms` is measured from the moment the animation started.
pub fn animated_asterisk_state(reduced_motion: bool, elapsed_ms: u64) -> AsteriskFrame {
    if reduced_motion || elapsed_ms >= TOTAL_ANIMATION_MS {
        return AsteriskFrame {
            done: true,
            color: SETTLED_GREY,
        };
    }
    let hue = ((elapsed_ms as f64 / SWEEP_DURATION_MS as f64) * 360.0) % 360.0;
    AsteriskFrame {
        done: false,
        color: hue_to_rgb(hue),
    }
}

/// HSL hue (0-360) to RGB with `s=0.7` and `l=0.6`. The hue is taken
/// modulo 360 first, and each channel is rounded to a `u8`.
pub fn hue_to_rgb(hue: f64) -> RgbColor {
    let h = hue.rem_euclid(360.0);
    let s = 0.7_f64;
    let l = 0.6_f64;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (((h / 60.0) % 2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = if h < 60.0 {
        (c, x, 0.0)
    } else if h < 120.0 {
        (x, c, 0.0)
    } else if h < 180.0 {
        (0.0, c, x)
    } else if h < 240.0 {
        (0.0, x, c)
    } else if h < 300.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    RgbColor {
        r: ((r + m) * 255.0).round() as u8,
        g: ((g + m) * 255.0).round() as u8,
        b: ((b + m) * 255.0).round() as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_are_pinned() {
        assert_eq!(SWEEP_DURATION_MS, 1_500);
        assert_eq!(SWEEP_COUNT, 2);
        assert_eq!(TOTAL_ANIMATION_MS, 3_000);
        assert_eq!(SETTLED_GREY, RgbColor::new(153, 153, 153));
    }

    #[test]
    fn reduced_motion_starts_done() {
        let f = animated_asterisk_state(true, 0);
        assert!(f.done);
        assert_eq!(f.color, SETTLED_GREY);
    }

    #[test]
    fn still_animating_before_total_duration() {
        let f = animated_asterisk_state(false, 1_200);
        assert!(!f.done);
        assert_ne!(f.color, SETTLED_GREY);
    }

    #[test]
    fn done_at_total_duration() {
        let f = animated_asterisk_state(false, TOTAL_ANIMATION_MS);
        assert!(f.done);
        assert_eq!(f.color, SETTLED_GREY);
    }

    #[test]
    fn hue_zero_matches_known_red() {
        assert_eq!(hue_to_rgb(0.0), RgbColor::new(224, 82, 82));
    }

    #[test]
    fn hue_wraps() {
        assert_eq!(hue_to_rgb(390.0), hue_to_rgb(30.0));
        assert_eq!(hue_to_rgb(-30.0), hue_to_rgb(330.0));
    }
}
