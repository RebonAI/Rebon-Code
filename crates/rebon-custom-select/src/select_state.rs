//! Single-select state.
//!
//! Holds the one piece of state that outlives a render — `value`, the
//! *selected* (as opposed to *focused*) option — plus the
//! `SelectFocused` action that copies the focused value into it.
//!
//! The change / cancel callbacks are deliberately absent: the reducer
//! stays pure, and the consumer routes those side effects when it
//! handles the action (see [`crate::select_input::SelectInputProps`]).

use crate::option::OptionId;

/// Single-select reducer action set.
#[derive(Debug, Clone)]
pub enum SelectStateAction<T: OptionId> {
    /// Copy the currently focused value into the selected slot.
    SelectFocused {
        /// The currently focused value (passed in from the navigation
        /// reducer).
        focused_value: Option<T>,
    },
    /// Programmatically set the selected value (used by the
    /// orchestrator on Enter when the focused option is a non-input).
    SetValue(Option<T>),
}

/// Single-select state: holds the selected `value`, set by the
/// `SelectFocused` action.
#[derive(Debug, Clone, Default)]
pub struct SelectState<T: OptionId> {
    value: Option<T>,
}

impl<T: OptionId> SelectState<T> {
    /// Build with no default selection.
    pub fn new() -> Self {
        Self { value: None }
    }

    /// Build with an initial selected value.
    pub fn with_default(default_value: T) -> Self {
        Self {
            value: Some(default_value),
        }
    }

    /// Read the current selection.
    pub fn value(&self) -> Option<&T> {
        self.value.as_ref()
    }

    /// Apply an action to the state.
    pub fn dispatch(self, action: SelectStateAction<T>) -> Self {
        match action {
            SelectStateAction::SelectFocused { focused_value } => Self {
                value: focused_value,
            },
            SelectStateAction::SetValue(v) => Self { value: v },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_has_no_value() {
        let state: SelectState<&str> = SelectState::new();
        assert_eq!(state.value(), None);
    }

    #[test]
    fn with_default_stores_value() {
        let state = SelectState::with_default("a");
        assert_eq!(state.value(), Some(&"a"));
    }

    #[test]
    fn select_focused_copies_focused_value() {
        let state: SelectState<&str> = SelectState::new();
        let state = state.dispatch(SelectStateAction::SelectFocused {
            focused_value: Some("b"),
        });
        assert_eq!(state.value(), Some(&"b"));
    }

    #[test]
    fn select_focused_with_none_clears_value() {
        let state = SelectState::with_default("a");
        let state = state.dispatch(SelectStateAction::SelectFocused {
            focused_value: None,
        });
        assert_eq!(state.value(), None);
    }

    #[test]
    fn set_value_replaces_value() {
        let state = SelectState::with_default("a");
        let state = state.dispatch(SelectStateAction::SetValue(Some("z")));
        assert_eq!(state.value(), Some(&"z"));
    }

    #[test]
    fn set_value_can_clear() {
        let state = SelectState::with_default("a");
        let state = state.dispatch(SelectStateAction::SetValue(None));
        assert_eq!(state.value(), None);
    }
}
