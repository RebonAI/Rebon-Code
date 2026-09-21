//! Implements the team UI behavior.
//!
//! Provides deterministic state transitions + command emissions that
//! the consumer can map to overlay registration, tmux commands, and swarm helpers.

/// Dialog level (list vs detail).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogLevel {
    /// List view showing teammates in a team.
    TeammateList {
        /// Name of the team being rendered.
        team_name: String,
    },
    /// Detail view for a specific teammate.
    TeammateDetail {
        /// Name of the team currently inspected.
        team_name: String,
        /// Name of the member whose detail is visible.
        member_name: String,
    },
}

/// Activity state matched from the `status` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeammateActivity {
    /// Teammate is actively running work.
    Running,
    /// Teammate is idle.
    Idle,
    /// Status could not be determined.
    Unknown,
}

/// Permission mode strings used by the teams dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    /// Normal permission flow.
    Default,
    /// Auto-accept edits.
    AcceptEdits,
    /// Plan-only mode.
    Plan,
    /// Bypass permission checks.
    BypassPermissions,
    /// Auto mode.
    Auto,
    /// Don't ask mode.
    DontAsk,
}

impl PermissionMode {
    /// Parse a permission-mode string (`acceptEdits`, `plan`, …);
    /// anything unrecognised is `Default`.
    pub fn from_mode_str(mode: &str) -> Self {
        match mode {
            "acceptEdits" => Self::AcceptEdits,
            "plan" => Self::Plan,
            "bypassPermissions" => Self::BypassPermissions,
            "auto" => Self::Auto,
            "dontAsk" => Self::DontAsk,
            _ => Self::Default,
        }
    }

    /// source-literal string form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
            Self::Auto => "auto",
            Self::DontAsk => "dontAsk",
        }
    }

    /// Advance to the next mode in the cycle the teams dialog steps through:
    /// `default` → `acceptEdits` → `plan` → `bypassPermissions` (when
    /// available, otherwise back to `default`); `bypassPermissions`, `auto`
    /// and `dontAsk` return to `default`.
    pub fn next(self, bypass_available: bool) -> Self {
        match self {
            Self::Default => Self::AcceptEdits,
            Self::AcceptEdits => Self::Plan,
            Self::Plan => {
                if bypass_available {
                    Self::BypassPermissions
                } else {
                    Self::Default
                }
            }
            Self::BypassPermissions | Self::Auto | Self::DontAsk => Self::Default,
        }
    }
}

/// Simplified teammate status info consumed by the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamsDialogTeammate {
    /// Display name of the teammate.
    pub name: String,
    /// Consumer-owned handle (`tmuxPaneId`, task id, etc.).
    pub target_id: String,
    /// Agent ID for kill/shutdown requests.
    pub agent_id: String,
    /// Optional model label.
    pub model: Option<String>,
    /// Optional prompt summary/body.
    pub prompt: Option<String>,
    /// Teammate activity state.
    pub status: TeammateActivity,
    /// Optional display color.
    pub color: Option<String>,
    /// Whether this teammate is hidden.
    pub is_hidden: bool,
    /// Whether the consumer runtime can actually hide/show this teammate.
    pub can_hide: bool,
    /// Optional permission mode string.
    pub mode: Option<String>,
}

/// Actions the dialog exposes to the consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeamsDialogAction {
    /// Move selection down.
    Next,
    /// Move selection up.
    Previous,
    /// Drill into the selected teammate detail.
    Enter,
    /// Go back to the teammate list level.
    Back,
    /// Request a kill command.
    Kill,
    /// Request a shutdown command.
    Shutdown,
    /// Toggle visibility for the selected teammate.
    ToggleHide,
    /// Toggle visibility for the full team.
    ToggleHideAll,
    /// Cycle teammate permission mode.
    CycleMode {
        /// Whether the bypass mode option is available.
        bypass_available: bool,
    },
    /// View the teammate's output pane.
    ViewOutput,
    /// Kill all idle teammates in the current team.
    PruneIdle,
}

