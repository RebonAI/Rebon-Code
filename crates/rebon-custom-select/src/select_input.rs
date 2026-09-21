//! Single-select input reducer.
//!
//! The reducer does two jobs:
//!
//! 1. Handles the semantic "select" events (next, previous, accept,
//!    cancel). This is conditional: while the focused option is an
//!    input type, next/previous/accept are *not* acted on, so that
//!    j/k/enter reach the text input instead.
//! 2. Handles the remaining raw keys: page up/down, tab, space,
//!    numeric jump, and the in-input arrow handling.
//!
//! It is modelled as a pure event reducer: it takes pre-resolved
//! semantic events and produces a [`SelectInputAction`] that the
//! consumer dispatches to the appropriate sub-reducers (navigation,
//! select state, callback queue).

use crate::option::{InputBehaviour, OptionId, OptionType, OptionWithDescription};
use crate::util::{normalize_full_width_digits, normalize_full_width_space};

/// Selection-disable mode: off, fully disabled, or numeric jump only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisableSelection {
    /// Selection is enabled.
    #[default]
    Off,
    /// Both Enter and numeric jump are disabled.
    All,
    /// Only the numeric jump is disabled. Enter still selects.
    Numeric,
}

/// Snapshot of the navigation/select state passed into the input
/// reducer: the state and inputs one [`reduce`] call reads.
#[derive(Debug, Clone)]
pub struct SelectInputProps<'a, T: OptionId> {
    /// Whether input is currently disabled (no events processed).
    pub is_disabled: bool,
    /// Selection-disable mode.
    pub disable_selection: DisableSelection,
    /// Whether multi-select space-toggle is enabled.
    pub is_multi_select: bool,
    /// Whether image-selection mode is currently active on a focused
    /// input option (suppresses arrow handling).
    pub images_selected: bool,
    /// Currently focused value.
    pub focused_value: Option<T>,
    /// Whether the currently focused option is an input type.
    pub is_in_input: bool,
    /// Whether the viewport's `visible_from_index` is 0 (used by the
    /// up-from-first-item check).
    pub visible_from_zero: bool,
    /// Input option list.
    pub options: &'a [OptionWithDescription<T>],
    /// Current input field values (for the numeric-jump auto-submit
    /// rule on input options).
    pub input_values: &'a std::collections::HashMap<T, String>,
    /// Whether the consumer has a cancel callback (i.e., escape is honoured).
    pub has_on_cancel: bool,
    /// Whether the consumer has an up-from-first-item callback.
    pub has_on_up_from_first: bool,
    /// Whether the consumer has a down-from-last-item callback.
    pub has_on_down_from_last: bool,
    /// Whether the consumer has an input-mode toggle callback.
    pub has_on_input_mode_toggle: bool,
    /// Whether the consumer has an image-selection callback AND it will return
    /// true (the consumer pre-evaluates the closure return value).
    pub on_enter_image_selection_returns_true: bool,
}

/// Pre-resolved input events: one variant per handled key.
#[derive(Debug, Clone)]
pub enum SelectInputEvent {
    /// Semantic `select:next` (down arrow / ctrl+n).
    SelectNext,
    /// Semantic `select:previous` (up arrow / ctrl+p).
    SelectPrevious,
    /// Semantic `select:accept` (enter).
    SelectAccept,
    /// Semantic `select:cancel` (escape).
    SelectCancel,
    /// Page-down key.
    PageDown,
    /// Page-up key.
    PageUp,
    /// Tab key (toggles input mode for the focused option).
    Tab,
    /// Space key (toggles selection in multi-select; passes through
    /// otherwise). The reducer normalises full-width space.
    Space {
        /// The raw key text — may be `" "` or `"\u{3000}"`.
        raw: char,
    },
    /// Numeric jump (1-9). Reducer normalises full-width digits.
    Numeric {
        /// The raw text typed (`"1"`, `"\u{FF12}"`, etc.).
        raw: String,
    },
    /// In-input down arrow (only meaningful when `is_in_input`
    /// is true). Used for the image-selection enter check + the
    /// down-from-last fallback.
    InInputDownArrow,
    /// In-input up arrow.
    InInputUpArrow,
}

