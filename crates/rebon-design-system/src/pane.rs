use crate::theme::ThemeName;

/// Resolved geometry for a pane: padding, divider visibility and flex
/// participation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneStyle {
    /// Horizontal padding, in cells.
    pub padding_x: u8,
    /// Top padding, in cells. `1` outside a modal, `0` inside one.
    pub padding_top: u8,
    /// True when the renderer should draw a divider line above the
    /// content. Never true inside a modal.
    pub show_divider: bool,
    /// Theme key for the divider color, copied through from the caller;
    /// `None` when the caller passed no color.
    pub divider_color_key: Option<String>,
    /// `flex_shrink` for the inner box: `Some(0)` inside a modal, `None`
    /// (the renderer's default flex behaviour) otherwise.
    pub flex_shrink: Option<u8>,
}

/// Resolve the [`PaneStyle`] for a pane.
///
/// `inside_modal` selects between the two geometries: a nested pane drops
/// its top padding and divider and shrinks to fit, while a regular pane
/// keeps both and lets the layout size it.
///
/// `color` is the theme key for the divider color. It is copied into the
/// result even in modal mode, where the divider itself is not drawn, so a
/// consumer can still override the color at its own layer.
///
/// `_theme` is accepted but deliberately unused: palette resolution is the
/// consumer renderer's job via [`crate::theme`]. Keeping the parameter
/// makes this projection uniform with the other design-system surfaces.
pub fn pane_style(color: Option<&str>, inside_modal: bool, _theme: ThemeName) -> PaneStyle {
    if inside_modal {
        return PaneStyle {
            padding_x: 1,
            padding_top: 0,
            show_divider: false,
            divider_color_key: color.map(str::to_string),
            flex_shrink: Some(0),
        };
    }
    PaneStyle {
        padding_x: 2,
        padding_top: 1,
        show_divider: true,
        divider_color_key: color.map(str::to_string),
        flex_shrink: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modal_mode_has_padding_one_no_divider_no_top_padding() {
        let s = pane_style(Some("permission"), true, ThemeName::Dark);
        assert_eq!(s.padding_x, 1);
        assert_eq!(s.padding_top, 0);
        assert_eq!(s.show_divider, false);
        assert_eq!(s.flex_shrink, Some(0));
    }

    #[test]
    fn non_modal_has_padding_two_with_divider_and_top_padding() {
        let s = pane_style(Some("permission"), false, ThemeName::Dark);
        assert_eq!(s.padding_x, 2);
        assert_eq!(s.padding_top, 1);
        assert_eq!(s.show_divider, true);
        assert_eq!(s.flex_shrink, None);
    }

    #[test]
    fn modal_mode_drops_top_padding_completely() {
        // Invariant: the modal branch deliberately omits padding_top —
        // it is only set in the non-modal branch.
        let s = pane_style(None, true, ThemeName::Dark);
        assert_eq!(s.padding_top, 0);
    }

    #[test]
    fn color_propagates_to_divider_in_non_modal_mode() {
        let s = pane_style(Some("permission"), false, ThemeName::Dark);
        assert_eq!(s.divider_color_key.as_deref(), Some("permission"));
    }

    #[test]
    fn color_propagates_in_modal_mode_even_though_divider_hidden() {
        // The color is not actually used in the modal branch, but
        // propagating it lets the consumer override at its own layer
        // if it wants to.
        let s = pane_style(Some("permission"), true, ThemeName::Dark);
        assert_eq!(s.divider_color_key.as_deref(), Some("permission"));
    }

    #[test]
    fn no_color_yields_none() {
        let s = pane_style(None, false, ThemeName::Dark);
        assert_eq!(s.divider_color_key, None);
    }

    #[test]
    fn pane_style_table() {
        let cases = [
            (None, false, 2, 1, true, None),
            (None, true, 1, 0, false, Some(0u8)),
            (Some("autoAccept"), false, 2, 1, true, None),
            (Some("autoAccept"), true, 1, 0, false, Some(0u8)),
        ];
        for (color, inside, px, pt, divider, fs) in cases {
            let s = pane_style(color, inside, ThemeName::Dark);
            assert_eq!(s.padding_x, px);
            assert_eq!(s.padding_top, pt);
            assert_eq!(s.show_divider, divider);
            assert_eq!(s.flex_shrink, fs);
        }
    }
}
