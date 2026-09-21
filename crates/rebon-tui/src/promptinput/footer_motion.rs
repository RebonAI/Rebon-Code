//! Footer-direction keybinding logic.
//!
//! This module composes the lower-level footer navigation helpers into
//! directional key plans for the up, down, next, previous, and
//! clear-selection keys.
//!
//! ## Key-consumption semantics
//!
//! All five plans apply only while the footer keybinding context is
//! active — a footer item is selected and no modal overlay is up. When
//! that context is active, the key event is **always consumed**
//! regardless of whether the plan's internal logic produces a state
//! change. The plans here mirror this by setting `handled: true`
//! whenever the footer context would be active
//! — including boundary cases where navigation has no effect (e.g.
//! pressing → at the last pill).

use crate::promptinput::footer_navigation::{
    navigate_footer, select_footer_item, FooterItem, FooterSelectionUpdate,
};

/// Inputs for footer motion handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FooterMotionInput {
    /// Current visible footer order.
    pub footer_items: Vec<FooterItem>,
    /// Current selected footer item.
    ///
    /// **Must be the resolved (post-[`resolve_visible_footer_selection`])
    /// value**, not the raw selection stored on the app state. The resolved
    /// value is `Some(item)` only when the raw selection is set *and* the
    /// item is present in `footer_items`.
    /// Passing the raw value could navigate to
    /// a hidden pill.
    ///
    /// [`resolve_visible_footer_selection`]: crate::promptinput::footer_navigation::resolve_visible_footer_selection
    pub footer_item_selected: Option<FooterItem>,
    /// Whether tasks pill is selected.
    pub tasks_selected: bool,
    /// Whether tasks pill is in teammate mode.
    pub is_teammate_mode: bool,
    /// Number of in-process teammates (excluding leader).
    pub in_process_teammate_count: usize,
    /// Current teammate footer index.
    pub teammate_footer_index: usize,
    /// Compile-time internal build gate for coordinator row navigation.
    pub internal_build: bool,
    /// Visible coordinator task row count.
    pub coordinator_task_count: usize,
    /// Current coordinator task index.
    pub coordinator_task_index: i32,
    /// Minimum coordinator index sentinel.
    pub min_coordinator_index: i32,
}

/// Plain-data result of one footer motion key.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FooterMotionPlan {
    /// Whether the key was handled.
    pub handled: bool,
    /// Optional selection update.
    pub selection_update: Option<FooterSelectionUpdate>,
    /// Optional direct coordinator index mutation.
    pub coordinator_index: Option<i32>,
    /// Optional direct teammate footer index mutation.
    pub teammate_footer_index: Option<usize>,
    /// Whether the bashes dialog should open.
    pub show_bashes_dialog: bool,
}

/// The up plan.
pub fn resolve_footer_up(input: &FooterMotionInput) -> FooterMotionPlan {
    if input.tasks_selected
        && input.internal_build
        && input.coordinator_task_count > 0
        && input.coordinator_task_index > input.min_coordinator_index
    {
        return FooterMotionPlan {
            handled: true,
            selection_update: None,
            coordinator_index: Some(input.coordinator_task_index - 1),
            teammate_footer_index: None,
            show_bashes_dialog: false,
        };
    }

    selection_plan_from_navigation(input, -1, true)
}

/// The down plan.
pub fn resolve_footer_down(input: &FooterMotionInput) -> FooterMotionPlan {
    if input.tasks_selected && input.internal_build && input.coordinator_task_count > 0 {
        let next_index = if input.coordinator_task_index < input.coordinator_task_count as i32 - 1 {
            Some(input.coordinator_task_index + 1)
        } else {
            None
        };
        return FooterMotionPlan {
            handled: true,
            selection_update: None,
            coordinator_index: next_index,
            teammate_footer_index: None,
            show_bashes_dialog: false,
        };
    }

    if input.tasks_selected && !input.is_teammate_mode {
        return FooterMotionPlan {
            handled: true,
            selection_update: Some(select_footer_item(None, input.min_coordinator_index)),
            coordinator_index: None,
            teammate_footer_index: None,
            show_bashes_dialog: true,
        };
    }

    selection_plan_from_navigation(input, 1, false)
}

/// The next plan.
pub fn resolve_footer_next(input: &FooterMotionInput) -> FooterMotionPlan {
    if input.tasks_selected && input.is_teammate_mode {
        let total_agents = 1 + input.in_process_teammate_count;
        return FooterMotionPlan {
            handled: true,
            selection_update: None,
            coordinator_index: None,
            teammate_footer_index: Some((input.teammate_footer_index + 1) % total_agents),
            show_bashes_dialog: false,
        };
    }

    selection_plan_from_navigation(input, 1, false)
}

/// The previous plan.
pub fn resolve_footer_previous(input: &FooterMotionInput) -> FooterMotionPlan {
    if input.tasks_selected && input.is_teammate_mode {
        let total_agents = 1 + input.in_process_teammate_count;
        return FooterMotionPlan {
            handled: true,
            selection_update: None,
            coordinator_index: None,
            teammate_footer_index: Some(
                (input.teammate_footer_index + total_agents - 1) % total_agents,
            ),
            show_bashes_dialog: false,
        };
    }

    selection_plan_from_navigation(input, -1, false)
}