/// Commands emitted by the dialog to trigger side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeamsDialogCommand {
    /// Cycle the teammate mode.
    CycleMode {
        /// Team performing the action.
        team_name: String,
        /// Teammate to cycle.
        teammate: String,
        /// Consumer-owned target handle.
        target_id: String,
        /// Target mode computed by the pure crate.
        target_mode: PermissionMode,
    },
    /// Cycle multiple teammates to the same mode.
    CycleModeAll {
        /// Team performing the action.
        team_name: String,
        /// Teammates to update.
        teammates: Vec<String>,
        /// Target mode computed by the pure crate.
        target_mode: PermissionMode,
    },
    /// Kill command.
    Kill {
        /// Team context.
        team_name: String,
        /// Teammate to kill.
        teammate: String,
        /// Agent id for downstream task/mailbox cleanup.
        agent_id: String,
        /// Consumer-owned target handle.
        target_id: String,
    },
    /// Shutdown command.
    Shutdown {
        /// Team context.
        team_name: String,
        /// Teammate to shutdown.
        teammate: String,
        /// Agent id for downstream task/mailbox cleanup.
        agent_id: String,
        /// Consumer-owned target handle.
        target_id: String,
    },
    /// Toggle single teammate visibility.
    ToggleHide {
        /// Team context.
        team_name: String,
        /// Target teammate.
        teammate: String,
        /// Consumer-owned target handle.
        target_id: String,
        /// Desired visibility state.
        hide: bool,
    },
    /// Toggle visibility for all teammates.
    ToggleHideAll {
        /// Team context.
        team_name: String,
        /// Visibility state to apply.
        hide: bool,
    },
    /// Switch to the teammate output pane.
    ViewOutput {
        /// Teammate whose output will be shown.
        teammate: String,
        /// Consumer-owned target handle.
        target_id: String,
    },
}

/// Result of applying an action.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TeamsDialogOutcome {
    /// Commands the consumer should execute.
    pub commands: Vec<TeamsDialogCommand>,
    /// Whether the dialog should close after the action.
    pub close_dialog: bool,
}

/// Dialog state machine.
#[derive(Debug, Clone)]
pub struct TeamsDialogState {
    /// Teammates rendered in the current dialog.
    pub teammates: Vec<TeamsDialogTeammate>,
    /// Currently selected index.
    pub selected_index: usize,
    /// Current dialog level (list vs detail).
    pub dialog_level: DialogLevel,
}

impl TeamsDialogState {
    /// Creates an initial state for a team.
    pub fn new(team_name: String, teammates: Vec<TeamsDialogTeammate>) -> Self {
        Self {
            teammates,
            selected_index: 0,
            dialog_level: DialogLevel::TeammateList { team_name },
        }
    }

    fn current_team_name(&self) -> &str {
        match &self.dialog_level {
            DialogLevel::TeammateList { team_name } => team_name,
            DialogLevel::TeammateDetail { team_name, .. } => team_name,
        }
    }

    /// Return the currently-selected teammate.
    pub fn current_teammate(&self) -> Option<&TeamsDialogTeammate> {
        self.teammates.get(self.selected_index)
    }

    fn reset_to_list(&mut self) {
        self.dialog_level = DialogLevel::TeammateList {
            team_name: self.current_team_name().to_string(),
        };
        self.selected_index = 0;
    }

    fn clamp_selected_index(&mut self) {
        let max = self.teammates.len().saturating_sub(1);
        self.selected_index = usize::min(self.selected_index, max);
        if self.teammates.is_empty() {
            self.selected_index = 0;
        }
    }

    fn current_mode(&self, teammate: &TeamsDialogTeammate) -> PermissionMode {
        teammate
            .mode
            .as_deref()
            .map(PermissionMode::from_mode_str)
            .unwrap_or(PermissionMode::Default)
    }

    /// Replace the teammate snapshot list on the dialog's refresh tick,
    /// keeping the selection in range.
    pub fn sync_teammates(&mut self, teammates: Vec<TeamsDialogTeammate>) {
        self.teammates = teammates;
        self.clamp_selected_index();

        if let DialogLevel::TeammateDetail { member_name, .. } = &self.dialog_level {
            let still_present = self.teammates.iter().any(|t| t.name == *member_name);
            if !still_present {
                self.reset_to_list();
            }
        }
    }

