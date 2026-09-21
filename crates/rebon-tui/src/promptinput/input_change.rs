//! Decision tree for a change to the prompt input's value.
//!
//! The side effects (closing help, dismissing the stash hint, buffer mutation,
//! speculation cancellation, footer-selection clearing) remain caller-owned.
//! This module only computes which of those effects should happen and what the
//! next input/mode payload should be.

use crate::promptinput::input_modes::{get_mode_from_input, get_value_from_input, HistoryMode};

/// Inputs to the input-change decision tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputChangeInput {
    /// Incoming value from the text input.
    pub next_value: String,
    /// Previous input value.
    pub current_input: String,
    /// Current cursor offset.
    pub cursor_offset: usize,
}

/// Result of handling an input change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputChangePlan {
    /// Special `?` shortcut toggles help and skips normal input handling.
    ToggleHelp,
    /// Normal input-change path.
    Apply(NormalizedInputChange),
}

/// Plain-data version of the side effects an input change triggers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedInputChange {
    /// Help is closed on every non-`?` input change.
    pub close_help: bool,
    /// Stash hint should be dismissed.
    pub dismiss_stash_hint: bool,
    /// Prompt suggestion should be aborted.
    pub abort_prompt_suggestion: bool,
    /// Speculation should be aborted.
    pub abort_speculation: bool,
    /// Optional prompt mode change.
    pub next_mode: Option<HistoryMode>,
    /// Whether the previous input should be pushed into undo history.
    pub push_previous_to_buffer: bool,
    /// Whether the caller should clear footer selection.
    pub clear_footer_selection: bool,
    /// Optional replacement input payload. `None` means keep the current input.
    pub replace_input_with: Option<String>,
    /// Optional cursor reset.
    pub next_cursor_offset: Option<usize>,
}

/// Plan an input change: `?` toggles help; a mode character typed at the
/// start switches mode (stripping it from a pasted value); otherwise the
/// value is applied with tabs expanded.
pub fn plan_input_change(input: &InputChangeInput) -> InputChangePlan {
    if input.next_value == "?" {
        return InputChangePlan::ToggleHelp;
    }

    let is_single_char_insertion = input.next_value.len() == input.current_input.len() + 1;
    let inserted_at_start = input.cursor_offset == 0;
    let mode = get_mode_from_input(&input.next_value);
    if inserted_at_start && mode != HistoryMode::Prompt {
        if is_single_char_insertion {
            return InputChangePlan::Apply(NormalizedInputChange {
                close_help: true,
                dismiss_stash_hint: true,
                abort_prompt_suggestion: true,
                abort_speculation: true,
                next_mode: Some(mode),
                push_previous_to_buffer: false,
                clear_footer_selection: false,
                replace_input_with: None,
                next_cursor_offset: None,
            });
        }

        if input.current_input.is_empty() {
            let value_without_mode = get_value_from_input(&input.next_value).replace('\t', "    ");
            return InputChangePlan::Apply(NormalizedInputChange {
                close_help: true,
                dismiss_stash_hint: true,
                abort_prompt_suggestion: true,
                abort_speculation: true,
                next_mode: Some(mode),
                push_previous_to_buffer: true,
                clear_footer_selection: false,
                replace_input_with: Some(value_without_mode.clone()),
                next_cursor_offset: Some(value_without_mode.len()),
            });
        }
    }

    let processed_value = input.next_value.replace('\t', "    ");
    InputChangePlan::Apply(NormalizedInputChange {
        close_help: true,
        dismiss_stash_hint: true,
        abort_prompt_suggestion: true,
        abort_speculation: true,
        next_mode: None,
        push_previous_to_buffer: input.current_input != processed_value,
        clear_footer_selection: true,
        replace_input_with: Some(processed_value),
        next_cursor_offset: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> InputChangeInput {
        InputChangeInput {
            next_value: String::from("hello"),
            current_input: String::from("hell"),
            cursor_offset: 4,
        }
    }

    #[test]
    fn question_mark_toggles_help_and_short_circuits() {
        let mut input = base();
        input.next_value = String::from("?");
        assert_eq!(plan_input_change(&input), InputChangePlan::ToggleHelp);
    }

    #[test]
    fn single_char_mode_prefix_at_start_switches_mode_only() {
        let input = InputChangeInput {
            next_value: String::from("!"),
            current_input: String::new(),
            cursor_offset: 0,
        };
        let InputChangePlan::Apply(plan) = plan_input_change(&input) else {
            panic!("expected normal plan");
        };
        assert_eq!(plan.next_mode, Some(HistoryMode::Bash));
        assert_eq!(plan.replace_input_with, None);
        assert!(!plan.push_previous_to_buffer);
        assert!(!plan.clear_footer_selection);
    }

    #[test]
    fn multi_char_prefix_into_empty_input_strips_mode_and_sets_cursor() {
        let input = InputChangeInput {
            next_value: String::from("!\tgit status"),
            current_input: String::new(),
            cursor_offset: 0,
        };
        let InputChangePlan::Apply(plan) = plan_input_change(&input) else {
            panic!("expected normal plan");
        };
        assert_eq!(plan.next_mode, Some(HistoryMode::Bash));
        assert_eq!(
            plan.replace_input_with,
            Some(String::from("    git status"))
        );
        assert_eq!(plan.next_cursor_offset, Some("    git status".len()));
        assert!(plan.push_previous_to_buffer);
    }

    #[test]
    fn normal_change_replaces_tabs_and_clears_footer_selection() {
        let input = InputChangeInput {
            next_value: String::from("a\tb"),
            current_input: String::from("ab"),
            cursor_offset: 2,
        };
        let InputChangePlan::Apply(plan) = plan_input_change(&input) else {
            panic!("expected normal plan");
        };
        assert_eq!(plan.next_mode, None);
        assert_eq!(plan.replace_input_with, Some(String::from("a    b")));
        assert!(plan.push_previous_to_buffer);
        assert!(plan.clear_footer_selection);
    }

    #[test]
    fn unchanged_processed_value_skips_buffer_push() {
        let input = InputChangeInput {
            next_value: String::from("hello"),
            current_input: String::from("hello"),
            cursor_offset: 5,
        };
        let InputChangePlan::Apply(plan) = plan_input_change(&input) else {
            panic!("expected normal plan");
        };
        assert!(!plan.push_previous_to_buffer);
        assert_eq!(plan.replace_input_with, Some(String::from("hello")));
    }
}
