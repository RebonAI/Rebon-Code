//! Up / Down history-navigation decisions for the prompt input.
//!
//! The actual history mutation (moving up or down in history) and queued
//! command pop are left to the caller; this module only chooses which path to
//! take.

use crate::promptinput::footer_navigation::{
    enter_footer_from_history, EnterFooterFromHistoryResult, FooterItem,
};

/// Pure result of handling Up in the prompt input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryUpAction {
    /// Do nothing.
    None,
    /// Move the queued command into the input for editing.
    PopQueuedCommand,
    /// Ask the history controller to move up.
    NavigateHistoryUp,
}

/// Pure result of handling Down in the prompt input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryDownAction {
    /// Do nothing.
    None,
    /// Ask the history controller to move down only.
    NavigateHistoryDownOnly,
    /// Enter the footer after history-down reached the bottom.
    EnterFooter(EnterFooterFromHistoryResult),
}

/// Up on the first line: pop an editable queued command if there is one,
/// otherwise move up in history. Ignored while more than one suggestion shows.
pub fn resolve_history_up_action(
    suggestion_count: usize,
    is_cursor_on_first_line: bool,
    has_editable_queued_command: bool,
) -> HistoryUpAction {
    if suggestion_count > 1 || !is_cursor_on_first_line {
        return HistoryUpAction::None;
    }
    if has_editable_queued_command {
        return HistoryUpAction::PopQueuedCommand;
    }
    HistoryUpAction::NavigateHistoryUp
}

/// Down on the last line: move down in history, entering the footer when
/// history-down reports it is already at the bottom. Ignored while more than
/// one suggestion shows.
pub fn resolve_history_down_action(
    suggestion_count: usize,
    is_cursor_on_last_line: bool,
    on_history_down_returned_true: bool,
    footer_items: &[FooterItem],
    has_seen_tasks_hint: bool,
) -> HistoryDownAction {
    if suggestion_count > 1 || !is_cursor_on_last_line {
        return HistoryDownAction::None;
    }

    match enter_footer_from_history(
        on_history_down_returned_true,
        footer_items,
        has_seen_tasks_hint,
    ) {
        Some(result) => HistoryDownAction::EnterFooter(result),
        None => HistoryDownAction::NavigateHistoryDownOnly,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_up_is_blocked_by_multi_suggestions_or_non_first_line() {
        assert_eq!(
            resolve_history_up_action(2, true, false),
            HistoryUpAction::None
        );
        assert_eq!(
            resolve_history_up_action(0, false, false),
            HistoryUpAction::None
        );
    }

    #[test]
    fn history_up_prefers_queued_command_before_history() {
        assert_eq!(
            resolve_history_up_action(0, true, true),
            HistoryUpAction::PopQueuedCommand
        );
        assert_eq!(
            resolve_history_up_action(1, true, false),
            HistoryUpAction::NavigateHistoryUp
        );
    }

    #[test]
    fn history_down_is_blocked_by_multi_suggestions_or_non_last_line() {
        let items = vec![FooterItem::Tasks];
        assert_eq!(
            resolve_history_down_action(2, true, true, &items, false),
            HistoryDownAction::None
        );
        assert_eq!(
            resolve_history_down_action(0, false, true, &items, false),
            HistoryDownAction::None
        );
    }

    #[test]
    fn history_down_enters_footer_when_bottom_reached() {
        let items = vec![FooterItem::Tasks, FooterItem::Teams];
        assert_eq!(
            resolve_history_down_action(0, true, true, &items, false),
            HistoryDownAction::EnterFooter(EnterFooterFromHistoryResult {
                selection: Some(FooterItem::Tasks),
                should_mark_tasks_hint_seen: true,
            })
        );
    }

    #[test]
    fn history_down_without_footer_entry_just_navigates_history() {
        assert_eq!(
            resolve_history_down_action(0, true, false, &[], false),
            HistoryDownAction::NavigateHistoryDownOnly
        );
    }
}
