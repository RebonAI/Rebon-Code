//! Agent server menu state machine.
//!
//! Agent servers (the `sdk` variant) are simpler than stdio/remote —
//! they only require a name. The Rust module includes this as a
//! five-step wizard (EditingName → Validating → Saving → Saved/Error).

use crate::runtime::config::{
    ConfigScope, McpSdkServerConfig, ScopedMcpServerConfig, ServerConfigKind,
};

/// The current step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentMenuStep {
    EditingName,
    Validating,
    Saving,
    Saved,
    Error,
}

/// The menu state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMenuState {
    pub step: AgentMenuStep,
    pub name: String,
    pub error: Option<String>,
}

impl Default for AgentMenuState {
    fn default() -> Self {
        Self {
            step: AgentMenuStep::EditingName,
            name: String::new(),
            error: None,
        }
    }
}

/// Events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentMenuEvent {
    SetName(String),
    Next,
    Back,
    Submit,
    SaveSucceeded,
    SaveFailed(String),
    Reset,
}

impl AgentMenuState {
    pub fn apply(mut self, event: AgentMenuEvent) -> Self {
        match event {
            AgentMenuEvent::SetName(n) => {
                self.name = n;
                self.error = None;
            }
            AgentMenuEvent::Next => {
                if self.step == AgentMenuStep::EditingName {
                    if self.name.trim().is_empty() {
                        self.error = Some("Name cannot be empty".into());
                        return self;
                    }
                    self.step = AgentMenuStep::Validating;
                }
            }
            AgentMenuEvent::Back => match self.step {
                AgentMenuStep::Validating => self.step = AgentMenuStep::EditingName,
                AgentMenuStep::Error => {
                    self.step = AgentMenuStep::EditingName;
                    self.error = None;
                }
                _ => {}
            },
            AgentMenuEvent::Submit => {
                if self.step == AgentMenuStep::Validating {
                    self.step = AgentMenuStep::Saving;
                }
            }
            AgentMenuEvent::SaveSucceeded => {
                if self.step == AgentMenuStep::Saving {
                    self.step = AgentMenuStep::Saved;
                }
            }
            AgentMenuEvent::SaveFailed(msg) => {
                self.error = Some(msg);
                self.step = AgentMenuStep::Error;
            }
            AgentMenuEvent::Reset => {
                self = Self::default();
            }
        }
        self
    }

    pub fn try_build_config(&self) -> Result<McpSdkServerConfig, &'static str> {
        if self.name.trim().is_empty() {
            return Err("Name cannot be empty");
        }
        Ok(McpSdkServerConfig {
            name: self.name.clone(),
        })
    }

    pub fn build_scoped(&self, scope: ConfigScope) -> Result<ScopedMcpServerConfig, &'static str> {
        let cfg = self.try_build_config()?;
        Ok(ScopedMcpServerConfig {
            config: ServerConfigKind::Sdk(cfg),
            scope,
            plugin_source: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state() {
        let s = AgentMenuState::default();
        assert_eq!(s.step, AgentMenuStep::EditingName);
        assert!(s.name.is_empty());
        assert!(s.error.is_none());
    }

    #[test]
    fn set_name_clears_error() {
        let mut s = AgentMenuState::default();
        s.error = Some("old".into());
        let s = s.apply(AgentMenuEvent::SetName("agent".into()));
        assert_eq!(s.name, "agent");
        assert!(s.error.is_none());
    }

    #[test]
    fn next_with_empty_name_errors() {
        let s = AgentMenuState::default().apply(AgentMenuEvent::Next);
        assert_eq!(s.step, AgentMenuStep::EditingName);
        assert!(s.error.is_some());
    }

    #[test]
    fn next_with_whitespace_only_name_errors() {
        let s = AgentMenuState::default()
            .apply(AgentMenuEvent::SetName("   ".into()))
            .apply(AgentMenuEvent::Next);
        assert_eq!(s.step, AgentMenuStep::EditingName);
        assert!(s.error.is_some());
    }

    #[test]
    fn happy_path() {
        let s = AgentMenuState::default()
            .apply(AgentMenuEvent::SetName("agent".into()))
            .apply(AgentMenuEvent::Next);
        assert_eq!(s.step, AgentMenuStep::Validating);
        let s = s.apply(AgentMenuEvent::Submit);
        assert_eq!(s.step, AgentMenuStep::Saving);
        let s = s.apply(AgentMenuEvent::SaveSucceeded);
        assert_eq!(s.step, AgentMenuStep::Saved);
    }

    #[test]
    fn save_failed_to_error() {
        let s = AgentMenuState {
            step: AgentMenuStep::Saving,
            name: "x".into(),
            error: None,
        };
        let s = s.apply(AgentMenuEvent::SaveFailed("oops".into()));
        assert_eq!(s.step, AgentMenuStep::Error);
        assert_eq!(s.error.as_deref(), Some("oops"));
    }

    #[test]
    fn back_from_validating() {
        let s = AgentMenuState {
            step: AgentMenuStep::Validating,
            name: "x".into(),
            error: None,
        };
        let s = s.apply(AgentMenuEvent::Back);
        assert_eq!(s.step, AgentMenuStep::EditingName);
    }

    #[test]
    fn back_from_error_clears_error() {
        let s = AgentMenuState {
            step: AgentMenuStep::Error,
            name: "x".into(),
            error: Some("e".into()),
        };
        let s = s.apply(AgentMenuEvent::Back);
        assert_eq!(s.step, AgentMenuStep::EditingName);
        assert!(s.error.is_none());
    }

    #[test]
    fn reset() {
        let s = AgentMenuState {
            step: AgentMenuStep::Saved,
            name: "x".into(),
            error: None,
        };
        let s = s.apply(AgentMenuEvent::Reset);
        assert_eq!(s, AgentMenuState::default());
    }

    #[test]
    fn try_build_config_requires_name() {
        assert!(AgentMenuState::default().try_build_config().is_err());
    }

    #[test]
    fn try_build_config_happy() {
        let s = AgentMenuState {
            step: AgentMenuStep::EditingName,
            name: "agent".into(),
            error: None,
        };
        let cfg = s.try_build_config().unwrap();
        assert_eq!(cfg.name, "agent");
    }

    #[test]
    fn build_scoped_wraps_in_sdk_variant() {
        let s = AgentMenuState {
            step: AgentMenuStep::EditingName,
            name: "agent".into(),
            error: None,
        };
        let scoped = s.build_scoped(ConfigScope::User).unwrap();
        assert_eq!(scoped.scope, ConfigScope::User);
        assert!(matches!(scoped.config, ServerConfigKind::Sdk(_)));
    }
}
