//! The `/history` prompt search: a filter over past prompts.
//!
//! A [`DialogModel`] over the [`crate::history_search`]
//! reducer. The prompts arrive as a snapshot at open time — the panel
//! filters what it was given rather than re-reading a store — so nothing
//! here does IO and the whole panel is portable to any surface that can
//! paint a [`SearchView`].

use rebon_customselect::{
    NavigationAction, NavigationProps, NavigationState, OptionWithDescription,
};

use crate::history_search::{self, HistoryItem, HistoryLoadState};
use crate::model::{
    DialogAction, DialogKey, DialogModel, DialogOutcome, KeyPress, SearchFooterSegment,
    SearchLayout, SearchRow, SearchView, ViewSpec,
};

/// Inner columns assumed before the host has reported a real frame.
const DEFAULT_COLS: u16 = 80;

/// Stable id used for action routing.
pub const DIALOG_ID: &str = "history-search";
/// Put the chosen prompt back in the composer. One value: its text.
pub const ACTION_APPLY: &str = "apply";

/// One past prompt, as the host collected it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryInput {
    /// The prompt text.
    pub display: String,
    /// Its age, already formatted and padded by the host.
    pub age: String,
    /// Its timestamp (epoch ms), which identifies the row.
    pub timestamp: u64,
}

/// Cloneable render + reducer state for the history-search dialog.
#[derive(Debug, Clone)]
pub struct HistorySearchDialogState {
    /// The typed filter.
    pub query: String,
    items: Vec<HistoryItem>,
    filtered: Vec<HistoryItem>,
    nav: Option<NavigationState<u64>>,
    /// Inner columns the host last reported. The layout maths needs a
    /// width and the first frame has not happened yet, so it starts at
    /// the width the panel is usually given.
    cols: u16,
}

impl HistorySearchDialogState {
    /// Open over `history` in store order (oldest first), with
    /// `initial_query` already typed.
    pub fn open(history: Vec<HistoryInput>, initial_query: String) -> Self {
        let items = build_items(history);
        let mut state = Self {
            query: initial_query,
            items,
            filtered: Vec::new(),
            nav: None,
            cols: DEFAULT_COLS,
        };
        state.refresh();
        state
    }

    fn refresh(&mut self) {
        self.filtered = history_search::filter_items(&self.items, &self.query);
        let focus = self.selected_item().map(|item| item.timestamp);
        if self.filtered.is_empty() {
            self.nav = None;
            return;
        }
        let options = self
            .filtered
            .iter()
            .map(|item| {
                OptionWithDescription::text(
                    history_search::build_row_label(item, 40),
                    item.timestamp,
                )
            })
            .collect();
        self.nav = Some(NavigationState::new(NavigationProps {
            visible_option_count: Some(history_search::PREVIEW_ROWS.max(4)),
            options,
            initial_focus_value: None,
            focus_value: focus,
        }));
    }

    fn selected_item(&self) -> Option<&HistoryItem> {
        let focused = self
            .nav
            .as_ref()
            .and_then(NavigationState::validated_focused_value)?;
        self.filtered.iter().find(|item| item.timestamp == focused)
    }
}

impl DialogModel for HistorySearchDialogState {
    crate::dialog_plumbing!();

    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn on_key(&mut self, press: KeyPress) -> DialogOutcome {
        match press.key {
            DialogKey::Escape => DialogOutcome::Close,
            DialogKey::Enter => self
                .selected_item()
                .map(|item| {
                    DialogOutcome::Action(DialogAction::closing(
                        DIALOG_ID,
                        ACTION_APPLY,
                        item.display.clone(),
                    ))
                })
                .unwrap_or(DialogOutcome::None),
            DialogKey::Up => {
                crate::model::dispatch_nav(&mut self.nav, NavigationAction::FocusPreviousOption);
                DialogOutcome::None
            }
            DialogKey::Down => {
                crate::model::dispatch_nav(&mut self.nav, NavigationAction::FocusNextOption);
                DialogOutcome::None
            }
            DialogKey::PageUp => {
                crate::model::dispatch_nav(&mut self.nav, NavigationAction::FocusPreviousPage);
                DialogOutcome::None
            }
            DialogKey::PageDown => {
                crate::model::dispatch_nav(&mut self.nav, NavigationAction::FocusNextPage);
                DialogOutcome::None
            }
            DialogKey::Backspace => {
                self.query.pop();
                self.refresh();
                DialogOutcome::None
            }
            DialogKey::Delete => {
                self.query.clear();
                self.refresh();
                DialogOutcome::None
            }
            // A control chord is a binding, not a character to type.
            DialogKey::Char { value, plain: true } => {
                self.query.push(value);
                self.refresh();
                DialogOutcome::None
            }
            _ => DialogOutcome::None,
        }
    }

    fn view(&self) -> ViewSpec {
        ViewSpec::Search(self.search_view())
    }

    fn note_viewport(&mut self, _rows: u16, cols: u16) {
        self.cols = cols;
    }
}

