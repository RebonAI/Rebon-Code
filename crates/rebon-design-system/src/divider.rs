/// Default divider glyph: `─` (U+2500).
pub const DIVIDER_GLYPH: &str = "─";

/// Resolved layout for one divider line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DividerStyle {
    /// Glyphs to the left of the title, or the whole line when there is no
    /// title.
    pub left: String,
    /// The title padded with one space on each side (`" {title} "`), or
    /// empty when no title was given.
    pub title: String,
    /// Glyphs to the right of the title.
    pub right: String,
    /// True when the renderer should fall back to its dim color, which
    /// happens exactly when the caller supplied no `color`.
    pub dim: bool,
    /// Theme key for the divider color, copied through from the caller.
    /// This being `None` is what sets `dim`.
    pub color_key: Option<String>,
    /// Total display width of the divider. Equals the left width plus the
    /// title width plus the right width, so callers that pre-measure their
    /// own strings can compare against it.
    pub effective_width: u32,
}

/// Lay out one divider line.
///
/// * `width` — explicit width in columns; `None` defers to `terminal_width`.
/// * `terminal_width` — the terminal's current column count.
/// * `color` — theme key for the divider color; `None` makes the result dim.
/// * `char` — glyph to repeat; defaults to [`DIVIDER_GLYPH`].
/// * `padding` — columns to reserve, subtracted before the line is filled.
/// * `title` — optional centered title. ANSI codes are not stripped here.
/// * `title_display_width` — the title's width in columns, as measured by
///   the caller. It is a parameter rather than something this function
///   computes so consumers can plug in a measurer that understands ANSI
///   sequences and East-Asian wide characters.
///
/// A title occupies its own width plus two spaces. What remains is split
/// floor-left and remainder-right, so an odd remainder leaves the right
/// side one column longer than the left.
///
/// An empty or missing title makes the whole effective width one run of
/// `left` glyphs, with `title` and `right` empty.
pub fn divider_style(
    width: Option<u32>,
    terminal_width: u32,
    color: Option<&str>,
    char: Option<&str>,
    padding: u32,
    title: Option<&str>,
    title_display_width: u32,
) -> DividerStyle {
    let glyph = char.unwrap_or(DIVIDER_GLYPH);

    // `effective_width` = `base` minus `padding`, clamped at 0 by
    // `saturating_sub`, where `base` is `width` or else `terminal_width`.
    let base = width.unwrap_or(terminal_width);
    let effective_width = base.saturating_sub(padding);

    if let Some(title_str) = title.filter(|s| !s.is_empty()) {
        // title_width = string_width(title) + 2
        let title_total = title_display_width + 2;
        // side_width = max(0, effective_width - title_width)
        let side_width = effective_width.saturating_sub(title_total);
        let left_width = side_width / 2; // floor
        let right_width = side_width - left_width;

        let left = glyph.repeat(left_width as usize);
        let right = glyph.repeat(right_width as usize);
        return DividerStyle {
            left,
            title: format!(" {title_str} "),
            right,
            dim: color.is_none(),
            color_key: color.map(str::to_string),
            effective_width,
        };
    }

    // No title — entire line is left.
    let left = glyph.repeat(effective_width as usize);
    DividerStyle {
        left,
        title: String::new(),
        right: String::new(),
        dim: color.is_none(),
        color_key: color.map(str::to_string),
        effective_width,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divider_glyph_pinned() {
        assert_eq!(DIVIDER_GLYPH, "─");
        assert_eq!(DIVIDER_GLYPH, "\u{2500}");
    }

    #[test]
    fn no_title_uses_terminal_width() {
        let s = divider_style(None, 10, None, None, 0, None, 0);
        assert_eq!(s.left, "──────────");
        assert_eq!(s.title, "");
        assert_eq!(s.right, "");
        assert_eq!(s.effective_width, 10);
    }

    #[test]
    fn explicit_width_overrides_terminal_width() {
        let s = divider_style(Some(5), 100, None, None, 0, None, 0);
        assert_eq!(s.left, "─────");
        assert_eq!(s.effective_width, 5);
    }

    #[test]
    fn padding_is_subtracted() {
        let s = divider_style(None, 10, None, None, 4, None, 0);
        assert_eq!(s.effective_width, 6);
        assert_eq!(s.left, "──────");
    }

    #[test]
    fn padding_clamped_to_zero() {
        let s = divider_style(None, 4, None, None, 10, None, 0);
        assert_eq!(s.effective_width, 0);
        assert_eq!(s.left, "");
    }

    #[test]
    fn no_color_sets_dim_true() {
        let s = divider_style(Some(10), 0, None, None, 0, None, 0);
        assert!(s.dim);
        assert_eq!(s.color_key, None);
    }

    #[test]
    fn explicit_color_clears_dim() {
        let s = divider_style(Some(10), 0, Some("permission"), None, 0, None, 0);
        assert!(!s.dim);
        assert_eq!(s.color_key.as_deref(), Some("permission"));
    }

    #[test]
    fn custom_char_is_used() {
        let s = divider_style(Some(5), 0, None, Some("="), 0, None, 0);
        assert_eq!(s.left, "=====");
    }

    // ────────────────────────────────────────────────────────────────
    // Title math
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn title_centered_even_split() {
        // effective_width=20, title_width=5+2=7, side_width=13,
        // left_width=floor(13/2)=6, right_width=7
        let s = divider_style(Some(20), 0, None, None, 0, Some("hello"), 5);
        assert_eq!(s.title, " hello ");
        assert_eq!(s.left.chars().count(), 6);
        assert_eq!(s.right.chars().count(), 7);
    }

    #[test]
    fn title_centered_exact_split() {
        // effective_width=10, title_width=2+2=4, side_width=6,
        // left_width=3, right_width=3
        let s = divider_style(Some(10), 0, None, None, 0, Some("hi"), 2);
        assert_eq!(s.left.chars().count(), 3);
        assert_eq!(s.right.chars().count(), 3);
    }

    #[test]
    fn title_overflows_effective_width_collapses_sides_to_zero() {
        let s = divider_style(Some(3), 0, None, None, 0, Some("longtitle"), 9);
        // effective_width=3, title_width=11, side_width = max(0, 3-11) = 0
        assert_eq!(s.left, "");
        assert_eq!(s.right, "");
    }

    #[test]
    fn empty_title_falls_through_to_no_title() {
        // Empty title is treated as no title.
        let s = divider_style(Some(5), 0, None, None, 0, Some(""), 0);
        assert_eq!(s.left, "─────");
        assert_eq!(s.title, "");
    }

    #[test]
    fn title_with_odd_remainder_floor_left_round_up_right() {
        // side_width=5, left=floor(5/2)=2, right=5-2=3
        let s = divider_style(Some(15), 0, None, None, 0, Some("hello"), 5);
        // effective=15, title_width=7, side=8, left=4, right=4
        assert_eq!(s.left.chars().count(), 4);
        assert_eq!(s.right.chars().count(), 4);
    }

    #[test]
    fn padding_and_title_combined() {
        // width=20, padding=4 -> effective=16
        // title_width = 3+2 = 5, side=11, left=5, right=6
        let s = divider_style(Some(20), 0, None, None, 4, Some("foo"), 3);
        assert_eq!(s.effective_width, 16);
        assert_eq!(s.left.chars().count(), 5);
        assert_eq!(s.right.chars().count(), 6);
    }
}
