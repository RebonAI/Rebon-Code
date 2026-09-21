//! The quick-open file picker: a fuzzy filter over the project's files.
//!
//! A [`DialogModel`] over the [`crate::quick_open`] reducer.
//! Unlike the panels that filter a snapshot, this one asks its index on
//! every keystroke, so a file that appears mid-search shows up. That is
//! what [`FileCandidates`] is for: the index stays with the host, and the
//! panel holds a handle to it. [`PreviewSource`] is the same arrangement
//! for reading a file.

use std::sync::Arc;

use rebon_customselect::{
    NavigationAction, NavigationProps, NavigationState, OptionWithDescription,
};

use crate::model::{
    DialogAction, DialogKey, DialogModel, DialogOutcome, KeyPress, RowTone, SearchFooterSegment,
    SearchLayout, SearchRow, SearchView, ViewSpec,
};
use crate::quick_open;

/// Stable id used for action routing.
pub const DIALOG_ID: &str = "quick-open";
/// Put the chosen path in the composer. Values: `[kind, path]`, where
/// `kind` is `select`, `mention` or `insert`.
pub const ACTION_APPLY: &str = "apply";

/// The files a query could match.
///
/// Implemented by the host over whatever index it keeps. Asked on every
/// keystroke, so an implementation that is slow makes typing slow.
pub trait FileCandidates: Send + Sync {
    /// Up to `limit` project-relative paths matching `query`, best
    /// first, separated by `/` on every platform.
    fn matching(&self, query: &str, limit: usize) -> Vec<String>;
}

/// The first lines of a file.
///
/// Reading a file is IO, which this crate does not do; the host's
/// implementation does it and hands back the lines.
pub trait PreviewSource: Send + Sync {
    /// Up to `rows` lines from `path`, or empty when it cannot be read.
    fn preview(&self, path: &str, rows: usize) -> Vec<String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Preview {
    path: String,
    lines: Vec<String>,
}

/// Reducer state for the quick-open picker.
#[derive(Clone)]
pub struct QuickOpenDialogState {
    /// The typed query.
    pub query: String,
    results: Vec<String>,
    nav: Option<NavigationState<String>>,
    preview: Option<Preview>,
    files: Arc<dyn FileCandidates>,
    previews: Arc<dyn PreviewSource>,
    /// Inner columns the host last reported.
    cols: u16,
}

impl std::fmt::Debug for QuickOpenDialogState {
    /// The two sources are the host's; naming them would say nothing.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuickOpenDialogState")
            .field("query", &self.query)
            .field("results", &self.results.len())
            .finish()
    }
}

/// How many matches one keystroke asks for.
const MATCH_LIMIT: usize = 64;
/// Inner columns assumed before the host has reported a real frame.
const DEFAULT_COLS: u16 = 80;

impl QuickOpenDialogState {
    /// Open over a host's file index and previewer.
    pub fn open(files: Arc<dyn FileCandidates>, previews: Arc<dyn PreviewSource>) -> Self {
        Self {
            query: String::new(),
            results: Vec::new(),
            nav: None,
            preview: None,
            files,
            previews,
            cols: DEFAULT_COLS,
        }
    }

    fn refresh(&mut self) {
        self.results = if self.query.trim().is_empty() {
            Vec::new()
        } else {
            self.files.matching(&self.query, MATCH_LIMIT)
        };
        let focus = self.selected_path();
        self.reset_nav(focus);
        self.sync_preview();
    }

    fn selected_path(&self) -> Option<String> {
        self.nav
            .as_ref()
            .and_then(NavigationState::validated_focused_value)
    }

    fn reset_nav(&mut self, focus: Option<String>) {
        if self.results.is_empty() {
            self.nav = None;
            return;
        }
        let options = self
            .results
            .iter()
            .map(|path| OptionWithDescription::text(path.clone(), path.clone()))
            .collect();
        self.nav = Some(NavigationState::new(NavigationProps {
            visible_option_count: Some(quick_open::VISIBLE_RESULTS),
            options,
            initial_focus_value: None,
            focus_value: focus,
        }));
    }

    fn sync_preview(&mut self) {
        let Some(path) = self.selected_path() else {
            self.preview = None;
            return;
        };
        let rows = quick_open::effective_preview_lines(self.cols);
        let lines = self.previews.preview(&path, rows);
        self.preview = Some(Preview { path, lines });
    }
}

