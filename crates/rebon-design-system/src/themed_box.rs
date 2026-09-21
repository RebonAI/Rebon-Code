use crate::color::is_raw_color_value;
use crate::theme::{get_theme, ThemeName};

/// Border glyph set a themed box can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BorderStyle {
    /// Single-line borders.
    Single,
    /// Double-line borders.
    Double,
    /// Rounded corners.
    Round,
    /// Heavy lines.
    Bold,
    /// ASCII-only borders, for terminals without box-drawing glyphs.
    Classic,
}

/// Compile-time pin: the border-style set is exactly these five variants.
/// Adding or removing one breaks this array's length.
const _: [BorderStyle; 5] = [
    BorderStyle::Single,
    BorderStyle::Double,
    BorderStyle::Round,
    BorderStyle::Bold,
    BorderStyle::Classic,
];

/// Resolved border and background colors for a themed box. Every field is
/// `None` when the caller supplied no color or a theme key that does not
/// exist.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThemedBoxStyle {
    /// Global border color. The four per-side fields take precedence
    /// wherever they are set.
    pub border_color: Option<String>,
    /// Resolved color of the top border.
    pub border_top_color: Option<String>,
    /// Resolved color of the bottom border.
    pub border_bottom_color: Option<String>,
    /// Resolved color of the left border.
    pub border_left_color: Option<String>,
    /// Resolved color of the right border.
    pub border_right_color: Option<String>,
    /// Resolved background color.
    pub background_color: Option<String>,
}

/// Resolve a single color. A raw color literal passes through untouched;
/// anything else is looked up as a theme key. `None` — either no color
/// given or an unknown key — means "not set".
fn resolve_one(color: Option<&str>, theme_name: ThemeName) -> Option<String> {
    let c = color?;
    if is_raw_color_value(c).is_some() {
        return Some(c.to_string());
    }
    let theme = get_theme(theme_name);
    theme.lookup(c).map(str::to_string)
}

/// Resolve the six colors of a themed box independently against one theme.
///
/// Each argument is either a raw color literal or a theme key and resolves
/// on its own, so a box can combine per-side overrides with a themed
/// background without any of them affecting the others.
#[allow(clippy::too_many_arguments)]
pub fn themed_box_style(
    border_color: Option<&str>,
    border_top_color: Option<&str>,
    border_bottom_color: Option<&str>,
    border_left_color: Option<&str>,
    border_right_color: Option<&str>,
    background_color: Option<&str>,
    theme_name: ThemeName,
) -> ThemedBoxStyle {
    ThemedBoxStyle {
        border_color: resolve_one(border_color, theme_name),
        border_top_color: resolve_one(border_top_color, theme_name),
        border_bottom_color: resolve_one(border_bottom_color, theme_name),
        border_left_color: resolve_one(border_left_color, theme_name),
        border_right_color: resolve_one(border_right_color, theme_name),
        background_color: resolve_one(background_color, theme_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dark() -> ThemeName {
        ThemeName::Dark
    }

    #[test]
    fn all_none_yields_default() {
        let s = themed_box_style(None, None, None, None, None, None, dark());
        assert_eq!(s, ThemedBoxStyle::default());
    }

    #[test]
    fn border_color_theme_key_resolved() {
        let s = themed_box_style(Some("rebon"), None, None, None, None, None, dark());
        assert_eq!(s.border_color.as_deref(), Some("rgb(138,181,227)"));
    }

    #[test]
    fn border_color_raw_value_passes_through() {
        let s = themed_box_style(Some("#abcdef"), None, None, None, None, None, dark());
        assert_eq!(s.border_color.as_deref(), Some("#abcdef"));
    }

    #[test]
    fn each_side_resolves_independently() {
        let s = themed_box_style(
            Some("rebon"),
            Some("error"),
            Some("warning"),
            Some("success"),
            Some("permission"),
            None,
            dark(),
        );
        assert_eq!(s.border_color.as_deref(), Some("rgb(138,181,227)"));
        assert_eq!(s.border_top_color.as_deref(), Some("rgb(255,107,128)"));
        assert_eq!(s.border_bottom_color.as_deref(), Some("rgb(255,193,7)"));
        assert_eq!(s.border_left_color.as_deref(), Some("rgb(78,186,101)"));
        assert_eq!(s.border_right_color.as_deref(), Some("rgb(177,185,249)"));
    }

    #[test]
    fn background_color_resolved() {
        let s = themed_box_style(None, None, None, None, None, Some("background"), dark());
        assert_eq!(s.background_color.as_deref(), Some("rgb(0,204,204)"));
    }

    #[test]
    fn unknown_theme_key_yields_none() {
        let s = themed_box_style(Some("not_a_key"), None, None, None, None, None, dark());
        assert_eq!(s.border_color, None);
    }

    #[test]
    fn ansi256_raw_value_passes_through() {
        let s = themed_box_style(Some("ansi256(123)"), None, None, None, None, None, dark());
        assert_eq!(s.border_color.as_deref(), Some("ansi256(123)"));
    }

    #[test]
    fn light_theme_resolves_against_light_palette() {
        let s = themed_box_style(
            Some("rebon"),
            None,
            None,
            None,
            None,
            None,
            ThemeName::Light,
        );
        assert_eq!(s.border_color.as_deref(), Some("rgb(70,117,164)"));
        // light resolves the slate accent's light variant, unlike dark
    }

    #[test]
    fn dark_ansi_theme_uses_ansi_literals() {
        let s = themed_box_style(
            Some("rebon"),
            None,
            None,
            None,
            None,
            None,
            ThemeName::DarkAnsi,
        );
        assert_eq!(s.border_color.as_deref(), Some("ansi:blue"));
    }
}
