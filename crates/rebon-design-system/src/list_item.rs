/// Pointer glyph marking the focused row (`❯`).
pub const POINTER_GLYPH: &str = "❯";
/// Scroll-down indicator (`↓`).
pub const ARROW_DOWN: &str = "↓";
/// Scroll-up indicator (`↑`).
pub const ARROW_UP: &str = "↑";
/// Checkmark for a selected item (`✔`).
pub const TICK_GLYPH: &str = "✔";

/// Inputs that decide how a list item is drawn. They travel as one struct
/// so the precedence between them is settled here instead of being
/// re-decided by every caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ListItemSelection {
    /// True when the item has keyboard focus.
    pub is_focused: bool,
    /// True when the item is selected (chosen or checked).
    pub is_selected: bool,
    /// True when the item cannot be interacted with.
    pub disabled: bool,
    /// Show the scroll-up indicator instead of the pointer. Applies only
    /// while the item is unfocused.
    pub show_scroll_up: bool,
    /// Show the scroll-down indicator instead of the pointer. Applies only
    /// while the item is unfocused.
    pub show_scroll_down: bool,
    /// True when the row's children get the standard styling.
    pub styled: bool,
}

impl ListItemSelection {
    /// Baseline selection: nothing set, children styled.
    pub fn defaults() -> Self {
        Self {
            is_focused: false,
            is_selected: false,
            disabled: false,
            show_scroll_up: false,
            show_scroll_down: false,
            styled: true,
        }
    }
}

/// Resolved projection for one list row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItemRow {
    /// Leading indicator glyph, one character wide.
    pub indicator: &'static str,
    /// True when the indicator should be drawn dim.
    pub indicator_dim: bool,
    /// Theme color key for the indicator; `None` means the renderer's
    /// default. The focused pointer uses `"suggestion"`.
    pub indicator_color: Option<&'static str>,
    /// Theme color key for the content; `None` means the renderer's
    /// default. Precedence: disabled → `"inactive"`; unstyled → `None`;
    /// selected → `"success"`; focused → `"suggestion"`; else `None`.
    pub content_color: Option<&'static str>,
    /// True when the content text should be drawn dim, which is exactly
    /// when the item is disabled.
    pub content_dim: bool,
    /// True when the trailing checkmark should be drawn.
    pub show_check: bool,
    /// Theme color for the trailing checkmark, always `"success"`
    /// whenever `show_check` is set.
    pub check_color: Option<&'static str>,
    /// Projected description line, when one was supplied.
    pub description: Option<DescriptionLine>,
    /// True when the row should declare the terminal cursor: focused, not
    /// disabled, and `declare_cursor` not explicitly turned off.
    pub declare_cursor: bool,
}

/// Projection of an item's secondary description line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptionLine {
    /// Left indent in cells (always 2).
    pub padding_left: u8,
    /// Theme color key (always `"inactive"`).
    pub color_key: &'static str,
    /// The text to draw.
    pub text: String,
}

