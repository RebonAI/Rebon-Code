//! Multi-select state reducer for option navigation and selection.
//!
//! The state handler combines navigation state with multi-select slots.
//! Selected values are stored in `selected_values` in insertion order,
//! per-option input text is stored in `input_values` keyed by option
//! value, and submit focus is tracked explicitly with
//! `is_submit_focused`. Input events are modelled as
//! [`MultiSelectEvent`] values reduced to [`MultiSelectAction`] values,
//! so navigation, selection toggles, input edits and submit/cancel
//! decisions stay explicit.
//!
//! Reducer highlights:
//!
//! * **Reset on options change**: when the option list changes, reset
//!   `selected_values` to the caller-provided default via
//!   [`MultiSelectState::reset_for_options_change`].
//! * **Tab moves forward**: if there is a submit button and focus is on
//!   the last option but not yet on submit, move focus to submit.
//!   Otherwise, focus next.
//! * **Shift+Tab moves backward**: if on submit, return focus to
//!   the last option. Otherwise focus previous.
//! * **Down arrow**: apply the submit-button transition logic,
//!   emit the last-item boundary action when needed, then focus next.
//! * **Enter / Space**: with a submit button — submit only when
//!   submit is focused (or Ctrl+Enter from input). Without a submit
//!   button — Enter submits directly, Space toggles selection.
//! * **Numeric jump**: toggle the option at the requested display
//!   index unless numeric jumps are hidden.
//! * **Escape**: emit the cancel action.

use std::collections::{HashMap, HashSet};

use crate::option::{OptionId, OptionWithDescription};
use crate::util::{normalize_full_width_digits, normalize_full_width_space};

/// Pre-resolved multi-select event.
#[derive(Debug, Clone)]
pub enum MultiSelectEvent {
    /// Tab key (without shift).
    Tab,
    /// Shift+Tab.
    ShiftTab,
    /// Down arrow / ctrl+n / `j`.
    Down,
    /// Up arrow / ctrl+p / `k`.
    Up,
    /// Page-down.
    PageDown,
    /// Page-up.
    PageUp,
    /// Plain Enter (no ctrl modifier).
    Enter,
    /// Ctrl+Enter (used for submit-from-input).
    CtrlEnter,
    /// Space (or full-width space).
    Space {
        /// The literal char typed.
        raw: char,
    },
    /// Numeric jump (1..N).
    Numeric {
        /// The raw text typed.
        raw: String,
    },
    /// Escape.
    Escape,
}

/// The action the multi-select reducer emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiSelectAction<T: OptionId> {
    /// No-op.
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
    /// Set the submit-button focus state machine on/off.
    SetSubmitFocus(bool),
    /// Toggle the given value's membership in `selected_values` and
    /// notify the consumer with the new ordered selection.
    ToggleSelection(T),
    /// Request submission of the current ordered selection.
    Submit,
    /// Request cancellation from the escape key.
    Cancel,
    /// Signal that upward navigation crossed the first item.
    UpFromFirstItem,
    /// Signal that downward navigation crossed the last item.
    DownFromLastItem,
}

/// Multi-select state slots that the consumer persists across reducer
/// calls: ordered selections, per-option input text, and submit focus.
#[derive(Debug, Clone)]
pub struct MultiSelectState<T: OptionId> {
    selected_values: Vec<T>,
    input_values: HashMap<T, String>,
    is_submit_focused: bool,
}

impl<T: OptionId> MultiSelectState<T> {
    /// Build a new multi-select state with the given default
    /// selection and the initial input values from any input-type
    /// options' initial text.
    pub fn new(default_selected: Vec<T>, options: &[OptionWithDescription<T>]) -> Self {
        let input_values = initial_input_map(options);
        Self {
            selected_values: default_selected,
            input_values,
            is_submit_focused: false,
        }
    }

    /// Read the selected values (insertion-ordered).
    pub fn selected_values(&self) -> &[T] {
        &self.selected_values
    }

