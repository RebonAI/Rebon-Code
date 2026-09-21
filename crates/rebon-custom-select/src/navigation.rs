//! Keyboard navigation reducer.
//!
//! Action types are the four movement actions,
//! `focus-next-option` / `focus-previous-option` /
//! `focus-next-page` / `focus-previous-page`, plus `set-focus` and an
//! internal `reset` used when the option list changes.
//!
//! The reducer maintains:
//!
//! * `option_map` — the [`crate::option_map::OptionMap`].
//! * `visible_option_count` — derived from the configured count,
//!   clamped to `options.len()`.
//! * `focused_value` — the currently-highlighted value.
//! * `visible_from_index` / `visible_to_index` — the viewport window.
//!
//! Three derived projections are also pinned:
//!
//! * `validated_focused_value` — falls back to the first option if
//!   the focused value is no longer in the list. This closes the
//!   between-render gap when the options change but no reset has been
//!   dispatched yet.
//! * `is_in_input` — true iff the validated focused option's type is
//!   `Input`.
//! * `focused_index` — 1-based index for scroll position display
//!   (returns 0 if no option is focused).
//!
//! Wrap-around rules:
//!
//! * Pressing next at the last item wraps to the first AND resets
//!   the viewport to `{0, visible_option_count}`.
//! * Pressing previous at the first item wraps to the last AND
//!   resets the viewport to `{size - visible_option_count, size}`
//!   (clamped to 0).
//!
//! Page-up/down rules:
//!
//! * Move by `visible_option_count` items, clamped to
//!   `[0, size - 1]`.
//! * Page-down puts the new focus on the bottom row of the window;
//!   page-up puts it on the top row.
//!
//! Set-focus rules:
//!
//! * No-op if already focused on this value.
//! * No-op if value is not in the map.
//! * If item is in the current viewport, just update focus.
//! * Otherwise, scroll minimally — put the item at the nearest
//!   viewport edge.

use crate::option::{OptionId, OptionWithDescription};
use crate::option_map::OptionMap;

/// Default visible option count.
pub const DEFAULT_VISIBLE_OPTION_COUNT: usize = 5;

/// Properties used to construct a navigation reducer state.
#[derive(Debug, Clone)]
pub struct NavigationProps<T: OptionId> {
    /// Number of visible items. `None` matches the default of 5;
    /// `Some(0)` is treated literally and clamps to options.len().
    pub visible_option_count: Option<usize>,
    /// Input option list.
    pub options: Vec<OptionWithDescription<T>>,
    /// Initially focused value.
    pub initial_focus_value: Option<T>,
    /// Programmatic focus override.
    pub focus_value: Option<T>,
}

impl<T: OptionId> NavigationProps<T> {
    /// Convenience: build with all defaults.
    pub fn new(options: Vec<OptionWithDescription<T>>) -> Self {
        Self {
            visible_option_count: None,
            options,
            initial_focus_value: None,
            focus_value: None,
        }
    }
}

/// One row of the visible window: an option plus its index in the
/// full option list.
#[derive(Debug, Clone)]
pub struct VisibleOption<T: OptionId> {
    /// The option payload.
    pub option: OptionWithDescription<T>,
    /// Position in the input options array.
    pub index: usize,
}

/// Internal navigation state. Public so tests and consumers can
/// inspect every field; mutated only via [`NavigationState::dispatch`].
#[derive(Debug, Clone)]
pub struct NavigationState<T: OptionId> {
    option_map: OptionMap<T>,
    visible_option_count: usize,
    focused_value: Option<T>,
    visible_from_index: usize,
    visible_to_index: usize,
    options: Vec<OptionWithDescription<T>>,
}