impl HistorySearchDialogState {
    /// Project the panel into the shape a surface paints.
    ///
    /// Every width comes from the reducer's own layout maths against the
    /// frame the host last reported, so the rows and the preview are
    /// built to the same widths the painter will place them at.
    fn search_view(&self) -> SearchView {
        let (list_width, row_width, preview_width) = history_search::compute_layout(self.cols);
        let focused = self
            .nav
            .as_ref()
            .and_then(NavigationState::validated_focused_value);
        let rows = self
            .nav
            .as_ref()
            .map(NavigationState::visible_options)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| {
                let timestamp = *row.option.value();
                let item = self
                    .filtered
                    .iter()
                    .find(|entry| entry.timestamp == timestamp)?;
                Some(SearchRow {
                    text: history_search::build_row_label(item, row_width),
                    focused: focused == Some(timestamp),
                })
            })
            .collect();
        let (preview, preview_more) = match self.selected_item() {
            Some(item) => history_search::build_preview(&item.display, preview_width),
            None => (Vec::new(), 0),
        };
        SearchView {
            title: format!(" {} ", history_search::TITLE),
            query: self.query.clone(),
            placeholder: history_search::PLACEHOLDER.to_string(),
            rows,
            empty_message: history_search::empty_message(HistoryLoadState::Loaded, &self.query)
                .to_string(),
            preview_header: None,
            preview,
            preview_tone: crate::model::RowTone::Dim,
            preview_empty_message: history_search::empty_message(
                HistoryLoadState::Loaded,
                &self.query,
            )
            .to_string(),
            preview_more,
            footer: vec![
                SearchFooterSegment {
                    text: " Enter ".into(),
                    emphasis: true,
                },
                SearchFooterSegment {
                    text: history_search::SELECT_ACTION.into(),
                    emphasis: false,
                },
                SearchFooterSegment {
                    text: " \u{b7} Esc ".into(),
                    emphasis: true,
                },
                SearchFooterSegment {
                    text: "close".into(),
                    emphasis: false,
                },
            ],
            layout: SearchLayout {
                list_width,
                preview_rows: history_search::PREVIEW_ROWS,
                preview_on_right: history_search::preview_on_right(self.cols),
                stacked_list_rows: None,
                body_min_rows: 4,
            },
        }
    }
}

fn build_items(history: Vec<HistoryInput>) -> Vec<HistoryItem> {
    // The store hands prompts back oldest first; the panel lists the
    // newest at the top.
    history
        .into_iter()
        .rev()
        .map(|entry| HistoryItem {
            lower: entry.display.to_lowercase(),
            first_line: entry.display.lines().next().unwrap_or("").to_string(),
            display: entry.display,
            age: entry.age,
            timestamp: entry.timestamp,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(display: &str, timestamp: u64) -> HistoryInput {
        HistoryInput {
            display: display.into(),
            age: "1m   ".into(),
            timestamp,
        }
    }

    #[test]
    fn open_sorts_newest_first() {
        let state = HistorySearchDialogState::open(
            vec![entry("older", 1), entry("newer", 2)],
            String::new(),
        );
        assert_eq!(state.filtered[0].display, "newer");
    }

    #[test]
    fn enter_returns_selected_value() {
        let mut state = HistorySearchDialogState::open(vec![entry("hello", 1)], String::from("he"));
        assert_eq!(
            state.on_key(DialogKey::Enter.into()),
            DialogOutcome::Action(DialogAction::closing(DIALOG_ID, ACTION_APPLY, "hello"))
        );
    }

    #[test]
    fn typing_filters_and_backspace_widens_again() {
        let mut state = HistorySearchDialogState::open(
            vec![entry("alpha", 1), entry("beta", 2)],
            String::new(),
        );
        assert_eq!(state.filtered.len(), 2);
        state.on_key(DialogKey::plain('a').into());
        state.on_key(DialogKey::plain('l').into());
        assert_eq!(state.filtered.len(), 1);
        state.on_key(DialogKey::Backspace.into());
        state.on_key(DialogKey::Backspace.into());
        assert_eq!(state.filtered.len(), 2);
    }

    #[test]
    fn a_control_chord_is_a_binding_not_a_character() {
        let mut state = HistorySearchDialogState::open(vec![entry("alpha", 1)], String::new());
        state.on_key(KeyPress::from(DialogKey::Char {
            value: 'u',
            plain: false,
        }));
        assert!(state.query.is_empty());
    }

    #[test]
    fn the_view_lays_out_against_the_frame_it_was_told_about() {
        let mut state = HistorySearchDialogState::open(vec![entry("alpha", 1)], String::new());
        state.note_viewport(20, 200);
        let ViewSpec::Search(wide) = state.view() else {
            panic!("expected a search view");
        };
        assert!(wide.layout.preview_on_right, "a wide frame splits sideways");
        assert_eq!(wide.rows.len(), 1);
        assert!(wide.rows[0].focused);
        assert_eq!(wide.query, "");
        assert_eq!(wide.placeholder, history_search::PLACEHOLDER);

        state.note_viewport(20, 40);
        let ViewSpec::Search(narrow) = state.view() else {
            panic!("expected a search view");
        };
        assert!(!narrow.layout.preview_on_right, "a narrow frame stacks");
    }

    #[test]
    fn an_empty_result_set_carries_the_reducers_message() {
        let mut state = HistorySearchDialogState::open(vec![entry("alpha", 1)], String::new());
        for ch in "zzz".chars() {
            state.on_key(DialogKey::plain(ch).into());
        }
        let ViewSpec::Search(view) = state.view() else {
            panic!("expected a search view");
        };
        assert!(view.rows.is_empty());
        assert_eq!(
            view.empty_message,
            history_search::empty_message(HistoryLoadState::Loaded, "zzz")
        );
    }
}