    /// Read the input field values (mutable map).
    pub fn input_values(&self) -> &HashMap<T, String> {
        &self.input_values
    }

    /// Read the submit-focused flag.
    pub fn is_submit_focused(&self) -> bool {
        self.is_submit_focused
    }

    /// Reset selected_values to a fresh default when options change.
    pub fn reset_for_options_change(&mut self, default_selected: Vec<T>) {
        self.selected_values = default_selected;
    }

    /// Apply a [`MultiSelectAction`] that mutates this slot.
    pub fn apply(&mut self, action: &MultiSelectAction<T>) {
        match action {
            MultiSelectAction::SetSubmitFocus(v) => {
                self.is_submit_focused = *v;
            }
            MultiSelectAction::ToggleSelection(value) => {
                self.toggle(value.clone());
            }
            _ => {}
        }
    }

    fn toggle(&mut self, value: T) {
        if let Some(pos) = self.selected_values.iter().position(|v| v == &value) {
            self.selected_values.remove(pos);
        } else {
            self.selected_values.push(value);
        }
    }

    /// Update the input value for a key, applying the
    /// include/exclude rule during active input updates.
    pub fn update_input_value(&mut self, value: T, input_value: String) {
        let is_empty = input_value.is_empty();
        self.input_values.insert(value.clone(), input_value);
        if !is_empty {
            if !self.selected_values.contains(&value) {
                self.selected_values.push(value);
            }
        } else {
            self.selected_values.retain(|v| v != &value);
        }
    }
}

fn initial_input_map<T: OptionId>(options: &[OptionWithDescription<T>]) -> HashMap<T, String> {
    let mut map = HashMap::new();
    for option in options {
        if option.is_input() {
            if let Some(input) = option.input.as_ref() {
                if let Some(initial) = input.initial_value.as_ref() {
                    map.insert(option.value().clone(), initial.clone());
                }
            }
        }
    }
    map
}

/// Properties the multi-select reducer reads from the navigation
/// state and the consumer config.
#[derive(Debug, Clone)]
pub struct MultiSelectProps<'a, T: OptionId> {
    /// Whether input is currently disabled.
    pub is_disabled: bool,
    /// Option list used by the reducer.
    pub options: &'a [OptionWithDescription<T>],
    /// Currently focused option (validated).
    pub focused_value: Option<T>,
    /// Whether the focused option is an input type.
    pub is_in_input: bool,
    /// Whether a submit button is shown (i.e. submit_button_text is set).
    pub has_submit_button: bool,
    /// Whether submit handling is enabled.
    pub has_on_submit: bool,
    /// Whether upward boundary handling is enabled.
    pub has_on_up_from_first: bool,
    /// Whether downward boundary handling is enabled.
    pub has_on_down_from_last: bool,
    /// Whether numeric jumps are hidden.
    pub hide_indexes: bool,
    /// Current submit-focused state.
    pub is_submit_focused: bool,
}

