//! View state for the prompt's text input widget.
//!
//! This module does not build the widget itself; it computes the plain
//! data choices that feed it: displayed value, placeholder, focus and
//! cursor visibility flags, and whether undo wiring should be enabled.

/// Pure inputs needed to derive the text-input view state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInputViewInput {
    /// Current raw prompt input.
    pub input: String,
    /// Optional history-match display text. When present and history search is
    /// active, this overrides the visible input value.
    pub history_match_display: Option<String>,
    /// Whether history search is active.
    pub is_searching_history: bool,
    /// Whether any modal overlay is active.
    pub is_modal_overlay_active: bool,
    /// Whether any footer item is selected.
    pub footer_item_selected: bool,
    /// Number of active suggestions.
    pub suggestion_count: usize,
    /// Whether the cursor currently sits on an image chip.
    pub cursor_at_image_chip: bool,
    /// Default placeholder from
    /// [`resolve_prompt_input_placeholder`](crate::promptinput::prompt_input_placeholder::resolve_prompt_input_placeholder).
    pub default_placeholder: Option<String>,
    /// Whether the prompt suggestion should be shown instead of the default placeholder.
    pub show_prompt_suggestion: bool,
    /// Prompt suggestion text when present.
    pub prompt_suggestion: Option<String>,
    /// Whether undo is currently available.
    pub can_undo: bool,
}

/// Plain-data view state for the text input widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInputViewState {
    /// Final visible input value.
    pub displayed_value: String,
    /// Final placeholder string.
    pub placeholder: Option<String>,
    /// Up/Down keys do not move the cursor (suggestions or a footer pill own them).
    pub disable_cursor_movement_for_up_down_keys: bool,
    /// Escape double-press is disabled while suggestions show.
    pub disable_escape_double_press: bool,
    /// Whether the text input has focus.
    pub focus: bool,
    /// Whether the cursor is drawn.
    pub show_cursor: bool,
    /// Whether the caller should wire up undo.
    pub undo_enabled: bool,
    /// Whether the raw input contains a newline.
    pub is_input_wrapped: bool,
}

/// Derive the displayed value, placeholder, and the focus / cursor / key flags.
pub fn build_text_input_view_state(input: &TextInputViewInput) -> TextInputViewState {
    let displayed_value = if input.is_searching_history {
        input
            .history_match_display
            .clone()
            .unwrap_or_else(|| input.input.clone())
    } else {
        // Strip mode prefixes (for example `!`) before showing the value.
        crate::promptinput::input_modes::get_value_from_input(&input.input)
    };

    let placeholder = if input.show_prompt_suggestion {
        input
            .prompt_suggestion
            .clone()
            .or_else(|| input.default_placeholder.clone())
    } else {
        input.default_placeholder.clone()
    };

    TextInputViewState {
        displayed_value,
        placeholder,
        disable_cursor_movement_for_up_down_keys: input.suggestion_count > 0
            || input.footer_item_selected,
        disable_escape_double_press: input.suggestion_count > 0,
        focus: !input.is_searching_history
            && !input.is_modal_overlay_active
            && !input.footer_item_selected,
        show_cursor: !input.footer_item_selected
            && !input.is_searching_history
            && !input.cursor_at_image_chip,
        undo_enabled: input.can_undo,
        is_input_wrapped: input.input.contains('\n'),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> TextInputViewInput {
        TextInputViewInput {
            input: String::from("hello"),
            history_match_display: None,
            is_searching_history: false,
            is_modal_overlay_active: false,
            footer_item_selected: false,
            suggestion_count: 0,
            cursor_at_image_chip: false,
            default_placeholder: Some(String::from("default")),
            show_prompt_suggestion: false,
            prompt_suggestion: Some(String::from("suggested")),
            can_undo: false,
        }
    }

    #[test]
    fn displayed_value_prefers_history_match_while_searching() {
        let mut input = base();
        input.is_searching_history = true;
        input.history_match_display = Some(String::from("history value"));
        let state = build_text_input_view_state(&input);
        assert_eq!(state.displayed_value, "history value");
    }

    #[test]
    fn placeholder_prefers_prompt_suggestion_when_enabled() {
        let mut input = base();
        input.show_prompt_suggestion = true;
        let state = build_text_input_view_state(&input);
        assert_eq!(state.placeholder.as_deref(), Some("suggested"));
    }

    #[test]
    fn placeholder_falls_back_to_default_when_no_prompt_suggestion_text_exists() {
        let mut input = base();
        input.show_prompt_suggestion = true;
        input.prompt_suggestion = None;
        let state = build_text_input_view_state(&input);
        assert_eq!(state.placeholder.as_deref(), Some("default"));
    }

    #[test]
    fn suggestion_or_footer_selection_disable_cursor_navigation_and_escape_double_press() {
        let mut input = base();
        input.suggestion_count = 2;
        let state = build_text_input_view_state(&input);
        assert!(state.disable_cursor_movement_for_up_down_keys);
        assert!(state.disable_escape_double_press);

        input.suggestion_count = 0;
        input.footer_item_selected = true;
        let state = build_text_input_view_state(&input);
        assert!(state.disable_cursor_movement_for_up_down_keys);
        assert!(!state.disable_escape_double_press);
    }

    #[test]
    fn focus_and_cursor_visibility_follow_search_modal_footer_and_chip_guards() {
        let mut input = base();
        input.is_searching_history = true;
        let state = build_text_input_view_state(&input);
        assert!(!state.focus);
        assert!(!state.show_cursor);

        input.is_searching_history = false;
        input.is_modal_overlay_active = true;
        let state = build_text_input_view_state(&input);
        assert!(!state.focus);

        input.is_modal_overlay_active = false;
        input.footer_item_selected = true;
        let state = build_text_input_view_state(&input);
        assert!(!state.focus);
        assert!(!state.show_cursor);

        input.footer_item_selected = false;
        input.cursor_at_image_chip = true;
        let state = build_text_input_view_state(&input);
        assert!(state.focus);
        assert!(!state.show_cursor);
    }

    #[test]
    fn undo_and_wrapped_flags_are_carried_through() {
        let mut input = base();
        input.can_undo = true;
        input.input = String::from("a\nb");
        let state = build_text_input_view_state(&input);
        assert!(state.undo_enabled);
        assert!(state.is_input_wrapped);
    }
}
