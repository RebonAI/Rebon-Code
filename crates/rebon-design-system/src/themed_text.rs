use crate::color::is_raw_color_value;
use crate::theme::{get_theme, ThemeName};

/// Resolved color and attribute set for one run of themed text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemedTextStyle {
    /// Foreground color, already resolved to an RGB or ANSI literal. `None`
    /// leaves the renderer's default in place.
    pub color: Option<String>,
    /// Background color, already resolved. `None` means no background.
    pub background_color: Option<String>,
    /// Bold text.
    pub bold: bool,
    /// Italic text.
    pub italic: bool,
    /// Underlined text.
    pub underline: bool,
    /// Struck-through text.
    pub strikethrough: bool,
    /// Swapped foreground and background.
    pub inverse: bool,
    /// How overflowing text is handled; [`TextWrap::Wrap`] unless the
    /// caller chooses otherwise.
    pub wrap: TextWrap,
}

/// How text that does not fit its width is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextWrap {
    /// Break onto further lines (the default).
    Wrap,
    /// Cut off the tail.
    TruncateEnd,
    /// Cut out the middle.
    TruncateMiddle,
    /// Cut off the head.
    TruncateStart,
}

impl Default for TextWrap {
    fn default() -> Self {
        TextWrap::Wrap
    }
}

/// Resolve a single color to its literal form: a raw color literal passes
/// through untouched, anything else is looked up in the theme. Both `None`
/// and an unknown theme key come back as `None`.
fn resolve_one(color: Option<&str>, theme_name: ThemeName) -> Option<String> {
    let c = color?;
    if is_raw_color_value(c).is_some() {
        return Some(c.to_string());
    }
    let theme = get_theme(theme_name);
    theme.lookup(c).map(str::to_string)
}

