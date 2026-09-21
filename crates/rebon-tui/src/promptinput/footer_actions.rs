//! The decision tree behind the footer's open-selected and close keys.
//!
//! The actual AppState mutation, dialogs, task stop/dismiss, and submit calls
//! remain caller-owned. This module only resolves which action should happen.

use crate::promptinput::footer_navigation::FooterItem;

/// Minimal visible task row used by footer actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleFooterTask {
    /// Task id.
    pub id: String,
    /// Task status string (`running`, `completed`, etc.).
    pub status: String,
}

/// Inputs for [`resolve_footer_open_selected_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FooterOpenSelectedInput {
    /// Currently selected footer item.
    pub footer_item_selected: Option<FooterItem>,
    /// Current view selection mode; `selecting-agent` suppresses the action.
    pub view_selection_mode: String,
    /// Whether tasks pill is in teammate mode.
    pub is_teammate_mode: bool,
    /// Current teammate footer index (0 = leader).
    pub teammate_footer_index: usize,
    /// Running in-process teammate ids in footer order (excluding leader).
    pub in_process_teammate_ids: Vec<String>,
    /// Current coordinator task index.
    pub coordinator_task_index: i32,
    /// Visible coordinator task rows in order.
    pub visible_agent_tasks: Vec<VisibleFooterTask>,
}

/// Outcome of [`resolve_footer_open_selected_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FooterOpenSelectedAction {
    /// Nothing handled.
    None,
    /// Clear footer selection only.
    ClearSelection,
    /// Exit teammate view.
    ExitTeammateView,
    /// Enter teammate/agent view for the given task id.
    EnterTeammateView {
        /// Task id to foreground/view.
        task_id: String,
    },
    /// Open bashes dialog and clear selection.
    ShowBashesDialog,
    /// Open teams dialog and clear selection.
    ShowTeamsDialog,
    /// Open bridge dialog and clear selection.
    ShowBridgeDialog,
}

/// Inputs for [`resolve_footer_close_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FooterCloseInput {
    /// Whether tasks pill is selected.
    pub tasks_selected: bool,
    /// Current coordinator task index.
    pub coordinator_task_index: i32,
    /// Visible coordinator task rows in order.
    pub visible_agent_tasks: Vec<VisibleFooterTask>,
    /// Current view selection mode; `selecting-agent` suppresses the action.
    pub view_selection_mode: String,
    /// Agent task currently being viewed, if any.
    pub viewing_agent_task_id: Option<String>,
}

/// Outcome of [`resolve_footer_close_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FooterCloseAction {
    /// Not handled; let the key fall through to normal typing.
    NotHandled,
    /// Insert literal `x` into the prompt input.
    InsertLiteralX,
    /// Stop or dismiss the selected agent task.
    StopOrDismissAgent {
        /// Task id to stop/dismiss.
        task_id: String,
        /// Whether to step the coordinator index back: true when the task is not running.
        should_step_coordinator_back: bool,
    },
}

/// Decide what the footer's open-selected key does.
pub fn resolve_footer_open_selected_action(
    input: &FooterOpenSelectedInput,
) -> FooterOpenSelectedAction {
    if input.view_selection_mode == "selecting-agent" {
        return FooterOpenSelectedAction::None;
    }

    match input.footer_item_selected {
        Some(FooterItem::Tasks) => {
            if input.is_teammate_mode {
                if input.teammate_footer_index == 0 {
                    FooterOpenSelectedAction::ExitTeammateView
                } else if let Some(task_id) = input
                    .in_process_teammate_ids
                    .get(input.teammate_footer_index - 1)
                    .cloned()
                {
                    FooterOpenSelectedAction::EnterTeammateView { task_id }
                } else {
                    FooterOpenSelectedAction::None
                }
            } else if input.coordinator_task_index == 0 && !input.visible_agent_tasks.is_empty() {
                FooterOpenSelectedAction::ExitTeammateView
            } else {
                let idx = input.coordinator_task_index.saturating_sub(1) as usize;
                if let Some(task) = input.visible_agent_tasks.get(idx) {
                    FooterOpenSelectedAction::EnterTeammateView {
                        task_id: task.id.clone(),
                    }
                } else {
                    FooterOpenSelectedAction::ShowBashesDialog
                }
            }
        }
        Some(FooterItem::Workflows) => FooterOpenSelectedAction::None,
        Some(FooterItem::Teams) => FooterOpenSelectedAction::ShowTeamsDialog,
        Some(FooterItem::Bridge) => FooterOpenSelectedAction::ShowBridgeDialog,
        None => FooterOpenSelectedAction::None,
    }
}

