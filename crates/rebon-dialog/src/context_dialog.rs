//! Interactive dialog for inspecting `/context` output without touching the
//! transcript.
//!
//! The overview and the per-category summaries are read out of the
//! command's own text, which is why they can be parsed here. The entries
//! under each category come from the transcript, which this crate cannot
//! see, so they arrive already collected.

use crate::model::{
    DialogAction, DialogKey, DialogModel, DialogOutcome, KeyPress, OutlineRow, OutlineView,
    ViewSpec,
};

/// Stable id used for action routing and native view lookup.
pub const DIALOG_ID: &str = "context";
/// Compact the transcript. Both actions close the dialog as they fire
/// and carry no values.
pub const ACTION_COMPACT: &str = "compact";
/// Run a prune sweep.
pub const ACTION_PRUNE_SWEEP: &str = "prune-sweep";

fn action(name: &'static str) -> DialogOutcome {
    DialogOutcome::Action(DialogAction::closing_many(DIALOG_ID, name, Vec::new()))
}

const TITLE: &str = "Context Usage";
const FOOTER_HINT: &str =
    "Up/Down select · Enter/Left/Right expand · c compact · p prune sweep · Esc close";
/// The browser's categories, and the order entry lists arrive in.
pub const CATEGORY_NAMES: [&str; 5] = [
    "Messages",
    "Tool calls",
    "Tool results",
    "Thinking",
    "Free space",
];

#[derive(Debug, Clone)]
struct ContextCategory {
    name: &'static str,
    summary: String,
    items: Vec<String>,
    expanded: bool,
}

impl ContextCategory {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            summary: String::new(),
            items: Vec::new(),
            expanded: false,
        }
    }
}

/// Reducer state for the `/context` browser.
#[derive(Debug, Clone)]
pub struct ContextDialogState {
    overview: Vec<String>,
    categories: Vec<ContextCategory>,
    selected: usize,
    scroll: u16,
}

impl ContextDialogState {
    /// Build the browser from the `/context` output and one list of
    /// entries per category, in [`CATEGORY_NAMES`] order. A shorter list
    /// leaves the remaining categories empty.
    pub fn open(content: &str, category_items: Vec<Vec<String>>) -> Self {
        let overview = content.lines().take(4).map(str::to_owned).collect();
        let mut categories: Vec<_> = CATEGORY_NAMES
            .into_iter()
            .map(ContextCategory::new)
            .collect();

        collect_category_summaries(content, &mut categories);
        for (category, items) in categories.iter_mut().zip(category_items) {
            category.items = items;
        }
        if categories[4].items.is_empty() {
            categories[4]
                .items
                .push("Remaining context capacity is shown in the usage header.".into());
        }

        Self {
            overview,
            categories,
            selected: 0,
            scroll: 0,
        }
    }

    /// The header lines read off the `/context` output.
    #[cfg(test)]
    pub fn overview(&self) -> &[String] {
        &self.overview
    }

    /// The first body line currently shown.
    #[cfg(test)]
    pub fn scroll(&self) -> u16 {
        self.scroll
    }

    #[cfg(test)]
    fn selected_category(&self) -> &str {
        self.categories[self.selected].name
    }

    #[cfg(test)]
    fn category_is_expanded(&self, name: &str) -> bool {
        self.categories
            .iter()
            .find(|category| category.name == name)
            .is_some_and(|category| category.expanded)
    }

    /// Apply one key against an explicit viewport height. Split out
    /// from [`DialogModel::on_key`] so paging can be driven against a
    /// chosen height; the real one is only known after a render.
    fn apply_key(&mut self, key: DialogKey, viewport_height: u16) -> DialogOutcome {
        let page = viewport_height.saturating_sub(1).max(1);
        // Letter shortcuts must ignore control chords so Ctrl+C is not read as
        // `c` (which would trigger a compaction) while the dialog owns the
        // keyboard.
        match key {
            DialogKey::Escape => DialogOutcome::Close,
            DialogKey::Char {
                value: 'c',
                plain: true,
            } => action(ACTION_COMPACT),
            DialogKey::Char {
                value: 'p',
                plain: true,
            } => action(ACTION_PRUNE_SWEEP),
            DialogKey::Up => {
                self.selected = self.selected.saturating_sub(1);
                self.reveal_selected(viewport_height);
                DialogOutcome::None
            }
            DialogKey::Down => {
                self.selected = (self.selected + 1).min(self.categories.len().saturating_sub(1));
                self.reveal_selected(viewport_height);
                DialogOutcome::None
            }
            DialogKey::Enter | DialogKey::Right => {
                self.categories[self.selected].expanded = true;
                DialogOutcome::None
            }
            DialogKey::Left => {
                self.categories[self.selected].expanded = false;
                DialogOutcome::None
            }
            DialogKey::PageUp => {
                self.scroll = self.scroll.saturating_sub(page);
                self.clamp_selected_to_viewport(viewport_height);
                DialogOutcome::None
            }
            DialogKey::PageDown => {
                self.scroll = self
                    .max_scroll(viewport_height)
                    .min(self.scroll.saturating_add(page));
                self.clamp_selected_to_viewport(viewport_height);
                DialogOutcome::None
            }
            DialogKey::Home => {
                self.selected = 0;
                self.scroll = 0;
                DialogOutcome::None
            }
            DialogKey::End => {
                self.selected = self.categories.len().saturating_sub(1);
                self.scroll = self.max_scroll(viewport_height);
                DialogOutcome::None
            }
            _ => DialogOutcome::None,
        }
    }

