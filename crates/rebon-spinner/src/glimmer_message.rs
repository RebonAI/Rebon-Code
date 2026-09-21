//! Top-level decision tree for glimmer message rendering.
//!
//! The branch selector chooses how a spinner message should be drawn from the
//! current state: empty messages render nothing, stalled messages fade toward
//! error red, tool-use messages flash, off-screen shimmer windows render plain
//! text, and in-window shimmer windows split the message into grapheme-aware
//! segments.
//!
//! Branches, in priority order:
//!
//! 1. **Empty message** → nothing is drawn.
//! 2. **Stalled** (`stalled_intensity > 0`) → render the whole message
//! in interpolated/error red.
//! 3. **Tool-use mode** → render the whole message in interpolated
//! flash color (or binary fallback).
//! 4. **Off-screen shimmer** (`shimmer_start >= message_width` or
//! `shimmer_end < 0`) → render the whole message in
//! the message color.
//! 5. **In-window shimmer** → render the three segments via the
//! grapheme loop in [`crate::shimmer_segments`].
//!
//! This module returns a [`GlimmerBranch`] the consumer maps to its renderer.

use crate::color::RgbColor;

/// The five branches a message can take.
#[derive(Debug, Clone, PartialEq)]
pub enum GlimmerBranch {
    /// Branch 1: empty message — render nothing.
    Empty,
    /// Branch 2: stalled — render whole message in this color.
    Stalled {
        /// The color to paint the whole message in.
        color: GlimmerColor,
    },
    /// Branch 3: tool-use mode — render whole message in this color.
    ToolUseFlash {
        /// The color to paint the whole message in.
        color: GlimmerColor,
    },
    /// Branch 4: shimmer is off-screen — render whole message in
    /// the message color.
    Plain,
    /// Branch 5: shimmer is in-window — render the three segments
    /// (call [`crate::shimmer_segments`] for the actual split).
    Shimmer {
        /// The center index of the shimmer window.
        glimmer_index: i64,
    },
}

/// The discriminated color shape used by [`GlimmerBranch::Stalled`]
/// and [`GlimmerBranch::ToolUseFlash`].
///
/// When the consumer's theme resolves the message color and the
/// shimmer color to RGB, the two are interpolated and an
/// `Interpolated` color is returned. Otherwise
/// it falls back to a binary "use error/shimmer color when intensity
/// or opacity > 0.5" rule.
#[derive(Debug, Clone, PartialEq)]
pub enum GlimmerColor {
    /// Theme resolved successfully — use this RGB color.
    Interpolated(RgbColor),
    /// Theme didn't resolve — use the error / shimmer color.
    NamedError,
    /// Theme didn't resolve — use the message color.
    NamedMessage,
    /// Theme didn't resolve — use the shimmer color.
    NamedShimmer,
}

/// Inputs to [`glimmer_message_branch`]: everything the branch choice
/// depends on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlimmerInputs {
    /// Whether the message is empty.
    pub message_empty: bool,
    /// The visual width of the message in display columns.
    pub message_width: i64,
    /// The smoothed stalled intensity in `[0, 1]`.
    pub stalled_intensity: f64,
    /// True when the spinner is in tool-use mode.
    pub is_tool_use: bool,
    /// The current shimmer flash opacity.
    pub flash_opacity: f64,
    /// The center of the shimmer window.
    pub glimmer_index: i64,
    /// Whether the theme resolved the message color to RGB.
    pub message_rgb: Option<RgbColor>,
    /// Whether the theme resolved the shimmer color to RGB.
    pub shimmer_rgb: Option<RgbColor>,
}

/// Choose the branch for the current state, in priority order.
pub fn glimmer_message_branch(input: GlimmerInputs) -> GlimmerBranch {
    if input.message_empty {
        return GlimmerBranch::Empty;
    }
    if input.stalled_intensity > 0.0 {
        return GlimmerBranch::Stalled {
            color: stalled_color(input),
        };
    }
    if input.is_tool_use {
        return GlimmerBranch::ToolUseFlash {
            color: tool_use_color(input),
        };
    }
    let shimmer_start = input.glimmer_index - 1;
    let shimmer_end = input.glimmer_index + 1;
    if shimmer_start >= input.message_width || shimmer_end < 0 {
        return GlimmerBranch::Plain;
    }
    GlimmerBranch::Shimmer {
        glimmer_index: input.glimmer_index,
    }
}

fn stalled_color(input: GlimmerInputs) -> GlimmerColor {
    if let Some(base) = input.message_rgb {
        let red = crate::glyph::ERROR_RED;
        let interpolated = crate::color::interpolate_color(base, red, input.stalled_intensity);
        return GlimmerColor::Interpolated(interpolated);
    }
    if input.stalled_intensity > 0.5 {
        GlimmerColor::NamedError
    } else {
        GlimmerColor::NamedMessage
    }
}