/// The navigation reducer's action set.
#[derive(Debug, Clone)]
pub enum NavigationAction<T: OptionId> {
    /// `focus-next-option`
    FocusNextOption,
    /// `focus-previous-option`
    FocusPreviousOption,
    /// `focus-next-page`
    FocusNextPage,
    /// `focus-previous-page`
    FocusPreviousPage,
    /// `set-focus { value }`
    SetFocus(T),
    /// `reset` — replaces the entire state. Used when the options
    /// list changes.
    Reset {
        /// The new options to drive the navigation.
        options: Vec<OptionWithDescription<T>>,
        /// New visible-option-count override.
        visible_option_count: Option<usize>,
        /// Focus value to use during the reset; when `None` the
        /// current focus is kept.
        focus_value: Option<T>,
    },
}

impl<T: OptionId> NavigationState<T> {
    /// Build a new navigation state from [`NavigationProps`].
    pub fn new(props: NavigationProps<T>) -> Self {
        Self::create_default(
            props.visible_option_count,
            props.options,
            props
                .focus_value
                .clone()
                .or(props.initial_focus_value.clone()),
            None,
        )
    }

    fn create_default(
        custom_count: Option<usize>,
        options: Vec<OptionWithDescription<T>>,
        initial_focus_value: Option<T>,
        current_viewport: Option<(usize, usize)>,
    ) -> Self {
        let option_map = OptionMap::new(&options);
        let visible_option_count = match custom_count {
            Some(n) => n.min(options.len()),
            None => options.len(),
        };

        let focused_item = initial_focus_value.as_ref().and_then(|v| option_map.get(v));
        let focused_value = if let Some(item) = focused_item.as_ref() {
            Some(item.value.clone())
        } else {
            option_map.first().map(|f| f.value)
        };

        let mut visible_from_index = 0usize;
        let mut visible_to_index = visible_option_count;

        if let Some(item) = focused_item {
            let focused_index = item.index;

            match current_viewport {
                Some((cur_from, cur_to)) => {
                    if focused_index >= cur_from && focused_index < cur_to {
                        // Already visible: preserve viewport.
                        visible_from_index = cur_from;
                        visible_to_index = option_map.size().min(cur_to);
                    } else if focused_index < cur_from {
                        visible_from_index = focused_index;
                        visible_to_index = option_map
                            .size()
                            .min(visible_from_index + visible_option_count);
                    } else {
                        visible_to_index = option_map.size().min(focused_index + 1);
                        visible_from_index = visible_to_index.saturating_sub(visible_option_count);
                    }
                }
                None => {
                    if focused_index >= visible_option_count {
                        visible_to_index = option_map.size().min(focused_index + 1);
                        visible_from_index = visible_to_index.saturating_sub(visible_option_count);
                    }
                }
            }

            // Clamp viewport bounds. `visible_from_index` is pulled
            // down to at most `size - 1`, and `visible_to_index` is
            // never allowed to shrink below the visible count.
            let cap = option_map.size().saturating_sub(1);
            visible_from_index = visible_from_index.min(cap);
            visible_to_index = option_map
                .size()
                .min(visible_option_count.max(visible_to_index));
        }

        Self {
            option_map,
            visible_option_count,
            focused_value,
            visible_from_index,
            visible_to_index,
            options,
        }
    }

    /// Apply an action to the state. Returns the new state.
    pub fn dispatch(self, action: NavigationAction<T>) -> Self {
        match action {
            NavigationAction::FocusNextOption => self.focus_next(),
            NavigationAction::FocusPreviousOption => self.focus_previous(),
            NavigationAction::FocusNextPage => self.focus_next_page(),
            NavigationAction::FocusPreviousPage => self.focus_previous_page(),
            NavigationAction::SetFocus(value) => self.set_focus(value),
            NavigationAction::Reset {
                options,
                visible_option_count,
                focus_value,
            } => {
                let viewport = (self.visible_from_index, self.visible_to_index);
                Self::create_default(
                    visible_option_count,
                    options,
                    focus_value.or(self.focused_value),
                    Some(viewport),
                )
            }
        }
    }