    /// Rendered line a category header occupies. Expanded categories push the
    /// ones below them down by one line per item (or by the single
    /// "No entries" line), so the category index alone is not the row.
    fn category_row(&self, index: usize) -> usize {
        let mut row = self.overview.len() + usize::from(!self.overview.is_empty());
        for category in self.categories.iter().take(index) {
            row += 1;
            if category.expanded {
                row += category.items.len().max(1);
            }
        }
        row
    }

    fn reveal_selected(&mut self, viewport_height: u16) {
        let row = self.category_row(self.selected);
        let visible = viewport_height.max(1) as usize;
        if row < self.scroll as usize {
            self.scroll = row as u16;
        } else if row >= self.scroll as usize + visible {
            self.scroll = row.saturating_add(1).saturating_sub(visible) as u16;
        }
    }

    /// Page keys move the viewport, not the selection. Pull the highlight along
    /// so it stays on a visible category instead of scrolling out of sight.
    fn clamp_selected_to_viewport(&mut self, viewport_height: u16) {
        let visible = viewport_height.max(1) as usize;
        let top = self.scroll as usize;
        let bottom = top.saturating_add(visible);
        let mut first = None;
        let mut last = None;
        let mut spanning = 0;
        for index in 0..self.categories.len() {
            let row = self.category_row(index);
            if row < top {
                spanning = index;
            }
            if row >= top && row < bottom {
                first.get_or_insert(index);
                last = Some(index);
            }
        }
        self.selected = match (first, last) {
            (Some(first), Some(last)) => self.selected.clamp(first, last),
            // A page can land inside a long expanded category, leaving no
            // header on screen. Stay on the category being read.
            _ => spanning,
        };
    }

    fn max_scroll(&self, viewport_height: u16) -> u16 {
        self.body_rows()
            .len()
            .saturating_sub(viewport_height as usize) as u16
    }

    /// Every body line, in display order: the overview, a blank, then
    /// each category header with its items indented under it when it is
    /// expanded. Both the view and the scroll maths read this, so the
    /// row a category occupies cannot drift from the row it is drawn on.
    fn body_rows(&self) -> Vec<OutlineRow> {
        let mut rows: Vec<OutlineRow> = self
            .overview
            .iter()
            .map(|line| OutlineRow::normal(line.clone()))
            .collect();
        if !rows.is_empty() {
            rows.push(OutlineRow::normal(String::new()));
        }
        for category in &self.categories {
            let marker = if category.expanded {
                "\u{25bc}"
            } else {
                "\u{25b6}"
            };
            let summary = if category.summary.is_empty() {
                String::new()
            } else {
                format!(" \u{2014} {}", category.summary)
            };
            rows.push(OutlineRow::normal(format!(
                "{marker} {}{summary}",
                category.name
            )));
            if category.expanded {
                if category.items.is_empty() {
                    rows.push(OutlineRow::dim("    No entries"));
                } else {
                    rows.extend(
                        category
                            .items
                            .iter()
                            .map(|item| OutlineRow::dim(format!("    {item}"))),
                    );
                }
            }
        }
        rows
    }
}

fn collect_category_summaries(content: &str, categories: &mut [ContextCategory]) {
    for line in content.lines().map(str::trim) {
        let target = if line.starts_with("user text:") || line.starts_with("assistant text:") {
            Some(0)
        } else if line.starts_with("tool calls:") {
            Some(1)
        } else if line.starts_with("tool results:") {
            Some(2)
        } else if line.starts_with("thinking:") {
            Some(3)
        } else {
            None
        };
        if let Some(index) = target {
            if !categories[index].summary.is_empty() {
                categories[index].summary.push_str(" · ");
            }
            categories[index].summary.push_str(line);
        }
    }
}

impl DialogModel for ContextDialogState {
    crate::dialog_plumbing!();

    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn on_key(&mut self, press: KeyPress) -> DialogOutcome {
        self.apply_key(press.key, press.viewport_rows_or(20))
    }