fn tool_use_color(input: GlimmerInputs) -> GlimmerColor {
    if let (Some(m), Some(s)) = (input.message_rgb, input.shimmer_rgb) {
        let interpolated = crate::color::interpolate_color(m, s, input.flash_opacity);
        return GlimmerColor::Interpolated(interpolated);
    }
    if input.flash_opacity > 0.5 {
        GlimmerColor::NamedShimmer
    } else {
        GlimmerColor::NamedMessage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input() -> GlimmerInputs {
        GlimmerInputs {
            message_empty: false,
            message_width: 10,
            stalled_intensity: 0.0,
            is_tool_use: false,
            flash_opacity: 0.0,
            glimmer_index: 5,
            message_rgb: Some(RgbColor::new(100, 100, 100)),
            shimmer_rgb: Some(RgbColor::new(200, 200, 200)),
        }
    }

    #[test]
    fn empty_message_returns_empty_branch() {
        let mut i = base_input();
        i.message_empty = true;
        assert_eq!(glimmer_message_branch(i), GlimmerBranch::Empty);
    }

    #[test]
    fn empty_takes_priority_over_stalled() {
        let mut i = base_input();
        i.message_empty = true;
        i.stalled_intensity = 1.0;
        assert_eq!(glimmer_message_branch(i), GlimmerBranch::Empty);
    }

    #[test]
    fn stalled_takes_priority_over_tool_use() {
        let mut i = base_input();
        i.stalled_intensity = 0.5;
        i.is_tool_use = true;
        match glimmer_message_branch(i) {
            GlimmerBranch::Stalled { .. } => {}
            other => panic!("expected stalled, got {other:?}"),
        }
    }

    #[test]
    fn stalled_with_theme_returns_interpolated() {
        let mut i = base_input();
        i.stalled_intensity = 0.5;
        match glimmer_message_branch(i) {
            GlimmerBranch::Stalled {
                color: GlimmerColor::Interpolated(_),
            } => {}
            other => panic!("expected interpolated stalled, got {other:?}"),
        }
    }

    #[test]
    fn stalled_no_theme_high_intensity_uses_error() {
        let mut i = base_input();
        i.stalled_intensity = 0.7;
        i.message_rgb = None;
        assert_eq!(
            glimmer_message_branch(i),
            GlimmerBranch::Stalled {
                color: GlimmerColor::NamedError
            }
        );
    }

    #[test]
    fn stalled_no_theme_low_intensity_uses_message() {
        let mut i = base_input();
        i.stalled_intensity = 0.3;
        i.message_rgb = None;
        assert_eq!(
            glimmer_message_branch(i),
            GlimmerBranch::Stalled {
                color: GlimmerColor::NamedMessage
            }
        );
    }

    #[test]
    fn tool_use_returns_flash_branch() {
        let mut i = base_input();
        i.is_tool_use = true;
        i.flash_opacity = 0.5;
        match glimmer_message_branch(i) {
            GlimmerBranch::ToolUseFlash { .. } => {}
            other => panic!("expected tool-use flash, got {other:?}"),
        }
    }

    #[test]
    fn tool_use_no_theme_high_opacity_uses_shimmer() {
        let mut i = base_input();
        i.is_tool_use = true;
        i.flash_opacity = 0.7;
        i.message_rgb = None;
        assert_eq!(
            glimmer_message_branch(i),
            GlimmerBranch::ToolUseFlash {
                color: GlimmerColor::NamedShimmer
            }
        );
    }

    #[test]
    fn tool_use_no_theme_low_opacity_uses_message() {
        let mut i = base_input();
        i.is_tool_use = true;
        i.flash_opacity = 0.3;
        i.shimmer_rgb = None;
        assert_eq!(
            glimmer_message_branch(i),
            GlimmerBranch::ToolUseFlash {
                color: GlimmerColor::NamedMessage
            }
        );
    }

    #[test]
    fn off_screen_shimmer_left_returns_plain() {
        let mut i = base_input();
        i.glimmer_index = -100;
        assert_eq!(glimmer_message_branch(i), GlimmerBranch::Plain);
    }

    #[test]
    fn off_screen_shimmer_right_returns_plain() {
        let mut i = base_input();
        i.glimmer_index = 100;
        assert_eq!(glimmer_message_branch(i), GlimmerBranch::Plain);
    }

    #[test]
    fn in_window_returns_shimmer_branch() {
        let i = base_input();
        match glimmer_message_branch(i) {
            GlimmerBranch::Shimmer { glimmer_index } => assert_eq!(glimmer_index, 5),
            other => panic!("expected shimmer, got {other:?}"),
        }
    }

    #[test]
    fn shimmer_at_zero_index_is_in_window() {
        let mut i = base_input();
        i.glimmer_index = 0;
        match glimmer_message_branch(i) {
            GlimmerBranch::Shimmer { .. } => {}
            other => panic!("expected shimmer, got {other:?}"),
        }
    }

    #[test]
    fn shimmer_just_past_right_is_plain() {
        let mut i = base_input();
        // shimmer_start = glimmer_index - 1 must be >= 10 (width)
        i.glimmer_index = 11;
        assert_eq!(glimmer_message_branch(i), GlimmerBranch::Plain);
    }
}
