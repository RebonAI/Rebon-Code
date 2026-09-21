//! Spinner glyph dispatch and platform-specific character choices.
//!
//! The twelve spinner frames are the six base characters followed by the
//! same six in reverse, indexed by `frame % 12`. Which six base
//! characters apply depends on the platform / terminal:
//!
//! * Ghostty: `·`, `✢`, `✳`, `✶`, `✻`, `*` — Ghostty renders the sixth glyph
//!   slightly offset, so it uses `*` instead.
//! * macOS: `·`, `✢`, `✳`, `✶`, `✻`, `✽`.
//! * everything else (the linux variant): `·`, `✢`, `*`, `✶`, `✻`, `✽` — the
//!   `*` at index 2 is the only difference from the macOS set.
//!
//! Reading `TERM` and the process platform happens in the caller, which passes
//! a [`GlyphPlatform`].

use crate::color::{interpolate_color, RgbColor};

/// The dim-orange "stalled" color: `r: 171, g: 43, b: 63`.
pub const ERROR_RED: RgbColor = RgbColor::new(171, 43, 63);

/// The reduced-motion fallback glyph: a single solid bullet.
pub const REDUCED_MOTION_DOT: &str = "●";

/// The reduced-motion cycle period in milliseconds. The dot toggles
/// between dim and bright every half-period: 1s on, 1s off.
pub const REDUCED_MOTION_CYCLE_MS: u64 = 2000;

/// Caller-supplied platform / terminal hint: which of the three base
/// character sets applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphPlatform {
    /// The terminal is Ghostty.
    Ghostty,
    /// The platform is macOS.
    Darwin,
    /// Anything else (linux, windows, msys, …).
    Other,
}

/// The six base spinner glyphs for the given platform.
pub fn default_characters(platform: GlyphPlatform) -> [&'static str; 6] {
    match platform {
        GlyphPlatform::Ghostty => ["·", "✢", "✳", "✶", "✻", "*"],
        GlyphPlatform::Darwin => ["·", "✢", "✳", "✶", "✻", "✽"],
        GlyphPlatform::Other => ["·", "✢", "*", "✶", "✻", "✽"],
    }
}

/// The twelve animation frames: the six base characters in forward
/// order, then the same six in reverse order.
pub fn spinner_frames(platform: GlyphPlatform) -> [&'static str; 12] {
    let base = default_characters(platform);
    [
        base[0], base[1], base[2], base[3], base[4], base[5], base[5], base[4], base[3], base[2],
        base[1], base[0],
    ]
}

/// Pick the spinner glyph for `frame`, wrapping modulo the twelve
/// frames.
pub fn glyph_for_frame(platform: GlyphPlatform, frame: u64) -> &'static str {
    let frames = spinner_frames(platform);
    frames[(frame as usize) % frames.len()]
}

const TOOL_CALL_SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TOOL_CALL_SPINNER_FRAME_MS: u64 = 80;

/// Return the current frame index for the tool-call spinner.
pub fn tool_call_spinner_frame(time_ms: u64) -> usize {
    ((time_ms / TOOL_CALL_SPINNER_FRAME_MS) % TOOL_CALL_SPINNER_FRAMES.len() as u64) as usize
}

/// Return the dedicated Braille spinner glyph for an active tool call.
pub fn tool_call_spinner_glyph(time_ms: u64) -> &'static str {
    TOOL_CALL_SPINNER_FRAMES[tool_call_spinner_frame(time_ms)]
}

/// Whether the reduced-motion dot should be dimmed at the given
/// monotonic `time_ms`: dim when
/// `(time_ms / (REDUCED_MOTION_CYCLE_MS / 2)) % 2 == 1`, i.e. for the
/// second half of every cycle.
pub fn reduced_motion_dot_is_dim(time_ms: u64) -> bool {
    let half = REDUCED_MOTION_CYCLE_MS / 2;
    if half == 0 {
        return false;
    }
    (time_ms / half) % 2 == 1
}

