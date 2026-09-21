/// Default number of items shown at once.
pub const DEFAULT_VISIBLE: u32 = 8;

/// Rows the picker's own chrome occupies: top padding, divider, title,
/// three gaps, the search box border and the hint line.
pub const CHROME_ROWS: u32 = 10;

/// Floor for the visible-item count, however little room is left.
pub const MIN_VISIBLE: u32 = 2;

/// Compact mode applies below this column count.
pub const COMPACT_COLUMN_THRESHOLD: u32 = 120;

/// Placeholder shown in an empty search box.
pub const DEFAULT_PLACEHOLDER: &str = "Type to search…";

/// Message shown when nothing matches.
pub const DEFAULT_EMPTY_MESSAGE: &str = "No results";

/// Action label for selecting an item.
pub const DEFAULT_SELECT_ACTION: &str = "select";

/// Score returned by the caller's fuzzy-match callback. Higher means a
/// better match; this crate never interprets the number itself.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct MatchScore(pub f64);

/// One item that survived filtering: where it came from and how it scored.
/// Higher scores rank first.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct ScoredItem {
    /// Index into the input slice.
    pub index: usize,
    /// Raw score from the callback.
    pub score: f64,
}

/// Filter and rank `items` with a caller-supplied scorer.
///
/// `score(item, query)` returns `Some(MatchScore)` for a match and `None`
/// to drop the item. Survivors are sorted by descending score using a
/// stable sort, so equal scores keep their input order.
///
/// The scorer is injected rather than imported: this crate never depends on
/// a fuzzy-matching library.
pub fn apply_fuzzy_filter<F>(items: &[&str], query: &str, mut score: F) -> Vec<ScoredItem>
where
    F: FnMut(&str, &str) -> Option<MatchScore>,
{
    let mut scored: Vec<ScoredItem> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            score(item, query).map(|s| ScoredItem {
                index: i,
                score: s.0,
            })
        })
        .collect();
    // Sort descending by score, stable so equal scores keep input
    // order.
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored
}

/// How many items the picker can show: the requested count, capped by what
/// the terminal has left after chrome, and never below [`MIN_VISIBLE`].
///
/// A match label costs one extra row, so `has_match_label` subtracts one
/// more from the space available.
pub fn compute_visible_count(requested: u32, rows: u32, has_match_label: bool) -> u32 {
    let chrome = CHROME_ROWS + if has_match_label { 1 } else { 0 };
    let cap = rows.saturating_sub(chrome);
    let bounded = requested.min(cap);
    bounded.max(MIN_VISIBLE)
}

/// True when the picker should switch to its compact layout at this column
/// width.
pub fn is_compact(columns: u32) -> bool {
    columns < COMPACT_COLUMN_THRESHOLD
}

/// Which way the picker's list grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FuzzyPickerDirection {
    /// Grows downward (the default).
    Down,
    /// Grows upward, so `items[0]` sits at the bottom.
    Up,
}

impl Default for FuzzyPickerDirection {
    fn default() -> Self {
        FuzzyPickerDirection::Down
    }
}

/// Reducer for the picker's focused row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyPickerState {
    /// Index of the row that currently has focus.
    pub focused_index: usize,
}

impl FuzzyPickerState {
    /// Fresh state with the first row focused.
    pub fn new() -> Self {
        Self { focused_index: 0 }
    }

    /// Apply a navigation event against a list of `item_count` items.
    /// Returns true when the focused index moved.
    ///
    /// The index is clamped after every move: to `0` for an empty list, and
    /// to `item_count - 1` when focus ends up past the end because the list
    /// shrank underneath it.
    pub fn step(&mut self, event: FuzzyPickerEvent, item_count: usize) -> bool {
        let old = self.focused_index;
        match event {
            FuzzyPickerEvent::Up => {
                self.focused_index = self.focused_index.saturating_sub(1);
            }
            FuzzyPickerEvent::Down => {
                if item_count > 0 {
                    self.focused_index = (self.focused_index + 1).min(item_count - 1);
                }
            }
            FuzzyPickerEvent::Reset => {
                self.focused_index = 0;
            }
        }
        // Always clamp to a valid index.
        if item_count == 0 {
            self.focused_index = 0;
        } else if self.focused_index >= item_count {
            self.focused_index = item_count - 1;
        }
        self.focused_index != old
    }
}

