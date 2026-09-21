//! Top-level mode-state union for the agents menu.
//!
//! The `ModeState` discriminated union of `main-menu`, `list-agents`, `agent-menu`,
//! `view-agent`, `create-agent`, `edit-agent`, `delete-confirm`.
//!
//! Each `*-with-previous` variant tracks where to go on Esc (via
//! `previous_mode`) and which agent we're acting on.

use crate::surface::types::AgentSummary;
use crate::surface::utils::AgentSourceFilter;

/// Top-level mode of the agents-menu surface. Pure state — the
/// reducer that drives transitions lives in [`crate::surface::agents_menu`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeState {
    /// Top-level menu picker.
    MainMenu,
    /// Agent list filtered to a specific source.
    ListAgents {
        /// Which source the list is filtered to.
        source: AgentSourceFilter,
    },
    /// Per-agent action menu (view / edit / delete).
    AgentMenu {
        /// The agent the menu is anchored on.
        agent: Box<AgentSummary>,
        /// Where to return to on Esc.
        previous_mode: Box<ModeState>,
    },
    /// Read-only detail view.
    ViewAgent {
        /// The agent being viewed.
        agent: Box<AgentSummary>,
        /// Where to return to on Esc.
        previous_mode: Box<ModeState>,
    },
    /// New agent creation wizard.
    CreateAgent,
    /// Edit-in-place flow for an existing agent.
    EditAgent {
        /// The agent being edited.
        agent: Box<AgentSummary>,
        /// Where to return to on Esc.
        previous_mode: Box<ModeState>,
    },
    /// Delete-confirmation dialog.
    DeleteConfirm {
        /// The agent the user is about to delete.
        agent: Box<AgentSummary>,
        /// Where to return to on Esc.
        previous_mode: Box<ModeState>,
    },
}

impl ModeState {
    /// Returns the "previous" mode for variants that track one. Used
    /// when the user presses Esc.
    pub fn previous(&self) -> Option<&ModeState> {
        match self {
            ModeState::AgentMenu { previous_mode, .. }
            | ModeState::ViewAgent { previous_mode, .. }
            | ModeState::EditAgent { previous_mode, .. }
            | ModeState::DeleteConfirm { previous_mode, .. } => Some(previous_mode.as_ref()),
            _ => None,
        }
    }

    /// True if this mode wraps an agent (used to know whether to show
    /// the agent header).
    pub fn anchored_agent(&self) -> Option<&AgentSummary> {
        match self {
            ModeState::AgentMenu { agent, .. }
            | ModeState::ViewAgent { agent, .. }
            | ModeState::EditAgent { agent, .. }
            | ModeState::DeleteConfirm { agent, .. } => Some(agent.as_ref()),
            _ => None,
        }
    }

    /// Stable kebab-case name of the mode.
    pub fn discriminator(&self) -> &'static str {
        match self {
            ModeState::MainMenu => "main-menu",
            ModeState::ListAgents { .. } => "list-agents",
            ModeState::AgentMenu { .. } => "agent-menu",
            ModeState::ViewAgent { .. } => "view-agent",
            ModeState::CreateAgent => "create-agent",
            ModeState::EditAgent { .. } => "edit-agent",
            ModeState::DeleteConfirm { .. } => "delete-confirm",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::types::AgentSource;

    fn sample_agent() -> AgentSummary {
        AgentSummary::minimal(
            "code-reviewer",
            "Use this agent when reviewing code",
            "You are a code reviewer.",
            AgentSource::Settings(crate::surface::types::SettingSource::UserSettings),
        )
    }

    #[test]
    fn main_menu_has_no_previous() {
        assert!(ModeState::MainMenu.previous().is_none());
    }

    #[test]
    fn list_agents_has_no_previous() {
        let m = ModeState::ListAgents {
            source: AgentSourceFilter::All,
        };
        assert!(m.previous().is_none());
    }

    #[test]
    fn create_agent_has_no_previous() {
        assert!(ModeState::CreateAgent.previous().is_none());
    }

    #[test]
    fn agent_menu_tracks_previous() {
        let prev = ModeState::ListAgents {
            source: AgentSourceFilter::All,
        };
        let m = ModeState::AgentMenu {
            agent: Box::new(sample_agent()),
            previous_mode: Box::new(prev.clone()),
        };
        assert_eq!(m.previous(), Some(&prev));
    }

    #[test]
    fn view_agent_tracks_previous_and_anchor() {
        let prev = ModeState::MainMenu;
        let agent = sample_agent();
        let m = ModeState::ViewAgent {
            agent: Box::new(agent.clone()),
            previous_mode: Box::new(prev.clone()),
        };
        assert_eq!(m.previous(), Some(&prev));
        assert_eq!(m.anchored_agent(), Some(&agent));
    }

    #[test]
    fn edit_agent_tracks_previous_and_anchor() {
        let agent = sample_agent();
        let m = ModeState::EditAgent {
            agent: Box::new(agent.clone()),
            previous_mode: Box::new(ModeState::MainMenu),
        };
        assert_eq!(m.anchored_agent(), Some(&agent));
    }

    #[test]
    fn delete_confirm_tracks_previous_and_anchor() {
        let agent = sample_agent();
        let m = ModeState::DeleteConfirm {
            agent: Box::new(agent.clone()),
            previous_mode: Box::new(ModeState::MainMenu),
        };
        assert_eq!(m.anchored_agent(), Some(&agent));
    }

    #[test]
    fn discriminator_table() {
        assert_eq!(ModeState::MainMenu.discriminator(), "main-menu");
        assert_eq!(
            ModeState::ListAgents {
                source: AgentSourceFilter::All
            }
            .discriminator(),
            "list-agents"
        );
        assert_eq!(ModeState::CreateAgent.discriminator(), "create-agent");
    }

    #[test]
    fn create_agent_has_no_anchor() {
        assert!(ModeState::CreateAgent.anchored_agent().is_none());
    }

    #[test]
    fn main_menu_has_no_anchor() {
        assert!(ModeState::MainMenu.anchored_agent().is_none());
    }
}