/// The action the input reducer emits. Consumers route these to the
/// appropriate sub-reducers (navigation, select_state) and the side
/// effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectInputAction<T: OptionId> {
    /// No-op (event was handled or ignored without state change).
    None,
    /// Forward to `navigation::FocusNextOption`.
    FocusNext,
    /// Forward to `navigation::FocusPreviousOption`.
    FocusPrevious,
    /// Forward to `navigation::FocusNextPage`.
    FocusNextPage,
    /// Forward to `navigation::FocusPreviousPage`.
    FocusPreviousPage,
    /// Forward to `navigation::SetFocus(value)`.
    FocusOption(T),
    /// Select the focused option, then report it as the change.
    SelectFocused(T),
    /// Report `value` as the change directly, without focusing first.
    Submit(T),
    /// Invoke the consumer's cancel callback.
    Cancel,
    /// Invoke the consumer's up-from-first-item callback.
    UpFromFirstItem,
    /// Invoke the consumer's down-from-last-item callback.
    DownFromLastItem,
    /// Invoke the consumer's input-mode toggle callback for `value`.
    ToggleInputMode(T),
    /// Image-selection mode was entered for the focused input option;
    /// the consumer must invoke its image-selection callback.
    EnterImageSelection,
}

/// The pure reducer. Returns the action the consumer should perform.
pub fn reduce<T: OptionId>(
    props: &SelectInputProps<T>,
    event: &SelectInputEvent,
) -> SelectInputAction<T> {
    if props.is_disabled {
        return SelectInputAction::None;
    }

    let focused_option = props
        .focused_value
        .as_ref()
        .and_then(|v| props.options.iter().find(|o| o.value() == v));
    let current_is_in_input = focused_option
        .map(|o| matches!(o.r#type, OptionType::Input))
        .unwrap_or(false);

    match event {
        // Tab → toggle input mode (regardless of in-input state).
        SelectInputEvent::Tab => {
            if props.has_on_input_mode_toggle {
                if let Some(v) = props.focused_value.clone() {
                    return SelectInputAction::ToggleInputMode(v);
                }
            }
            SelectInputAction::None
        }

        // In-input arrow handling — supersedes the normal next/prev.
        SelectInputEvent::InInputDownArrow if current_is_in_input => {
            if props.images_selected {
                return SelectInputAction::None;
            }
            if props.on_enter_image_selection_returns_true {
                return SelectInputAction::EnterImageSelection;
            }
            if props.has_on_down_from_last {
                if let (Some(last), Some(focused)) =
                    (props.options.last(), props.focused_value.as_ref())
                {
                    if last.value() == focused {
                        return SelectInputAction::DownFromLastItem;
                    }
                }
            }
            SelectInputAction::FocusNext
        }
        SelectInputEvent::InInputUpArrow if current_is_in_input => {
            if props.images_selected {
                return SelectInputAction::None;
            }
            if props.has_on_up_from_first && props.visible_from_zero {
                if let (Some(first), Some(focused)) =
                    (props.options.first(), props.focused_value.as_ref())
                {
                    if first.value() == focused {
                        return SelectInputAction::UpFromFirstItem;
                    }
                }
            }
            SelectInputAction::FocusPrevious
        }
        // In input mode but no arrow — pass through.
        SelectInputEvent::InInputDownArrow | SelectInputEvent::InInputUpArrow => {
            SelectInputAction::None
        }

        // Page navigation — suppressed in input mode so that page_down
        // and page_up reach the text input instead of driving the
        // select.
        SelectInputEvent::PageDown => {
            if current_is_in_input {
                return SelectInputAction::None;
            }
            SelectInputAction::FocusNextPage
        }
        SelectInputEvent::PageUp => {
            if current_is_in_input {
                return SelectInputAction::None;
            }
            SelectInputAction::FocusPreviousPage
        }

        // The normal next/prev/accept set — only when NOT in input.
        SelectInputEvent::SelectNext if !current_is_in_input => {
            if props.has_on_down_from_last {
                if let (Some(last), Some(focused)) =
                    (props.options.last(), props.focused_value.as_ref())
                {
                    if last.value() == focused {
                        return SelectInputAction::DownFromLastItem;
                    }
                }
            }
            SelectInputAction::FocusNext
        }
        SelectInputEvent::SelectPrevious if !current_is_in_input => {
            if props.has_on_up_from_first && props.visible_from_zero {
                if let (Some(first), Some(focused)) =
                    (props.options.first(), props.focused_value.as_ref())
                {
                    if first.value() == focused {
                        return SelectInputAction::UpFromFirstItem;
                    }
                }
            }
            SelectInputAction::FocusPrevious
        }
        SelectInputEvent::SelectAccept if !current_is_in_input => {
            if matches!(props.disable_selection, DisableSelection::All) {
                return SelectInputAction::None;
            }
            let value = match props.focused_value.clone() {
                Some(v) => v,
                None => return SelectInputAction::None,
            };
            if let Some(opt) = focused_option {
                if opt.is_disabled() {
                    return SelectInputAction::None;
                }
            }
            SelectInputAction::SelectFocused(value)
        }
        SelectInputEvent::SelectNext
        | SelectInputEvent::SelectPrevious
        | SelectInputEvent::SelectAccept => SelectInputAction::None,

        SelectInputEvent::SelectCancel => {
            if props.has_on_cancel {
                SelectInputAction::Cancel
            } else {
                SelectInputAction::None
            }
        }

        // Space — multi-select toggle. Suppressed in input mode so
        // that typing a space inserts a literal space character into
        // the field rather than toggling selection.
        SelectInputEvent::Space { raw } => {
            if current_is_in_input {
                return SelectInputAction::None;
            }
            if matches!(props.disable_selection, DisableSelection::All) {
                return SelectInputAction::None;
            }
            let normalised = normalize_full_width_space(&raw.to_string());
            if normalised != " " {
                return SelectInputAction::None;
            }
            if !props.is_multi_select {
                return SelectInputAction::None;
            }
            let value = match props.focused_value.clone() {
                Some(v) => v,
                None => return SelectInputAction::None,
            };
            if let Some(opt) = focused_option {
                if opt.is_disabled() {
                    return SelectInputAction::None;
                }
            }
            SelectInputAction::SelectFocused(value)
        }

        // Numeric jump (1..N). Suppressed in input mode so that digit
        // keys type literally into the focused text input instead of
        // jumping to another option.
        SelectInputEvent::Numeric { raw } => {
            if current_is_in_input {
                return SelectInputAction::None;
            }
            if matches!(
                props.disable_selection,
                DisableSelection::All | DisableSelection::Numeric
            ) {
                return SelectInputAction::None;
            }
            let normalised = normalize_full_width_digits(raw);
            if !normalised.chars().all(|c| c.is_ascii_digit()) || normalised.is_empty() {
                return SelectInputAction::None;
            }
            let n: usize = match normalised.parse() {
                Ok(n) => n,
                Err(_) => return SelectInputAction::None,
            };
            if n == 0 {
                return SelectInputAction::None;
            }
            let index = n - 1;
            let selected = match props.options.get(index) {
                Some(o) => o,
                None => return SelectInputAction::None,
            };
            if selected.is_disabled() {
                return SelectInputAction::None;
            }
            if matches!(selected.r#type, OptionType::Input) {
                let current = props
                    .input_values
                    .get(selected.value())
                    .map(|s| s.as_str())
                    .unwrap_or("");
                if !current.trim().is_empty() {
                    return SelectInputAction::Submit(selected.value().clone());
                }
                if let Some(input_meta) = selected.input.as_ref() {
                    if matches!(input_meta.behaviour, InputBehaviour::EmptySubmits) {
                        return SelectInputAction::Submit(selected.value().clone());
                    }
                }
                return SelectInputAction::FocusOption(selected.value().clone());
            }
            SelectInputAction::Submit(selected.value().clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::option::InputOption;
    use std::collections::HashMap;

    fn opts(values: &[&'static str]) -> Vec<OptionWithDescription<&'static str>> {
        values
            .iter()
            .map(|v| OptionWithDescription::text(*v, *v))
            .collect()
    }

    fn empty_inputs() -> HashMap<&'static str, String> {
        HashMap::new()
    }

    fn props<'a>(
        options: &'a [OptionWithDescription<&'static str>],
        input_values: &'a HashMap<&'static str, String>,
    ) -> SelectInputProps<'a, &'static str> {
        SelectInputProps {
            is_disabled: false,
            disable_selection: DisableSelection::Off,
            is_multi_select: false,
            images_selected: false,
            focused_value: options.first().map(|o| *o.value()),
            is_in_input: false,
            visible_from_zero: true,
            options,
            input_values,
            has_on_cancel: true,
            has_on_up_from_first: false,
            has_on_down_from_last: false,
            has_on_input_mode_toggle: false,
            on_enter_image_selection_returns_true: false,
        }
    }

    #[test]
    fn select_next_within_list() {
        let options = opts(&["a", "b"]);
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(&p, &SelectInputEvent::SelectNext),
            SelectInputAction::FocusNext
        );
    }

    #[test]
    fn select_previous_within_list() {
        let options = opts(&["a", "b"]);
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(&p, &SelectInputEvent::SelectPrevious),
            SelectInputAction::FocusPrevious
        );
    }

    #[test]
    fn select_accept_emits_select_focused() {
        let options = opts(&["a", "b"]);
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(&p, &SelectInputEvent::SelectAccept),
            SelectInputAction::SelectFocused("a")
        );
    }

    #[test]
    fn select_accept_when_focused_disabled_no_op() {
        let mut options = opts(&["a", "b"]);
        options[0].base.disabled = true;
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(&p, &SelectInputEvent::SelectAccept),
            SelectInputAction::None
        );
    }

    #[test]
    fn select_accept_with_disable_selection_all_no_op() {
        let options = opts(&["a"]);
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.disable_selection = DisableSelection::All;
        assert_eq!(
            reduce(&p, &SelectInputEvent::SelectAccept),
            SelectInputAction::None
        );
    }

    #[test]
    fn select_accept_with_disable_selection_numeric_still_works() {
        let options = opts(&["a"]);
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.disable_selection = DisableSelection::Numeric;
        assert_eq!(
            reduce(&p, &SelectInputEvent::SelectAccept),
            SelectInputAction::SelectFocused("a")
        );
    }

    #[test]
    fn select_cancel_when_on_cancel_present() {
        let options = opts(&["a"]);
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(&p, &SelectInputEvent::SelectCancel),
            SelectInputAction::Cancel
        );
    }

    #[test]
    fn select_cancel_when_no_on_cancel_no_op() {
        let options = opts(&["a"]);
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.has_on_cancel = false;
        assert_eq!(
            reduce(&p, &SelectInputEvent::SelectCancel),
            SelectInputAction::None
        );
    }

    #[test]
    fn page_down_emits_focus_next_page() {
        let options = opts(&["a", "b"]);
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(&p, &SelectInputEvent::PageDown),
            SelectInputAction::FocusNextPage
        );
    }

    #[test]
    fn page_up_emits_focus_previous_page() {
        let options = opts(&["a", "b"]);
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(&p, &SelectInputEvent::PageUp),
            SelectInputAction::FocusPreviousPage
        );
    }

    #[test]
    fn numeric_one_jumps_to_first() {
        let options = opts(&["a", "b", "c"]);
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.focused_value = Some("c");
        assert_eq!(
            reduce(&p, &SelectInputEvent::Numeric { raw: "1".into() }),
            SelectInputAction::Submit("a")
        );
    }

    #[test]
    fn numeric_full_width_one_jumps_to_first() {
        let options = opts(&["a", "b"]);
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(
                &p,
                &SelectInputEvent::Numeric {
                    raw: "\u{FF11}".into()
                }
            ),
            SelectInputAction::Submit("a")
        );
    }

    #[test]
    fn numeric_out_of_range_no_op() {
        let options = opts(&["a", "b"]);
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(&p, &SelectInputEvent::Numeric { raw: "9".into() }),
            SelectInputAction::None
        );
    }

    #[test]
    fn numeric_zero_no_op() {
        let options = opts(&["a"]);
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(&p, &SelectInputEvent::Numeric { raw: "0".into() }),
            SelectInputAction::None
        );
    }

    #[test]
    fn numeric_disabled_when_disable_selection_numeric() {
        let options = opts(&["a"]);
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.disable_selection = DisableSelection::Numeric;
        assert_eq!(
            reduce(&p, &SelectInputEvent::Numeric { raw: "1".into() }),
            SelectInputAction::None
        );
    }

    #[test]
    fn numeric_to_disabled_option_no_op() {
        let mut options = opts(&["a", "b"]);
        options[1].base.disabled = true;
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(&p, &SelectInputEvent::Numeric { raw: "2".into() }),
            SelectInputAction::None
        );
    }

    #[test]
    fn numeric_to_input_with_filled_value_submits() {
        let mut options = opts(&["a"]);
        options.push(OptionWithDescription::input("type", "i"));
        let mut inputs = empty_inputs();
        inputs.insert("i", "hello".into());
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(&p, &SelectInputEvent::Numeric { raw: "2".into() }),
            SelectInputAction::Submit("i")
        );
    }

    #[test]
    fn numeric_to_input_with_only_whitespace_focuses_not_submits() {
        let mut options = opts(&["a"]);
        options.push(OptionWithDescription::input("type", "i"));
        let mut inputs = empty_inputs();
        inputs.insert("i", "   ".into());
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(&p, &SelectInputEvent::Numeric { raw: "2".into() }),
            SelectInputAction::FocusOption("i")
        );
    }

    #[test]
    fn numeric_to_input_with_empty_submits_when_allow_empty() {
        let mut options = opts(&["a"]);
        let mut input_opt = OptionWithDescription::input("type", "i");
        input_opt.input = Some(InputOption {
            behaviour: InputBehaviour::EmptySubmits,
            ..Default::default()
        });
        options.push(input_opt);
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(&p, &SelectInputEvent::Numeric { raw: "2".into() }),
            SelectInputAction::Submit("i")
        );
    }

    #[test]
    fn numeric_to_input_with_empty_focuses_when_default_behaviour() {
        let mut options = opts(&["a"]);
        options.push(OptionWithDescription::input("type", "i"));
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(&p, &SelectInputEvent::Numeric { raw: "2".into() }),
            SelectInputAction::FocusOption("i")
        );
    }

    #[test]
    fn space_in_multi_select_toggles_focus() {
        let options = opts(&["a", "b"]);
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.is_multi_select = true;
        assert_eq!(
            reduce(&p, &SelectInputEvent::Space { raw: ' ' }),
            SelectInputAction::SelectFocused("a")
        );
    }

    #[test]
    fn space_in_single_select_no_op() {
        let options = opts(&["a"]);
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(&p, &SelectInputEvent::Space { raw: ' ' }),
            SelectInputAction::None
        );
    }

    #[test]
    fn full_width_space_normalised_in_multi() {
        let options = opts(&["a"]);
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.is_multi_select = true;
        assert_eq!(
            reduce(&p, &SelectInputEvent::Space { raw: '\u{3000}' }),
            SelectInputAction::SelectFocused("a")
        );
    }

    #[test]
    fn space_with_disabled_focused_no_op() {
        let mut options = opts(&["a"]);
        options[0].base.disabled = true;
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.is_multi_select = true;
        assert_eq!(
            reduce(&p, &SelectInputEvent::Space { raw: ' ' }),
            SelectInputAction::None
        );
    }

    #[test]
    fn tab_with_handler_emits_toggle_input_mode() {
        let options = opts(&["a"]);
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.has_on_input_mode_toggle = true;
        assert_eq!(
            reduce(&p, &SelectInputEvent::Tab),
            SelectInputAction::ToggleInputMode("a")
        );
    }

    #[test]
    fn tab_without_handler_no_op() {
        let options = opts(&["a"]);
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        assert_eq!(reduce(&p, &SelectInputEvent::Tab), SelectInputAction::None);
    }

    #[test]
    fn down_from_last_with_handler_at_last_emits_callback() {
        let options = opts(&["a", "b"]);
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.has_on_down_from_last = true;
        p.focused_value = Some("b");
        assert_eq!(
            reduce(&p, &SelectInputEvent::SelectNext),
            SelectInputAction::DownFromLastItem
        );
    }

    #[test]
    fn down_from_last_handler_at_non_last_focuses_next() {
        let options = opts(&["a", "b"]);
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.has_on_down_from_last = true;
        // focused = a (not last)
        assert_eq!(
            reduce(&p, &SelectInputEvent::SelectNext),
            SelectInputAction::FocusNext
        );
    }

    #[test]
    fn up_from_first_with_handler_at_first_emits_callback() {
        let options = opts(&["a", "b"]);
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.has_on_up_from_first = true;
        p.visible_from_zero = true;
        assert_eq!(
            reduce(&p, &SelectInputEvent::SelectPrevious),
            SelectInputAction::UpFromFirstItem
        );
    }

    #[test]
    fn up_from_first_with_window_offset_focuses_previous() {
        let options = opts(&["a", "b"]);
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.has_on_up_from_first = true;
        p.visible_from_zero = false; // window scrolled
        assert_eq!(
            reduce(&p, &SelectInputEvent::SelectPrevious),
            SelectInputAction::FocusPrevious
        );
    }

    #[test]
    fn in_input_state_blocks_select_next() {
        let mut options = opts(&[]);
        options.push(OptionWithDescription::input("type", "i"));
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.is_in_input = true;
        assert_eq!(
            reduce(&p, &SelectInputEvent::SelectNext),
            SelectInputAction::None
        );
    }

    #[test]
    fn in_input_down_arrow_focuses_next_when_not_at_last_with_no_callback() {
        let mut options = opts(&["a"]);
        options.insert(0, OptionWithDescription::input("type", "i"));
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.focused_value = Some("i");
        // i is at index 0, "a" is the last; NOT at the last → focus next.
        assert_eq!(
            reduce(&p, &SelectInputEvent::InInputDownArrow),
            SelectInputAction::FocusNext
        );
    }

    #[test]
    fn in_input_down_arrow_with_image_selection_no_op() {
        let mut options = opts(&[]);
        options.push(OptionWithDescription::input("type", "i"));
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.focused_value = Some("i");
        p.images_selected = true;
        assert_eq!(
            reduce(&p, &SelectInputEvent::InInputDownArrow),
            SelectInputAction::None
        );
    }

    #[test]
    fn in_input_down_arrow_enters_image_selection_when_callback_returns_true() {
        let mut options = opts(&[]);
        options.push(OptionWithDescription::input("type", "i"));
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.focused_value = Some("i");
        p.on_enter_image_selection_returns_true = true;
        assert_eq!(
            reduce(&p, &SelectInputEvent::InInputDownArrow),
            SelectInputAction::EnterImageSelection
        );
    }

    #[test]
    fn page_down_in_input_mode_is_no_op() {
        // The in-input arrow branch returns early, so page_down does
        // nothing when the focused option is an input-type.
        let mut options = opts(&["a"]);
        options.insert(0, OptionWithDescription::input("type", "i"));
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.focused_value = Some("i");
        assert_eq!(
            reduce(&p, &SelectInputEvent::PageDown),
            SelectInputAction::None
        );
    }

    #[test]
    fn page_up_in_input_mode_is_no_op() {
        let mut options = opts(&["a"]);
        options.insert(0, OptionWithDescription::input("type", "i"));
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.focused_value = Some("i");
        assert_eq!(
            reduce(&p, &SelectInputEvent::PageUp),
            SelectInputAction::None
        );
    }

    #[test]
    fn page_down_outside_input_still_works() {
        let options = opts(&["a", "b"]);
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        assert_eq!(
            reduce(&p, &SelectInputEvent::PageDown),
            SelectInputAction::FocusNextPage
        );
    }

    #[test]
    fn space_in_input_mode_is_no_op_even_with_multi_select() {
        // Digits and space type literally into the focused text input —
        // the space handler is unreachable in input mode.
        let mut options = opts(&["a"]);
        options.insert(0, OptionWithDescription::input("type", "i"));
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.focused_value = Some("i");
        p.is_multi_select = true;
        assert_eq!(
            reduce(&p, &SelectInputEvent::Space { raw: ' ' }),
            SelectInputAction::None
        );
    }

    #[test]
    fn numeric_in_input_mode_is_no_op() {
        // Digits type literally into the input rather than selecting
        // options.
        let mut options = opts(&["a", "b"]);
        options.insert(0, OptionWithDescription::input("type", "i"));
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.focused_value = Some("i");
        assert_eq!(
            reduce(&p, &SelectInputEvent::Numeric { raw: "2".into() }),
            SelectInputAction::None
        );
    }

    #[test]
    fn numeric_to_input_target_still_works_when_not_in_input() {
        // Sanity: the in-input gate applies to the *focused* option,
        // not the numeric *target*. Jumping from a text option to an
        // input option by its index is still honoured.
        let mut options = opts(&["a"]);
        options.push(OptionWithDescription::input("type", "i"));
        let inputs = empty_inputs();
        let p = props(&options, &inputs);
        // Focused is "a" (text), pressing "2" should still route to
        // the input option's focus-or-submit handler.
        assert_eq!(
            reduce(&p, &SelectInputEvent::Numeric { raw: "2".into() }),
            SelectInputAction::FocusOption("i")
        );
    }

    #[test]
    fn disabled_swallows_all_events() {
        let options = opts(&["a"]);
        let inputs = empty_inputs();
        let mut p = props(&options, &inputs);
        p.is_disabled = true;
        assert_eq!(
            reduce(&p, &SelectInputEvent::SelectNext),
            SelectInputAction::None
        );
        assert_eq!(
            reduce(&p, &SelectInputEvent::SelectAccept),
            SelectInputAction::None
        );
        assert_eq!(reduce(&p, &SelectInputEvent::Tab), SelectInputAction::None);
    }
}