    fn focus_next(self) -> Self {
        let focused = match self.focused_value.clone() {
            Some(v) => v,
            None => return self,
        };
        let item = match self.option_map.get(&focused) {
            Some(i) => i,
            None => return self,
        };
        let next = self
            .option_map
            .next_of(&focused)
            .or_else(|| self.option_map.first());
        let next = match next {
            Some(n) => n,
            None => return self,
        };

        // Wrap to first.
        if self.option_map.next_of(&focused).is_none() {
            // Confirm wrap target is first.
            if let Some(first) = self.option_map.first() {
                if first.index == next.index {
                    return Self {
                        focused_value: Some(next.value),
                        visible_from_index: 0,
                        visible_to_index: self.visible_option_count,
                        ..self
                    };
                }
            }
        }

        let _ = item; // index unused after the wrap branch
        let needs_to_scroll = next.index >= self.visible_to_index;
        if !needs_to_scroll {
            return Self {
                focused_value: Some(next.value),
                ..self
            };
        }
        let next_visible_to = self.option_map.size().min(self.visible_to_index + 1);
        let next_visible_from = next_visible_to.saturating_sub(self.visible_option_count);
        Self {
            focused_value: Some(next.value),
            visible_from_index: next_visible_from,
            visible_to_index: next_visible_to,
            ..self
        }
    }

    fn focus_previous(self) -> Self {
        let focused = match self.focused_value.clone() {
            Some(v) => v,
            None => return self,
        };
        let item = match self.option_map.get(&focused) {
            Some(i) => i,
            None => return self,
        };
        let previous = self
            .option_map
            .previous_of(&focused)
            .or_else(|| self.option_map.last());
        let previous = match previous {
            Some(p) => p,
            None => return self,
        };

        // Wrap to last.
        if self.option_map.previous_of(&focused).is_none() {
            if let Some(last) = self.option_map.last() {
                if last.index == previous.index {
                    let next_visible_to = self.option_map.size();
                    let next_visible_from =
                        next_visible_to.saturating_sub(self.visible_option_count);
                    return Self {
                        focused_value: Some(previous.value),
                        visible_from_index: next_visible_from,
                        visible_to_index: next_visible_to,
                        ..self
                    };
                }
            }
        }

        let _ = item;
        let needs_to_scroll = previous.index <= self.visible_from_index;
        if !needs_to_scroll {
            return Self {
                focused_value: Some(previous.value),
                ..self
            };
        }
        let next_visible_from = self.visible_from_index.saturating_sub(1);
        let next_visible_to = next_visible_from + self.visible_option_count;
        Self {
            focused_value: Some(previous.value),
            visible_from_index: next_visible_from,
            visible_to_index: next_visible_to,
            ..self
        }
    }

    fn focus_next_page(self) -> Self {
        let focused = match self.focused_value.clone() {
            Some(v) => v,
            None => return self,
        };
        let item = match self.option_map.get(&focused) {
            Some(i) => i,
            None => return self,
        };
        let target_index = self
            .option_map
            .size()
            .saturating_sub(1)
            .min(item.index + self.visible_option_count);
        let target = match self.option_map.at(target_index) {
            Some(t) => t,
            None => return self,
        };
        let next_visible_to = self.option_map.size().min(target.index + 1);
        let next_visible_from = next_visible_to.saturating_sub(self.visible_option_count);
        Self {
            focused_value: Some(target.value),
            visible_from_index: next_visible_from,
            visible_to_index: next_visible_to,
            ..self
        }
    }

    fn focus_previous_page(self) -> Self {
        let focused = match self.focused_value.clone() {
            Some(v) => v,
            None => return self,
        };
        let item = match self.option_map.get(&focused) {
            Some(i) => i,
            None => return self,
        };
        let target_index = item.index.saturating_sub(self.visible_option_count);
        let target = match self.option_map.at(target_index) {
            Some(t) => t,
            None => return self,
        };
        let next_visible_from = target.index;
        let next_visible_to = self
            .option_map
            .size()
            .min(next_visible_from + self.visible_option_count);
        Self {
            focused_value: Some(target.value),
            visible_from_index: next_visible_from,
            visible_to_index: next_visible_to,
            ..self
        }
    }

