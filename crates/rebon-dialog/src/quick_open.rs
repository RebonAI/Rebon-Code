//! Fuzzy file finder with a syntax-highlighted preview.
//!
//! ## Behaviour
//!
//! * The `VISIBLE_RESULTS = 8` and `PREVIEW_LINES = 20` constants.
//! * The visible-rows clamp (`min(8, max(4, rows - 14))`).
//! * The `preview_on_right` threshold (`columns >= 120`).
//! * The layout column math.
//! * The mention vs insert path action discriminator.
//! * The path projection (normalize sep to `/`, keeping directory rows).
//! * The fuzzy-results state machine (empty / loading / loaded).
//! * The "No matching files" / "Start typing…" empty messages.
//!
//! ## Outbound seam
//!
//! Generating file suggestions and reading preview lines are async IO. This
//! module exposes a small reducer that takes pre-built suggestions and
//! preview content.

/// Number of result rows kept visible at once.
pub const VISIBLE_RESULTS: usize = 8;

/// Number of lines shown in the preview pane.
pub const PREVIEW_LINES: usize = 20;

/// Empty-state message shown when the query has no matches.
pub const EMPTY_MATCHING: &str = "No matching files";

/// Empty-state message shown before the user types.
pub const EMPTY_INITIAL: &str = "Start typing to search…";

/// The dialog title.
pub const TITLE: &str = "Quick Open";

/// The query input placeholder.
pub const PLACEHOLDER: &str = "Type to search files…";

/// Label for the select action.
pub const SELECT_ACTION: &str = "open in editor";

/// Action emitted when the user picks a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuickOpenAction {
    /// Open the file in the external editor.
    OpenInEditor {
        /// Relative path that the user picked (already normalized).
        path: String,
    },
    /// Insert the path text into the input as a `@`-mention.
    InsertMention {
        /// `"@{path} "`.
        text: String,
    },
    /// Insert the plain path text into the input.
    InsertPath {
        /// `"{path} "`.
        text: String,
    },
    /// Cancel the dialog.
    Cancel,
}

/// Compute the visible-rows count given the terminal height.
pub fn compute_visible_rows(rows: u16) -> usize {
    let r = rows as usize;
    let lower = 4_usize;
    let upper = VISIBLE_RESULTS;
    let candidate = r.saturating_sub(14).max(lower);
    candidate.min(upper)
}

/// True when the preview should be shown on the right of the list.
pub fn preview_on_right(columns: u16) -> bool {
    columns >= 120
}

/// Compute the effective preview-lines count.
pub fn effective_preview_lines(columns: u16) -> usize {
    if preview_on_right(columns) {
        VISIBLE_RESULTS - 1
    } else {
        PREVIEW_LINES
    }
}

/// Compute `(max_path_width, preview_width)` from the terminal width.
pub fn compute_layout(columns: u16) -> (usize, usize) {
    let cols = columns as usize;
    let on_right = preview_on_right(columns);
    let max_path = if on_right {
        // `(columns - 10) * 40 / 100`, integer-floored, with a floor of 20
        // (`columns < 10` falls back to 20 as well).
        cols.saturating_sub(10)
            .checked_mul(40)
            .map(|v| v / 100)
            .unwrap_or(20)
            .max(20)
    } else {
        cols.saturating_sub(8).max(20)
    };
    let preview_width = if on_right {
        cols.saturating_sub(max_path).saturating_sub(14).max(40)
    } else {
        cols.saturating_sub(6)
    };
    (max_path, preview_width)
}

/// Build the empty-state message.
pub fn empty_message(query: &str) -> &'static str {
    if query.trim().is_empty() {
        EMPTY_INITIAL
    } else {
        EMPTY_MATCHING
    }
}

/// Pre-built file suggestion input. The caller builds this from its
/// file suggestions for the query. This module consumes a
/// pre-projected list with id + display-text + slash-normalized path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSuggestionInput {
    /// e.g. `"file-12"`.
    pub id: String,
    /// The full display text — for files, the relative path.
    pub display_text: String,
}

/// Filter raw suggestions to file rows only and normalize the path
/// separator.
pub fn project_suggestions(items: &[FileSuggestionInput]) -> Vec<String> {
    items
        .iter()
        .filter(|i| i.id.starts_with("file-"))
        .map(|i| i.display_text.clone())
        .map(|p| p.replace('\\', "/"))
        .collect()
}

/// Reducer for the on-select event.
pub fn handle_select(path: &str) -> QuickOpenAction {
    QuickOpenAction::OpenInEditor {
        path: path.to_string(),
    }
}

/// Reducer for the on-insert (mention) event.
pub fn handle_insert(path: &str, mention: bool) -> QuickOpenAction {
    if mention {
        QuickOpenAction::InsertMention {
            text: format!("@{path} "),
        }
    } else {
        QuickOpenAction::InsertPath {
            text: format!("{path} "),
        }
    }
}