impl Default for FuzzyPickerState {
    fn default() -> Self {
        FuzzyPickerState::new()
    }
}

/// Navigation events the picker reducer accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuzzyPickerEvent {
    /// Move focus up one row.
    Up,
    /// Move focus down one row.
    Down,
    /// Return focus to row 0, e.g. because the query changed.
    Reset,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ────────────────────────────────────────────────────────────────
    // Constants
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn constants_pinned() {
        assert_eq!(DEFAULT_VISIBLE, 8);
        assert_eq!(CHROME_ROWS, 10);
        assert_eq!(MIN_VISIBLE, 2);
        assert_eq!(COMPACT_COLUMN_THRESHOLD, 120);
        assert_eq!(DEFAULT_PLACEHOLDER, "Type to search…");
        assert_eq!(DEFAULT_EMPTY_MESSAGE, "No results");
        assert_eq!(DEFAULT_SELECT_ACTION, "select");
    }

    // ────────────────────────────────────────────────────────────────
    // compute_visible_count
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn visible_count_uses_requested_when_room() {
        // rows=30, chrome=10, cap=20; requested=8 → 8
        assert_eq!(compute_visible_count(8, 30, false), 8);
    }

    #[test]
    fn visible_count_clamped_to_terminal_rows() {
        // rows=12, chrome=10, cap=2; requested=8 → max(MIN_VISIBLE, 2) = 2
        assert_eq!(compute_visible_count(8, 12, false), 2);
    }

    #[test]
    fn visible_count_clamped_below_to_min_visible() {
        // rows=10, chrome=10, cap=0; max(MIN_VISIBLE, 0) = 2
        assert_eq!(compute_visible_count(8, 10, false), 2);
    }

    #[test]
    fn visible_count_match_label_subtracts_one() {
        // rows=20, chrome=10+1=11, cap=9; requested=8 → 8
        assert_eq!(compute_visible_count(8, 20, true), 8);
        // rows=12, chrome=11, cap=1; → 2 (MIN_VISIBLE)
        assert_eq!(compute_visible_count(8, 12, true), 2);
    }

    #[test]
    fn visible_count_below_chrome_floor_is_min_visible() {
        // rows=5 < CHROME_ROWS = 10. saturating_sub → 0. Bounded to
        // MIN_VISIBLE.
        assert_eq!(compute_visible_count(8, 5, false), MIN_VISIBLE);
    }

    #[test]
    fn visible_count_requested_smaller_than_min_clamped_up() {
        assert_eq!(compute_visible_count(1, 100, false), MIN_VISIBLE);
    }

    #[test]
    fn visible_count_requested_zero_clamped_up_to_min() {
        assert_eq!(compute_visible_count(0, 100, false), MIN_VISIBLE);
    }

    // ────────────────────────────────────────────────────────────────
    // is_compact
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn compact_threshold_is_strict() {
        assert!(is_compact(119));
        assert!(!is_compact(120));
        assert!(!is_compact(121));
        assert!(is_compact(0));
    }

    // ────────────────────────────────────────────────────────────────
    // FuzzyPickerState reducer
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn fresh_state_focused_zero() {
        let s = FuzzyPickerState::new();
        assert_eq!(s.focused_index, 0);
    }

    #[test]
    fn down_increments_focus() {
        let mut s = FuzzyPickerState::new();
        s.step(FuzzyPickerEvent::Down, 5);
        assert_eq!(s.focused_index, 1);
    }

    #[test]
    fn up_decrements_focus() {
        let mut s = FuzzyPickerState { focused_index: 3 };
        s.step(FuzzyPickerEvent::Up, 5);
        assert_eq!(s.focused_index, 2);
    }

    #[test]
    fn up_at_zero_clamps() {
        let mut s = FuzzyPickerState::new();
        s.step(FuzzyPickerEvent::Up, 5);
        assert_eq!(s.focused_index, 0);
    }

    #[test]
    fn down_at_last_clamps() {
        let mut s = FuzzyPickerState { focused_index: 4 };
        s.step(FuzzyPickerEvent::Down, 5);
        assert_eq!(s.focused_index, 4);
    }

    #[test]
    fn reset_returns_to_zero() {
        let mut s = FuzzyPickerState { focused_index: 3 };
        s.step(FuzzyPickerEvent::Reset, 5);
        assert_eq!(s.focused_index, 0);
    }

    #[test]
    fn down_on_empty_list_stays_at_zero() {
        let mut s = FuzzyPickerState::new();
        s.step(FuzzyPickerEvent::Down, 0);
        assert_eq!(s.focused_index, 0);
    }

    #[test]
    fn focus_clamped_when_item_count_shrinks() {
        let mut s = FuzzyPickerState { focused_index: 10 };
        s.step(FuzzyPickerEvent::Up, 5);
        // Up first decrements to 9, then the post-step clamp brings
        // it down to item_count - 1 = 4.
        assert_eq!(s.focused_index, 4);
    }

    #[test]
    fn step_returns_true_on_change() {
        let mut s = FuzzyPickerState::new();
        let changed = s.step(FuzzyPickerEvent::Down, 5);
        assert!(changed);
    }

    #[test]
    fn step_returns_false_when_already_at_boundary() {
        let mut s = FuzzyPickerState::new();
        let changed = s.step(FuzzyPickerEvent::Up, 5);
        assert!(!changed);
    }

    #[test]
    fn direction_default_is_down() {
        assert_eq!(FuzzyPickerDirection::default(), FuzzyPickerDirection::Down);
    }

    // ────────────────────────────────────────────────────────────────
    // apply_fuzzy_filter (callback seam)
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn fuzzy_filter_drops_unmatched_items() {
        let items = ["apple", "banana", "cherry"];
        let scoring = |item: &str, query: &str| -> Option<MatchScore> {
            if item.contains(query) {
                Some(MatchScore(1.0))
            } else {
                None
            }
        };
        let r = apply_fuzzy_filter(&items, "an", scoring);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].index, 1);
    }

    #[test]
    fn fuzzy_filter_sorts_descending_by_score() {
        let items = ["a", "b", "c"];
        let scoring = |_: &str, q: &str| -> Option<MatchScore> {
            // Score based on which letter
            Some(MatchScore(
                q.len() as f64 - "abc".find(q).unwrap_or(0) as f64,
            ))
        };
        // Use a different scoring: 'a' -> 3, 'b' -> 2, 'c' -> 1
        let scoring2 = |item: &str, _: &str| -> Option<MatchScore> {
            Some(MatchScore(match item {
                "a" => 3.0,
                "b" => 2.0,
                "c" => 1.0,
                _ => 0.0,
            }))
        };
        let _ = scoring;
        let r = apply_fuzzy_filter(&items, "x", scoring2);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].index, 0);
        assert_eq!(r[1].index, 1);
        assert_eq!(r[2].index, 2);
    }

    #[test]
    fn fuzzy_filter_preserves_input_order_on_score_tie() {
        let items = ["a", "b", "c"];
        let scoring = |_: &str, _: &str| -> Option<MatchScore> { Some(MatchScore(1.0)) };
        let r = apply_fuzzy_filter(&items, "x", scoring);
        // Stable sort preserves order on equal scores.
        assert_eq!(r[0].index, 0);
        assert_eq!(r[1].index, 1);
        assert_eq!(r[2].index, 2);
    }

    #[test]
    fn fuzzy_filter_empty_input() {
        let items: [&str; 0] = [];
        let scoring = |_: &str, _: &str| -> Option<MatchScore> { Some(MatchScore(1.0)) };
        let r = apply_fuzzy_filter(&items, "x", scoring);
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn fuzzy_filter_no_matches() {
        let items = ["a", "b"];
        let scoring = |_: &str, _: &str| -> Option<MatchScore> { None };
        let r = apply_fuzzy_filter(&items, "x", scoring);
        assert_eq!(r.len(), 0);
    }
}
