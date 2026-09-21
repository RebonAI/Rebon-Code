//! Model-selector state machine ([`ModelSelectorState`]).
//!
//! Builds a picker from model option rows supplied by the caller, tracks the
//! highlighted row, and reports the selected model on confirm. If the initial
//! model is a custom value that is not already present in the supplied rows, it
//! is prepended as a preserved current-model row so it can round-trip safely.
//! When no initial model is provided, the selector falls back to `sonnet`.
//!
//! On cancel, the state reports whether the caller should run its cancel
//! handler or complete with no selected model.

/// One option in the model picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    /// Stable identifier (the value handed back on confirm).
    pub value: String,
    /// Display label.
    pub label: String,
    /// One-line description shown under the label.
    pub description: String,
}

/// The full picker option list, with a synthetic "current model" row
/// prepended if `initial_model` isn't already in the alias list.
pub fn build_model_options(base: &[ModelOption], initial_model: Option<&str>) -> Vec<ModelOption> {
    if let Some(initial) = initial_model {
        if !base.iter().any(|o| o.value == initial) {
            let mut out = Vec::with_capacity(base.len() + 1);
            out.push(ModelOption {
                value: initial.to_string(),
                label: initial.to_string(),
                description: "Current model (custom ID)".to_string(),
            });
            out.extend_from_slice(base);
            return out;
        }
    }
    base.to_vec()
}

/// The fallback default value when `initial_model` is `None`.
pub const DEFAULT_MODEL_VALUE: &str = "sonnet";

/// Compute the default highlighted value.
pub fn default_value(initial_model: Option<&str>) -> &str {
    initial_model.unwrap_or(DEFAULT_MODEL_VALUE)
}

/// One line of help text shown above the picker.
pub const MODEL_HELP_TEXT: &str = "Model determines the agent's reasoning capabilities and speed.";

/// Reducer state — just the currently-highlighted index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelectorState {
    /// Highlighted index into the option list.
    pub selected_index: usize,
    /// The full option list (computed once at construction).
    pub options: Vec<ModelOption>,
}

impl ModelSelectorState {
    /// Build a new state with the cursor on `initial_model` if
    /// present, else on `DEFAULT_MODEL_VALUE`, else on index 0.
    pub fn new(base: &[ModelOption], initial_model: Option<&str>) -> Self {
        let options = build_model_options(base, initial_model);
        let want = default_value(initial_model);
        let selected = options.iter().position(|o| o.value == want).unwrap_or(0);
        ModelSelectorState {
            selected_index: selected,
            options,
        }
    }

    /// Apply a navigation event.
    pub fn handle_event(self, event: ModelSelectorEvent) -> Self {
        let max = self.options.len().saturating_sub(1);
        match event {
            ModelSelectorEvent::Up => ModelSelectorState {
                selected_index: self.selected_index.saturating_sub(1),
                options: self.options,
            },
            ModelSelectorEvent::Down => ModelSelectorState {
                selected_index: (self.selected_index + 1).min(max),
                options: self.options,
            },
        }
    }

    /// The current option (if any).
    pub fn current(&self) -> Option<&ModelOption> {
        self.options.get(self.selected_index)
    }

    /// Confirm — returns the current value, or `None` if the option
    /// list is empty.
    pub fn confirm(&self) -> Option<String> {
        self.current().map(|o| o.value.clone())
    }

    /// Cancel reports whether the caller-provided cancel handler should run,
    /// or whether cancellation should complete with no selected model.
    pub fn cancel_action(has_cancel_handler: bool) -> CancelAction {
        if has_cancel_handler {
            CancelAction::CallOnCancel
        } else {
            CancelAction::CallOnCompleteWithNone
        }
    }
}

/// Events that drive [`ModelSelectorState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSelectorEvent {
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
}

