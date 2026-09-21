//! Stdio server menu state machine.
//!
//! The Rust module is a wizard:
//! 1. EditingName — asking for the server name
//! 2. EditingCommand — asking for the command
//! 3. EditingArgs — asking for args (one per line)
//! 4. EditingEnv — asking for env vars
//! 5. Validating — validating the collected config
//! 6. Saving — writing the config
//! 7. Saved — happy path terminal
//! 8. Error — validation / write failed
//!
//! The Rust module includes these as a [`StdioMenuStep`] enum and a
//! [`StdioMenuState`] reducer. Each transition is pinned by a test.

use crate::runtime::config::{McpStdioServerConfig, ScopedMcpServerConfig, ServerConfigKind};
use std::collections::BTreeMap;

/// The current step in the wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StdioMenuStep {
    EditingName,
    EditingCommand,
    EditingArgs,
    EditingEnv,
    Validating,
    Saving,
    Saved,
    Error,
}

/// Draft state accumulated as the user walks through the wizard.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StdioMenuDraft {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// The full menu state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdioMenuState {
    pub step: StdioMenuStep,
    pub draft: StdioMenuDraft,
    pub error: Option<String>,
}

impl Default for StdioMenuState {
    fn default() -> Self {
        Self {
            step: StdioMenuStep::EditingName,
            draft: StdioMenuDraft::default(),
            error: None,
        }
    }
}

/// Events driving the menu reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdioMenuEvent {
    SetName(String),
    SetCommand(String),
    SetArgs(Vec<String>),
    SetEnv(BTreeMap<String, String>),
    Next,
    Back,
    Submit,
    /// The async save completed successfully.
    SaveSucceeded,
    /// The async save failed.
    SaveFailed(String),
    Reset,
}

impl StdioMenuState {
    /// Apply an event and yield the next state.
    pub fn apply(mut self, event: StdioMenuEvent) -> Self {
        match event {
            StdioMenuEvent::SetName(n) => {
                self.draft.name = n;
                self.error = None;
            }
            StdioMenuEvent::SetCommand(c) => {
                self.draft.command = c;
                self.error = None;
            }
            StdioMenuEvent::SetArgs(a) => {
                self.draft.args = a;
                self.error = None;
            }
            StdioMenuEvent::SetEnv(e) => {
                self.draft.env = e;
                self.error = None;
            }
            StdioMenuEvent::Next => {
                self.step = match self.step {
                    StdioMenuStep::EditingName => {
                        if self.draft.name.trim().is_empty() {
                            self.error = Some("Name cannot be empty".into());
                            return self;
                        }
                        StdioMenuStep::EditingCommand
                    }
                    StdioMenuStep::EditingCommand => {
                        if self.draft.command.trim().is_empty() {
                            self.error = Some("Command cannot be empty".into());
                            return self;
                        }
                        StdioMenuStep::EditingArgs
                    }
                    StdioMenuStep::EditingArgs => StdioMenuStep::EditingEnv,
                    StdioMenuStep::EditingEnv => StdioMenuStep::Validating,
                    other => other,
                };
            }
            StdioMenuEvent::Back => {
                self.step = match self.step {
                    StdioMenuStep::EditingCommand => StdioMenuStep::EditingName,
                    StdioMenuStep::EditingArgs => StdioMenuStep::EditingCommand,
                    StdioMenuStep::EditingEnv => StdioMenuStep::EditingArgs,
                    StdioMenuStep::Validating => StdioMenuStep::EditingEnv,
                    StdioMenuStep::Error => StdioMenuStep::EditingName,
                    other => other,
                };
                self.error = None;
            }
            StdioMenuEvent::Submit => {
                if self.step == StdioMenuStep::Validating {
                    self.step = StdioMenuStep::Saving;
                }
            }
            StdioMenuEvent::SaveSucceeded => {
                if self.step == StdioMenuStep::Saving {
                    self.step = StdioMenuStep::Saved;
                }
            }
            StdioMenuEvent::SaveFailed(msg) => {
                self.error = Some(msg);
                self.step = StdioMenuStep::Error;
            }
            StdioMenuEvent::Reset => {
                self = Self::default();
            }
        }
        self
    }

