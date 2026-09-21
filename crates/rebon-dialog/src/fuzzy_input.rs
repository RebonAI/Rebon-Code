//! Shared state and matching helpers for fuzzy input dialogs.

/// Query and selection state shared by fuzzy input dialogs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FuzzyInputState {
    query: String,
    selected: usize,
}

impl FuzzyInputState {
    /// Returns the current query.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Returns the selected result index, clamped to the available results.
    pub fn selected(&self, result_count: usize) -> Option<usize> {
        (result_count > 0).then(|| self.selected.min(result_count - 1))
    }

    /// Replaces the query and resets selection to the first result.
    pub fn set_query(&mut self, query: impl Into<String>) {
        self.query = query.into();
        self.selected = 0;
    }

    /// Clears the query and resets selection to the first result.
    pub fn reset(&mut self) {
        self.query.clear();
        self.selected = 0;
    }

    /// Clamps selection after the result set changes.
    pub fn reconcile(&mut self, result_count: usize) -> Option<usize> {
        if result_count == 0 {
            self.selected = 0;
            None
        } else {
            self.selected = self.selected.min(result_count - 1);
            Some(self.selected)
        }
    }

    /// Moves selection by `delta`, wrapping at both ends of the result set.
    pub fn move_selection(&mut self, delta: i32, result_count: usize) -> Option<usize> {
        if result_count == 0 {
            self.selected = 0;
            return None;
        }
        self.selected =
            (self.selected as i64 + i64::from(delta)).rem_euclid(result_count as i64) as usize;
        Some(self.selected)
    }
}

/// Returns whether every whitespace-delimited query token occurs in the search text.
///
/// Matching is Unicode-preserving and case-insensitive. An empty query matches.
pub fn matches_all_tokens(search_text: &str, query: &str) -> bool {
    let search_text = search_text.to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|token| search_text.contains(token))
}

/// Maps a selected item index to its rendered row when section headers precede groups.
///
/// `sections` must contain one section value per visible item, in render order.
pub fn sectioned_row_index<T: PartialEq>(sections: &[T], selected: usize) -> usize {
    let mut section_count = 0;
    let mut previous = None;
    for section in sections.iter().take(selected.saturating_add(1)) {
        if previous != Some(section) {
            section_count += 1;
            previous = Some(section);
        }
    }
    selected.saturating_add(section_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_starts_empty_at_the_first_result() {
        let state = FuzzyInputState::default();
        assert_eq!(state.query(), "");
        assert_eq!(state.selected(3), Some(0));
    }

    #[test]
    fn setting_query_resets_selection() {
        let mut state = FuzzyInputState::default();
        state.move_selection(2, 4);
        state.set_query("project");
        assert_eq!(state.query(), "project");
        assert_eq!(state.selected(4), Some(0));
    }

    #[test]
    fn reset_clears_query_and_selection() {
        let mut state = FuzzyInputState::default();
        state.set_query("project");
        state.move_selection(2, 4);
        state.reset();
        assert_eq!(state, FuzzyInputState::default());
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut state = FuzzyInputState::default();
        assert_eq!(state.move_selection(-1, 3), Some(2));
        assert_eq!(state.move_selection(1, 3), Some(0));
        assert_eq!(state.move_selection(4, 3), Some(1));
    }

    #[test]
    fn empty_results_clear_selection_without_panicking() {
        let mut state = FuzzyInputState::default();
        state.move_selection(2, 3);
        assert_eq!(state.move_selection(1, 0), None);
        assert_eq!(state.selected(0), None);
    }

    #[test]
    fn shrinking_results_clamps_selection() {
        let mut state = FuzzyInputState::default();
        state.move_selection(4, 5);
        assert_eq!(state.reconcile(2), Some(1));
        assert_eq!(state.selected(2), Some(1));
        assert_eq!(state.reconcile(0), None);
    }

    #[test]
    fn token_matching_is_case_insensitive_and_multilingual() {
        let search_text = "Projects Workspaces 项目 工作区 プロジェクト ワークスペース";
        for query in ["PROJECTS", "项目", "プロジェクト", "project workspace"] {
            assert!(matches_all_tokens(search_text, query));
        }
    }

    #[test]
    fn token_matching_requires_every_token() {
        assert!(matches_all_tokens("project workspace", "project work"));
        assert!(!matches_all_tokens("project workspace", "project moon"));
        assert!(matches_all_tokens("project workspace", "   "));
    }

    #[test]
    fn sectioned_rows_account_for_each_group_header() {
        let sections = ["navigation", "navigation", "operations", "appearance"];
        assert_eq!(sectioned_row_index(&sections, 0), 1);
        assert_eq!(sectioned_row_index(&sections, 1), 2);
        assert_eq!(sectioned_row_index(&sections, 2), 4);
        assert_eq!(sectioned_row_index(&sections, 3), 6);
    }
}
