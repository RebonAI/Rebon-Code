//! Voice-recording waveform cursor.
//!
//! This module covers:
//!
//! * The `BARS`, `CURSOR_WAVEFORM_WIDTH`, `SMOOTH`, `LEVEL_BOOST`,
//!   `SILENCE_THRESHOLD` constants.
//! * The [`VoiceCursor`] selection tree: the accessibility-disabled
//!   short-circuit, the "voice recording + not-reduced-motion"
//!   branch that picks a bar glyph + hue-driven colour, and the
//!   fallthrough to plain inverse.
//!
//! No rendering, animation, or state plumbing lives here. The module
//! returns a discriminated enum describing what the renderer should
//! draw; the renderer owns the ANSI output.
//!
//! The `hue_to_rgb` helper is defined locally because it is only
//! used by the voice cursor code; keeping it here keeps the module
//! self-contained.

/// The 9-glyph bar ramp. First char is a
/// plain space ("silent"), then 8 rising Unicode block characters.
///
/// Exactly: `' \u2581\u2582\u2583\u2584\u2585\u2586\u2587\u2588'`.
pub const BARS: &str = " \u{2581}\u{2582}\u{2583}\u{2584}\u{2585}\u{2586}\u{2587}\u{2588}";

/// Number of bars in [`BARS`]. Baked so tests can assert the ramp
/// shape without re-counting in every callsite.
pub const BARS_LEN: usize = 9;

/// Width of the mini waveform cursor. Always 1; surfaced as a
/// constant for consumers that want to pre-allocate.
pub const CURSOR_WAVEFORM_WIDTH: usize = 1;

/// EMA smoothing factor. 0 = instant, 1 = frozen.
/// Applied as `new = old * SMOOTH + target * (1 - SMOOTH)`.
pub const SMOOTH: f64 = 0.7;

/// Audio-level boost factor. Level computation
/// normalises with a conservative divisor, so normal speech sits
/// around 0.3-0.5; this multiplier lets the bar use the full range.
pub const LEVEL_BOOST: f64 = 1.8;

/// Pre-boost silence threshold. Below this the
/// cursor is grey instead of hue-coloured. Speech typically starts
/// around 0.2+.
pub const SILENCE_THRESHOLD: f64 = 0.15;

/// Input to [`compute_waveform_cursor`].
///
/// The subset of caller-resolved inputs that feed into the
/// [`VoiceCursor`] selection. Every field is caller-resolved — no state
/// or subsystem reads happen here.
#[derive(Debug, Clone, Copy)]
pub struct VoiceCursorInput {
    /// Whether the terminal window currently has OS-level focus.
    /// When false the cursor is not drawn at all
    /// (`is_terminal_focused && !accessibility_enabled`).
    pub is_terminal_focused: bool,
    /// Whether accessibility mode is enabled (resolved by the caller).
    /// When true the cursor is not drawn at all.
    pub accessibility_enabled: bool,
    /// Whether the voice subsystem currently reports that it is
    /// recording. When false the cursor falls back to plain
    /// inverse.
    pub is_voice_recording: bool,
    /// Whether the user's reduced-motion preference is on. When
    /// true the waveform branch is suppressed and the cursor falls
    /// back to plain inverse even during recording.
    pub reduced_motion: bool,
    /// The previously-smoothed level (a single f64 slot the caller
    /// owns across frames). The reducer returns an
    /// updated value for the caller to store.
    pub previous_smoothed: f64,
    /// The most recent raw audio level from the voice subsystem
    /// (the last element of the level buffer, falling back to 0);
    /// the caller pre-resolves that here.
    pub raw_level: f64,
    /// Current animation frame time in milliseconds. Drives the hue
    /// rotation — `anim_time_ms / 1000 * 90`, wrapped into `[0, 360)`.
    pub anim_time_ms: f64,
}