/// The discriminated result of [`stalled_glyph_color`].
#[derive(Debug, Clone, PartialEq)]
pub enum StalledColor {
    /// `stalled_intensity == 0`: render with the plain message color
    /// theme key.
    PlainMessage,
    /// `stalled_intensity > 0` and theme lookup succeeded: render
    /// with this interpolated RGB color.
    Interpolated(RgbColor),
    /// `stalled_intensity > 0` but theme key wasn't found:
    /// fallback to either `"error"` (when intensity > 0.5) or the
    /// plain message color.
    Fallback {
        /// True when intensity > 0.5, meaning the `"error"` key is used;
        /// false means the message color.
        use_error_color: bool,
    },
}

/// Choose the color to paint a stalled row.
///
/// * `stalled_intensity <= 0` gives [`StalledColor::PlainMessage`]; the test is
/// `<=` so negative values are treated as plain and the function stays total.
/// * `stalled_intensity > 0` with `theme_color` set gives
/// [`StalledColor::Interpolated`] over
/// `interpolate_color(theme, ERROR_RED, stalled_intensity)`.
/// * `stalled_intensity > 0` with `theme_color` unset gives
/// [`StalledColor::Fallback`], which uses the `"error"` key when the
/// intensity is strictly above 0.5.
pub fn stalled_glyph_color(stalled_intensity: f64, theme_color: Option<RgbColor>) -> StalledColor {
    if stalled_intensity <= 0.0 {
        return StalledColor::PlainMessage;
    }
    if let Some(base) = theme_color {
        return StalledColor::Interpolated(interpolate_color(base, ERROR_RED, stalled_intensity));
    }
    StalledColor::Fallback {
        use_error_color: stalled_intensity > 0.5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ghostty_glyphs_pinned() {
        assert_eq!(
            default_characters(GlyphPlatform::Ghostty),
            ["·", "✢", "✳", "✶", "✻", "*"]
        );
    }

    #[test]
    fn darwin_glyphs_pinned() {
        assert_eq!(
            default_characters(GlyphPlatform::Darwin),
            ["·", "✢", "✳", "✶", "✻", "✽"]
        );
    }

    #[test]
    fn other_glyphs_pinned() {
        // Note linux uses '*' at index 2, not '✳'.
        assert_eq!(
            default_characters(GlyphPlatform::Other),
            ["·", "✢", "*", "✶", "✻", "✽"]
        );
    }

    #[test]
    fn spinner_frames_length_is_twelve() {
        assert_eq!(spinner_frames(GlyphPlatform::Darwin).len(), 12);
    }

    #[test]
    fn spinner_frames_first_six_are_forward() {
        let f = spinner_frames(GlyphPlatform::Darwin);
        let base = default_characters(GlyphPlatform::Darwin);
        for i in 0..6 {
            assert_eq!(f[i], base[i]);
        }
    }

    #[test]
    fn spinner_frames_second_six_are_reverse() {
        let f = spinner_frames(GlyphPlatform::Darwin);
        let base = default_characters(GlyphPlatform::Darwin);
        for i in 0..6 {
            assert_eq!(f[6 + i], base[5 - i]);
        }
    }

    #[test]
    fn glyph_for_frame_zero_is_first_char() {
        assert_eq!(glyph_for_frame(GlyphPlatform::Darwin, 0), "·");
    }

    #[test]
    fn glyph_for_frame_six_is_last_char() {
        // Index 6 in [0..12] is the same as index 5 (reverse).
        assert_eq!(glyph_for_frame(GlyphPlatform::Darwin, 6), "✽");
    }

    #[test]
    fn glyph_for_frame_wraps_at_twelve() {
        let g0 = glyph_for_frame(GlyphPlatform::Darwin, 0);
        let g12 = glyph_for_frame(GlyphPlatform::Darwin, 12);
        let g24 = glyph_for_frame(GlyphPlatform::Darwin, 24);
        assert_eq!(g0, g12);
        assert_eq!(g0, g24);
    }

    #[test]
    fn glyph_for_frame_wraparound_large() {
        // The frame may grow without bound; we mod-12 it.
        assert_eq!(
            glyph_for_frame(GlyphPlatform::Darwin, 1000_000_005),
            glyph_for_frame(GlyphPlatform::Darwin, 1000_000_005 % 12),
        );
    }

    #[test]
    fn tool_call_spinner_uses_braille_orbit() {
        let frames = (0..10)
            .map(|frame| tool_call_spinner_glyph(frame * TOOL_CALL_SPINNER_FRAME_MS))
            .collect::<Vec<_>>();
        assert_eq!(frames, ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]);
    }

    #[test]
    fn tool_call_spinner_holds_and_wraps_frames() {
        assert_eq!(tool_call_spinner_frame(0), 0);
        assert_eq!(tool_call_spinner_frame(79), 0);
        assert_eq!(tool_call_spinner_frame(80), 1);
        assert_eq!(tool_call_spinner_frame(799), 9);
        assert_eq!(tool_call_spinner_frame(800), 0);
        assert_eq!(tool_call_spinner_glyph(800), "⠋");
    }

    #[test]
    fn reduced_motion_dim_zero_is_bright() {
        // floor(0 / 1000) % 2 == 0 → bright.
        assert!(!reduced_motion_dot_is_dim(0));
    }

    #[test]
    fn reduced_motion_dim_at_one_second_flips() {
        // floor(1000 / 1000) % 2 == 1 → dim.
        assert!(reduced_motion_dot_is_dim(1000));
    }

    #[test]
    fn reduced_motion_dim_at_two_seconds_resets() {
        assert!(!reduced_motion_dot_is_dim(2000));
    }

    #[test]
    fn reduced_motion_dim_just_under_flip_point() {
        assert!(!reduced_motion_dot_is_dim(999));
    }

    #[test]
    fn stalled_color_zero_intensity_is_plain() {
        assert_eq!(
            stalled_glyph_color(0.0, Some(RgbColor::new(100, 100, 100))),
            StalledColor::PlainMessage
        );
    }

    #[test]
    fn stalled_color_full_intensity_is_error_red() {
        let c = stalled_glyph_color(1.0, Some(RgbColor::new(100, 100, 100)));
        assert_eq!(c, StalledColor::Interpolated(ERROR_RED));
    }

    #[test]
    fn stalled_color_half_intensity_is_midpoint() {
        let base = RgbColor::new(100, 100, 100);
        let c = stalled_glyph_color(0.5, Some(base));
        let expected = interpolate_color(base, ERROR_RED, 0.5);
        assert_eq!(c, StalledColor::Interpolated(expected));
    }

    #[test]
    fn stalled_color_no_theme_falls_back() {
        // intensity > 0.5 → use_error_color: true
        assert_eq!(
            stalled_glyph_color(0.7, None),
            StalledColor::Fallback {
                use_error_color: true,
            }
        );
        // intensity ≤ 0.5 → use_error_color: false
        assert_eq!(
            stalled_glyph_color(0.4, None),
            StalledColor::Fallback {
                use_error_color: false,
            }
        );
    }

    #[test]
    fn stalled_color_intensity_just_above_zero_uses_message_color_when_no_theme() {
        // 0.1 > 0 but ≤ 0.5 → fallback uses the message color.
        assert_eq!(
            stalled_glyph_color(0.1, None),
            StalledColor::Fallback {
                use_error_color: false,
            }
        );
    }

    #[test]
    fn stalled_color_negative_intensity_treated_as_plain() {
        // Only `> 0` counts as stalled; negative values shouldn't
        // happen in practice but are handled as plain to keep the
        // function total.
        assert_eq!(
            stalled_glyph_color(-1.0, Some(RgbColor::new(50, 50, 50))),
            StalledColor::PlainMessage
        );
    }
}
