//! Agents-menu top-level state machine ([`AgentsMenuState`]).
//!
//! Drives the [`crate::surface::mode_state::ModeState`] discriminated union
//! through event-driven transitions.
//!
//! This module models ONLY the pure state-machine pieces — the
//! transitions and the per-mode menu item lists. UI plumbing
//! (rendering, selectors, view-state tracking) is the consumer's
//! problem.
//!
//! ## What's covered
//!
//! * The agent-menu items list (`View / Edit / Delete / Back`) and
//!   the editability rule (built-in / plugin / flagSettings agents
//!   can only `View` + `Back`).
//! * The mode transition reducer.
//! * The "fresh agent" lookup that re-resolves the latest copy of the
//!   anchored agent from the active list.
//! * The delete confirmation transitions.
//!
//! ## Out of scope
//!
//! * Rendering and reading the live agent registry (the consumer
//!   provides the resolved agent list as a value parameter).
//! * The fully reactive change tracking (the consumer drives
//!   updates with explicit events).

use crate::surface::mode_state::ModeState;
use crate::surface::types::AgentSummary;
use crate::surface::utils::AgentSourceFilter;

/// One row in the per-agent action menu.
///
/// The order is fixed:
/// `View / [Edit / Delete] / Back`. Edit + Delete are only included
/// when the agent is editable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentMenuItem {
    /// View detail (always available).
    View,
    /// Edit agent (only for editable agents).
    Edit,
    /// Delete agent (only for editable agents).
    Delete,
    /// Back to previous mode.
    Back,
}

impl AgentMenuItem {
    /// Display label.
    pub fn label(self) -> &'static str {
        match self {
            AgentMenuItem::View => "View agent",
            AgentMenuItem::Edit => "Edit agent",
            AgentMenuItem::Delete => "Delete agent",
            AgentMenuItem::Back => "Back",
        }
    }

    /// Stable option value for this row (`view`, `edit`, …).
    pub fn value(self) -> &'static str {
        match self {
            AgentMenuItem::View => "view",
            AgentMenuItem::Edit => "edit",
            AgentMenuItem::Delete => "delete",
            AgentMenuItem::Back => "back",
        }
    }
}

/// Returns the menu items for an agent.
///
/// An agent is editable iff its
/// source is none of `built-in`, `plugin`, or `flagSettings`.
pub fn agent_menu_items(agent: &AgentSummary) -> Vec<AgentMenuItem> {
    let editable = is_editable(agent);
    let mut out = vec![AgentMenuItem::View];
    if editable {
        out.push(AgentMenuItem::Edit);
        out.push(AgentMenuItem::Delete);
    }
    out.push(AgentMenuItem::Back);
    out
}

/// Editability rule — see [`AgentSummary::is_editable`].
pub fn is_editable(agent: &AgentSummary) -> bool {
    agent.is_editable()
}

/// Look up the latest copy of `agent` in the active list, so an
/// in-place edit doesn't show stale data.
pub fn fresh_agent<'a>(
    active: &'a [AgentSummary],
    anchor: &AgentSummary,
) -> Option<&'a AgentSummary> {
    active
        .iter()
        .find(|a| a.agent_type == anchor.agent_type && a.source == anchor.source)
}

/// Top-level menu reducer state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsMenuState {
    /// Current mode.
    pub mode: ModeState,
}

impl AgentsMenuState {
    /// Default initial state — `list-agents` with `source: all`.
    pub fn default_initial() -> Self {
        AgentsMenuState {
            mode: ModeState::ListAgents {
                source: AgentSourceFilter::All,
            },
        }
    }