/// Resolve the style for a run of themed text — foreground, background and
/// every attribute flag — into a [`ThemedTextStyle`].
///
/// Foreground precedence, in order:
///
/// 1. `hover_color`, whenever no explicit `color` was given.
/// 2. The theme's `inactive` color when `dim_color` is set. This sits
///    *above* step 3, so dimming overrides an explicit `color` rather than
///    the other way around.
/// 3. The explicit `color`, resolved; `None` leaves the renderer default.
#[allow(clippy::too_many_arguments)]
pub fn themed_text_style(
    color: Option<&str>,
    background_color: Option<&str>,
    hover_color: Option<&str>,
    dim_color: bool,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    inverse: bool,
    wrap: TextWrap,
    theme_name: ThemeName,
) -> ThemedTextStyle {
    let theme = get_theme(theme_name);

    let resolved_color = if color.is_none() && hover_color.is_some() {
        resolve_one(hover_color, theme_name)
    } else if dim_color {
        Some(theme.inactive.to_string())
    } else {
        resolve_one(color, theme_name)
    };

    let resolved_bg = resolve_one(background_color, theme_name);

    ThemedTextStyle {
        color: resolved_color,
        background_color: resolved_bg,
        bold,
        italic,
        underline,
        strikethrough,
        inverse,
        wrap,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dark() -> ThemeName {
        ThemeName::Dark
    }

    #[test]
    fn explicit_color_theme_key_is_resolved() {
        let s = themed_text_style(
            Some("rebon"),
            None,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
            TextWrap::Wrap,
            dark(),
        );
        assert_eq!(s.color.as_deref(), Some("rgb(138,181,227)"));
    }

    #[test]
    fn explicit_color_raw_value_passes_through() {
        let s = themed_text_style(
            Some("rgb(1,2,3)"),
            None,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
            TextWrap::Wrap,
            dark(),
        );
        assert_eq!(s.color.as_deref(), Some("rgb(1,2,3)"));
    }

    #[test]
    fn dim_color_uses_theme_inactive() {
        let s = themed_text_style(
            None,
            None,
            None,
            true,
            false,
            false,
            false,
            false,
            false,
            TextWrap::Wrap,
            dark(),
        );
        // dark theme's inactive is rgb(153,153,153)
        assert_eq!(s.color.as_deref(), Some("rgb(153,153,153)"));
    }

    #[test]
    fn dim_color_overrides_explicit_color_when_no_hover() {
        // Load-bearing: "no color and a hover color" is the FIRST arm.
        // If `color` is set, the first arm fails; the second arm
        // (`dim_color`) wins, and the explicit color is *dropped*. With
        // no hover color the choice is "inactive when dim, otherwise the
        // resolved color", so dim_color takes priority.
        let s = themed_text_style(
            Some("rebon"),
            None,
            None,
            true, // dim_color is true
            false,
            false,
            false,
            false,
            false,
            TextWrap::Wrap,
            dark(),
        );
        // dark theme's inactive
        assert_eq!(s.color.as_deref(), Some("rgb(153,153,153)"));
    }

    #[test]
    fn explicit_color_used_when_dim_color_false() {
        let s = themed_text_style(
            Some("rebon"),
            None,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
            TextWrap::Wrap,
            dark(),
        );
        assert_eq!(s.color.as_deref(), Some("rgb(138,181,227)"));
    }

    #[test]
    fn explicit_color_overrides_hover_color() {
        let s = themed_text_style(
            Some("rebon"),
            None,
            Some("error"),
            false,
            false,
            false,
            false,
            false,
            false,
            TextWrap::Wrap,
            dark(),
        );
        assert_eq!(s.color.as_deref(), Some("rgb(138,181,227)"));
    }

    #[test]
    fn hover_color_used_when_no_explicit_color() {
        let s = themed_text_style(
            None,
            None,
            Some("rebon"),
            false,
            false,
            false,
            false,
            false,
            false,
            TextWrap::Wrap,
            dark(),
        );
        assert_eq!(s.color.as_deref(), Some("rgb(138,181,227)"));
    }

    #[test]
    fn hover_color_takes_priority_over_dim_color() {
        // "No color and a hover color" is the FIRST arm; dim_color is
        // the second arm. If hover_color is set and
        // there's no explicit color, hover_color wins regardless of
        // dim_color.
        let s = themed_text_style(
            None,
            None,
            Some("rebon"),
            true, // dim_color
            false,
            false,
            false,
            false,
            false,
            TextWrap::Wrap,
            dark(),
        );
        assert_eq!(s.color.as_deref(), Some("rgb(138,181,227)"));
    }

    #[test]
    fn no_color_no_hover_no_dim_yields_none() {
        let s = themed_text_style(
            None,
            None,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
            TextWrap::Wrap,
            dark(),
        );
        assert_eq!(s.color, None);
    }

    #[test]
    fn background_color_resolved_from_theme_key() {
        let s = themed_text_style(
            None,
            Some("background"),
            None,
            false,
            false,
            false,
            false,
            false,
            false,
            TextWrap::Wrap,
            dark(),
        );
        assert_eq!(s.background_color.as_deref(), Some("rgb(0,204,204)"));
    }

    #[test]
    fn background_color_raw_passes_through() {
        let s = themed_text_style(
            None,
            Some("rgb(99,88,77)"),
            None,
            false,
            false,
            false,
            false,
            false,
            false,
            TextWrap::Wrap,
            dark(),
        );
        assert_eq!(s.background_color.as_deref(), Some("rgb(99,88,77)"));
    }

    #[test]
    fn flags_pass_through_unchanged() {
        let s = themed_text_style(
            None,
            None,
            None,
            false,
            true, // bold
            true, // italic
            true, // underline
            true, // strikethrough
            true, // inverse
            TextWrap::TruncateEnd,
            dark(),
        );
        assert!(s.bold);
        assert!(s.italic);
        assert!(s.underline);
        assert!(s.strikethrough);
        assert!(s.inverse);
        assert_eq!(s.wrap, TextWrap::TruncateEnd);
    }

    #[test]
    fn default_wrap_is_wrap() {
        assert_eq!(TextWrap::default(), TextWrap::Wrap);
    }

    #[test]
    fn light_theme_inactive_used_for_dim() {
        let s = themed_text_style(
            None,
            None,
            None,
            true,
            false,
            false,
            false,
            false,
            false,
            TextWrap::Wrap,
            ThemeName::Light,
        );
        assert_eq!(s.color.as_deref(), Some("rgb(102,102,102)"));
    }
}
