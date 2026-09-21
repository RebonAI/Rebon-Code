//! Text-input filter over a list of recent prompts with a fuzzy fallback.
//!
//! ## Behaviour
//!
//! * The fixed title + placeholder.
//! * The `PREVIEW_ROWS = 6` and `AGE_WIDTH = 8` constants.
//! * The two-step filter: exact substring matches first, fuzzy
//!   subsequence matches second (via [`is_subsequence`]).
//! * The empty-message branch table (`Loading…`, `No matching prompts`,
//!   `No history yet`).
//!
//! ## Outbound seam
//!
//! Loading the timestamped prompt history is async and done by the caller.
//! This module accepts a pre-built [`HistoryItem`] vector and exposes
//! [`filter_items`] for the per-keystroke filter.

use crate::common::truncate_chars;

/// The dialog title.
pub const TITLE: &str = "Search prompts";

/// The filter input placeholder.
pub const PLACEHOLDER: &str = "Filter history…";

/// Number of rows shown in the preview pane.
pub const PREVIEW_ROWS: usize = 6;

/// Column width reserved for the age field.
pub const AGE_WIDTH: usize = 8;

/// Label for the "select to use" hint.
pub const SELECT_ACTION: &str = "use";

/// Pre-built history item input. The consumer builds this from the
/// timestamped prompt history. This module consumes the projected
/// fields directly so the test matrix can pin all branches without IO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryItem {
    /// The prior prompt text.
    pub display: String,
    /// `display` lowercased — pre-projected for the filter loop.
    pub lower: String,
    /// First line of `display` (split at `\n`).
    pub first_line: String,
    /// Padded relative-time-ago string (right-padded to [`AGE_WIDTH`]).
    pub age: String,
    /// Prior timestamp (epoch ms).
    pub timestamp: u64,
}

/// Loading state of the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HistoryLoadState {
    /// Initial state — items still loading from disk.
    #[default]
    Loading,
    /// Items have loaded.
    Loaded,
}

/// Build the empty-state message for the picker
/// (`No matching prompts` with a query, `No history yet` without).
pub fn empty_message(load_state: HistoryLoadState, query: &str) -> &'static str {
    match load_state {
        HistoryLoadState::Loading => "Loading…",
        HistoryLoadState::Loaded if !query.trim().is_empty() => "No matching prompts",
        HistoryLoadState::Loaded => "No history yet",
    }
}

/// True when the preview should be shown to the right of the list,
/// false when it should be shown below.
pub fn preview_on_right(columns: u16) -> bool {
    columns >= 100
}

/// Returns `(list_width, row_width, preview_width)` for the given
/// terminal width.
pub fn compute_layout(columns: u16) -> (usize, usize, usize) {
    let cols = columns as usize;
    let on_right = preview_on_right(columns);
    let list_width = if on_right {
        // Equivalent to `floor((columns - 6) * 0.5)`. Divide first
        // to avoid an underflow on small terminals.
        cols.saturating_sub(6) / 2
    } else {
        cols.saturating_sub(6)
    };
    let row_width = list_width
        .saturating_sub(AGE_WIDTH)
        .saturating_sub(1)
        .max(20);
    let preview_width = if on_right {
        cols.saturating_sub(list_width).saturating_sub(12).max(20)
    } else {
        cols.saturating_sub(10).max(20)
    };
    (list_width, row_width, preview_width)
}

/// True iff `query` is a subsequence of `text`.
pub fn is_subsequence(text: &str, query: &str) -> bool {
    let q: Vec<char> = query.chars().collect();
    if q.is_empty() {
        return true;
    }
    let mut j = 0;
    for c in text.chars() {
        if j >= q.len() {
            break;
        }
        if c == q[j] {
            j += 1;
        }
    }
    j == q.len()
}

/// Filter the items by query. Exact matches go first, fuzzy
/// (subsequence) matches go second. Empty query returns all.
pub fn filter_items(items: &[HistoryItem], query: &str) -> Vec<HistoryItem> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return items.to_vec();
    }
    let mut exact: Vec<HistoryItem> = Vec::new();
    let mut fuzzy: Vec<HistoryItem> = Vec::new();
    for item in items {
        if item.lower.contains(&q) {
            exact.push(item.clone());
        } else if is_subsequence(&item.lower, &q) {
            fuzzy.push(item.clone());
        }
    }
    exact.append(&mut fuzzy);
    exact
}

/// Build the per-row label (age + first-line truncated to row width).
pub fn build_row_label(item: &HistoryItem, row_width: usize) -> String {
    format!(
        "{} {}",
        item.age,
        truncate_chars(&item.first_line, row_width)
    )
}