    fn view(&self) -> ViewSpec {
        ViewSpec::Outline(OutlineView {
            title: format!(" {TITLE} "),
            selected: Some(self.category_row(self.selected)),
            rows: self.body_rows(),
            scroll: Some(self.scroll),
            footer: FOOTER_HINT.into(),
            empty_text: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RowTone;

    fn dialog(content: &str) -> ContextDialogState {
        ContextDialogState::open(content, Vec::new())
    }

    fn expanded(items: usize) -> ContextDialogState {
        let mut dialog = dialog("Context Usage");
        dialog.categories[0].items = (0..items).map(|index| format!("#{index} User")).collect();
        dialog.categories[0].expanded = true;
        dialog
    }

    #[test]
    fn category_navigation_and_expansion_are_structured() {
        let mut dialog = dialog(
            "Context Usage\nToken Breakdown (estimated)\n  user text: ~20\n  tool calls: ~10",
        );

        assert_eq!(dialog.selected_category(), "Messages");
        dialog.apply_key(DialogKey::Down, 10);
        assert_eq!(dialog.selected_category(), "Tool calls");
        dialog.apply_key(DialogKey::Enter, 10);
        assert!(dialog.category_is_expanded("Tool calls"));
        dialog.apply_key(DialogKey::Left, 10);
        assert!(!dialog.category_is_expanded("Tool calls"));
    }

    #[test]
    fn compact_and_prune_keys_return_actions() {
        let mut dialog = dialog("Context Usage");
        assert_eq!(
            dialog.apply_key(
                DialogKey::Char {
                    value: 'c',
                    plain: true
                },
                20
            ),
            DialogOutcome::Action(DialogAction::closing_many(
                DIALOG_ID,
                ACTION_COMPACT,
                Vec::new()
            ))
        );
        assert_eq!(
            dialog.apply_key(
                DialogKey::Char {
                    value: 'p',
                    plain: true
                },
                20
            ),
            DialogOutcome::Action(DialogAction::closing_many(
                DIALOG_ID,
                ACTION_PRUNE_SWEEP,
                Vec::new()
            ))
        );
    }

    #[test]
    fn control_chords_do_not_trigger_letter_shortcuts() {
        let mut dialog = dialog("Context Usage");
        assert_eq!(
            dialog.apply_key(
                DialogKey::Char {
                    value: 'c',
                    plain: false
                },
                20
            ),
            DialogOutcome::None
        );
        assert_eq!(
            dialog.apply_key(
                DialogKey::Char {
                    value: 'p',
                    plain: false
                },
                20
            ),
            DialogOutcome::None
        );
    }

    #[test]
    fn page_navigation_clamps_within_rendered_rows() {
        let mut dialog = dialog("Context Usage");

        dialog.apply_key(DialogKey::PageDown, 3);
        assert!(dialog.scroll() > 0);
        dialog.apply_key(DialogKey::Home, 3);
        assert_eq!(dialog.scroll(), 0);
    }

    #[test]
    fn moving_below_an_expanded_category_scrolls_past_its_items() {
        let mut dialog = expanded(10);

        dialog.apply_key(DialogKey::Down, 5);

        // Overview line, blank, "Messages", ten items, then "Tool calls".
        assert_eq!(dialog.selected_category(), "Tool calls");
        assert_eq!(dialog.scroll(), 9);
    }

    #[test]
    fn paging_keeps_the_highlight_on_a_visible_category() {
        let mut dialog = expanded(6);
        // A 10-row frame leaves 7 body rows: 10 minus the two borders
        // and the pinned footer.
        let viewport = 7u16;

        dialog.apply_key(DialogKey::PageDown, viewport);
        let row = dialog.category_row(dialog.selected);
        assert!(row >= dialog.scroll() as usize);
        assert!(row < dialog.scroll() as usize + viewport as usize);
        assert_eq!(dialog.selected_category(), "Tool calls");

        dialog.apply_key(DialogKey::PageUp, viewport);
        assert_eq!(dialog.scroll(), 0);
        assert_eq!(dialog.selected_category(), "Messages");
    }

    #[test]
    fn escape_closes_dialog() {
        let mut dialog = dialog("Context Usage");
        let outcome = dialog.apply_key(DialogKey::Escape, 20);

        assert_eq!(outcome, DialogOutcome::Close);
    }

    #[test]
    fn the_view_flattens_categories_and_indents_expanded_items() {
        let dialog = expanded(2);
        let ViewSpec::Outline(view) = dialog.view() else {
            panic!("expected an outline view");
        };
        assert_eq!(view.title, " Context Usage ");
        assert_eq!(view.scroll, Some(0));
        // The highlighted line is the category header's flattened row.
        assert_eq!(view.selected, Some(dialog.category_row(0)));
        let header = &view.rows[view.selected.unwrap()];
        assert!(header.text.starts_with('\u{25bc}'), "{header:?}");
        assert_eq!(header.tone, RowTone::Normal);
        // Its two items follow, indented and dim.
        let first_item = &view.rows[view.selected.unwrap() + 1];
        assert!(first_item.text.starts_with("    "), "{first_item:?}");
        assert_eq!(first_item.tone, RowTone::Dim);
    }

    #[test]
    fn a_collapsed_category_shows_a_closed_marker_and_no_children() {
        let dialog = dialog("Context Usage");
        let ViewSpec::Outline(view) = dialog.view() else {
            panic!("expected an outline view");
        };
        let header = &view.rows[view.selected.unwrap()];
        assert!(header.text.starts_with('\u{25b6}'), "{header:?}");
        assert!(view.rows.iter().all(|row| !row.text.starts_with("    ")));
    }
}
