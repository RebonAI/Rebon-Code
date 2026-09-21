//! Footer-pill navigation helpers.
//!
//! The AppState plumbing is out of scope. This module only resolves the pure
//! footer item order, selection sanitization, navigation, and the
//! task-pill-specific reset/clamp behavior.

/// A selectable pill in the prompt footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FooterItem {
    /// Background tasks / coordinator pill.
    Tasks,
    /// Session workflow runs pill.
    Workflows,
    /// Teams pill.
    Teams,
    /// Bridge pill.
    Bridge,
}

/// Visibility booleans for the footer pills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FooterVisibility {
    /// Whether the tasks pill is visible.
    pub tasks: bool,
    /// Whether the workflows pill is visible.
    pub workflows: bool,
    /// Whether the teams pill is visible.
    pub teams: bool,
    /// Whether the bridge pill is visible.
    pub bridge: bool,
}

/// Result of selecting a footer item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FooterSelectionUpdate {
    /// The next selected item.
    pub selection: Option<FooterItem>,
    /// Tasks selection resets teammate footer focus to zero.
    pub reset_teammate_footer_index: bool,
    /// Tasks selection also resets the coordinator index to `min_coordinator_index`.
    pub coordinator_index_after_select: Option<i32>,
}

/// Result of a footer navigation attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FooterNavigationResult {
    /// Whether navigation changed anything.
    pub changed: bool,
    /// The next selection.
    pub selection: Option<FooterItem>,
}

/// Result of entering the footer from history-down at the bottom of the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnterFooterFromHistoryResult {
    /// The pill that should become selected.
    pub selection: Option<FooterItem>,
    /// Whether the tasks hint should be marked as seen.
    pub should_mark_tasks_hint_seen: bool,
}

/// Builds the visible footer list in navigation order.
pub fn build_footer_items(visibility: FooterVisibility) -> Vec<FooterItem> {
    let mut items = Vec::new();
    if visibility.tasks {
        items.push(FooterItem::Tasks);
    }
    if visibility.workflows {
        items.push(FooterItem::Workflows);
    }
    if visibility.teams {
        items.push(FooterItem::Teams);
    }
    if visibility.bridge {
        items.push(FooterItem::Bridge);
    }
    items
}

/// The raw selection, dropped when that pill is not currently visible.
pub fn resolve_visible_footer_selection(
    raw_selection: Option<FooterItem>,
    footer_items: &[FooterItem],
) -> Option<FooterItem> {
    raw_selection.filter(|item| footer_items.contains(item))
}

/// Select a pill; selecting Tasks also resets the teammate footer index and
/// the coordinator index.
pub fn select_footer_item(
    item: Option<FooterItem>,
    min_coordinator_index: i32,
) -> FooterSelectionUpdate {
    FooterSelectionUpdate {
        selection: item,
        reset_teammate_footer_index: item == Some(FooterItem::Tasks),
        coordinator_index_after_select: (item == Some(FooterItem::Tasks))
            .then_some(min_coordinator_index),
    }
}

/// Move the selection by `delta` (±1); moving left off the first pill clears
/// the selection when `exit_at_start` is set.
pub fn navigate_footer(
    footer_items: &[FooterItem],
    current_selection: Option<FooterItem>,
    delta: i32,
    exit_at_start: bool,
) -> FooterNavigationResult {
    debug_assert!(delta == -1 || delta == 1);

    let idx = current_selection
        .and_then(|item| footer_items.iter().position(|candidate| *candidate == item))
        .map(|index| index as i32)
        .unwrap_or(-1);
    let next_idx = idx + delta;

    if next_idx >= 0 && (next_idx as usize) < footer_items.len() {
        return FooterNavigationResult {
            changed: true,
            selection: Some(footer_items[next_idx as usize]),
        };
    }

    if delta < 0 && exit_at_start {
        return FooterNavigationResult {
            changed: true,
            selection: None,
        };
    }

    FooterNavigationResult {
        changed: false,
        selection: current_selection,
    }
}

/// History-down at the bottom of the prompt selects the first footer pill.
pub fn enter_footer_from_history(
    on_history_down_returned_true: bool,
    footer_items: &[FooterItem],
    has_seen_tasks_hint: bool,
) -> Option<EnterFooterFromHistoryResult> {
    if !on_history_down_returned_true || footer_items.is_empty() {
        return None;
    }

    let first = footer_items[0];
    Some(EnterFooterFromHistoryResult {
        selection: Some(first),
        should_mark_tasks_hint_seen: first == FooterItem::Tasks && !has_seen_tasks_hint,
    })
}

