//! Input-option compound widget logic.
//!
//! Most of the input-option widget is UI state plumbing (cursor
//! offset, image-paste lifecycle, attachment rendering, external
//! editor invocation). The pure logic lives here:
//!
//! 1. **`image_attachments` filter** — keeps only the pasted entries
//!    that are images, and counts them for the image-selection enter
//!    key.
//! 2. **`show_label`** — `show_label_prop || option.input.show_label_with_value`,
//!    a pure flag OR.
//! 3. **Submit gating** — a non-blank value, an image attachment, or
//!    an input option whose behaviour is [`InputBehaviour::EmptySubmits`]
//!    lets the submit through. On true → [`SubmitDecision::Change`];
//!    on false → [`SubmitDecision::Cancel`].
//!
//! The gating rule is shared by every layout branch, so pinning it
//! here keeps its test surface in one place.

use crate::option::{InputBehaviour, OptionId, OptionWithDescription};

/// Pre-resolved event from a select-input-option's text input. The
/// consumer routes the actual input callbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputOptionEvent {
    /// User typed into the field; reducer should update the value.
    InputChanged(String),
    /// User pressed Enter on the input; reducer should compute a
    /// [`SubmitDecision`].
    Submit(String),
    /// User pressed Escape on the input; reducer should leave the
    /// input.
    Exit,
    /// User pressed Tab on the input; reducer should toggle input
    /// mode.
    ToggleInputMode,
    /// Image-paste event from the clipboard.
    ImagePaste {
        /// Base-64 image data.
        base64: String,
        /// Media type (e.g. `image/png`).
        media_type: Option<String>,
    },
}

/// Display row for a select-input-option: what a surface needs to
/// paint one input row.
#[derive(Debug, Clone)]
pub struct InputOptionRow<T: OptionId> {
    /// The option payload.
    pub option: OptionWithDescription<T>,
    /// Whether the option is currently focused.
    pub is_focused: bool,
    /// Whether the option is currently selected.
    pub is_selected: bool,
    /// Whether the down arrow indicator should show.
    pub should_show_down_arrow: bool,
    /// Whether the up arrow indicator should show.
    pub should_show_up_arrow: bool,
    /// 1-based display index.
    pub index: usize,
    /// Max width of the index column.
    pub max_index_width: usize,
    /// Current input value.
    pub input_value: String,
    /// Resolved `show_label` (see [`resolve_show_label`]).
    pub show_label: bool,
}

/// Decision returned by [`submit_decision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitDecision {
    /// The submit goes through for an edit (the value is non-empty,
    /// has an image attachment, or the option allows empty submits).
    Change,
    /// Empty submit and no allow-empty flag.
    Cancel,
}

/// Compute the submit decision for an input option: a non-blank
/// value, an image attachment, or an option whose behaviour is
/// [`InputBehaviour::EmptySubmits`] lets the submit through.
pub fn submit_decision<T: OptionId>(
    option: &OptionWithDescription<T>,
    typed_value: &str,
    has_image_attachments: bool,
) -> SubmitDecision {
    let trimmed_non_empty = !typed_value.trim().is_empty();
    let allow_empty = option
        .input
        .as_ref()
        .map(|i| matches!(i.behaviour, InputBehaviour::EmptySubmits))
        .unwrap_or(false);
    if trimmed_non_empty || has_image_attachments || allow_empty {
        SubmitDecision::Change
    } else {
        SubmitDecision::Cancel
    }
}

/// Resolve `show_label`: the caller's flag, or the input option's
/// `show_label_with_value`.
pub fn resolve_show_label<T: OptionId>(
    show_label_prop: bool,
    option: &OptionWithDescription<T>,
) -> bool {
    if show_label_prop {
        return true;
    }
    option
        .input
        .as_ref()
        .map(|i| i.show_label_with_value)
        .unwrap_or(false)
}

/// Count image attachments — the number of `true` flags in the
/// consumer-supplied iterator of "is image" predicates.
pub fn image_attachments_count<I: IntoIterator<Item = bool>>(predicate: I) -> usize {
    predicate.into_iter().filter(|b| *b).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_option_with_behaviour(
        behaviour: InputBehaviour,
    ) -> OptionWithDescription<&'static str> {
        let mut o = OptionWithDescription::input("label", "v");
        o.input.as_mut().unwrap().behaviour = behaviour;
        o
    }

    #[test]
    fn submit_decision_change_when_value_non_empty() {
        let opt = OptionWithDescription::input("l", "v");
        assert_eq!(
            submit_decision(&opt, "hello", false),
            SubmitDecision::Change
        );
    }

    #[test]
    fn submit_decision_cancel_when_empty_default_behaviour() {
        let opt = OptionWithDescription::input("l", "v");
        assert_eq!(submit_decision(&opt, "", false), SubmitDecision::Cancel);
    }

    #[test]
    fn submit_decision_cancel_when_only_whitespace_default_behaviour() {
        let opt = OptionWithDescription::input("l", "v");
        assert_eq!(submit_decision(&opt, "   ", false), SubmitDecision::Cancel);
    }

    #[test]
    fn submit_decision_change_when_image_attachments_present() {
        let opt = OptionWithDescription::input("l", "v");
        assert_eq!(submit_decision(&opt, "", true), SubmitDecision::Change);
    }

    #[test]
    fn submit_decision_change_when_allow_empty_submit_to_cancel() {
        let opt = input_option_with_behaviour(InputBehaviour::EmptySubmits);
        assert_eq!(submit_decision(&opt, "", false), SubmitDecision::Change);
    }

    #[test]
    fn submit_decision_change_when_only_whitespace_with_allow_empty() {
        let opt = input_option_with_behaviour(InputBehaviour::EmptySubmits);
        assert_eq!(submit_decision(&opt, "   ", false), SubmitDecision::Change);
    }

    #[test]
    fn resolve_show_label_prop_takes_priority() {
        let opt = OptionWithDescription::input("l", "v");
        assert!(resolve_show_label(true, &opt));
    }

    #[test]
    fn resolve_show_label_falls_back_to_option_flag() {
        let mut opt = OptionWithDescription::input("l", "v");
        opt.input.as_mut().unwrap().show_label_with_value = true;
        assert!(resolve_show_label(false, &opt));
    }

    #[test]
    fn resolve_show_label_false_when_neither_set() {
        let opt = OptionWithDescription::input("l", "v");
        assert!(!resolve_show_label(false, &opt));
    }

    #[test]
    fn resolve_show_label_for_text_option_falls_back_to_prop() {
        let opt = OptionWithDescription::text("l", "v");
        assert!(resolve_show_label(true, &opt));
        assert!(!resolve_show_label(false, &opt));
    }

    #[test]
    fn image_attachments_count_basic() {
        assert_eq!(image_attachments_count([true, false, true, true]), 3);
        assert_eq!(image_attachments_count([false, false]), 0);
        assert_eq!(image_attachments_count(std::iter::empty()), 0);
    }
}