    /// Apply an event. Pure reducer.
    pub fn handle_event(self, event: AgentsMenuEvent) -> AgentsMenuState {
        match event {
            AgentsMenuEvent::OpenAgentMenu(agent) => AgentsMenuState {
                mode: ModeState::AgentMenu {
                    agent: Box::new(agent),
                    previous_mode: Box::new(self.mode),
                },
            },
            AgentsMenuEvent::PickAgentMenuItem(item) => self.handle_menu_pick(item),
            AgentsMenuEvent::Back => AgentsMenuState {
                mode: self.back_or_previous(),
            },
            AgentsMenuEvent::OpenCreateAgent => AgentsMenuState {
                mode: ModeState::CreateAgent,
            },
            AgentsMenuEvent::CreateCancelled => AgentsMenuState {
                mode: ModeState::ListAgents {
                    source: AgentSourceFilter::All,
                },
            },
            AgentsMenuEvent::CreateSaved => AgentsMenuState {
                mode: ModeState::ListAgents {
                    source: AgentSourceFilter::All,
                },
            },
            AgentsMenuEvent::EditSaved | AgentsMenuEvent::EditCancelled => AgentsMenuState {
                mode: self.previous_or_root(),
            },
            AgentsMenuEvent::DeleteConfirmed => {
                // After a successful delete, return to the previous
                // mode of the *delete-confirm* state, which is the
                // AgentMenu state's previous mode.
                AgentsMenuState {
                    mode: self.previous_or_root_double(),
                }
            }
            AgentsMenuEvent::DeleteCancelled => AgentsMenuState {
                mode: self.previous_or_root(),
            },
        }
    }

    fn handle_menu_pick(self, item: AgentMenuItem) -> AgentsMenuState {
        let (agent, previous) = match self.mode.clone() {
            ModeState::AgentMenu {
                agent,
                previous_mode,
            } => (agent, previous_mode),
            _ => return self,
        };

        let new_mode = match item {
            AgentMenuItem::View => ModeState::ViewAgent {
                agent,
                previous_mode: previous,
            },
            AgentMenuItem::Edit => ModeState::EditAgent {
                agent,
                previous_mode: Box::new(self.mode),
            },
            AgentMenuItem::Delete => ModeState::DeleteConfirm {
                agent,
                previous_mode: Box::new(self.mode),
            },
            AgentMenuItem::Back => *previous,
        };
        AgentsMenuState { mode: new_mode }
    }

    fn back_or_previous(&self) -> ModeState {
        match self.mode.clone() {
            ModeState::ViewAgent {
                agent,
                previous_mode,
            } => ModeState::AgentMenu {
                agent,
                previous_mode,
            },
            _ => self.previous_or_root(),
        }
    }

    fn previous_or_root(&self) -> ModeState {
        self.mode
            .previous()
            .cloned()
            .unwrap_or_else(|| ModeState::ListAgents {
                source: AgentSourceFilter::All,
            })
    }

    /// Drill DOWN through TWO previous_mode hops — used for
    /// post-delete navigation, where the user wants to land back at
    /// the *list*, not the *agent menu* of the now-deleted agent.
    fn previous_or_root_double(&self) -> ModeState {
        self.mode
            .previous()
            .and_then(|m| m.previous())
            .cloned()
            .unwrap_or_else(|| ModeState::ListAgents {
                source: AgentSourceFilter::All,
            })
    }
}