/// Decide what the footer's close key does.
pub fn resolve_footer_close_action(input: &FooterCloseInput) -> FooterCloseAction {
    if !input.tasks_selected || input.coordinator_task_index < 1 {
        return FooterCloseAction::NotHandled;
    }

    let idx = (input.coordinator_task_index - 1) as usize;
    let Some(task) = input.visible_agent_tasks.get(idx) else {
        return FooterCloseAction::NotHandled;
    };

    if input.view_selection_mode == "viewing-agent"
        && input.viewing_agent_task_id.as_deref() == Some(task.id.as_str())
    {
        return FooterCloseAction::InsertLiteralX;
    }

    FooterCloseAction::StopOrDismissAgent {
        task_id: task.id.clone(),
        should_step_coordinator_back: task.status != "running",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: &str) -> VisibleFooterTask {
        VisibleFooterTask {
            id: id.into(),
            status: status.into(),
        }
    }

    fn base_open() -> FooterOpenSelectedInput {
        FooterOpenSelectedInput {
            footer_item_selected: None,
            view_selection_mode: "none".into(),
            is_teammate_mode: false,
            teammate_footer_index: 0,
            in_process_teammate_ids: vec![],
            coordinator_task_index: -1,
            visible_agent_tasks: vec![],
        }
    }

    #[test]
    fn selecting_agent_mode_blocks_open_selected() {
        let mut input = base_open();
        input.view_selection_mode = "selecting-agent".into();
        input.footer_item_selected = Some(FooterItem::Tasks);
        assert_eq!(
            resolve_footer_open_selected_action(&input),
            FooterOpenSelectedAction::None
        );
    }

    #[test]
    fn tasks_open_handles_teammate_mode_and_coordinator_mode() {
        let mut input = base_open();
        input.footer_item_selected = Some(FooterItem::Tasks);
        input.is_teammate_mode = true;
        input.teammate_footer_index = 0;
        assert_eq!(
            resolve_footer_open_selected_action(&input),
            FooterOpenSelectedAction::ExitTeammateView
        );

        input.teammate_footer_index = 1;
        input.in_process_teammate_ids = vec!["t-1".into()];
        assert_eq!(
            resolve_footer_open_selected_action(&input),
            FooterOpenSelectedAction::EnterTeammateView {
                task_id: "t-1".into()
            }
        );

        input.is_teammate_mode = false;
        input.visible_agent_tasks = vec![task("a-1", "running")];
        input.coordinator_task_index = 0;
        assert_eq!(
            resolve_footer_open_selected_action(&input),
            FooterOpenSelectedAction::ExitTeammateView
        );

        input.coordinator_task_index = 1;
        assert_eq!(
            resolve_footer_open_selected_action(&input),
            FooterOpenSelectedAction::EnterTeammateView {
                task_id: "a-1".into()
            }
        );

        input.coordinator_task_index = -1;
        input.visible_agent_tasks.clear();
        assert_eq!(
            resolve_footer_open_selected_action(&input),
            FooterOpenSelectedAction::ShowBashesDialog
        );
    }

    #[test]
    fn teams_bridge_and_workflows_open_actions_are_explicit() {
        let mut input = base_open();
        input.footer_item_selected = Some(FooterItem::Teams);
        assert_eq!(
            resolve_footer_open_selected_action(&input),
            FooterOpenSelectedAction::ShowTeamsDialog
        );
        input.footer_item_selected = Some(FooterItem::Bridge);
        assert_eq!(
            resolve_footer_open_selected_action(&input),
            FooterOpenSelectedAction::ShowBridgeDialog
        );
        input.footer_item_selected = Some(FooterItem::Workflows);
        assert_eq!(
            resolve_footer_open_selected_action(&input),
            FooterOpenSelectedAction::None
        );
    }

    #[test]
    fn footer_close_returns_not_handled_when_not_on_task_row() {
        assert_eq!(
            resolve_footer_close_action(&FooterCloseInput {
                tasks_selected: false,
                coordinator_task_index: 1,
                visible_agent_tasks: vec![task("a-1", "running")],
                view_selection_mode: "none".into(),
                viewing_agent_task_id: None,
            }),
            FooterCloseAction::NotHandled
        );
        assert_eq!(
            resolve_footer_close_action(&FooterCloseInput {
                tasks_selected: true,
                coordinator_task_index: 0,
                visible_agent_tasks: vec![task("a-1", "running")],
                view_selection_mode: "none".into(),
                viewing_agent_task_id: None,
            }),
            FooterCloseAction::NotHandled
        );
    }

    #[test]
    fn footer_close_inserts_x_when_selected_row_is_viewed_agent() {
        let action = resolve_footer_close_action(&FooterCloseInput {
            tasks_selected: true,
            coordinator_task_index: 1,
            visible_agent_tasks: vec![task("a-1", "running")],
            view_selection_mode: "viewing-agent".into(),
            viewing_agent_task_id: Some("a-1".into()),
        });
        assert_eq!(action, FooterCloseAction::InsertLiteralX);
    }

    #[test]
    fn footer_close_stops_or_dismisses_other_tasks_and_steps_back_when_not_running() {
        let running = resolve_footer_close_action(&FooterCloseInput {
            tasks_selected: true,
            coordinator_task_index: 1,
            visible_agent_tasks: vec![task("a-1", "running")],
            view_selection_mode: "viewing-agent".into(),
            viewing_agent_task_id: Some("other".into()),
        });
        assert_eq!(
            running,
            FooterCloseAction::StopOrDismissAgent {
                task_id: "a-1".into(),
                should_step_coordinator_back: false
            }
        );

        let completed = resolve_footer_close_action(&FooterCloseInput {
            tasks_selected: true,
            coordinator_task_index: 1,
            visible_agent_tasks: vec![task("a-2", "completed")],
            view_selection_mode: "none".into(),
            viewing_agent_task_id: None,
        });
        assert_eq!(
            completed,
            FooterCloseAction::StopOrDismissAgent {
                task_id: "a-2".into(),
                should_step_coordinator_back: true
            }
        );
    }
}
