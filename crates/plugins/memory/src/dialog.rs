//! Selectable `/memory` browser for loaded instruction files.
//!
//! It lives with the memory plugin rather than in the terminal because
//! the files it lists are this plugin's to find: switch the plugin off
//! and the panel goes with it, which is what registering it on the
//! `ui-registry` seat buys.
//!
//! A [`DialogModel`] described as a [`ViewSpec::Outline`]: the
//! proportional bar charts are rows this module has already formatted,
//! so the shared painter draws them and the host owns the stack, the
//! keys and the modal predicate.

use rebon_dialog::model::{
    DialogAction, DialogKey, DialogModel, DialogOutcome, KeyPress, OutlineRow, OutlineView,
    ViewSpec,
};
use rebon_ui_seat::ids;

/// Stable id used for action routing.
pub const DIALOG_ID: &str = ids::dialog::MEMORY;
/// The only action this dialog emits: open the highlighted file.
pub const ACTION_OPEN: &str = ids::action::OPEN;

/// Collect the instruction files loaded for `cwd`, in the order they were
/// loaded. This is the plugin's own store talking, which is why the browser
/// could come here at all.
pub fn entries_for(cwd: &str) -> Vec<MemoryFileEntry> {
    crate::memory::loaded_files::gather_memory_files(cwd)
        .into_iter()
        .map(|file| MemoryFileEntry {
            tokens: file.approx_tokens(),
            path: file.path.to_string_lossy().into_owned(),
        })
        .collect()
}

const BAR_WIDTH: usize = 12;
const FOOTER: &str = "Up/Down select · Enter open in editor · Esc close";

/// One loaded instruction file and its share of the memory budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryFileEntry {
    pub path: String,
    pub tokens: usize,
}

#[derive(Debug, Clone)]
pub struct MemoryDialogState {
    entries: Vec<MemoryFileEntry>,
    selected: usize,
}

impl MemoryDialogState {
    pub fn open(entries: Vec<MemoryFileEntry>) -> Self {
        Self {
            entries,
            selected: 0,
        }
    }

    /// One row per file: a proportional bar, its share, its token cost
    /// and its path. Widths are fixed so the columns line up.
    fn body_rows(&self) -> Vec<OutlineRow> {
        let total_tokens: usize = self.entries.iter().map(|entry| entry.tokens).sum();
        self.entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let ratio = if total_tokens == 0 {
                    0.0
                } else {
                    entry.tokens as f64 / total_tokens as f64
                };
                let filled = ((ratio * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH);
                let bar = format!(
                    "{}{}",
                    "\u{2588}".repeat(filled),
                    "\u{2591}".repeat(BAR_WIDTH.saturating_sub(filled))
                );
                let marker = if index == self.selected { ">" } else { " " };
                OutlineRow::normal(format!(
                    "{marker} {bar} {:>5.1}%  ~{} tokens  {}",
                    ratio * 100.0,
                    entry.tokens,
                    entry.path
                ))
            })
            .collect()
    }

    #[cfg(test)]
    fn selected(&self) -> usize {
        self.selected
    }
}

impl DialogModel for MemoryDialogState {
    rebon_dialog::dialog_plumbing!();

    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn on_key(&mut self, press: KeyPress) -> DialogOutcome {
        match press.key {
            DialogKey::Escape => DialogOutcome::Close,
            DialogKey::Up => {
                self.selected = self.selected.saturating_sub(1);
                DialogOutcome::None
            }
            DialogKey::Down => {
                self.selected = self
                    .selected
                    .saturating_add(1)
                    .min(self.entries.len().saturating_sub(1));
                DialogOutcome::None
            }
            DialogKey::Home => {
                self.selected = 0;
                DialogOutcome::None
            }
            DialogKey::End => {
                self.selected = self.entries.len().saturating_sub(1);
                DialogOutcome::None
            }
            // Opening a file in an external editor leaves the browser
            // up, so the user can open a second one.
            DialogKey::Enter => self
                .entries
                .get(self.selected)
                .map(|entry| {
                    DialogOutcome::Action(DialogAction::staying(
                        DIALOG_ID,
                        ACTION_OPEN,
                        entry.path.clone(),
                    ))
                })
                .unwrap_or(DialogOutcome::None),
            _ => DialogOutcome::None,
        }
    }

