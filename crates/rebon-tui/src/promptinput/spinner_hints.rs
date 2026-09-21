//! Spinner and footer hint selection.
//!
//! No styling or layout lives here: this module only resolves which
//! shortcut/action pairs should be shown, and in what order.

/// Which footer detail panel is expanded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandedView {
    /// No footer detail panel is expanded.
    None,
    /// The tasks panel is expanded.
    Tasks,
    /// The teammate spinner tree is expanded.
    Teammates,
}

/// Semantic kind of spinner/footer hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpinnerHintKind {
    /// Interrupt the current assistant turn.
    Interrupt,
    /// Stop running local agents.
    StopAgents,
    /// Toggle tasks / teammate panes.
    ToggleTasks,
}

/// One resolved hint row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpinnerHintAction {
    /// Semantic kind of hint.
    pub kind: SpinnerHintKind,
    /// Shortcut text shown to the user.
    pub shortcut: String,
    /// Action label shown next to the shortcut.
    pub action: String,
}

/// Input bag for the pure spinner-hint resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpinnerHintInput {
    /// Whether a turn is in flight.
    pub is_loading: bool,
    /// Shortcut text that interrupts the current turn.
    pub esc_shortcut: String,
    /// Shortcut text that toggles the tasks / teammate panes.
    pub todos_shortcut: String,
    /// Shortcut text that stops running local agents.
    pub kill_agents_shortcut: String,
    /// True when there are task items to show.
    pub has_task_items: bool,
    /// Which panel is currently expanded.
    pub expanded_view: ExpandedView,
    /// True when there are teammate rows to cycle to.
    pub has_teammates: bool,
    /// True when any local agent task is currently running.
    pub has_running_agent_tasks: bool,
    /// True when the kill-agents confirmation banner is already showing.
    pub is_kill_agents_confirm_showing: bool,
}

/// The toggle action label for the current expansion state.
pub fn resolve_toggle_action(has_teammates: bool, expanded_view: ExpandedView) -> &'static str {
    if has_teammates {
        match expanded_view {
            ExpandedView::None => "show tasks",
            ExpandedView::Tasks => "show teammates",
            ExpandedView::Teammates => "hide",
        }
    } else if expanded_view == ExpandedView::Tasks {
        "hide tasks"
    } else {
        "show tasks"
    }
}

/// Resolve the hint rows in display order: interrupt, stop agents, toggle.
pub fn compute_spinner_hint_actions(input: &SpinnerHintInput) -> Vec<SpinnerHintAction> {
    let mut actions = Vec::new();

    if input.is_loading {
        actions.push(SpinnerHintAction {
            kind: SpinnerHintKind::Interrupt,
            shortcut: input.esc_shortcut.clone(),
            action: "interrupt".to_string(),
        });
    }

    if !input.is_loading && input.has_running_agent_tasks && !input.is_kill_agents_confirm_showing {
        actions.push(SpinnerHintAction {
            kind: SpinnerHintKind::StopAgents,
            shortcut: input.kill_agents_shortcut.clone(),
            action: "stop agents".to_string(),
        });
    }

    if input.has_task_items || input.has_teammates {
        actions.push(SpinnerHintAction {
            kind: SpinnerHintKind::ToggleTasks,
            shortcut: input.todos_shortcut.clone(),
            action: resolve_toggle_action(input.has_teammates, input.expanded_view).to_string(),
        });
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> SpinnerHintInput {
        SpinnerHintInput {
            is_loading: false,
            esc_shortcut: "esc".into(),
            todos_shortcut: "ctrl+t".into(),
            kill_agents_shortcut: "ctrl+x ctrl+k".into(),
            has_task_items: false,
            expanded_view: ExpandedView::None,
            has_teammates: false,
            has_running_agent_tasks: false,
            is_kill_agents_confirm_showing: false,
        }
    }

    #[test]
    fn toggle_action_cycles_through_teammate_views() {
        assert_eq!(
            resolve_toggle_action(true, ExpandedView::None),
            "show tasks"
        );
        assert_eq!(
            resolve_toggle_action(true, ExpandedView::Tasks),
            "show teammates"
        );
        assert_eq!(resolve_toggle_action(true, ExpandedView::Teammates), "hide");
    }

    #[test]
    fn toggle_action_without_teammates_only_toggles_tasks() {
        assert_eq!(
            resolve_toggle_action(false, ExpandedView::None),
            "show tasks"
        );
        assert_eq!(
            resolve_toggle_action(false, ExpandedView::Tasks),
            "hide tasks"
        );
    }

    #[test]
    fn loading_shows_interrupt_before_toggle() {
        let mut input = base();
        input.is_loading = true;
        input.has_task_items = true;

        assert_eq!(
            compute_spinner_hint_actions(&input),
            vec![
                SpinnerHintAction {
                    kind: SpinnerHintKind::Interrupt,
                    shortcut: "esc".into(),
                    action: "interrupt".into(),
                },
                SpinnerHintAction {
                    kind: SpinnerHintKind::ToggleTasks,
                    shortcut: "ctrl+t".into(),
                    action: "show tasks".into(),
                },
            ]
        );
    }

    #[test]
    fn stop_agents_is_hidden_while_confirmation_is_showing() {
        let mut input = base();
        input.has_running_agent_tasks = true;
        input.is_kill_agents_confirm_showing = true;

        assert!(compute_spinner_hint_actions(&input).is_empty());
    }

    #[test]
    fn stop_agents_shows_when_idle_and_agents_are_running() {
        let mut input = base();
        input.has_running_agent_tasks = true;

        assert_eq!(
            compute_spinner_hint_actions(&input),
            vec![SpinnerHintAction {
                kind: SpinnerHintKind::StopAgents,
                shortcut: "ctrl+x ctrl+k".into(),
                action: "stop agents".into(),
            }]
        );
    }
}
