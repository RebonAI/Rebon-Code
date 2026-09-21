//! Row projection — turns the visible option window into display rows.
//!
//! The layout (`compact`, `expanded`, or `compact-vertical`) is the
//! consumer's choice; every one of them walks the same visible window and
//! derives the same per-row flags:
//!
//! * whether an option is the first or last visible row (for the `▲`
//!   and `▼` overflow arrows);
//! * whether more options exist above or below the window;
//! * the 1-based display index;
//! * whether the option is focused (and not disabled) or selected;
//! * the row body — a highlight-sliced label, or an input value.
//!
//! The `highlight_text` slicing rule splits a label into the text
//! before the match, the match itself, and the text after it.
//!
//! The index-width math is `options_len.to_string().len()` (zero when
//! indexes are hidden). String-width padding for `compact-vertical` is
//! *not* computed here: the consumer injects its own width function so
//! this crate needs no unicode-width dependency.

use crate::navigation::NavigationState;
use crate::option::{OptionId, OptionType, OptionWithDescription};
use crate::select_state::SelectState;

/// Layout variant for the select widget: `compact`, `expanded`, or
/// `compact-vertical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectLayout {
    /// Default — one line per option.
    #[default]
    Compact,
    /// Multi-line per option, blank line between options.
    Expanded,
    /// Compact index formatting with descriptions below labels.
    CompactVertical,
}

/// Row variant — text or input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectRowKind {
    /// A normal text option.
    Text {
        /// Label, possibly highlight-sliced.
        label_segments: Vec<LabelSegment>,
    },
    /// An input-type option.
    Input {
        /// Current input value (or initial value if untouched).
        value: String,
    },
}

/// One piece of a label after `highlight_text` slicing — literal
/// text, or the matching slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelSegment {
    /// Plain literal text.
    Text(String),
    /// The matching highlight slice.
    Highlight(String),
}

/// One projected row of the select widget.
#[derive(Debug, Clone)]
pub struct SelectRow<T: OptionId> {
    /// The option payload.
    pub option: OptionWithDescription<T>,
    /// Position in the options list.
    pub option_index: usize,
    /// 1-based display index within the visible window.
    pub display_index: usize,
    /// Whether this row is the first visible (for the `▲` arrow).
    pub is_first_visible: bool,
    /// Whether this row is the last visible (for the `▼` arrow).
    pub is_last_visible: bool,
    /// Whether more options exist below (used with `is_last_visible`).
    pub are_more_below: bool,
    /// Whether more options exist above (used with `is_first_visible`).
    pub are_more_above: bool,
    /// Whether the option is currently focused (and `!is_disabled`).
    pub is_focused: bool,
    /// Whether the option is currently selected.
    pub is_selected: bool,
    /// Whether the option is disabled.
    pub is_disabled: bool,
    /// Row body.
    pub kind: SelectRowKind,
}

/// Compute the maximum index width for the option list: the number of
/// decimal digits in the option count.
pub fn compute_max_index_width(options_len: usize, hide_indexes: bool) -> usize {
    if hide_indexes || options_len == 0 {
        0
    } else {
        options_len.to_string().len()
    }
}

/// Apply the `highlight_text` slicing rule to a label.
pub fn slice_highlight(label: &str, highlight: Option<&str>) -> Vec<LabelSegment> {
    let Some(needle) = highlight else {
        return vec![LabelSegment::Text(label.to_string())];
    };
    if needle.is_empty() {
        return vec![LabelSegment::Text(label.to_string())];
    }
    if let Some(pos) = label.find(needle) {
        let before = &label[..pos];
        let needle_slice = &label[pos..pos + needle.len()];
        let after = &label[pos + needle.len()..];
        let mut out = Vec::with_capacity(3);
        if !before.is_empty() {
            out.push(LabelSegment::Text(before.to_string()));
        }
        out.push(LabelSegment::Highlight(needle_slice.to_string()));
        if !after.is_empty() {
            out.push(LabelSegment::Text(after.to_string()));
        }
        out
    } else {
        vec![LabelSegment::Text(label.to_string())]
    }
}