/// What [`compute_waveform_cursor`] returned.
///
/// The renderer looks at this and produces its chosen ANSI /
/// ratatui / plain output. None of the styling is done here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VoiceCursor {
    /// Cursor is disabled (terminal not focused, or accessibility
    /// mode on). The rendered text should pass through unchanged.
    Hidden,
    /// Cursor is the plain inverse style. Renderer
    /// should apply inverse to whatever character is at the cursor.
    PlainInverse,
    /// Cursor is a voice waveform bar at the given glyph and RGB
    /// colour. `smoothed` is the updated EMA slot value the caller
    /// should persist for the next frame.
    Waveform {
        /// The bar glyph picked from [`BARS`]. Always one of the 9
        /// characters in [`BARS`].
        glyph: char,
        /// RGB colour the renderer should apply to `glyph`. Grey
        /// (`(128, 128, 128)`) when the raw level is below
        /// [`SILENCE_THRESHOLD`]; hue-driven otherwise.
        rgb: (u8, u8, u8),
        /// Updated EMA slot — the caller stores this for the next
        /// frame.
        smoothed: f64,
        /// Whether the silence branch was taken. Exposed so tests
        /// and renderers can introspect the decision.
        is_silent: bool,
        /// The picked bar index into [`BARS`]. Exposed so tests can
        /// pin the clamp into `1..=BARS_LEN - 1`.
        bar_index: usize,
    },
}

/// Compute the voice cursor for one animation frame.
///
/// Given the caller-resolved input, return a [`VoiceCursor`] that
/// describes what glyph and colour the renderer should draw at the
/// cursor position:
///
/// 1. If the cursor cannot be shown at all (terminal unfocused or
///    accessibility mode on), return `Hidden`.
/// 2. Else if not voice-recording, or reduced motion is on, return
///    `PlainInverse`.
/// 3. Else compute the EMA-smoothed level, pick a bar index and glyph
///    from [`BARS`], decide silence via [`SILENCE_THRESHOLD`], and
///    pick an RGB colour (grey when silent, hue-driven otherwise).
pub fn compute_waveform_cursor(input: VoiceCursorInput) -> VoiceCursor {
    let can_show_cursor = input.is_terminal_focused && !input.accessibility_enabled;
    if !can_show_cursor {
        return VoiceCursor::Hidden;
    }
    if !input.is_voice_recording || input.reduced_motion {
        return VoiceCursor::PlainInverse;
    }

    // Clamp the boosted raw level at 1.
    let target = (input.raw_level * LEVEL_BOOST).min(1.0);
    // EMA: smoothed[0] * SMOOTH + target * (1 - SMOOTH)
    let smoothed = input.previous_smoothed * SMOOTH + target * (1.0 - SMOOTH);
    let display_level = smoothed;

    // Round `display_level * (BARS_LEN - 1)` and clamp it into
    // [1, BARS_LEN - 1]. Ties on .5 round upward (round half away
    // from zero for positive values), not to even.
    let raw_idx = round_half_up(display_level * (BARS_LEN as f64 - 1.0));
    let clamped_upper = raw_idx.min(BARS_LEN as i64 - 1);
    let bar_index = clamped_upper.max(1) as usize;

    // Pick glyph from BARS.
    let glyph = BARS.chars().nth(bar_index).expect("bar_index in range");

    let is_silent = input.raw_level < SILENCE_THRESHOLD;
    let rgb = if is_silent {
        (128u8, 128u8, 128u8)
    } else {
        let hue = (input.anim_time_ms / 1000.0 * 90.0).rem_euclid(360.0);
        hue_to_rgb(hue)
    };

    VoiceCursor::Waveform {
        glyph,
        rgb,
        smoothed,
        is_silent,
        bar_index,
    }
}

/// HSL hue (0-360) to RGB, with a fixed saturation of 0.7
/// and lightness of 0.6.
pub fn hue_to_rgb(hue: f64) -> (u8, u8, u8) {
    // Normalize the hue into [0, 360) using rem_euclid so negative
    // hues wrap correctly.
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
    (
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    )
}