impl DialogModel for QuickOpenDialogState {
    crate::dialog_plumbing!();

    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn on_key(&mut self, press: KeyPress) -> DialogOutcome {
        match press.key {
            DialogKey::Escape => DialogOutcome::Close,
            DialogKey::Enter => self.apply("select"),
            DialogKey::Tab => self.apply("mention"),
            DialogKey::BackTab => self.apply("insert"),
            DialogKey::Up => self.navigate(NavigationAction::FocusPreviousOption),
            DialogKey::Down => self.navigate(NavigationAction::FocusNextOption),
            DialogKey::PageUp => self.navigate(NavigationAction::FocusPreviousPage),
            DialogKey::PageDown => self.navigate(NavigationAction::FocusNextPage),
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

impl QuickOpenDialogState {
    /// Hand the host the focused path and what to do with it. Nothing
    /// focused means nothing to apply, and the picker stays up.
    fn apply(&self, kind: &'static str) -> DialogOutcome {
        match self.selected_path() {
            Some(path) => DialogOutcome::Action(DialogAction::closing_many(
                DIALOG_ID,
                ACTION_APPLY,
                vec![kind.to_string(), path],
            )),
            None => DialogOutcome::None,
        }
    }

    fn navigate(&mut self, action: NavigationAction<String>) -> DialogOutcome {
        crate::model::dispatch_nav(&mut self.nav, action);
        self.sync_preview();
        DialogOutcome::None
    }

    /// Project the picker into the shape a surface paints.
    fn search_view(&self) -> SearchView {
        let focused = self.selected_path();
        let rows = self
            .nav
            .as_ref()
            .map(NavigationState::visible_options)
            .unwrap_or_default()
            .into_iter()
            .map(|row| {
                let path = row.option.value().clone();
                SearchRow {
                    focused: focused.as_ref() == Some(&path),
                    text: path,
                }
            })
            .collect();
        let (preview_header, preview) = match &self.preview {
            Some(preview) => (Some(preview.path.clone()), preview.lines.clone()),
            None => (None, Vec::new()),
        };
        SearchView {
            title: format!(" {} ", quick_open::TITLE),
            query: self.query.clone(),
            placeholder: quick_open::PLACEHOLDER.to_string(),
            rows,
            empty_message: quick_open::empty_message(&self.query).to_string(),
            preview_header,
            preview,
            preview_tone: RowTone::Normal,
            preview_empty_message: "(preview unavailable)".into(),
            preview_more: 0,
            footer: vec![
                segment(" Enter ", true),
                segment(quick_open::SELECT_ACTION, false),
                segment(" \u{b7} Tab ", true),
                segment("mention", false),
                segment(" \u{b7} shift+tab ", true),
                segment("insert path", false),
                segment(" \u{b7} Esc ", true),
                segment("close", false),
            ],
            layout: SearchLayout {
                list_width: quick_open::compute_layout(self.cols).0,
                preview_rows: quick_open::effective_preview_lines(self.cols),
                preview_on_right: quick_open::preview_on_right(self.cols),
                stacked_list_rows: Some(quick_open::compute_visible_rows(self.cols)),
                body_min_rows: 3,
            },
        }
    }
}

fn segment(text: &str, emphasis: bool) -> SearchFooterSegment {
    SearchFooterSegment {
        text: text.to_string(),
        emphasis,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for the host's index: a fixed list, filtered by
    /// substring, so the tests exercise the seam and not the matcher.
    struct Files(Vec<String>);

    impl FileCandidates for Files {
        fn matching(&self, query: &str, limit: usize) -> Vec<String> {
            self.0
                .iter()
                .filter(|path| path.contains(query))
                .take(limit)
                .cloned()
                .collect()
        }
    }

    struct Previews;

    impl PreviewSource for Previews {
        fn preview(&self, path: &str, rows: usize) -> Vec<String> {
            (0..rows.min(2)).map(|n| format!("{path}:{n}")).collect()
        }
    }

    fn state() -> QuickOpenDialogState {
        QuickOpenDialogState::open(
            Arc::new(Files(vec![
                "src/main.rs".into(),
                "src/lib.rs".into(),
                "Cargo.toml".into(),
            ])),
            Arc::new(Previews),
        )
    }

    #[test]
    fn typing_populates_results() {
        let mut state = state();
        state.on_key(DialogKey::plain('m').into());
        assert_eq!(state.query, "m");
        assert!(!state.results.is_empty());
    }

    #[test]
    fn every_keystroke_asks_the_index_again() {
        let mut state = state();
        for ch in "lib".chars() {
            state.on_key(DialogKey::plain(ch).into());
        }
        assert_eq!(state.results, vec!["src/lib.rs".to_string()]);
        state.on_key(DialogKey::Backspace.into());
        state.on_key(DialogKey::Backspace.into());
        state.on_key(DialogKey::Backspace.into());
        // An empty query asks for nothing rather than everything.
        assert!(state.results.is_empty());
    }

    #[test]
    fn tab_applies_a_mention_and_enter_a_selection() {
        let mut state = state();
        state.on_key(DialogKey::plain('m').into());
        let DialogOutcome::Action(mention) = state.on_key(DialogKey::Tab.into()) else {
            panic!("expected an apply action");
        };
        assert_eq!(mention.value(), "mention");
        assert!(mention.value_at(1).contains("main.rs"));

        let DialogOutcome::Action(select) = state.on_key(DialogKey::Enter.into()) else {
            panic!("expected an apply action");
        };
        assert_eq!(select.value(), "select");
    }

    #[test]
    fn nothing_focused_applies_nothing() {
        let mut state = state();
        assert_eq!(state.on_key(DialogKey::Enter.into()), DialogOutcome::None);
    }

    #[test]
    fn the_preview_follows_the_focused_row_and_names_it() {
        let mut state = state();
        state.note_viewport(20, 200);
        state.on_key(DialogKey::plain('s').into());
        let ViewSpec::Search(view) = state.view() else {
            panic!("expected a search view");
        };
        let header = view.preview_header.expect("a focused row has a preview");
        assert!(view.preview[0].starts_with(&header));
        assert_eq!(view.preview_tone, RowTone::Normal);
        assert!(view.layout.preview_on_right, "a wide frame splits sideways");
        assert_eq!(view.layout.body_min_rows, 3);
        assert!(view.layout.stacked_list_rows.is_some());
    }

    #[test]
    fn a_control_chord_is_a_binding_not_a_character() {
        let mut state = state();
        state.on_key(KeyPress::from(DialogKey::Char {
            value: 'u',
            plain: false,
        }));
        assert!(state.query.is_empty());
    }
}