/// Project the visible window into a vector of [`SelectRow`]s.
pub fn project_select_rows<T: OptionId>(
    navigation: &NavigationState<T>,
    select_state: &SelectState<T>,
    is_disabled: bool,
    highlight_text: Option<&str>,
    input_values: &std::collections::HashMap<T, String>,
) -> Vec<SelectRow<T>> {
    let visible = navigation.visible_options();
    let visible_to = navigation.visible_to_index();
    let visible_from = navigation.visible_from_index();
    let total = navigation.options().len();
    let focused = navigation.validated_focused_value();
    let selected = select_state.value().cloned();
    visible
        .into_iter()
        .enumerate()
        .map(|(within, vo)| {
            let option = vo.option;
            let option_index = vo.index;
            let display_index = visible_from + within + 1;
            let is_first_visible = option_index == visible_from;
            let is_last_visible = option_index == visible_to.saturating_sub(1);
            let are_more_below = visible_to < total;
            let are_more_above = visible_from > 0;
            let is_focused = !is_disabled && focused.as_ref() == Some(option.value());
            let is_selected = selected.as_ref() == Some(option.value());
            let is_disabled_option = option.is_disabled();
            let kind = match option.r#type {
                OptionType::Input => {
                    let value = input_values
                        .get(option.value())
                        .cloned()
                        .or_else(|| option.input.as_ref().and_then(|i| i.initial_value.clone()))
                        .unwrap_or_default();
                    SelectRowKind::Input { value }
                }
                OptionType::Text => {
                    let label_segments = slice_highlight(option.label(), highlight_text);
                    SelectRowKind::Text { label_segments }
                }
            };
            SelectRow {
                option,
                option_index,
                display_index,
                is_first_visible,
                is_last_visible,
                are_more_below,
                are_more_above,
                is_focused,
                is_selected,
                is_disabled: is_disabled_option,
                kind,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigation::NavigationProps;
    use std::collections::HashMap;

    fn opts(values: &[&'static str]) -> Vec<OptionWithDescription<&'static str>> {
        values
            .iter()
            .map(|v| OptionWithDescription::text(*v, *v))
            .collect()
    }

    #[test]
    fn max_index_width_for_three_options_is_one() {
        assert_eq!(compute_max_index_width(3, false), 1);
    }

    #[test]
    fn max_index_width_for_ten_options_is_two() {
        assert_eq!(compute_max_index_width(10, false), 2);
    }

    #[test]
    fn max_index_width_zero_when_hide_indexes() {
        assert_eq!(compute_max_index_width(10, true), 0);
    }

    #[test]
    fn max_index_width_zero_for_empty() {
        assert_eq!(compute_max_index_width(0, false), 0);
    }

    #[test]
    fn slice_highlight_no_needle() {
        let segments = slice_highlight("hello world", None);
        assert_eq!(segments, vec![LabelSegment::Text("hello world".into())]);
    }

    #[test]
    fn slice_highlight_empty_needle() {
        let segments = slice_highlight("hello", Some(""));
        assert_eq!(segments, vec![LabelSegment::Text("hello".into())]);
    }

    #[test]
    fn slice_highlight_match_in_middle() {
        let segments = slice_highlight("hello world", Some("o w"));
        assert_eq!(
            segments,
            vec![
                LabelSegment::Text("hell".into()),
                LabelSegment::Highlight("o w".into()),
                LabelSegment::Text("orld".into()),
            ]
        );
    }

    #[test]
    fn slice_highlight_match_at_start() {
        let segments = slice_highlight("hello", Some("he"));
        assert_eq!(
            segments,
            vec![
                LabelSegment::Highlight("he".into()),
                LabelSegment::Text("llo".into()),
            ]
        );
    }

    #[test]
    fn slice_highlight_match_at_end() {
        let segments = slice_highlight("hello", Some("llo"));
        assert_eq!(
            segments,
            vec![
                LabelSegment::Text("he".into()),
                LabelSegment::Highlight("llo".into()),
            ]
        );
    }

    #[test]
    fn slice_highlight_no_match() {
        let segments = slice_highlight("hello", Some("xyz"));
        assert_eq!(segments, vec![LabelSegment::Text("hello".into())]);
    }

    #[test]
    fn project_rows_marks_first_and_last_visible() {
        let options = opts(&["a", "b", "c", "d", "e"]);
        let nav = NavigationState::new(NavigationProps {
            visible_option_count: Some(3),
            options,
            initial_focus_value: None,
            focus_value: None,
        });
        let select: SelectState<&'static str> = SelectState::new();
        let inputs: HashMap<&'static str, String> = HashMap::new();
        let rows = project_select_rows(&nav, &select, false, None, &inputs);
        assert_eq!(rows.len(), 3);
        assert!(rows[0].is_first_visible);
        assert!(!rows[0].is_last_visible);
        assert!(!rows[2].is_first_visible);
        assert!(rows[2].is_last_visible);
        assert!(rows[2].are_more_below);
        assert!(!rows[0].are_more_above);
    }

    #[test]
    fn project_rows_marks_focused() {
        let options = opts(&["a", "b"]);
        let nav = NavigationState::new(NavigationProps {
            visible_option_count: Some(5),
            options,
            initial_focus_value: None,
            focus_value: None,
        });
        let select: SelectState<&'static str> = SelectState::new();
        let inputs: HashMap<&'static str, String> = HashMap::new();
        let rows = project_select_rows(&nav, &select, false, None, &inputs);
        assert!(rows[0].is_focused);
        assert!(!rows[1].is_focused);
    }

    #[test]
    fn project_rows_with_disabled_renders_no_focus() {
        let options = opts(&["a", "b"]);
        let nav = NavigationState::new(NavigationProps {
            visible_option_count: Some(5),
            options,
            initial_focus_value: None,
            focus_value: None,
        });
        let select: SelectState<&'static str> = SelectState::new();
        let inputs: HashMap<&'static str, String> = HashMap::new();
        let rows = project_select_rows(&nav, &select, true, None, &inputs);
        assert!(!rows[0].is_focused);
    }

    #[test]
    fn project_rows_marks_selected() {
        let options = opts(&["a", "b"]);
        let nav = NavigationState::new(NavigationProps {
            visible_option_count: Some(5),
            options,
            initial_focus_value: None,
            focus_value: None,
        });
        let select = SelectState::with_default("b");
        let inputs: HashMap<&'static str, String> = HashMap::new();
        let rows = project_select_rows(&nav, &select, false, None, &inputs);
        assert!(!rows[0].is_selected);
        assert!(rows[1].is_selected);
    }

    #[test]
    fn project_rows_input_uses_input_values_map() {
        let mut options = opts(&[]);
        options.push(OptionWithDescription::input("type", "i"));
        let nav = NavigationState::new(NavigationProps {
            visible_option_count: Some(5),
            options,
            initial_focus_value: None,
            focus_value: None,
        });
        let select: SelectState<&'static str> = SelectState::new();
        let mut inputs = HashMap::new();
        inputs.insert("i", "current".to_string());
        let rows = project_select_rows(&nav, &select, false, None, &inputs);
        assert_eq!(rows.len(), 1);
        match &rows[0].kind {
            SelectRowKind::Input { value } => assert_eq!(value, "current"),
            _ => panic!("expected input row"),
        }
    }

    #[test]
    fn project_rows_input_falls_back_to_initial_value() {
        let mut options = opts(&[]);
        let mut input_opt = OptionWithDescription::input("type", "i");
        input_opt.input.as_mut().unwrap().initial_value = Some("seed".into());
        options.push(input_opt);
        let nav = NavigationState::new(NavigationProps {
            visible_option_count: Some(5),
            options,
            initial_focus_value: None,
            focus_value: None,
        });
        let select: SelectState<&'static str> = SelectState::new();
        let inputs: HashMap<&'static str, String> = HashMap::new();
        let rows = project_select_rows(&nav, &select, false, None, &inputs);
        match &rows[0].kind {
            SelectRowKind::Input { value } => assert_eq!(value, "seed"),
            _ => panic!("expected input row"),
        }
    }

    #[test]
    fn project_rows_highlight_segments_text_label() {
        let options = vec![OptionWithDescription::text("hello world", "h")];
        let nav = NavigationState::new(NavigationProps {
            visible_option_count: Some(5),
            options,
            initial_focus_value: None,
            focus_value: None,
        });
        let select: SelectState<&'static str> = SelectState::new();
        let inputs: HashMap<&'static str, String> = HashMap::new();
        let rows = project_select_rows(&nav, &select, false, Some("o w"), &inputs);
        match &rows[0].kind {
            SelectRowKind::Text { label_segments } => {
                assert_eq!(
                    label_segments,
                    &vec![
                        LabelSegment::Text("hell".into()),
                        LabelSegment::Highlight("o w".into()),
                        LabelSegment::Text("orld".into()),
                    ]
                );
            }
            _ => panic!("expected text row"),
        }
    }
}