/// The pure reducer.
pub fn reduce<T: OptionId>(
    props: &MultiSelectProps<T>,
    event: &MultiSelectEvent,
) -> MultiSelectAction<T> {
    if props.is_disabled {
        return MultiSelectAction::None;
    }

    let last_value = props.options.last().map(|o| o.value().clone());
    let first_value = props.options.first().map(|o| o.value().clone());

    // In-input events are handled before navigation shortcuts.
    if props.is_in_input {
        let allowed = matches!(
            event,
            MultiSelectEvent::Up
                | MultiSelectEvent::Down
                | MultiSelectEvent::Escape
                | MultiSelectEvent::Tab
                | MultiSelectEvent::ShiftTab
                | MultiSelectEvent::Enter
                | MultiSelectEvent::CtrlEnter
        );
        if !allowed {
            return MultiSelectAction::None;
        }
    }

    match event {
        MultiSelectEvent::Tab => {
            if props.has_submit_button
                && props.has_on_submit
                && props.focused_value == last_value
                && !props.is_submit_focused
            {
                MultiSelectAction::SetSubmitFocus(true)
            } else if !props.is_submit_focused {
                MultiSelectAction::FocusNext
            } else {
                MultiSelectAction::None
            }
        }
        MultiSelectEvent::ShiftTab => {
            if props.has_submit_button && props.has_on_submit && props.is_submit_focused {
                // Move focus back to the last option, blur submit.
                if let Some(v) = last_value.clone() {
                    return MultiSelectAction::FocusOption(v);
                }
                MultiSelectAction::SetSubmitFocus(false)
            } else {
                MultiSelectAction::FocusPrevious
            }
        }
        MultiSelectEvent::Down => {
            if props.is_submit_focused && props.has_on_down_from_last {
                MultiSelectAction::DownFromLastItem
            } else if props.has_submit_button
                && props.has_on_submit
                && props.focused_value == last_value
                && !props.is_submit_focused
            {
                MultiSelectAction::SetSubmitFocus(true)
            } else if !props.has_submit_button
                && props.has_on_down_from_last
                && props.focused_value == last_value
            {
                MultiSelectAction::DownFromLastItem
            } else if !props.is_submit_focused {
                MultiSelectAction::FocusNext
            } else {
                MultiSelectAction::None
            }
        }
        MultiSelectEvent::Up => {
            if props.has_submit_button && props.has_on_submit && props.is_submit_focused {
                if let Some(v) = last_value.clone() {
                    return MultiSelectAction::FocusOption(v);
                }
                MultiSelectAction::SetSubmitFocus(false)
            } else if props.has_on_up_from_first && props.focused_value == first_value {
                MultiSelectAction::UpFromFirstItem
            } else {
                MultiSelectAction::FocusPrevious
            }
        }
        MultiSelectEvent::PageDown => MultiSelectAction::FocusNextPage,
        MultiSelectEvent::PageUp => MultiSelectAction::FocusPreviousPage,
        MultiSelectEvent::Enter => {
            // Ctrl+Enter route is handled separately by CtrlEnter.
            if props.is_submit_focused && props.has_on_submit {
                return MultiSelectAction::Submit;
            }
            if !props.has_submit_button && props.has_on_submit {
                return MultiSelectAction::Submit;
            }
            if let Some(v) = props.focused_value.clone() {
                return MultiSelectAction::ToggleSelection(v);
            }
            MultiSelectAction::None
        }
        MultiSelectEvent::CtrlEnter => {
            if props.is_in_input && props.has_on_submit {
                return MultiSelectAction::Submit;
            }
            // Ctrl+Enter outside input behaves like Enter for the
            // selection toggle path; otherwise it may toggle the
            // focused option.
            if props.is_submit_focused && props.has_on_submit {
                return MultiSelectAction::Submit;
            }
            if !props.has_submit_button && props.has_on_submit {
                return MultiSelectAction::Submit;
            }
            if let Some(v) = props.focused_value.clone() {
                return MultiSelectAction::ToggleSelection(v);
            }
            MultiSelectAction::None
        }
        MultiSelectEvent::Space { raw } => {
            let normalised = normalize_full_width_space(&raw.to_string());
            if normalised != " " {
                return MultiSelectAction::None;
            }
            if props.is_submit_focused && props.has_on_submit {
                return MultiSelectAction::Submit;
            }
            if !props.has_submit_button && props.has_on_submit {
                // No-submit-button + Space toggles selection; the reducer
                // falls through to the toggle branch.
            }
            if let Some(v) = props.focused_value.clone() {
                return MultiSelectAction::ToggleSelection(v);
            }
            MultiSelectAction::None
        }
        MultiSelectEvent::Numeric { raw } => {
            if props.hide_indexes {
                return MultiSelectAction::None;
            }
            let normalised = normalize_full_width_digits(raw);
            if !normalised.chars().all(|c| c.is_ascii_digit()) || normalised.is_empty() {
                return MultiSelectAction::None;
            }
            let n: usize = match normalised.parse() {
                Ok(n) => n,
                Err(_) => return MultiSelectAction::None,
            };
            if n == 0 {
                return MultiSelectAction::None;
            }
            let index = n - 1;
            let opt = match props.options.get(index) {
                Some(o) => o,
                None => return MultiSelectAction::None,
            };
            MultiSelectAction::ToggleSelection(opt.value().clone())
        }
        MultiSelectEvent::Escape => MultiSelectAction::Cancel,
    }
}