    fn set_focus(self, value: T) -> Self {
        if self.focused_value.as_ref() == Some(&value) {
            return self;
        }
        let item = match self.option_map.get(&value) {
            Some(i) => i,
            None => return self,
        };
        if item.index >= self.visible_from_index && item.index < self.visible_to_index {
            return Self {
                focused_value: Some(value),
                ..self
            };
        }
        let (next_from, next_to) = if item.index < self.visible_from_index {
            let from = item.index;
            let to = self.option_map.size().min(from + self.visible_option_count);
            (from, to)
        } else {
            let to = self.option_map.size().min(item.index + 1);
            let from = to.saturating_sub(self.visible_option_count);
            (from, to)
        };
        Self {
            focused_value: Some(value),
            visible_from_index: next_from,
            visible_to_index: next_to,
            ..self
        }
    }

    /// Borrow the underlying option list (post-reset).
    pub fn options(&self) -> &[OptionWithDescription<T>] {
        &self.options
    }

    /// Borrow the option map.
    pub fn option_map(&self) -> &OptionMap<T> {
        &self.option_map
    }

    /// Visible option count (clamped to options.len()).
    pub fn visible_option_count(&self) -> usize {
        self.visible_option_count
    }

    /// Raw focused value (may be stale; use [`Self::validated_focused_value`]
    /// for the displayed value).
    pub fn raw_focused_value(&self) -> Option<&T> {
        self.focused_value.as_ref()
    }

    /// Inclusive lower bound of the visible window.
    pub fn visible_from_index(&self) -> usize {
        self.visible_from_index
    }

    /// Exclusive upper bound of the visible window.
    pub fn visible_to_index(&self) -> usize {
        self.visible_to_index
    }

    /// Validated focused value. Falls back to the first option if
    /// the raw value is no longer in the list.
    pub fn validated_focused_value(&self) -> Option<T> {
        let raw = self.focused_value.as_ref()?;
        if self.options.iter().any(|o| o.value() == raw) {
            Some(raw.clone())
        } else {
            self.options.first().map(|o| o.value().clone())
        }
    }

    /// Whether the validated focused option is an input-type option.
    pub fn is_in_input(&self) -> bool {
        match self.validated_focused_value() {
            Some(v) => self
                .options
                .iter()
                .find(|o| o.value() == &v)
                .map(|o| o.is_input())
                .unwrap_or(false),
            None => false,
        }
    }

    /// 1-based focused index for scroll-position display. Returns 0
    /// if no option is focused.
    pub fn focused_index(&self) -> usize {
        let v = match self.validated_focused_value() {
            Some(v) => v,
            None => return 0,
        };
        match self.options.iter().position(|o| o.value() == &v) {
            Some(i) => i + 1,
            None => 0,
        }
    }