/// Build the preview block from the full display, wrapped to
/// `preview_width` and clipped to [`PREVIEW_ROWS`]. Returns
/// `(shown_lines, more_count)`.
pub fn build_preview(display: &str, preview_width: usize) -> (Vec<String>, usize) {
    // Hard-wraps by chars and drops blank lines (no ANSI handling
    // because this crate doesn't import ANSI helpers).
    let wrapped: Vec<String> = display
        .lines()
        .flat_map(|line| {
            if line.is_empty() {
                Vec::new()
            } else if preview_width == 0 {
                vec![line.to_string()]
            } else {
                let chars: Vec<char> = line.chars().collect();
                chars
                    .chunks(preview_width)
                    .map(|c| c.iter().collect())
                    .collect()
            }
        })
        .filter(|s: &String| !s.trim().is_empty())
        .collect();
    let overflow = wrapped.len() > PREVIEW_ROWS;
    let take = if overflow {
        PREVIEW_ROWS - 1
    } else {
        PREVIEW_ROWS
    };
    let shown: Vec<String> = wrapped.iter().take(take).cloned().collect();
    let more = wrapped.len().saturating_sub(shown.len());
    (shown, more)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(display: &str, ts: u64) -> HistoryItem {
        let first_line = display.lines().next().unwrap_or("").to_string();
        HistoryItem {
            display: display.to_string(),
            lower: display.to_lowercase(),
            first_line,
            age: "1m      ".to_string(),
            timestamp: ts,
        }
    }

    #[test]
    fn constants_pinned() {
        assert_eq!(TITLE, "Search prompts");
        assert_eq!(PLACEHOLDER, "Filter history…");
        assert_eq!(PREVIEW_ROWS, 6);
        assert_eq!(AGE_WIDTH, 8);
        assert_eq!(SELECT_ACTION, "use");
    }

    #[test]
    fn empty_message_table() {
        assert_eq!(empty_message(HistoryLoadState::Loading, ""), "Loading…");
        assert_eq!(empty_message(HistoryLoadState::Loading, "foo"), "Loading…");
        assert_eq!(
            empty_message(HistoryLoadState::Loaded, ""),
            "No history yet"
        );
        assert_eq!(
            empty_message(HistoryLoadState::Loaded, "   "),
            "No history yet"
        );
        assert_eq!(
            empty_message(HistoryLoadState::Loaded, "foo"),
            "No matching prompts"
        );
    }

    #[test]
    fn preview_on_right_threshold() {
        assert!(!preview_on_right(99));
        assert!(preview_on_right(100));
        assert!(preview_on_right(200));
    }

    #[test]
    fn compute_layout_wide() {
        let (list, row, prev) = compute_layout(120);
        // (120 - 6) / 2 = 57
        assert_eq!(list, 57);
        // max(20, 57 - 8 - 1) = 48
        assert_eq!(row, 48);
        // max(20, 120 - 57 - 12) = 51
        assert_eq!(prev, 51);
    }

    #[test]
    fn compute_layout_narrow() {
        let (list, row, prev) = compute_layout(80);
        // 80 - 6 = 74
        assert_eq!(list, 74);
        // max(20, 74 - 9) = 65
        assert_eq!(row, 65);
        // max(20, 80 - 10) = 70
        assert_eq!(prev, 70);
    }

    #[test]
    fn compute_layout_tiny_clamps_to_min_20() {
        let (_list, row, prev) = compute_layout(20);
        assert_eq!(row, 20);
        assert_eq!(prev, 20);
    }

    #[test]
    fn is_subsequence_table() {
        assert!(is_subsequence("hello world", "hwd"));
        assert!(is_subsequence("hello world", "hello"));
        assert!(is_subsequence("hello", ""));
        assert!(!is_subsequence("hello", "world"));
        assert!(!is_subsequence("ab", "abc"));
    }

    #[test]
    fn filter_items_empty_query_returns_all() {
        let items = vec![item("foo", 1), item("bar", 2)];
        let filtered = filter_items(&items, "");
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn filter_items_exact_first() {
        let items = vec![item("foo", 1), item("bar", 2), item("foobar", 3)];
        let filtered = filter_items(&items, "foo");
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].timestamp, 1);
        assert_eq!(filtered[1].timestamp, 3);
    }

    #[test]
    fn filter_items_fuzzy_after_exact() {
        // "foo" doesn't substring-match "fxoxox" but is a subsequence.
        let items = vec![item("foo", 1), item("fxoxox", 2)];
        let filtered = filter_items(&items, "foo");
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].timestamp, 1);
        assert_eq!(filtered[1].timestamp, 2);
    }

    #[test]
    fn filter_items_no_match() {
        let items = vec![item("hello", 1)];
        let filtered = filter_items(&items, "z");
        assert!(filtered.is_empty());
    }

    #[test]
    fn filter_items_case_insensitive() {
        let items = vec![item("HELLO", 1)];
        let filtered = filter_items(&items, "hello");
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn build_row_label_pads_age_and_truncates_first_line() {
        let it = item("a very long line that should be truncated", 1);
        let label = build_row_label(&it, 10);
        assert!(label.starts_with("1m       "));
        assert!(label.ends_with("…"));
    }

    #[test]
    fn build_preview_caps_at_preview_rows() {
        let display = (0..10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (shown, more) = build_preview(&display, 80);
        // 10 lines wrapped to width 80 = 10 lines. PREVIEW_ROWS = 6.
        // overflow → take PREVIEW_ROWS - 1 = 5 lines, more = 5.
        assert_eq!(shown.len(), 5);
        assert_eq!(more, 5);
    }

    #[test]
    fn build_preview_drops_blank_lines() {
        let display = "first\n\n\nsecond";
        let (shown, more) = build_preview(display, 80);
        assert_eq!(shown, vec!["first".to_string(), "second".to_string()]);
        assert_eq!(more, 0);
    }

    #[test]
    fn build_preview_short_no_overflow() {
        let display = "one\ntwo\nthree";
        let (shown, more) = build_preview(display, 80);
        assert_eq!(shown.len(), 3);
        assert_eq!(more, 0);
    }
}
