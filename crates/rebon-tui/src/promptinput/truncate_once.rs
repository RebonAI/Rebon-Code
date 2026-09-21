//! One-shot input truncation.
//!
//! This is an explicit state machine so consumers can drive the
//! "truncate a long input only once until the prompt is cleared" behavior
//! without relying on hidden state.

use std::collections::BTreeMap;

use crate::promptinput::input_paste::{
    maybe_truncate_input, pasted_text_ref_num_lines, PastedContent, TruncateInputResult,
    TRUNCATION_THRESHOLD,
};

/// Truncation the caller should apply, emitted by [`TruncateOnceState::maybe_apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncateOnceEffect {
    /// Input text after truncation.
    pub new_input: String,
    /// Cursor offset, placed at the end of the truncated input.
    pub new_cursor_offset: usize,
    /// Pasted contents after truncation.
    pub new_pasted_contents: BTreeMap<u32, PastedContent>,
}

/// Explicit state for the one-shot truncation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TruncateOnceState {
    /// True once truncation has been applied for the current non-empty prompt.
    pub has_applied_truncation_to_input: bool,
}

impl TruncateOnceState {
    /// Truncate an over-long input, at most once until the prompt is cleared.
    pub fn maybe_apply(
        &mut self,
        input: &str,
        pasted_contents: &BTreeMap<u32, PastedContent>,
    ) -> Option<TruncateOnceEffect> {
        self.maybe_apply_with(input, pasted_contents, pasted_text_ref_num_lines)
    }

    /// Dependency-injected variant used by tests.
    pub fn maybe_apply_with(
        &mut self,
        input: &str,
        pasted_contents: &BTreeMap<u32, PastedContent>,
        count_lines: impl Fn(&str) -> usize,
    ) -> Option<TruncateOnceEffect> {
        if self.has_applied_truncation_to_input || input.len() <= TRUNCATION_THRESHOLD {
            return None;
        }

        let TruncateInputResult {
            new_input,
            new_pasted_contents,
        } = maybe_truncate_input(input, pasted_contents, count_lines);

        if new_input == input {
            return None;
        }

        self.has_applied_truncation_to_input = true;
        Some(TruncateOnceEffect {
            new_cursor_offset: new_input.len(),
            new_input,
            new_pasted_contents,
        })
    }

    /// Drop the once-flag when the new input is empty.
    pub fn on_input_change(&mut self, input: &str) {
        if input.is_empty() {
            self.has_applied_truncation_to_input = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_counter(text: &str) -> usize {
        text.matches('\n').count()
    }

    #[test]
    fn long_input_is_truncated_once() {
        let mut state = TruncateOnceState::default();
        let input = "x".repeat(TRUNCATION_THRESHOLD + 10);

        let effect = state
            .maybe_apply_with(&input, &BTreeMap::new(), line_counter)
            .expect("expected truncation");

        assert!(state.has_applied_truncation_to_input);
        assert!(effect.new_input.contains("Truncated text #1"));
        assert_eq!(effect.new_cursor_offset, effect.new_input.len());
    }

    #[test]
    fn repeated_apply_is_ignored_until_cleared() {
        let mut state = TruncateOnceState::default();
        let input = "x".repeat(TRUNCATION_THRESHOLD + 10);

        assert!(state
            .maybe_apply_with(&input, &BTreeMap::new(), line_counter)
            .is_some());
        assert!(state
            .maybe_apply_with(&input, &BTreeMap::new(), line_counter)
            .is_none());
    }

    #[test]
    fn short_input_is_a_no_op() {
        let mut state = TruncateOnceState::default();
        assert!(state
            .maybe_apply_with("hello", &BTreeMap::new(), line_counter)
            .is_none());
        assert!(!state.has_applied_truncation_to_input);
    }

    #[test]
    fn clearing_input_resets_the_once_flag() {
        let mut state = TruncateOnceState {
            has_applied_truncation_to_input: true,
        };

        state.on_input_change("still there");
        assert!(state.has_applied_truncation_to_input);

        state.on_input_change("");
        assert!(!state.has_applied_truncation_to_input);
    }
}