    /// Visible options window.
    pub fn visible_options(&self) -> Vec<VisibleOption<T>> {
        self.options
            .iter()
            .enumerate()
            .map(|(index, option)| VisibleOption {
                option: option.clone(),
                index,
            })
            .skip(self.visible_from_index)
            .take(
                self.visible_to_index
                    .saturating_sub(self.visible_from_index),
            )
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(values: &[&'static str]) -> Vec<OptionWithDescription<&'static str>> {
        values
            .iter()
            .map(|v| OptionWithDescription::text(*v, *v))
            .collect()
    }

    fn make(visible: usize, values: &[&'static str]) -> NavigationState<&'static str> {
        NavigationState::new(NavigationProps {
            visible_option_count: Some(visible),
            options: opts(values),
            initial_focus_value: None,
            focus_value: None,
        })
    }

    #[test]
    fn empty_options_no_focus() {
        let state = make(5, &[]);
        assert_eq!(state.raw_focused_value(), None);
        assert_eq!(state.focused_index(), 0);
        assert!(!state.is_in_input());
        assert_eq!(state.visible_options().len(), 0);
    }

    #[test]
    fn defaults_focus_first_option() {
        let state = make(3, &["a", "b", "c", "d"]);
        assert_eq!(state.raw_focused_value(), Some(&"a"));
        assert_eq!(state.visible_from_index(), 0);
        assert_eq!(state.visible_to_index(), 3);
        assert_eq!(state.focused_index(), 1);
    }

    #[test]
    fn focus_next_within_window() {
        let state = make(3, &["a", "b", "c", "d", "e"]);
        let state = state.dispatch(NavigationAction::FocusNextOption);
        assert_eq!(state.raw_focused_value(), Some(&"b"));
        assert_eq!(state.visible_from_index(), 0);
        assert_eq!(state.visible_to_index(), 3);
    }

    #[test]
    fn focus_next_scrolls_window_down_by_one() {
        let mut state = make(3, &["a", "b", "c", "d", "e"]);
        for _ in 0..3 {
            state = state.dispatch(NavigationAction::FocusNextOption);
        }
        // After 3 nexts, focused on "d", window must include it.
        assert_eq!(state.raw_focused_value(), Some(&"d"));
        assert_eq!(state.visible_from_index(), 1);
        assert_eq!(state.visible_to_index(), 4);
    }

    #[test]
    fn focus_next_at_last_wraps_to_first() {
        let mut state = make(3, &["a", "b", "c", "d", "e"]);
        // Walk to last.
        for _ in 0..4 {
            state = state.dispatch(NavigationAction::FocusNextOption);
        }
        assert_eq!(state.raw_focused_value(), Some(&"e"));
        // Wrap.
        let state = state.dispatch(NavigationAction::FocusNextOption);
        assert_eq!(state.raw_focused_value(), Some(&"a"));
        assert_eq!(state.visible_from_index(), 0);
        assert_eq!(state.visible_to_index(), 3);
    }

    #[test]
    fn focus_previous_at_first_wraps_to_last() {
        let state = make(3, &["a", "b", "c", "d", "e"]);
        let state = state.dispatch(NavigationAction::FocusPreviousOption);
        assert_eq!(state.raw_focused_value(), Some(&"e"));
        // Window should be the tail.
        assert_eq!(state.visible_from_index(), 2);
        assert_eq!(state.visible_to_index(), 5);
    }

    #[test]
    fn focus_previous_within_window() {
        let mut state = make(3, &["a", "b", "c"]);
        state = state.dispatch(NavigationAction::FocusNextOption);
        state = state.dispatch(NavigationAction::FocusNextOption);
        assert_eq!(state.raw_focused_value(), Some(&"c"));
        let state = state.dispatch(NavigationAction::FocusPreviousOption);
        assert_eq!(state.raw_focused_value(), Some(&"b"));
        assert_eq!(state.visible_from_index(), 0);
        assert_eq!(state.visible_to_index(), 3);
    }

    #[test]
    fn focus_previous_scrolls_up_by_one() {
        let mut state = make(3, &["a", "b", "c", "d", "e"]);
        // Move to "d" (window slides to 1..4).
        for _ in 0..3 {
            state = state.dispatch(NavigationAction::FocusNextOption);
        }
        assert_eq!(state.visible_from_index(), 1);
        // Now go up — to "c".
        let state = state.dispatch(NavigationAction::FocusPreviousOption);
        assert_eq!(state.raw_focused_value(), Some(&"c"));
        assert_eq!(state.visible_from_index(), 1);
    }

    #[test]
    fn page_down_jumps_full_window() {
        let state = make(3, &["a", "b", "c", "d", "e", "f", "g"]);
        let state = state.dispatch(NavigationAction::FocusNextPage);
        // From "a"(0), jump 3 → "d"(3).
        assert_eq!(state.raw_focused_value(), Some(&"d"));
    }

    #[test]
    fn page_down_clamps_to_last() {
        let state = make(3, &["a", "b", "c", "d"]);
        let state = state.dispatch(NavigationAction::FocusNextPage);
        // From 0, jump 3 → 3 → "d".
        assert_eq!(state.raw_focused_value(), Some(&"d"));
        let state = state.dispatch(NavigationAction::FocusNextPage);
        // From 3, jump 3 → 6 → clamped to 3 → still "d".
        assert_eq!(state.raw_focused_value(), Some(&"d"));
    }

    #[test]
    fn page_up_jumps_full_window() {
        let mut state = make(3, &["a", "b", "c", "d", "e", "f", "g"]);
        for _ in 0..5 {
            state = state.dispatch(NavigationAction::FocusNextOption);
        }
        // Now at "f" (index 5).
        assert_eq!(state.raw_focused_value(), Some(&"f"));
        let state = state.dispatch(NavigationAction::FocusPreviousPage);
        // 5 - 3 = 2 → "c".
        assert_eq!(state.raw_focused_value(), Some(&"c"));
    }

    #[test]
    fn page_up_clamps_to_first() {
        let state = make(3, &["a", "b", "c", "d"]);
        let state = state.dispatch(NavigationAction::FocusPreviousPage);
        assert_eq!(state.raw_focused_value(), Some(&"a"));
    }

    #[test]
    fn set_focus_to_visible_option_keeps_window() {
        let state = make(3, &["a", "b", "c", "d", "e"]);
        let state = state.dispatch(NavigationAction::SetFocus("c"));
        assert_eq!(state.raw_focused_value(), Some(&"c"));
        assert_eq!(state.visible_from_index(), 0);
        assert_eq!(state.visible_to_index(), 3);
    }

    #[test]
    fn set_focus_to_below_window_scrolls_min() {
        let state = make(3, &["a", "b", "c", "d", "e"]);
        let state = state.dispatch(NavigationAction::SetFocus("e"));
        assert_eq!(state.raw_focused_value(), Some(&"e"));
        // 4 + 1 = 5 → 5; from = 5 - 3 = 2.
        assert_eq!(state.visible_from_index(), 2);
        assert_eq!(state.visible_to_index(), 5);
    }

    #[test]
    fn set_focus_to_above_window_scrolls_min() {
        let mut state = make(3, &["a", "b", "c", "d", "e"]);
        // Move down to make window {2,5}.
        for _ in 0..4 {
            state = state.dispatch(NavigationAction::FocusNextOption);
        }
        assert_eq!(state.visible_from_index(), 2);
        let state = state.dispatch(NavigationAction::SetFocus("a"));
        assert_eq!(state.raw_focused_value(), Some(&"a"));
        assert_eq!(state.visible_from_index(), 0);
        assert_eq!(state.visible_to_index(), 3);
    }

    #[test]
    fn set_focus_unknown_value_is_noop() {
        let state = make(3, &["a", "b", "c"]);
        let state = state.dispatch(NavigationAction::SetFocus("zzz"));
        assert_eq!(state.raw_focused_value(), Some(&"a"));
        assert_eq!(state.visible_from_index(), 0);
    }

    #[test]
    fn set_focus_already_focused_is_noop() {
        let state = make(3, &["a", "b", "c"]);
        let before = state.clone();
        let state = state.dispatch(NavigationAction::SetFocus("a"));
        assert_eq!(state.raw_focused_value(), before.raw_focused_value());
        assert_eq!(state.visible_from_index(), before.visible_from_index());
    }

    #[test]
    fn validated_focused_value_falls_back_when_focus_stale() {
        // Build state, then mutate options without dispatching reset
        // (the between-render gap when options change).
        let mut state = make(3, &["a", "b", "c"]);
        state = state.dispatch(NavigationAction::SetFocus("c"));
        // Replace options.
        state.options = opts(&["x", "y"]);
        // Raw focus is still "c", but validated returns "x".
        assert_eq!(state.raw_focused_value(), Some(&"c"));
        assert_eq!(state.validated_focused_value(), Some("x"));
    }

    #[test]
    fn focused_index_returns_one_based() {
        let state = make(5, &["a", "b", "c"]);
        assert_eq!(state.focused_index(), 1);
        let state = state.dispatch(NavigationAction::FocusNextOption);
        assert_eq!(state.focused_index(), 2);
    }

    #[test]
    fn focused_index_zero_when_empty() {
        let state = make(5, &[]);
        assert_eq!(state.focused_index(), 0);
    }

    #[test]
    fn is_in_input_detects_input_option() {
        let mut options = opts(&["a", "b"]);
        options.push(OptionWithDescription::input("Type", "c"));
        let state = NavigationState::new(NavigationProps {
            visible_option_count: Some(3),
            options,
            initial_focus_value: Some("c"),
            focus_value: None,
        });
        assert!(state.is_in_input());
    }

    #[test]
    fn is_in_input_false_for_text_option() {
        let state = make(3, &["a", "b", "c"]);
        assert!(!state.is_in_input());
    }

    #[test]
    fn visible_options_window_size_correct() {
        let state = make(3, &["a", "b", "c", "d", "e"]);
        let v = state.visible_options();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].option.value(), &"a");
        assert_eq!(v[2].option.value(), &"c");
    }

    #[test]
    fn visible_count_clamps_to_options_len() {
        let state = make(10, &["a", "b"]);
        assert_eq!(state.visible_option_count(), 2);
    }

    #[test]
    fn initial_focus_value_outside_default_window_scrolls_to_show() {
        let state = NavigationState::new(NavigationProps {
            visible_option_count: Some(3),
            options: opts(&["a", "b", "c", "d", "e", "f"]),
            initial_focus_value: Some("f"),
            focus_value: None,
        });
        assert_eq!(state.raw_focused_value(), Some(&"f"));
        assert_eq!(state.visible_from_index(), 3);
        assert_eq!(state.visible_to_index(), 6);
    }

    #[test]
    fn focus_value_overrides_initial_focus_value() {
        let state = NavigationState::new(NavigationProps {
            visible_option_count: Some(3),
            options: opts(&["a", "b", "c"]),
            initial_focus_value: Some("a"),
            focus_value: Some("c"),
        });
        assert_eq!(state.raw_focused_value(), Some(&"c"));
    }

    #[test]
    fn reset_action_swaps_options_and_preserves_focus_when_possible() {
        let state = make(3, &["a", "b", "c"]);
        let state = state.dispatch(NavigationAction::SetFocus("b"));
        let state = state.dispatch(NavigationAction::Reset {
            options: opts(&["b", "c", "d"]),
            visible_option_count: Some(3),
            focus_value: None,
        });
        // "b" still exists, so focus is preserved.
        assert_eq!(state.raw_focused_value(), Some(&"b"));
    }

    #[test]
    fn reset_action_falls_back_to_first_when_focus_gone() {
        let state = make(3, &["a", "b", "c"]);
        let state = state.dispatch(NavigationAction::SetFocus("b"));
        let state = state.dispatch(NavigationAction::Reset {
            options: opts(&["x", "y", "z"]),
            visible_option_count: Some(3),
            focus_value: None,
        });
        assert_eq!(state.raw_focused_value(), Some(&"x"));
    }

    #[test]
    fn focus_next_then_pages_back_to_top_clamps_window() {
        let mut state = make(3, &["a", "b", "c", "d", "e", "f"]);
        for _ in 0..4 {
            state = state.dispatch(NavigationAction::FocusNextOption);
        }
        // At "e" (index 4), window {2,5}.
        let state = state.dispatch(NavigationAction::FocusPreviousPage);
        // 4 - 3 = 1 → "b". Window from 1, to = min(6, 1+3)=4.
        assert_eq!(state.raw_focused_value(), Some(&"b"));
        assert_eq!(state.visible_from_index(), 1);
        assert_eq!(state.visible_to_index(), 4);
    }

    #[test]
    fn single_option_navigation_no_op_on_next_or_previous() {
        let state = make(1, &["only"]);
        let state = state.dispatch(NavigationAction::FocusNextOption);
        // Only one option — wraps to itself, viewport stays {0,1}.
        assert_eq!(state.raw_focused_value(), Some(&"only"));
        assert_eq!(state.visible_from_index(), 0);
        assert_eq!(state.visible_to_index(), 1);
        let state = state.dispatch(NavigationAction::FocusPreviousOption);
        assert_eq!(state.raw_focused_value(), Some(&"only"));
    }
}