/// Cancel handler.
pub fn handle_cancel() -> QuickOpenAction {
    QuickOpenAction::Cancel
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_pinned() {
        assert_eq!(VISIBLE_RESULTS, 8);
        assert_eq!(PREVIEW_LINES, 20);
        assert_eq!(EMPTY_INITIAL, "Start typing to search…");
        assert_eq!(EMPTY_MATCHING, "No matching files");
        assert_eq!(TITLE, "Quick Open");
    }

    #[test]
    fn compute_visible_rows_clamps_low() {
        assert_eq!(compute_visible_rows(15), 4);
        assert_eq!(compute_visible_rows(0), 4);
    }

    #[test]
    fn compute_visible_rows_clamps_high() {
        assert_eq!(compute_visible_rows(50), 8);
        assert_eq!(compute_visible_rows(100), 8);
    }

    #[test]
    fn compute_visible_rows_middle() {
        // rows 20 → 20-14 = 6 → max(4, 6) = 6 → min(8, 6) = 6
        assert_eq!(compute_visible_rows(20), 6);
    }

    #[test]
    fn preview_on_right_threshold() {
        assert!(!preview_on_right(119));
        assert!(preview_on_right(120));
        assert!(preview_on_right(200));
    }

    #[test]
    fn effective_preview_lines_table() {
        assert_eq!(effective_preview_lines(80), PREVIEW_LINES);
        assert_eq!(effective_preview_lines(120), VISIBLE_RESULTS - 1);
    }

    #[test]
    fn compute_layout_narrow() {
        let (path_width, preview_width) = compute_layout(80);
        // path width: max(20, 80-8) = 72
        assert_eq!(path_width, 72);
        // preview: 80-6 = 74
        assert_eq!(preview_width, 74);
    }

    #[test]
    fn compute_layout_wide() {
        let (path_width, preview_width) = compute_layout(140);
        // path width: max(20, floor((140-10)*0.4)) = max(20, 52) = 52
        assert_eq!(path_width, 52);
        // preview: max(40, 140-52-14) = max(40, 74) = 74
        assert_eq!(preview_width, 74);
    }

    #[test]
    fn compute_layout_tiny() {
        let (path_width, preview_width) = compute_layout(20);
        assert_eq!(path_width, 20);
        // preview: 20-6 = 14
        assert_eq!(preview_width, 14);
    }

    #[test]
    fn empty_message_table() {
        assert_eq!(empty_message(""), EMPTY_INITIAL);
        assert_eq!(empty_message("   "), EMPTY_INITIAL);
        assert_eq!(empty_message("foo"), EMPTY_MATCHING);
    }

    #[test]
    fn project_suggestions_filters_to_file_rows() {
        let items = vec![
            FileSuggestionInput {
                id: "file-1".into(),
                display_text: "src/foo.rs".into(),
            },
            FileSuggestionInput {
                id: "command-2".into(),
                display_text: "doit".into(),
            },
            FileSuggestionInput {
                id: "file-3".into(),
                display_text: "src/bar.rs".into(),
            },
        ];
        let projected = project_suggestions(&items);
        assert_eq!(
            projected,
            vec!["src/foo.rs".to_string(), "src/bar.rs".to_string()]
        );
    }

    #[test]
    fn project_suggestions_keeps_dirs() {
        let items = vec![
            FileSuggestionInput {
                id: "file-1".into(),
                display_text: "src/".into(),
            },
            FileSuggestionInput {
                id: "file-2".into(),
                display_text: "src/foo.rs".into(),
            },
        ];
        let projected = project_suggestions(&items);
        assert_eq!(
            projected,
            vec!["src/".to_string(), "src/foo.rs".to_string()]
        );
    }

    #[test]
    fn project_suggestions_normalizes_separator() {
        let items = vec![FileSuggestionInput {
            id: "file-1".into(),
            display_text: "src\\foo.rs".into(),
        }];
        let projected = project_suggestions(&items);
        assert_eq!(projected, vec!["src/foo.rs".to_string()]);
    }

    #[test]
    fn handle_select_emits_open() {
        assert_eq!(
            handle_select("src/foo.rs"),
            QuickOpenAction::OpenInEditor {
                path: "src/foo.rs".into()
            }
        );
    }

    #[test]
    fn handle_insert_mention() {
        assert_eq!(
            handle_insert("src/foo.rs", true),
            QuickOpenAction::InsertMention {
                text: "@src/foo.rs ".into()
            }
        );
    }

    #[test]
    fn handle_insert_no_mention() {
        assert_eq!(
            handle_insert("src/foo.rs", false),
            QuickOpenAction::InsertPath {
                text: "src/foo.rs ".into()
            }
        );
    }

    #[test]
    fn handle_cancel_emits_cancel() {
        assert_eq!(handle_cancel(), QuickOpenAction::Cancel);
    }
}