/// Project a [`ListItemSelection`] plus an optional description into a
/// [`ListItemRow`].
///
/// `description` is the secondary text shown under the content; an empty
/// string counts as absent. `declare_cursor` lets a caller suppress the
/// cursor declaration even for a focused row.
pub fn list_item_row(
    selection: ListItemSelection,
    description: Option<&str>,
    declare_cursor: bool,
) -> ListItemRow {
    // Indicator dispatch
    let (indicator, indicator_dim, indicator_color) = if selection.disabled {
        (" ", false, None)
    } else if selection.is_focused {
        (POINTER_GLYPH, false, Some("suggestion"))
    } else if selection.show_scroll_down {
        (ARROW_DOWN, true, None)
    } else if selection.show_scroll_up {
        (ARROW_UP, true, None)
    } else {
        (" ", false, None)
    };

    // Content color dispatch
    let content_color: Option<&'static str> = if selection.disabled {
        Some("inactive")
    } else if !selection.styled {
        None
    } else if selection.is_selected {
        Some("success")
    } else if selection.is_focused {
        Some("suggestion")
    } else {
        None
    };

    // Trailing checkmark
    let show_check = selection.is_selected && !selection.disabled;
    let check_color = if show_check { Some("success") } else { None };

    // Description line
    let description = description
        .filter(|s| !s.is_empty())
        .map(|s| DescriptionLine {
            padding_left: 2,
            color_key: "inactive",
            text: s.to_string(),
        });

    // Cursor declaration
    let declare_cursor = selection.is_focused && !selection.disabled && declare_cursor;

    ListItemRow {
        indicator,
        indicator_dim,
        indicator_color,
        content_color,
        content_dim: selection.disabled,
        show_check,
        check_color,
        description,
        declare_cursor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> ListItemSelection {
        ListItemSelection::defaults()
    }

    #[test]
    fn glyphs_pinned() {
        assert_eq!(POINTER_GLYPH, "❯");
        assert_eq!(ARROW_DOWN, "↓");
        assert_eq!(ARROW_UP, "↑");
        assert_eq!(TICK_GLYPH, "✔");
    }

    // ────────────────────────────────────────────────────────────────
    // Indicator dispatch
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn indicator_disabled_is_space() {
        let s = ListItemSelection {
            disabled: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert_eq!(r.indicator, " ");
        assert_eq!(r.indicator_color, None);
    }

    #[test]
    fn indicator_focused_is_pointer_with_suggestion_color() {
        let s = ListItemSelection {
            is_focused: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert_eq!(r.indicator, POINTER_GLYPH);
        assert_eq!(r.indicator_color, Some("suggestion"));
    }

    #[test]
    fn indicator_scroll_down_is_arrow_dim() {
        let s = ListItemSelection {
            show_scroll_down: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert_eq!(r.indicator, ARROW_DOWN);
        assert!(r.indicator_dim);
    }

    #[test]
    fn indicator_scroll_up_is_arrow_dim() {
        let s = ListItemSelection {
            show_scroll_up: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert_eq!(r.indicator, ARROW_UP);
        assert!(r.indicator_dim);
    }

    #[test]
    fn indicator_default_is_space() {
        let r = list_item_row(defaults(), None, true);
        assert_eq!(r.indicator, " ");
    }

    #[test]
    fn indicator_disabled_takes_priority_over_focus() {
        let s = ListItemSelection {
            is_focused: true,
            disabled: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert_eq!(r.indicator, " ");
    }

    #[test]
    fn indicator_focused_takes_priority_over_scroll_indicators() {
        let s = ListItemSelection {
            is_focused: true,
            show_scroll_up: true,
            show_scroll_down: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert_eq!(r.indicator, POINTER_GLYPH);
    }

    #[test]
    fn indicator_scroll_down_takes_priority_over_scroll_up() {
        let s = ListItemSelection {
            show_scroll_up: true,
            show_scroll_down: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert_eq!(r.indicator, ARROW_DOWN);
    }

    // ────────────────────────────────────────────────────────────────
    // Content color
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn content_color_disabled_is_inactive() {
        let s = ListItemSelection {
            disabled: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert_eq!(r.content_color, Some("inactive"));
        assert!(r.content_dim);
    }

    #[test]
    fn content_color_unstyled_is_none() {
        let s = ListItemSelection {
            styled: false,
            is_focused: true,
            is_selected: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert_eq!(r.content_color, None);
    }

    #[test]
    fn content_color_selected_is_success() {
        let s = ListItemSelection {
            is_selected: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert_eq!(r.content_color, Some("success"));
    }

    #[test]
    fn content_color_focused_is_suggestion() {
        let s = ListItemSelection {
            is_focused: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert_eq!(r.content_color, Some("suggestion"));
    }

    #[test]
    fn content_color_default_is_none() {
        let r = list_item_row(defaults(), None, true);
        assert_eq!(r.content_color, None);
    }

    #[test]
    fn content_color_selected_takes_priority_over_focused() {
        // is_selected check is BEFORE is_focused.
        let s = ListItemSelection {
            is_selected: true,
            is_focused: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert_eq!(r.content_color, Some("success"));
    }

    #[test]
    fn content_color_disabled_takes_priority_over_everything() {
        let s = ListItemSelection {
            disabled: true,
            is_selected: true,
            is_focused: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert_eq!(r.content_color, Some("inactive"));
    }

    // ────────────────────────────────────────────────────────────────
    // Trailing checkmark
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn check_shown_when_selected_and_not_disabled() {
        let s = ListItemSelection {
            is_selected: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert!(r.show_check);
        assert_eq!(r.check_color, Some("success"));
    }

    #[test]
    fn check_hidden_when_disabled_even_if_selected() {
        let s = ListItemSelection {
            is_selected: true,
            disabled: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert!(!r.show_check);
        assert_eq!(r.check_color, None);
    }

    #[test]
    fn check_hidden_by_default() {
        let r = list_item_row(defaults(), None, true);
        assert!(!r.show_check);
    }

    // ────────────────────────────────────────────────────────────────
    // Description line
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn description_present() {
        let r = list_item_row(defaults(), Some("hello"), true);
        let desc = r.description.unwrap();
        assert_eq!(desc.padding_left, 2);
        assert_eq!(desc.color_key, "inactive");
        assert_eq!(desc.text, "hello");
    }

    #[test]
    fn description_empty_treated_as_none() {
        let r = list_item_row(defaults(), Some(""), true);
        assert_eq!(r.description, None);
    }

    #[test]
    fn description_none() {
        let r = list_item_row(defaults(), None, true);
        assert_eq!(r.description, None);
    }

    // ────────────────────────────────────────────────────────────────
    // Cursor declaration
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn declare_cursor_when_focused_not_disabled() {
        let s = ListItemSelection {
            is_focused: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert!(r.declare_cursor);
    }

    #[test]
    fn declare_cursor_false_when_not_focused() {
        let r = list_item_row(defaults(), None, true);
        assert!(!r.declare_cursor);
    }

    #[test]
    fn declare_cursor_false_when_disabled() {
        let s = ListItemSelection {
            is_focused: true,
            disabled: true,
            ..defaults()
        };
        let r = list_item_row(s, None, true);
        assert!(!r.declare_cursor);
    }

    #[test]
    fn declare_cursor_explicitly_off() {
        let s = ListItemSelection {
            is_focused: true,
            ..defaults()
        };
        let r = list_item_row(s, None, false);
        assert!(!r.declare_cursor);
    }
}