/// Events the top-level menu reducer accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentsMenuEvent {
    /// User picked an agent in the list — open the per-agent menu.
    OpenAgentMenu(AgentSummary),
    /// User picked a menu item in the agent menu.
    PickAgentMenuItem(AgentMenuItem),
    /// Generic back navigation.
    Back,
    /// User picked "Create new agent" in the list.
    OpenCreateAgent,
    /// Create wizard cancelled.
    CreateCancelled,
    /// Create wizard saved.
    CreateSaved,
    /// Editor saved successfully.
    EditSaved,
    /// Editor cancelled.
    EditCancelled,
    /// Delete confirmed by user.
    DeleteConfirmed,
    /// Delete dismissed.
    DeleteCancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::types::{AgentSource, SettingSource};

    fn editable_agent() -> AgentSummary {
        AgentSummary::minimal(
            "code-reviewer",
            "use",
            "system prompt long enough",
            AgentSource::Settings(SettingSource::UserSettings),
        )
    }

    fn built_in_agent() -> AgentSummary {
        AgentSummary::minimal(
            "general-purpose",
            "use",
            "system prompt long enough",
            AgentSource::BuiltIn,
        )
    }

    // ---- editability ----

    #[test]
    fn user_settings_is_editable() {
        assert!(is_editable(&editable_agent()));
    }

    #[test]
    fn built_in_not_editable() {
        assert!(!is_editable(&built_in_agent()));
    }

    #[test]
    fn plugin_not_editable() {
        let mut a = editable_agent();
        a.source = AgentSource::Plugin { plugin: "p".into() };
        assert!(!is_editable(&a));
    }

    #[test]
    fn flag_settings_not_editable() {
        let mut a = editable_agent();
        a.source = AgentSource::Settings(SettingSource::FlagSettings);
        assert!(!is_editable(&a));
    }

    // ---- menu items ----

    #[test]
    fn editable_menu_has_view_edit_delete_back() {
        let items = agent_menu_items(&editable_agent());
        assert_eq!(
            items,
            vec![
                AgentMenuItem::View,
                AgentMenuItem::Edit,
                AgentMenuItem::Delete,
                AgentMenuItem::Back
            ]
        );
    }

    #[test]
    fn built_in_menu_has_view_back_only() {
        let items = agent_menu_items(&built_in_agent());
        assert_eq!(items, vec![AgentMenuItem::View, AgentMenuItem::Back]);
    }

    #[test]
    fn menu_item_labels_pinned() {
        assert_eq!(AgentMenuItem::View.label(), "View agent");
        assert_eq!(AgentMenuItem::Edit.label(), "Edit agent");
        assert_eq!(AgentMenuItem::Delete.label(), "Delete agent");
        assert_eq!(AgentMenuItem::Back.label(), "Back");
    }

    #[test]
    fn menu_item_values_pinned() {
        assert_eq!(AgentMenuItem::View.value(), "view");
        assert_eq!(AgentMenuItem::Edit.value(), "edit");
        assert_eq!(AgentMenuItem::Delete.value(), "delete");
    }

    // ---- fresh_agent ----

    #[test]
    fn fresh_agent_finds_match() {
        let active = vec![editable_agent()];
        let anchor = editable_agent();
        let found = fresh_agent(&active, &anchor);
        assert!(found.is_some());
    }

    #[test]
    fn fresh_agent_returns_none_when_missing() {
        let active = vec![];
        let anchor = editable_agent();
        let found = fresh_agent(&active, &anchor);
        assert!(found.is_none());
    }

    // ---- transitions ----

    #[test]
    fn default_state_is_list_all() {
        let s = AgentsMenuState::default_initial();
        assert!(matches!(s.mode, ModeState::ListAgents { .. }));
    }

    #[test]
    fn open_agent_menu_transitions() {
        let s = AgentsMenuState::default_initial();
        let s = s.handle_event(AgentsMenuEvent::OpenAgentMenu(editable_agent()));
        assert!(matches!(s.mode, ModeState::AgentMenu { .. }));
    }

    #[test]
    fn pick_view_transitions_to_view_agent() {
        let s = AgentsMenuState::default_initial();
        let s = s.handle_event(AgentsMenuEvent::OpenAgentMenu(editable_agent()));
        let s = s.handle_event(AgentsMenuEvent::PickAgentMenuItem(AgentMenuItem::View));
        assert!(matches!(s.mode, ModeState::ViewAgent { .. }));
    }

    #[test]
    fn pick_edit_transitions_to_edit_agent() {
        let s = AgentsMenuState::default_initial();
        let s = s.handle_event(AgentsMenuEvent::OpenAgentMenu(editable_agent()));
        let s = s.handle_event(AgentsMenuEvent::PickAgentMenuItem(AgentMenuItem::Edit));
        assert!(matches!(s.mode, ModeState::EditAgent { .. }));
    }

    #[test]
    fn pick_delete_transitions_to_delete_confirm() {
        let s = AgentsMenuState::default_initial();
        let s = s.handle_event(AgentsMenuEvent::OpenAgentMenu(editable_agent()));
        let s = s.handle_event(AgentsMenuEvent::PickAgentMenuItem(AgentMenuItem::Delete));
        assert!(matches!(s.mode, ModeState::DeleteConfirm { .. }));
    }

    #[test]
    fn pick_back_returns_to_previous() {
        let s = AgentsMenuState::default_initial();
        let s = s.handle_event(AgentsMenuEvent::OpenAgentMenu(editable_agent()));
        let s = s.handle_event(AgentsMenuEvent::PickAgentMenuItem(AgentMenuItem::Back));
        assert!(matches!(s.mode, ModeState::ListAgents { .. }));
    }

    #[test]
    fn back_event_returns_to_previous() {
        let s = AgentsMenuState::default_initial();
        let s = s.handle_event(AgentsMenuEvent::OpenAgentMenu(editable_agent()));
        let s = s.handle_event(AgentsMenuEvent::Back);
        assert!(matches!(s.mode, ModeState::ListAgents { .. }));
    }

    #[test]
    fn back_from_view_returns_to_agent_menu() {
        let s = AgentsMenuState::default_initial();
        let s = s.handle_event(AgentsMenuEvent::OpenAgentMenu(editable_agent()));
        let s = s.handle_event(AgentsMenuEvent::PickAgentMenuItem(AgentMenuItem::View));
        let s = s.handle_event(AgentsMenuEvent::Back);
        assert!(matches!(s.mode, ModeState::AgentMenu { .. }));
    }

    #[test]
    fn back_from_view_preserves_original_previous_mode() {
        let s = AgentsMenuState::default_initial();
        let s = s.handle_event(AgentsMenuEvent::OpenAgentMenu(editable_agent()));
        let s = s.handle_event(AgentsMenuEvent::PickAgentMenuItem(AgentMenuItem::View));
        let s = s.handle_event(AgentsMenuEvent::Back);
        match s.mode {
            ModeState::AgentMenu { previous_mode, .. } => {
                assert!(matches!(*previous_mode, ModeState::ListAgents { .. }));
            }
            _ => panic!("expected agent menu"),
        }
    }

    #[test]
    fn back_at_root_stays_at_root() {
        let s = AgentsMenuState::default_initial();
        let s = s.handle_event(AgentsMenuEvent::Back);
        assert!(matches!(s.mode, ModeState::ListAgents { .. }));
    }

    #[test]
    fn open_create_agent_transitions() {
        let s = AgentsMenuState::default_initial();
        let s = s.handle_event(AgentsMenuEvent::OpenCreateAgent);
        assert_eq!(s.mode, ModeState::CreateAgent);
    }

    #[test]
    fn create_cancelled_returns_to_list() {
        let s = AgentsMenuState {
            mode: ModeState::CreateAgent,
        };
        let s = s.handle_event(AgentsMenuEvent::CreateCancelled);
        assert!(matches!(s.mode, ModeState::ListAgents { .. }));
    }

    #[test]
    fn create_saved_returns_to_list() {
        let s = AgentsMenuState {
            mode: ModeState::CreateAgent,
        };
        let s = s.handle_event(AgentsMenuEvent::CreateSaved);
        assert!(matches!(s.mode, ModeState::ListAgents { .. }));
    }

    #[test]
    fn delete_confirmed_returns_to_list() {
        let s = AgentsMenuState::default_initial();
        let s = s.handle_event(AgentsMenuEvent::OpenAgentMenu(editable_agent()));
        let s = s.handle_event(AgentsMenuEvent::PickAgentMenuItem(AgentMenuItem::Delete));
        // Now in DeleteConfirm. Confirming should drill back to list.
        let s = s.handle_event(AgentsMenuEvent::DeleteConfirmed);
        assert!(matches!(s.mode, ModeState::ListAgents { .. }));
    }

    #[test]
    fn delete_cancelled_returns_to_agent_menu() {
        let s = AgentsMenuState::default_initial();
        let s = s.handle_event(AgentsMenuEvent::OpenAgentMenu(editable_agent()));
        let s = s.handle_event(AgentsMenuEvent::PickAgentMenuItem(AgentMenuItem::Delete));
        let s = s.handle_event(AgentsMenuEvent::DeleteCancelled);
        // Cancelled returns to AgentMenu (the previous_mode of
        // DeleteConfirm).
        assert!(matches!(s.mode, ModeState::AgentMenu { .. }));
    }

    #[test]
    fn edit_saved_returns_to_previous_after_edit() {
        let s = AgentsMenuState::default_initial();
        let s = s.handle_event(AgentsMenuEvent::OpenAgentMenu(editable_agent()));
        let s = s.handle_event(AgentsMenuEvent::PickAgentMenuItem(AgentMenuItem::Edit));
        let s = s.handle_event(AgentsMenuEvent::EditSaved);
        // Returns to AgentMenu.
        assert!(matches!(s.mode, ModeState::AgentMenu { .. }));
    }
}