/// What the consumer should do when the user cancels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelAction {
    /// Invoke the consumer-supplied cancel handler.
    CallOnCancel,
    /// Complete cancellation with no selected model because no cancel handler exists.
    CallOnCompleteWithNone,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(v: &str, l: &str, d: &str) -> ModelOption {
        ModelOption {
            value: v.into(),
            label: l.into(),
            description: d.into(),
        }
    }

    #[test]
    fn build_options_no_initial() {
        let base = vec![opt("sonnet", "Sonnet", "fast")];
        let out = build_model_options(&base, None);
        assert_eq!(out, base);
    }

    #[test]
    fn build_options_initial_already_in_base() {
        let base = vec![opt("sonnet", "Sonnet", "fast")];
        let out = build_model_options(&base, Some("sonnet"));
        assert_eq!(out, base);
    }

    #[test]
    fn build_options_initial_not_in_base_prepends_synthetic() {
        let base = vec![opt("sonnet", "Sonnet", "fast")];
        let out = build_model_options(&base, Some("claude-opus-4-5"));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].value, "claude-opus-4-5");
        assert_eq!(out[0].label, "claude-opus-4-5");
        assert_eq!(out[0].description, "Current model (custom ID)");
        assert_eq!(out[1].value, "sonnet");
    }

    #[test]
    fn default_value_with_initial() {
        assert_eq!(default_value(Some("opus")), "opus");
    }

    #[test]
    fn default_value_without_initial() {
        assert_eq!(default_value(None), "sonnet");
    }

    #[test]
    fn state_new_uses_initial_model() {
        let base = vec![opt("sonnet", "Sonnet", "x"), opt("opus", "Opus", "x")];
        let s = ModelSelectorState::new(&base, Some("opus"));
        assert_eq!(s.current().unwrap().value, "opus");
    }

    #[test]
    fn state_new_falls_back_to_sonnet() {
        let base = vec![opt("sonnet", "Sonnet", "x"), opt("opus", "Opus", "x")];
        let s = ModelSelectorState::new(&base, None);
        assert_eq!(s.current().unwrap().value, "sonnet");
    }

    #[test]
    fn state_new_unknown_initial_prepends_and_selects_it() {
        let base = vec![opt("sonnet", "Sonnet", "x")];
        let s = ModelSelectorState::new(&base, Some("custom-id"));
        assert_eq!(s.current().unwrap().value, "custom-id");
    }

    #[test]
    fn down_advances_and_saturates() {
        let base = vec![opt("sonnet", "Sonnet", "x"), opt("opus", "Opus", "x")];
        let s = ModelSelectorState::new(&base, None);
        let s = s.handle_event(ModelSelectorEvent::Down);
        assert_eq!(s.current().unwrap().value, "opus");
        let s = s.handle_event(ModelSelectorEvent::Down);
        // Saturated.
        assert_eq!(s.current().unwrap().value, "opus");
    }

    #[test]
    fn up_at_top_saturates() {
        let base = vec![opt("sonnet", "Sonnet", "x")];
        let s = ModelSelectorState::new(&base, None);
        let s = s.handle_event(ModelSelectorEvent::Up);
        assert_eq!(s.selected_index, 0);
    }

    #[test]
    fn confirm_returns_current_value() {
        let base = vec![opt("sonnet", "Sonnet", "x"), opt("opus", "Opus", "x")];
        let s = ModelSelectorState::new(&base, Some("opus"));
        assert_eq!(s.confirm(), Some("opus".to_string()));
    }

    #[test]
    fn confirm_empty_list_is_none() {
        let s = ModelSelectorState::new(&[], None);
        assert_eq!(s.confirm(), None);
    }

    #[test]
    fn cancel_action_with_handler() {
        assert_eq!(
            ModelSelectorState::cancel_action(true),
            CancelAction::CallOnCancel
        );
    }

    #[test]
    fn cancel_action_without_handler() {
        assert_eq!(
            ModelSelectorState::cancel_action(false),
            CancelAction::CallOnCompleteWithNone
        );
    }

    #[test]
    fn help_text_pinned() {
        assert!(MODEL_HELP_TEXT.contains("reasoning capabilities and speed"));
    }
}