    /// Attempt to build the final config. Returns `Err` if the draft
    /// is incomplete. This is the wizard's final validation step.
    pub fn try_build_config(&self) -> Result<McpStdioServerConfig, &'static str> {
        if self.draft.command.trim().is_empty() {
            return Err("Command cannot be empty");
        }
        Ok(McpStdioServerConfig {
            command: self.draft.command.clone(),
            args: self.draft.args.clone(),
            env: if self.draft.env.is_empty() {
                None
            } else {
                Some(self.draft.env.clone())
            },
        })
    }

    /// Build a scoped config (adds scope). Used at Save step.
    pub fn build_scoped(
        &self,
        scope: crate::runtime::config::ConfigScope,
    ) -> Result<ScopedMcpServerConfig, &'static str> {
        let cfg = self.try_build_config()?;
        Ok(ScopedMcpServerConfig {
            config: ServerConfigKind::Stdio(cfg),
            scope,
            plugin_source: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::config::ConfigScope;

    #[test]
    fn default_state() {
        let s = StdioMenuState::default();
        assert_eq!(s.step, StdioMenuStep::EditingName);
        assert!(s.draft.name.is_empty());
        assert!(s.error.is_none());
    }

    #[test]
    fn set_name_updates_draft_clears_error() {
        let mut s = StdioMenuState::default();
        s.error = Some("old".into());
        let s = s.apply(StdioMenuEvent::SetName("linear".into()));
        assert_eq!(s.draft.name, "linear");
        assert!(s.error.is_none());
    }

    #[test]
    fn next_from_name_requires_non_empty() {
        let s = StdioMenuState::default();
        let s = s.apply(StdioMenuEvent::Next);
        assert_eq!(s.step, StdioMenuStep::EditingName);
        assert_eq!(s.error.as_deref(), Some("Name cannot be empty"));
    }

    #[test]
    fn next_from_name_with_whitespace_is_empty() {
        let s = StdioMenuState::default().apply(StdioMenuEvent::SetName("   ".into()));
        let s = s.apply(StdioMenuEvent::Next);
        assert_eq!(s.step, StdioMenuStep::EditingName);
        assert!(s.error.is_some());
    }

    #[test]
    fn full_happy_path() {
        let s = StdioMenuState::default();
        let s = s
            .apply(StdioMenuEvent::SetName("linear".into()))
            .apply(StdioMenuEvent::Next);
        assert_eq!(s.step, StdioMenuStep::EditingCommand);
        let s = s
            .apply(StdioMenuEvent::SetCommand("node".into()))
            .apply(StdioMenuEvent::Next);
        assert_eq!(s.step, StdioMenuStep::EditingArgs);
        let s = s
            .apply(StdioMenuEvent::SetArgs(vec!["server.js".into()]))
            .apply(StdioMenuEvent::Next);
        assert_eq!(s.step, StdioMenuStep::EditingEnv);
        let mut env = BTreeMap::new();
        env.insert("API_KEY".into(), "secret".into());
        let s = s
            .apply(StdioMenuEvent::SetEnv(env))
            .apply(StdioMenuEvent::Next);
        assert_eq!(s.step, StdioMenuStep::Validating);
        let s = s.apply(StdioMenuEvent::Submit);
        assert_eq!(s.step, StdioMenuStep::Saving);
        let s = s.apply(StdioMenuEvent::SaveSucceeded);
        assert_eq!(s.step, StdioMenuStep::Saved);
    }

    #[test]
    fn back_from_command_returns_to_name() {
        let s = StdioMenuState::default()
            .apply(StdioMenuEvent::SetName("x".into()))
            .apply(StdioMenuEvent::Next)
            .apply(StdioMenuEvent::Back);
        assert_eq!(s.step, StdioMenuStep::EditingName);
    }

    #[test]
    fn back_from_name_stays_on_name() {
        let s = StdioMenuState::default().apply(StdioMenuEvent::Back);
        assert_eq!(s.step, StdioMenuStep::EditingName);
    }

    #[test]
    fn save_failed_transitions_to_error() {
        let s = StdioMenuState {
            step: StdioMenuStep::Saving,
            draft: StdioMenuDraft::default(),
            error: None,
        };
        let s = s.apply(StdioMenuEvent::SaveFailed("disk full".into()));
        assert_eq!(s.step, StdioMenuStep::Error);
        assert_eq!(s.error.as_deref(), Some("disk full"));
    }

    #[test]
    fn back_from_error_returns_to_name() {
        let s = StdioMenuState {
            step: StdioMenuStep::Error,
            draft: StdioMenuDraft::default(),
            error: Some("x".into()),
        };
        let s = s.apply(StdioMenuEvent::Back);
        assert_eq!(s.step, StdioMenuStep::EditingName);
        assert!(s.error.is_none());
    }

    #[test]
    fn reset_returns_to_default() {
        let s = StdioMenuState {
            step: StdioMenuStep::Saved,
            draft: StdioMenuDraft {
                name: "x".into(),
                command: "y".into(),
                args: vec!["z".into()],
                env: BTreeMap::new(),
            },
            error: Some("e".into()),
        };
        let s = s.apply(StdioMenuEvent::Reset);
        assert_eq!(s, StdioMenuState::default());
    }

    #[test]
    fn try_build_config_requires_command() {
        let s = StdioMenuState::default();
        assert!(s.try_build_config().is_err());
    }

    #[test]
    fn try_build_config_returns_stdio_variant() {
        let mut s = StdioMenuState::default();
        s.draft.command = "node".into();
        s.draft.args = vec!["server.js".into()];
        let cfg = s.try_build_config().unwrap();
        assert_eq!(cfg.command, "node");
        assert_eq!(cfg.args, vec!["server.js".to_string()]);
        assert!(cfg.env.is_none());
    }

    #[test]
    fn try_build_config_keeps_env_when_non_empty() {
        let mut s = StdioMenuState::default();
        s.draft.command = "node".into();
        s.draft.env.insert("K".into(), "V".into());
        let cfg = s.try_build_config().unwrap();
        assert!(cfg.env.is_some());
        assert_eq!(cfg.env.unwrap().get("K"), Some(&"V".to_string()));
    }

    #[test]
    fn build_scoped_wraps_in_correct_scope() {
        let mut s = StdioMenuState::default();
        s.draft.command = "x".into();
        let scoped = s.build_scoped(ConfigScope::Project).unwrap();
        assert_eq!(scoped.scope, ConfigScope::Project);
        assert!(matches!(scoped.config, ServerConfigKind::Stdio(_)));
    }

    #[test]
    fn next_from_command_requires_non_empty() {
        let s = StdioMenuState::default()
            .apply(StdioMenuEvent::SetName("x".into()))
            .apply(StdioMenuEvent::Next);
        assert_eq!(s.step, StdioMenuStep::EditingCommand);
        let s = s.apply(StdioMenuEvent::Next);
        assert_eq!(s.step, StdioMenuStep::EditingCommand);
        assert!(s.error.is_some());
    }
}