/// Round half away from zero, so a positive tie rounds upward.
/// `f64::round` already does that; the helper spells it out so the
/// intent is obvious.
fn round_half_up(x: f64) -> i64 {
    x.round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------- constants pinning --------------------------------------------

    #[test]
    fn bars_ramp_has_nine_glyphs_starting_with_space() {
        assert_eq!(BARS.chars().count(), BARS_LEN);
        assert_eq!(BARS.chars().next(), Some(' '));
        // Last is the full block.
        assert_eq!(BARS.chars().last(), Some('\u{2588}'));
    }

    #[test]
    fn smoothing_factors_are_pinned() {
        assert!((SMOOTH - 0.7).abs() < 1e-12);
        assert!((LEVEL_BOOST - 1.8).abs() < 1e-12);
        assert!((SILENCE_THRESHOLD - 0.15).abs() < 1e-12);
        assert_eq!(CURSOR_WAVEFORM_WIDTH, 1);
    }

    // ------- cursor-visibility short-circuit ----------------------------------

    fn base_input() -> VoiceCursorInput {
        VoiceCursorInput {
            is_terminal_focused: true,
            accessibility_enabled: false,
            is_voice_recording: false,
            reduced_motion: false,
            previous_smoothed: 0.0,
            raw_level: 0.0,
            anim_time_ms: 0.0,
        }
    }

    #[test]
    fn terminal_unfocused_hides_cursor() {
        let mut p = base_input();
        p.is_terminal_focused = false;
        assert_eq!(compute_waveform_cursor(p), VoiceCursor::Hidden);
    }

    #[test]
    fn accessibility_enabled_hides_cursor() {
        let mut p = base_input();
        p.accessibility_enabled = true;
        assert_eq!(compute_waveform_cursor(p), VoiceCursor::Hidden);
    }

    #[test]
    fn both_terminal_and_accessibility_still_hidden() {
        let mut p = base_input();
        p.is_terminal_focused = false;
        p.accessibility_enabled = true;
        assert_eq!(compute_waveform_cursor(p), VoiceCursor::Hidden);
    }

    // ------- plain-inverse fallthrough ------------------------------------

    #[test]
    fn not_recording_yields_plain_inverse() {
        let p = base_input();
        assert_eq!(compute_waveform_cursor(p), VoiceCursor::PlainInverse);
    }

    #[test]
    fn recording_with_reduced_motion_still_plain_inverse() {
        let mut p = base_input();
        p.is_voice_recording = true;
        p.reduced_motion = true;
        assert_eq!(compute_waveform_cursor(p), VoiceCursor::PlainInverse);
    }

    // ------- waveform branch ----------------------------------------------

    #[test]
    fn recording_silent_uses_grey_and_bar_index_one() {
        let mut p = base_input();
        p.is_voice_recording = true;
        p.previous_smoothed = 0.0;
        p.raw_level = 0.0;
        p.anim_time_ms = 0.0;
        match compute_waveform_cursor(p) {
            VoiceCursor::Waveform {
                glyph,
                rgb,
                smoothed,
                is_silent,
                bar_index,
            } => {
                // target = 0 -> smoothed = 0 -> display_level = 0 ->
                // round(0) = 0 -> max(1, min(0, 8)) = 1
                assert_eq!(bar_index, 1);
                assert_eq!(glyph, '\u{2581}');
                assert!(is_silent);
                assert_eq!(rgb, (128, 128, 128));
                assert!((smoothed - 0.0).abs() < 1e-12);
            }
            other => panic!("expected Waveform, got {:?}", other),
        }
    }

    #[test]
    fn recording_loud_picks_max_bar() {
        let mut p = base_input();
        p.is_voice_recording = true;
        // Simulate many frames at max level — smoothed converges to 1.
        p.previous_smoothed = 1.0;
        p.raw_level = 1.0;
        p.anim_time_ms = 500.0;
        match compute_waveform_cursor(p) {
            VoiceCursor::Waveform {
                bar_index,
                glyph,
                is_silent,
                ..
            } => {
                assert_eq!(bar_index, BARS_LEN - 1);
                assert_eq!(glyph, '\u{2588}');
                assert!(!is_silent);
            }
            other => panic!("expected Waveform, got {:?}", other),
        }
    }

    #[test]
    fn recording_ema_smoothing_is_correct() {
        let mut p = base_input();
        p.is_voice_recording = true;
        p.previous_smoothed = 0.5;
        p.raw_level = 0.2; // below silence threshold
        p.anim_time_ms = 0.0;
        match compute_waveform_cursor(p) {
            VoiceCursor::Waveform { smoothed, .. } => {
                // target = min(0.2 * 1.8, 1) = 0.36
                // smoothed = 0.5 * 0.7 + 0.36 * 0.3 = 0.35 + 0.108 = 0.458
                let expected = 0.5 * 0.7 + 0.36 * 0.3;
                assert!((smoothed - expected).abs() < 1e-12);
            }
            other => panic!("expected Waveform, got {:?}", other),
        }
    }

    #[test]
    fn raw_level_above_boost_ceiling_clamps_target_to_one() {
        let mut p = base_input();
        p.is_voice_recording = true;
        p.previous_smoothed = 0.0;
        // raw * LEVEL_BOOST = 2 * 1.8 = 3.6 -> clamps to 1
        p.raw_level = 2.0;
        p.anim_time_ms = 0.0;
        match compute_waveform_cursor(p) {
            VoiceCursor::Waveform {
                smoothed,
                is_silent,
                ..
            } => {
                // smoothed = 0 * 0.7 + 1 * 0.3 = 0.3
                assert!((smoothed - 0.3).abs() < 1e-12);
                // raw_level=2 > SILENCE_THRESHOLD so not silent
                assert!(!is_silent);
            }
            other => panic!("expected Waveform, got {:?}", other),
        }
    }

    #[test]
    fn bar_index_clamps_upward_minimum_of_one() {
        // Even a tiny smoothed value still yields bar_index = 1, not 0.
        let mut p = base_input();
        p.is_voice_recording = true;
        p.previous_smoothed = 0.0001;
        p.raw_level = 0.0;
        p.anim_time_ms = 0.0;
        match compute_waveform_cursor(p) {
            VoiceCursor::Waveform { bar_index, .. } => {
                assert_eq!(bar_index, 1);
            }
            other => panic!("expected Waveform, got {:?}", other),
        }
    }

    #[test]
    fn bar_index_clamps_upward_when_over_one() {
        // smoothed stuck at >1 (shouldn't happen given clamp, but
        // just in case the EMA overshoots) — still clamps to max.
        let mut p = base_input();
        p.is_voice_recording = true;
        p.previous_smoothed = 1.5;
        p.raw_level = 0.2;
        p.anim_time_ms = 0.0;
        match compute_waveform_cursor(p) {
            VoiceCursor::Waveform { bar_index, .. } => {
                assert!(bar_index < BARS_LEN);
                assert!(bar_index >= 1);
            }
            other => panic!("expected Waveform, got {:?}", other),
        }
    }

    #[test]
    fn silence_threshold_boundary_raw_exactly_equal_is_not_silent() {
        // The silence check is `raw < SILENCE_THRESHOLD` — 0.15 exactly
        // is NOT silent.
        let mut p = base_input();
        p.is_voice_recording = true;
        p.raw_level = 0.15;
        p.anim_time_ms = 0.0;
        match compute_waveform_cursor(p) {
            VoiceCursor::Waveform { is_silent, rgb, .. } => {
                assert!(!is_silent);
                assert_ne!(rgb, (128, 128, 128));
            }
            other => panic!("expected Waveform, got {:?}", other),
        }
    }

    #[test]
    fn silence_threshold_just_below_is_silent() {
        let mut p = base_input();
        p.is_voice_recording = true;
        p.raw_level = 0.14999;
        match compute_waveform_cursor(p) {
            VoiceCursor::Waveform { is_silent, rgb, .. } => {
                assert!(is_silent);
                assert_eq!(rgb, (128, 128, 128));
            }
            other => panic!("expected Waveform, got {:?}", other),
        }
    }

    #[test]
    fn hue_rotation_at_anim_time_1000_is_90() {
        // hue = 1000 / 1000 * 90 % 360 = 90
        let mut p = base_input();
        p.is_voice_recording = true;
        p.raw_level = 0.5; // not silent
        p.anim_time_ms = 1000.0;
        match compute_waveform_cursor(p) {
            VoiceCursor::Waveform { rgb, .. } => {
                // 90 degrees — yellow-green range. Expect g > r.
                let (r, g, _b) = rgb;
                assert!(g > r, "expected g>r at hue=90, got {:?}", rgb);
            }
            other => panic!("expected Waveform, got {:?}", other),
        }
    }

    #[test]
    fn hue_rotation_wraps_past_360() {
        // hue = 5000 / 1000 * 90 % 360 = 450 % 360 = 90
        let mut p = base_input();
        p.is_voice_recording = true;
        p.raw_level = 0.5;
        p.anim_time_ms = 5000.0;
        let long = compute_waveform_cursor(p);
        p.anim_time_ms = 1000.0;
        let short = compute_waveform_cursor(p);
        assert_eq!(long, short);
    }

    // ------- hue_to_rgb pinning ---------------------------------------------

    #[test]
    fn hue_to_rgb_zero_is_red_dominant() {
        let (r, g, b) = hue_to_rgb(0.0);
        assert!(
            r > g && r > b,
            "hue=0 should be red-dominant: {:?}",
            (r, g, b)
        );
    }

    #[test]
    fn hue_to_rgb_120_is_green_dominant() {
        let (r, g, b) = hue_to_rgb(120.0);
        assert!(
            g > r && g > b,
            "hue=120 should be green-dominant: {:?}",
            (r, g, b)
        );
    }

    #[test]
    fn hue_to_rgb_240_is_blue_dominant() {
        let (r, g, b) = hue_to_rgb(240.0);
        assert!(
            b > r && b > g,
            "hue=240 should be blue-dominant: {:?}",
            (r, g, b)
        );
    }

    #[test]
    fn hue_to_rgb_wraps_past_360() {
        assert_eq!(hue_to_rgb(0.0), hue_to_rgb(360.0));
        assert_eq!(hue_to_rgb(0.0), hue_to_rgb(720.0));
    }

    #[test]
    fn hue_to_rgb_handles_negative_hue() {
        // Negative hues wrap into [0, 360), so -30 -> 330.
        assert_eq!(hue_to_rgb(-30.0), hue_to_rgb(330.0));
    }

    #[test]
    fn hue_to_rgb_produces_values_in_byte_range() {
        for h in (0..360).step_by(5) {
            let (r, g, b) = hue_to_rgb(h as f64);
            // Every channel is a valid u8; nothing to assert except
            // the call didn't panic. (u8 already bounds-checks.)
            let _ = (r, g, b);
        }
    }

    #[test]
    fn hue_to_rgb_at_red_returns_expected_rgb() {
        // With s=0.7 l=0.6: c = (1 - |2*0.6 - 1|) * 0.7 = 0.8 * 0.7 = 0.56
        // x = 0.56 * (1 - |((0/60) % 2) - 1|) = 0.56 * 0 = 0
        // m = 0.6 - 0.28 = 0.32
        // r = 0.56 + 0.32 = 0.88 -> round(0.88*255) = 224
        // g = 0 + 0.32 = 0.32   -> round(0.32*255) = 82
        // b = 0 + 0.32 = 0.32   -> round(0.32*255) = 82
        let (r, g, b) = hue_to_rgb(0.0);
        assert_eq!((r, g, b), (224, 82, 82));
    }
}