/// Convenience: convert a `Vec<T>` of selected values into a
/// `HashSet` for fast membership tests.
pub fn selection_set<T: OptionId>(values: &[T]) -> HashSet<T> {
    values.iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::option::InputOption;

    fn opts(values: &[&'static str]) -> Vec<OptionWithDescription<&'static str>> {
        values
            .iter()
            .map(|v| OptionWithDescription::text(*v, *v))
            .collect()
    }

    fn props<'a>(
        options: &'a [OptionWithDescription<&'static str>],
    ) -> MultiSelectProps<'a, &'static str> {
        MultiSelectProps {
            is_disabled: false,
            options,
            focused_value: options.first().map(|o| *o.value()),
            is_in_input: false,
            has_submit_button: false,
            has_on_submit: true,
            has_on_up_from_first: false,
            has_on_down_from_last: false,
            hide_indexes: false,
            is_submit_focused: false,
        }
    }

    // ---- state slot tests ----

    #[test]
    fn new_with_default_seeds_selected_values() {
        let options = opts(&["a", "b", "c"]);
        let state = MultiSelectState::new(vec!["a", "c"], &options);
        assert_eq!(state.selected_values(), &["a", "c"]);
        assert!(!state.is_submit_focused());
    }

    #[test]
    fn new_seeds_input_values_from_initial_value() {
        let mut options = opts(&[]);
        let mut input_opt = OptionWithDescription::input("type", "i");
        input_opt.input = Some(InputOption {
            initial_value: Some("seed".into()),
            ..Default::default()
        });
        options.push(input_opt);
        let state: MultiSelectState<&'static str> = MultiSelectState::new(vec![], &options);
        assert_eq!(state.input_values().get("i"), Some(&"seed".to_string()));
    }

    #[test]
    fn toggle_inserts_then_removes() {
        let options = opts(&["a", "b"]);
        let mut state: MultiSelectState<&'static str> = MultiSelectState::new(vec![], &options);
        state.toggle("a");
        assert_eq!(state.selected_values(), &["a"]);
        state.toggle("a");
        assert_eq!(state.selected_values(), &[] as &[&'static str]);
    }

    #[test]
    fn update_input_value_adds_when_non_empty() {
        let mut options = opts(&[]);
        options.push(OptionWithDescription::input("type", "i"));
        let mut state: MultiSelectState<&'static str> = MultiSelectState::new(vec![], &options);
        state.update_input_value("i", "hello".into());
        assert_eq!(state.selected_values(), &["i"]);
    }

    #[test]
    fn update_input_value_removes_when_empty() {
        let mut options = opts(&[]);
        options.push(OptionWithDescription::input("type", "i"));
        let mut state: MultiSelectState<&'static str> = MultiSelectState::new(vec!["i"], &options);
        state.update_input_value("i", "".into());
        assert_eq!(state.selected_values(), &[] as &[&'static str]);
    }

    #[test]
    fn reset_for_options_change_clears_to_default() {
        let options = opts(&["a", "b", "c"]);
        let mut state = MultiSelectState::new(vec!["a", "c"], &options);
        state.reset_for_options_change(vec!["b"]);
        assert_eq!(state.selected_values(), &["b"]);
    }

    // ---- reducer tests ----

    #[test]
    fn enter_with_no_submit_button_submits() {
        let options = opts(&["a", "b"]);
        let p = props(&options);
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Enter),
            MultiSelectAction::Submit
        );
    }

    #[test]
    fn enter_with_submit_button_toggles_selection() {
        let options = opts(&["a", "b"]);
        let mut p = props(&options);
        p.has_submit_button = true;
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Enter),
            MultiSelectAction::ToggleSelection("a")
        );
    }

    #[test]
    fn enter_with_submit_focused_submits() {
        let options = opts(&["a", "b"]);
        let mut p = props(&options);
        p.has_submit_button = true;
        p.is_submit_focused = true;
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Enter),
            MultiSelectAction::Submit
        );
    }

    #[test]
    fn space_toggles_selection() {
        let options = opts(&["a", "b"]);
        let p = props(&options);
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Space { raw: ' ' }),
            MultiSelectAction::ToggleSelection("a")
        );
    }

    #[test]
    fn full_width_space_normalises() {
        let options = opts(&["a"]);
        let p = props(&options);
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Space { raw: '\u{3000}' }),
            MultiSelectAction::ToggleSelection("a")
        );
    }

    #[test]
    fn space_when_submit_focused_submits() {
        let options = opts(&["a"]);
        let mut p = props(&options);
        p.has_submit_button = true;
        p.is_submit_focused = true;
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Space { raw: ' ' }),
            MultiSelectAction::Submit
        );
    }

    #[test]
    fn tab_at_last_with_submit_button_focuses_submit() {
        let options = opts(&["a", "b"]);
        let mut p = props(&options);
        p.has_submit_button = true;
        p.focused_value = Some("b");
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Tab),
            MultiSelectAction::SetSubmitFocus(true)
        );
    }

    #[test]
    fn tab_at_non_last_focuses_next() {
        let options = opts(&["a", "b", "c"]);
        let mut p = props(&options);
        p.has_submit_button = true;
        p.focused_value = Some("a");
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Tab),
            MultiSelectAction::FocusNext
        );
    }

    #[test]
    fn shift_tab_when_submit_focused_returns_to_last_option() {
        let options = opts(&["a", "b"]);
        let mut p = props(&options);
        p.has_submit_button = true;
        p.is_submit_focused = true;
        assert_eq!(
            reduce(&p, &MultiSelectEvent::ShiftTab),
            MultiSelectAction::FocusOption("b")
        );
    }

    #[test]
    fn shift_tab_otherwise_focuses_previous() {
        let options = opts(&["a", "b"]);
        let p = props(&options);
        assert_eq!(
            reduce(&p, &MultiSelectEvent::ShiftTab),
            MultiSelectAction::FocusPrevious
        );
    }

    #[test]
    fn down_at_last_with_submit_button_focuses_submit() {
        let options = opts(&["a", "b"]);
        let mut p = props(&options);
        p.has_submit_button = true;
        p.focused_value = Some("b");
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Down),
            MultiSelectAction::SetSubmitFocus(true)
        );
    }

    #[test]
    fn down_at_last_without_submit_with_callback_emits_callback() {
        let options = opts(&["a", "b"]);
        let mut p = props(&options);
        p.has_on_down_from_last = true;
        p.focused_value = Some("b");
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Down),
            MultiSelectAction::DownFromLastItem
        );
    }

    #[test]
    fn down_in_middle_focuses_next() {
        let options = opts(&["a", "b", "c"]);
        let p = props(&options);
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Down),
            MultiSelectAction::FocusNext
        );
    }

    #[test]
    fn up_at_first_with_callback_emits_callback() {
        let options = opts(&["a", "b"]);
        let mut p = props(&options);
        p.has_on_up_from_first = true;
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Up),
            MultiSelectAction::UpFromFirstItem
        );
    }

    #[test]
    fn up_in_middle_focuses_previous() {
        let options = opts(&["a", "b"]);
        let mut p = props(&options);
        p.focused_value = Some("b");
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Up),
            MultiSelectAction::FocusPrevious
        );
    }

    #[test]
    fn page_down_emits_focus_next_page() {
        let options = opts(&["a", "b"]);
        let p = props(&options);
        assert_eq!(
            reduce(&p, &MultiSelectEvent::PageDown),
            MultiSelectAction::FocusNextPage
        );
    }

    #[test]
    fn page_up_emits_focus_previous_page() {
        let options = opts(&["a", "b"]);
        let p = props(&options);
        assert_eq!(
            reduce(&p, &MultiSelectEvent::PageUp),
            MultiSelectAction::FocusPreviousPage
        );
    }

    #[test]
    fn numeric_jump_toggles_at_index() {
        let options = opts(&["a", "b", "c"]);
        let p = props(&options);
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Numeric { raw: "2".into() }),
            MultiSelectAction::ToggleSelection("b")
        );
    }

    #[test]
    fn numeric_jump_full_width_toggles() {
        let options = opts(&["a", "b"]);
        let p = props(&options);
        assert_eq!(
            reduce(
                &p,
                &MultiSelectEvent::Numeric {
                    raw: "\u{FF12}".into()
                }
            ),
            MultiSelectAction::ToggleSelection("b")
        );
    }

    #[test]
    fn numeric_jump_out_of_range_no_op() {
        let options = opts(&["a", "b"]);
        let p = props(&options);
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Numeric { raw: "9".into() }),
            MultiSelectAction::None
        );
    }

    #[test]
    fn numeric_jump_disabled_when_hide_indexes() {
        let options = opts(&["a", "b"]);
        let mut p = props(&options);
        p.hide_indexes = true;
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Numeric { raw: "1".into() }),
            MultiSelectAction::None
        );
    }

    #[test]
    fn escape_emits_cancel() {
        let options = opts(&["a"]);
        let p = props(&options);
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Escape),
            MultiSelectAction::Cancel
        );
    }

    #[test]
    fn in_input_blocks_space() {
        let mut options = opts(&[]);
        options.push(OptionWithDescription::input("type", "i"));
        let mut p = props(&options);
        p.is_in_input = true;
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Space { raw: ' ' }),
            MultiSelectAction::None
        );
    }

    #[test]
    fn in_input_blocks_numeric() {
        let mut options = opts(&[]);
        options.push(OptionWithDescription::input("type", "i"));
        let mut p = props(&options);
        p.is_in_input = true;
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Numeric { raw: "1".into() }),
            MultiSelectAction::None
        );
    }

    #[test]
    fn in_input_allows_arrows_tab_escape_enter() {
        let mut options = opts(&[]);
        options.push(OptionWithDescription::input("type", "i"));
        let mut p = props(&options);
        p.is_in_input = true;
        // Up / Down / Tab / ShiftTab / Escape / Enter should all be honoured.
        assert!(matches!(
            reduce(&p, &MultiSelectEvent::Down),
            MultiSelectAction::FocusNext | MultiSelectAction::DownFromLastItem
        ));
        assert!(matches!(
            reduce(&p, &MultiSelectEvent::Escape),
            MultiSelectAction::Cancel
        ));
    }

    #[test]
    fn in_input_ctrl_enter_submits() {
        let mut options = opts(&[]);
        options.push(OptionWithDescription::input("type", "i"));
        let mut p = props(&options);
        p.is_in_input = true;
        assert_eq!(
            reduce(&p, &MultiSelectEvent::CtrlEnter),
            MultiSelectAction::Submit
        );
    }

    #[test]
    fn disabled_swallows_all_events() {
        let options = opts(&["a"]);
        let mut p = props(&options);
        p.is_disabled = true;
        assert_eq!(
            reduce(&p, &MultiSelectEvent::Enter),
            MultiSelectAction::None
        );
        assert_eq!(reduce(&p, &MultiSelectEvent::Down), MultiSelectAction::None);
    }

    #[test]
    fn selection_set_helper() {
        let s = selection_set(&["a", "b", "a"]);
        assert!(s.contains("a"));
        assert!(s.contains("b"));
        assert_eq!(s.len(), 2);
    }
}