    /// Applies an action and returns emitted commands plus close behavior.
    pub fn apply_action(&mut self, action: TeamsDialogAction) -> TeamsDialogOutcome {
        match action {
            TeamsDialogAction::Next => {
                let max = self.teammates.len().saturating_sub(1);
                self.selected_index = usize::min(max, self.selected_index + 1);
                TeamsDialogOutcome::default()
            }
            TeamsDialogAction::Previous => {
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                }
                TeamsDialogOutcome::default()
            }
            TeamsDialogAction::Enter => match &self.dialog_level {
                DialogLevel::TeammateList { .. } => {
                    if let Some(teammate) = self.current_teammate() {
                        self.dialog_level = DialogLevel::TeammateDetail {
                            team_name: self.current_team_name().to_string(),
                            member_name: teammate.name.clone(),
                        };
                    }
                    TeamsDialogOutcome::default()
                }
                DialogLevel::TeammateDetail { .. } => {
                    self.apply_action(TeamsDialogAction::ViewOutput)
                }
            },
            TeamsDialogAction::Back => {
                if matches!(self.dialog_level, DialogLevel::TeammateDetail { .. }) {
                    self.reset_to_list();
                }
                TeamsDialogOutcome::default()
            }
            TeamsDialogAction::Kill => {
                let command = self.current_teammate().map(|t| TeamsDialogCommand::Kill {
                    team_name: self.current_team_name().to_string(),
                    teammate: t.name.clone(),
                    agent_id: t.agent_id.clone(),
                    target_id: t.target_id.clone(),
                });
                if matches!(self.dialog_level, DialogLevel::TeammateDetail { .. }) {
                    self.reset_to_list();
                }
                TeamsDialogOutcome {
                    commands: command.into_iter().collect(),
                    close_dialog: false,
                }
            }
            TeamsDialogAction::Shutdown => {
                let command = self
                    .current_teammate()
                    .map(|t| TeamsDialogCommand::Shutdown {
                        team_name: self.current_team_name().to_string(),
                        teammate: t.name.clone(),
                        agent_id: t.agent_id.clone(),
                        target_id: t.target_id.clone(),
                    });
                if matches!(self.dialog_level, DialogLevel::TeammateDetail { .. }) {
                    self.reset_to_list();
                }
                TeamsDialogOutcome {
                    commands: command.into_iter().collect(),
                    close_dialog: false,
                }
            }
            TeamsDialogAction::ToggleHide => {
                let command = self.current_teammate().and_then(|t| {
                    t.can_hide.then(|| TeamsDialogCommand::ToggleHide {
                        team_name: self.current_team_name().to_string(),
                        teammate: t.name.clone(),
                        target_id: t.target_id.clone(),
                        hide: !t.is_hidden,
                    })
                });
                if matches!(self.dialog_level, DialogLevel::TeammateDetail { .. }) {
                    self.reset_to_list();
                }
                TeamsDialogOutcome {
                    commands: command.into_iter().collect(),
                    close_dialog: false,
                }
            }
            TeamsDialogAction::ToggleHideAll => {
                let hideable: Vec<&TeamsDialogTeammate> =
                    self.teammates.iter().filter(|t| t.can_hide).collect();
                if hideable.is_empty() {
                    return TeamsDialogOutcome::default();
                }
                let any_visible = hideable.iter().any(|t| !t.is_hidden);
                TeamsDialogOutcome {
                    commands: vec![TeamsDialogCommand::ToggleHideAll {
                        team_name: self.current_team_name().to_string(),
                        hide: any_visible,
                    }],
                    close_dialog: false,
                }
            }
            TeamsDialogAction::CycleMode { bypass_available } => match &self.dialog_level {
                DialogLevel::TeammateDetail { .. } => {
                    if let Some(teammate) = self.current_teammate() {
                        TeamsDialogOutcome {
                            commands: vec![TeamsDialogCommand::CycleMode {
                                team_name: self.current_team_name().to_string(),
                                teammate: teammate.name.clone(),
                                target_id: teammate.target_id.clone(),
                                target_mode: self.current_mode(teammate).next(bypass_available),
                            }],
                            close_dialog: false,
                        }
                    } else {
                        TeamsDialogOutcome::default()
                    }
                }
                DialogLevel::TeammateList { .. } => {
                    if self.teammates.is_empty() {
                        return TeamsDialogOutcome::default();
                    }
                    let modes: Vec<PermissionMode> = self
                        .teammates
                        .iter()
                        .map(|t| self.current_mode(t))
                        .collect();
                    let all_same = modes.iter().all(|m| *m == modes[0]);
                    let target_mode = if all_same {
                        modes[0].next(bypass_available)
                    } else {
                        PermissionMode::Default
                    };
                    TeamsDialogOutcome {
                        commands: vec![TeamsDialogCommand::CycleModeAll {
                            team_name: self.current_team_name().to_string(),
                            teammates: self.teammates.iter().map(|t| t.name.clone()).collect(),
                            target_mode,
                        }],
                        close_dialog: false,
                    }
                }
            },
            TeamsDialogAction::ViewOutput => {
                if let Some(teammate) = self.current_teammate() {
                    TeamsDialogOutcome {
                        commands: vec![TeamsDialogCommand::ViewOutput {
                            teammate: teammate.name.clone(),
                            target_id: teammate.target_id.clone(),
                        }],
                        close_dialog: true,
                    }
                } else {
                    TeamsDialogOutcome::default()
                }
            }
            TeamsDialogAction::PruneIdle => TeamsDialogOutcome {
                commands: self
                    .teammates
                    .iter()
                    .filter(|t| t.status == TeammateActivity::Idle)
                    .map(|t| TeamsDialogCommand::Kill {
                        team_name: self.current_team_name().to_string(),
                        teammate: t.name.clone(),
                        agent_id: t.agent_id.clone(),
                        target_id: t.target_id.clone(),
                    })
                    .collect(),
                close_dialog: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn teammates() -> Vec<TeamsDialogTeammate> {
        (1..=3)
            .map(|i| TeamsDialogTeammate {
                name: format!("teammate-{i}"),
                target_id: format!("task-{i}"),
                agent_id: format!("agent-{i}"),
                model: None,
                prompt: None,
                status: TeammateActivity::Running,
                color: None,
                is_hidden: false,
                can_hide: false,
                mode: Some("default".into()),
            })
            .collect()
    }

    #[test]
    fn enter_moves_to_detail() {
        let mut state = TeamsDialogState::new("team".into(), teammates());
        state.apply_action(TeamsDialogAction::Enter);
        assert!(matches!(
            state.dialog_level,
            DialogLevel::TeammateDetail { member_name, .. } if member_name == "teammate-1"
        ));
    }

    #[test]
    fn navigation_cycles_selection() {
        let mut state = TeamsDialogState::new("team".into(), teammates());
        state.apply_action(TeamsDialogAction::Next);
        assert_eq!(state.selected_index, 1);
        state.apply_action(TeamsDialogAction::Previous);
        assert_eq!(state.selected_index, 0);
    }

    #[test]
    fn kill_emits_command() {
        let mut state = TeamsDialogState::new("team".into(), teammates());
        let outcome = state.apply_action(TeamsDialogAction::Kill);
        assert_eq!(
            outcome.commands,
            vec![TeamsDialogCommand::Kill {
                team_name: "team".into(),
                teammate: "teammate-1".into(),
                agent_id: "agent-1".into(),
                target_id: "task-1".into(),
            }]
        );
    }

    #[test]
    fn toggle_hide_all_hides_when_any_visible() {
        let mut state = TeamsDialogState::new("team".into(), {
            let mut ts = teammates();
            ts[0].is_hidden = true;
            ts[0].can_hide = true;
            ts[1].can_hide = true;
            ts[2].can_hide = true;
            ts
        });
        let outcome = state.apply_action(TeamsDialogAction::ToggleHideAll);
        assert_eq!(
            outcome.commands,
            vec![TeamsDialogCommand::ToggleHideAll {
                team_name: "team".into(),
                hide: true
            }]
        );
    }

    #[test]
    fn enter_from_detail_views_output_and_closes() {
        let mut state = TeamsDialogState::new("team".into(), teammates());
        state.apply_action(TeamsDialogAction::Enter);
        let outcome = state.apply_action(TeamsDialogAction::Enter);
        assert!(outcome.close_dialog);
        assert_eq!(
            outcome.commands,
            vec![TeamsDialogCommand::ViewOutput {
                teammate: "teammate-1".into(),
                target_id: "task-1".into(),
            }]
        );
    }

    #[test]
    fn detail_kill_returns_to_list() {
        let mut state = TeamsDialogState::new("team".into(), teammates());
        state.apply_action(TeamsDialogAction::Enter);
        let _ = state.apply_action(TeamsDialogAction::Kill);
        assert!(matches!(
            state.dialog_level,
            DialogLevel::TeammateList { .. }
        ));
        assert_eq!(state.selected_index, 0);
    }

    #[test]
    fn cycle_all_resets_mixed_modes_to_default() {
        let mut state = TeamsDialogState::new("team".into(), {
            let mut ts = teammates();
            ts[0].mode = Some("default".into());
            ts[1].mode = Some("plan".into());
            ts
        });
        let outcome = state.apply_action(TeamsDialogAction::CycleMode {
            bypass_available: true,
        });
        assert_eq!(
            outcome.commands,
            vec![TeamsDialogCommand::CycleModeAll {
                team_name: "team".into(),
                teammates: vec![
                    "teammate-1".into(),
                    "teammate-2".into(),
                    "teammate-3".into(),
                ],
                target_mode: PermissionMode::Default,
            }]
        );
    }

    #[test]
    fn prune_idle_emits_kill_for_each_idle_teammate() {
        let mut state = TeamsDialogState::new("team".into(), {
            let mut ts = teammates();
            ts[0].status = TeammateActivity::Idle;
            ts[2].status = TeammateActivity::Idle;
            ts
        });
        let outcome = state.apply_action(TeamsDialogAction::PruneIdle);
        assert_eq!(outcome.commands.len(), 2);
        assert!(matches!(
            &outcome.commands[0],
            TeamsDialogCommand::Kill { teammate, .. } if teammate == "teammate-1"
        ));
        assert!(matches!(
            &outcome.commands[1],
            TeamsDialogCommand::Kill { teammate, .. } if teammate == "teammate-3"
        ));
    }

    #[test]
    fn toggle_hide_is_noop_when_selected_teammate_cannot_hide() {
        let mut state = TeamsDialogState::new("team".into(), teammates());
        let outcome = state.apply_action(TeamsDialogAction::ToggleHide);
        assert!(outcome.commands.is_empty());
    }
}
