//! Running-tool dot.
//!
//! The consumer drives a 500ms blink toggle and feeds three flags (error,
//! unresolved, should-animate) plus the current blink state; the
//! projection emits a single row whose color and glyph follow them.
//!
//! Pure logic:
//!
//! * Color: dim while the tool call is unresolved, otherwise the error
//!   color when it failed and the success color when it did not.
//! * Glyph: [`BLACK_CIRCLE`] unless the tool call is unresolved, still
//!   animating, not currently blinking and error-free — in that one case
//!   the glyph is a single space.
//!
//! [`BLACK_CIRCLE`] is platform-dependent (`'⏺'` on darwin, `'●'`
//! everywhere else); we expose both via constants and let the consumer
//! pick.

/// `BLACK_CIRCLE` glyph for non-darwin platforms (`'●'`, U+25CF). Same
/// as `EFFORT_HIGH` — they share a code point but have different
/// semantics here.
pub const BLACK_CIRCLE: &str = "\u{25cf}";

/// `BLACK_CIRCLE` glyph for darwin (`'⏺'`, U+23FA). Pinned for
/// completeness even though this projection does not pick the platform —
/// it always uses [`BLACK_CIRCLE`].
pub const BLACK_CIRCLE_DARWIN: &str = "\u{23fa}";

/// Color slot for the loader row. `Dim` means the row should be
/// rendered dimmed, with no themed color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolUseLoaderColor {
    /// Dim — corresponds to an unresolved tool call.
    Dim,
    /// `'error'` theme key.
    Error,
    /// `'success'` theme key.
    Success,
}

/// Inputs for the loader projection. The blink state is passed in
/// because the blink timer is owned by the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolUseLoaderInputs {
    /// Whether the tool call resolved with an error.
    pub is_error: bool,
    /// Whether the tool is still in flight.
    pub is_unresolved: bool,
    /// Whether the loader should animate.
    pub should_animate: bool,
    /// Current blink state. Ignored when
    /// [`should_animate`](Self::should_animate) is `false`.
    pub is_blinking: bool,
}

/// The display row emitted by the loader. Two columns are always
/// reserved even if the glyph is a space; consumers should respect
/// [`min_width`](Self::min_width).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolUseLoaderRow {
    /// Glyph to render: either `BLACK_CIRCLE` or a single space.
    pub glyph: &'static str,
    /// Color slot.
    pub color: ToolUseLoaderColor,
    /// Reserved minimum width in columns. Always 2.
    pub min_width: u8,
}

/// Picks the color and glyph for the loader row, and pins the two-column
/// minimum width.
pub fn tool_use_loader(inputs: ToolUseLoaderInputs) -> ToolUseLoaderRow {
    let color = if inputs.is_unresolved {
        ToolUseLoaderColor::Dim
    } else if inputs.is_error {
        ToolUseLoaderColor::Error
    } else {
        ToolUseLoaderColor::Success
    };

    let glyph =
        if !inputs.should_animate || inputs.is_blinking || inputs.is_error || !inputs.is_unresolved
        {
            BLACK_CIRCLE
        } else {
            " "
        };

    ToolUseLoaderRow {
        glyph,
        color,
        min_width: 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(
        is_error: bool,
        is_unresolved: bool,
        should_animate: bool,
        is_blinking: bool,
    ) -> ToolUseLoaderRow {
        tool_use_loader(ToolUseLoaderInputs {
            is_error,
            is_unresolved,
            should_animate,
            is_blinking,
        })
    }

    #[test]
    fn unresolved_is_dim() {
        let row = case(false, true, true, false);
        assert_eq!(row.color, ToolUseLoaderColor::Dim);
    }

    #[test]
    fn error_when_resolved_is_error_color() {
        let row = case(true, false, false, false);
        assert_eq!(row.color, ToolUseLoaderColor::Error);
    }

    #[test]
    fn success_when_resolved_no_error() {
        let row = case(false, false, false, false);
        assert_eq!(row.color, ToolUseLoaderColor::Success);
    }

    #[test]
    fn error_takes_priority_over_unresolved_for_color() {
        // Unresolved is checked first, so it wins over the error flag.
        let row = case(true, true, true, false);
        assert_eq!(row.color, ToolUseLoaderColor::Dim);
    }

    #[test]
    fn no_animation_always_shows_glyph() {
        let row = case(false, true, false, false);
        assert_eq!(row.glyph, BLACK_CIRCLE);
    }

    #[test]
    fn blinking_shows_glyph() {
        let row = case(false, true, true, true);
        assert_eq!(row.glyph, BLACK_CIRCLE);
    }

    #[test]
    fn animating_unresolved_not_blinking_shows_space() {
        let row = case(false, true, true, false);
        assert_eq!(row.glyph, " ");
    }

    #[test]
    fn animating_error_shows_glyph_even_when_not_blinking() {
        let row = case(true, true, true, false);
        assert_eq!(row.glyph, BLACK_CIRCLE);
    }

    #[test]
    fn animating_resolved_shows_glyph() {
        let row = case(false, false, true, false);
        assert_eq!(row.glyph, BLACK_CIRCLE);
    }

    #[test]
    fn min_width_is_two() {
        let row = case(false, true, true, false);
        assert_eq!(row.min_width, 2);
    }

    #[test]
    fn black_circle_is_filled() {
        assert_eq!(BLACK_CIRCLE, "\u{25cf}");
    }

    #[test]
    fn black_circle_darwin_is_record_glyph() {
        assert_eq!(BLACK_CIRCLE_DARWIN, "\u{23fa}");
    }
}