/// The clear-selection plan: always handled, and it clears the
/// selected footer item.
pub fn resolve_footer_clear_selection(input: &FooterMotionInput) -> FooterMotionPlan {
    FooterMotionPlan {
        handled: true,
        selection_update: Some(select_footer_item(None, input.min_coordinator_index)),
        coordinator_index: None,
        teammate_footer_index: None,
        show_bashes_dialog: false,
    }
}

fn selection_plan_from_navigation(
    input: &FooterMotionInput,
    delta: i32,
    exit_at_start: bool,
) -> FooterMotionPlan {
    let result = navigate_footer(
        &input.footer_items,
        input.footer_item_selected,
        delta,
        exit_at_start,
    );
    // Always `handled: true` — the footer keybinding context always
    // consumes the key event, even when `navigate_footer` reports no
    // movement (boundary).
    FooterMotionPlan {
        handled: true,
        selection_update: if result.changed {
            Some(select_footer_item(
                result.selection,
                input.min_coordinator_index,
            ))
        } else {
            None
        },
        coordinator_index: None,
        teammate_footer_index: None,
        show_bashes_dialog: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> FooterMotionInput {
        FooterMotionInput {
            footer_items: vec![FooterItem::Tasks, FooterItem::Workflows, FooterItem::Teams],
            footer_item_selected: Some(FooterItem::Tasks),
            tasks_selected: true,
            is_teammate_mode: false,
            in_process_teammate_count: 0,
            teammate_footer_index: 0,
            internal_build: false,
            coordinator_task_count: 0,
            coordinator_task_index: -1,
            min_coordinator_index: -1,
        }
    }

    #[test]
    fn footer_up_prefers_coordinator_row_navigation_before_leaving_tasks() {
        let mut input = base();
        input.internal_build = true;
        input.coordinator_task_count = 3;
        input.coordinator_task_index = 1;
        let plan = resolve_footer_up(&input);
        assert!(plan.handled);
        assert_eq!(plan.coordinator_index, Some(0));
        assert!(plan.selection_update.is_none());
    }

    #[test]
    fn footer_up_can_exit_selection_at_start() {
        let input = base();
        let plan = resolve_footer_up(&input);
        assert!(plan.handled);
        assert_eq!(plan.selection_update, Some(select_footer_item(None, -1)));
    }

    #[test]
    fn footer_down_scrolls_coordinator_rows_or_opens_bashes() {
        let mut rows = base();
        rows.internal_build = true;
        rows.coordinator_task_count = 3;
        rows.coordinator_task_index = 0;
        let plan = resolve_footer_down(&rows);
        assert!(plan.handled);
        assert_eq!(plan.coordinator_index, Some(1));

        let bashes = base();
        let plan = resolve_footer_down(&bashes);
        assert!(plan.handled);
        assert!(plan.show_bashes_dialog);
        assert_eq!(plan.selection_update, Some(select_footer_item(None, -1)));
    }

    #[test]
    fn footer_next_and_previous_cycle_teammate_index_in_teammate_mode() {
        let mut input = base();
        input.is_teammate_mode = true;
        input.in_process_teammate_count = 2;
        input.teammate_footer_index = 0;
        let next = resolve_footer_next(&input);
        assert_eq!(next.teammate_footer_index, Some(1));

        input.teammate_footer_index = 0;
        let previous = resolve_footer_previous(&input);
        assert_eq!(previous.teammate_footer_index, Some(2));
    }

    #[test]
    fn footer_next_and_previous_fall_back_to_footer_navigation() {
        let mut input = base();
        input.tasks_selected = false;
        input.footer_item_selected = Some(FooterItem::Workflows);
        let next = resolve_footer_next(&input);
        assert_eq!(
            next.selection_update,
            Some(select_footer_item(Some(FooterItem::Teams), -1))
        );

        let previous = resolve_footer_previous(&input);
        assert_eq!(
            previous.selection_update,
            Some(select_footer_item(Some(FooterItem::Tasks), -1))
        );
    }

    /// Pressing → at the last pill must still consume the key so it
    /// does NOT fall through to the input buffer. The footer keybinding
    /// context always consumes the key event while it is active.
    #[test]
    fn navigation_at_boundary_is_handled_but_has_no_selection_update() {
        let mut input = base();
        input.tasks_selected = false;
        input.footer_item_selected = Some(FooterItem::Teams);
        let plan = resolve_footer_next(&input);
        assert!(plan.handled, "key must be consumed even at boundary");
        assert!(
            plan.selection_update.is_none(),
            "no selection change at boundary"
        );
    }

    /// Previous at the first pill without `exit_at_start`
    /// should still consume the key but produce no selection change.
    #[test]
    fn previous_at_first_pill_without_exit_at_start_is_handled() {
        let mut input = base();
        input.tasks_selected = false;
        input.footer_item_selected = Some(FooterItem::Tasks);
        let plan = resolve_footer_previous(&input);
        assert!(plan.handled);
        assert!(plan.selection_update.is_none());
    }

    /// Clear-selection sets the selection to `None`.
    #[test]
    fn clear_selection_sets_selection_to_none() {
        let input = base();
        let plan = resolve_footer_clear_selection(&input);
        assert!(plan.handled);
        assert_eq!(plan.selection_update, Some(select_footer_item(None, -1)));
        assert!(!plan.show_bashes_dialog);
    }
}