    fn view(&self) -> ViewSpec {
        ViewSpec::Outline(OutlineView {
            title: " Memory Files ".into(),
            selected: Some(self.selected),
            rows: self.body_rows(),
            // The browser moves by selection only, so the host scrolls
            // the least it can to keep the highlighted file on screen.
            scroll: None,
            footer: FOOTER.into(),
            empty_text: Some(
                "No loaded memory or instruction files were discovered for this session.".into(),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog() -> MemoryDialogState {
        MemoryDialogState::open(vec![
            MemoryFileEntry {
                path: "REBON.md".into(),
                tokens: 25,
            },
            MemoryFileEntry {
                path: "rules.md".into(),
                tokens: 75,
            },
        ])
    }

    fn open(path: &str) -> DialogOutcome {
        DialogOutcome::Action(DialogAction::staying(DIALOG_ID, ACTION_OPEN, path))
    }

    #[test]
    fn navigation_clamps_to_available_files() {
        let mut dialog = dialog();
        dialog.on_key(DialogKey::Down.into());
        dialog.on_key(DialogKey::Down.into());
        assert_eq!(dialog.selected(), 1);
        dialog.on_key(DialogKey::Up.into());
        assert_eq!(dialog.selected(), 0);
    }

    #[test]
    fn home_and_end_jump_to_the_ends() {
        let mut dialog = dialog();
        dialog.on_key(DialogKey::End.into());
        assert_eq!(dialog.selected(), 1);
        dialog.on_key(DialogKey::Home.into());
        assert_eq!(dialog.selected(), 0);
    }

    #[test]
    fn enter_returns_selected_path_and_keeps_the_browser_open() {
        let mut dialog = dialog();
        dialog.on_key(DialogKey::End.into());
        let outcome = dialog.on_key(DialogKey::Enter.into());
        assert_eq!(outcome, open("rules.md"));
        let DialogOutcome::Action(action) = outcome else {
            unreachable!()
        };
        assert!(!action.close, "the browser stays up after opening a file");
    }

    #[test]
    fn empty_dialog_has_no_open_action() {
        let mut dialog = MemoryDialogState::open(Vec::new());
        assert_eq!(dialog.on_key(DialogKey::Enter.into()), DialogOutcome::None);
        assert_eq!(
            dialog.on_key(DialogKey::Escape.into()),
            DialogOutcome::Close
        );
    }

    #[test]
    fn the_view_lists_one_bar_row_per_file() {
        let ViewSpec::Outline(view) = dialog().view() else {
            panic!("expected an outline view");
        };
        assert_eq!(view.title, " Memory Files ");
        assert_eq!(view.rows.len(), 2);
        assert_eq!(view.selected, Some(0));
        assert_eq!(view.scroll, None, "the host reveals the selection");
        // 25 of 100 tokens is a quarter of the bar, and the path trails.
        assert!(view.rows[0].text.contains("25.0%"), "{:?}", view.rows[0]);
        assert!(
            view.rows[0].text.ends_with("REBON.md"),
            "{:?}",
            view.rows[0]
        );
        assert!(view.rows[1].text.contains("75.0%"), "{:?}", view.rows[1]);
    }

    #[test]
    fn an_empty_browser_offers_explanatory_text_instead_of_rows() {
        let ViewSpec::Outline(view) = MemoryDialogState::open(Vec::new()).view() else {
            panic!("expected an outline view");
        };
        assert!(view.rows.is_empty());
        assert!(view.empty_text.is_some());
    }
}