/// Lowest coordinator index: `-1` when the background-task pill is shown, else `0`.
pub fn min_coordinator_index(has_background_task_pill: bool) -> i32 {
    if has_background_task_pill {
        -1
    } else {
        0
    }
}

/// Clamp the coordinator task index into range; `None` when it already is.
pub fn clamp_coordinator_task_index(
    current_index: i32,
    coordinator_task_count: usize,
    min_coordinator_index: i32,
) -> Option<i32> {
    let max_index = std::cmp::max(min_coordinator_index, coordinator_task_count as i32 - 1);
    if current_index >= coordinator_task_count as i32 {
        Some(max_index)
    } else if current_index < min_coordinator_index {
        Some(min_coordinator_index)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_footer_items_keeps_navigation_order() {
        let items = build_footer_items(FooterVisibility {
            tasks: true,
            workflows: false,
            teams: true,
            bridge: false,
        });
        assert_eq!(items, vec![FooterItem::Tasks, FooterItem::Teams]);

        let items = build_footer_items(FooterVisibility {
            tasks: true,
            workflows: true,
            teams: true,
            bridge: true,
        });
        assert_eq!(
            items,
            vec![
                FooterItem::Tasks,
                FooterItem::Workflows,
                FooterItem::Teams,
                FooterItem::Bridge,
            ]
        );
    }

    #[test]
    fn visible_footer_selection_drops_hidden_raw_selection() {
        let items = vec![FooterItem::Tasks, FooterItem::Teams];
        assert_eq!(
            resolve_visible_footer_selection(Some(FooterItem::Bridge), &items),
            None
        );
        assert_eq!(
            resolve_visible_footer_selection(Some(FooterItem::Teams), &items),
            Some(FooterItem::Teams)
        );
    }

    #[test]
    fn selecting_tasks_resets_teammate_and_coordinator_focus() {
        let update = select_footer_item(Some(FooterItem::Tasks), -1);
        assert_eq!(update.selection, Some(FooterItem::Tasks));
        assert!(update.reset_teammate_footer_index);
        assert_eq!(update.coordinator_index_after_select, Some(-1));
    }

    #[test]
    fn selecting_non_tasks_has_no_extra_resets() {
        let update = select_footer_item(Some(FooterItem::Teams), -1);
        assert_eq!(update.selection, Some(FooterItem::Teams));
        assert!(!update.reset_teammate_footer_index);
        assert_eq!(update.coordinator_index_after_select, None);
    }

    #[test]
    fn navigate_footer_moves_forward_backward_and_can_exit_at_start() {
        let items = vec![FooterItem::Tasks, FooterItem::Workflows, FooterItem::Teams];
        assert_eq!(
            navigate_footer(&items, Some(FooterItem::Tasks), 1, false),
            FooterNavigationResult {
                changed: true,
                selection: Some(FooterItem::Workflows),
            }
        );
        assert_eq!(
            navigate_footer(&items, Some(FooterItem::Workflows), -1, false),
            FooterNavigationResult {
                changed: true,
                selection: Some(FooterItem::Tasks),
            }
        );
        assert_eq!(
            navigate_footer(&items, Some(FooterItem::Tasks), -1, true),
            FooterNavigationResult {
                changed: true,
                selection: None,
            }
        );
    }

    #[test]
    fn navigate_footer_stops_at_end_without_change() {
        let items = vec![FooterItem::Tasks, FooterItem::Teams];
        assert_eq!(
            navigate_footer(&items, Some(FooterItem::Teams), 1, false),
            FooterNavigationResult {
                changed: false,
                selection: Some(FooterItem::Teams),
            }
        );
    }

    #[test]
    fn history_down_enters_first_visible_footer_item_and_marks_tasks_hint() {
        let items = vec![FooterItem::Tasks, FooterItem::Teams];
        assert_eq!(
            enter_footer_from_history(true, &items, false),
            Some(EnterFooterFromHistoryResult {
                selection: Some(FooterItem::Tasks),
                should_mark_tasks_hint_seen: true,
            })
        );
        assert_eq!(enter_footer_from_history(false, &items, false), None);
    }

    #[test]
    fn coordinator_index_min_and_clamp_sentinels() {
        assert_eq!(min_coordinator_index(true), -1);
        assert_eq!(min_coordinator_index(false), 0);
        assert_eq!(clamp_coordinator_task_index(5, 3, -1), Some(2));
        assert_eq!(clamp_coordinator_task_index(-2, 3, -1), Some(-1));
        assert_eq!(clamp_coordinator_task_index(0, 0, 0), Some(0));
        assert_eq!(clamp_coordinator_task_index(1, 3, -1), None);
    }
}
