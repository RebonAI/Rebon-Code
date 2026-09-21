//! Permission-mode callback planning.
//!
//! This module intentionally stays at a high abstraction level: the
//! next-mode lookup, the mode cycle, and the mode transition stay
//! caller-owned. This module
//! only decides which branch should run and which side effects the caller
//! should perform around those existing helpers.

/// Debounce before showing the auto-mode opt-in dialog.
pub const AUTO_MODE_OPT_IN_DELAY_MS: u64 = 400;

/// Inputs for [`plan_mode_cycle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeCycleInput {
    /// Whether the teammate-view branch is active.
    pub is_viewing_teammate: bool,
    /// Task id of the viewed teammate, when present.
    pub viewing_agent_task_id: Option<String>,
    /// Next mode for the viewed teammate, already computed by the caller.
    pub teammate_next_mode: Option<String>,
    /// Current leader permission mode.
    pub current_mode: String,
    /// Next leader permission mode, already computed by the caller.
    pub next_mode: String,
    /// Whether the transcript classifier feature is enabled.
    pub auto_mode_feature_enabled: bool,
    /// Whether trusted settings have already accepted the auto-mode opt-in.
    pub has_auto_mode_opt_in: bool,
    /// Whether a primary-agent view is active. Non-`None` blocks first-time auto dialog.
    pub viewing_agent_task_id_present_for_leader: bool,
    /// Whether the auto-mode opt-in dialog is currently visible.
    pub show_auto_mode_opt_in: bool,
    /// Whether a debounce timeout is already pending.
    pub auto_mode_opt_in_timeout_pending: bool,
    /// Team name to sync the teammate mode from.
    pub team_name: Option<String>,
    /// Whether help is currently open.
    pub help_open: bool,
}

/// Outcome of [`plan_mode_cycle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeCyclePlan {
    /// No-op.
    None,
    /// Update the viewed teammate's mode only.
    UpdateViewedTeammate {
        /// Task id of the teammate being updated.
        task_id: String,
        /// Next permission mode for that teammate.
        next_mode: String,
        /// Whether help should be closed.
        close_help: bool,
    },
    /// Enter the first-time auto-mode preview path.
    PreviewAutoModeOptIn {
        /// Previous mode to remember for a potential revert.
        previous_mode_before_auto: String,
        /// UI mode to preview immediately (`auto`).
        preview_mode: String,
        /// Whether any existing timeout should be cleared first.
        clear_existing_timeout: bool,
        /// Delay before showing the dialog.
        schedule_dialog_after_ms: u64,
        /// Whether help should be closed.
        close_help: bool,
    },
    /// Apply a normal mode cycle.
    ApplyModeCycle {
        /// Final next mode.
        next_mode: String,
        /// Whether to dismiss the currently shown auto dialog.
        dismiss_auto_mode_opt_in: bool,
        /// Whether to clear a pending auto-dialog timeout.
        clear_pending_timeout: bool,
        /// Whether to clear the stored mode from before auto.
        clear_previous_mode_before_auto: bool,
        /// Whether to log the dialog decline event before cycling away.
        log_auto_mode_dialog_decline: bool,
        /// Whether the last plan-mode use time should be recorded.
        record_last_plan_mode_use: bool,
        /// Team whose teammates should be synced to the new mode, if any.
        sync_teammate_mode_team_name: Option<String>,
        /// Whether help should be closed.
        close_help: bool,
    },
}

/// Inputs for the accept callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoModeOptInAcceptInput {
    /// Whether the transcript classifier feature is enabled.
    pub auto_mode_feature_enabled: bool,
    /// Stored previous mode, if any.
    pub previous_mode_before_auto: Option<String>,
    /// Current visible tool-permission mode.
    pub current_mode: String,
    /// Whether help is currently open.
    pub help_open: bool,
}

/// Outcome of [`plan_auto_mode_opt_in_accept`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoModeOptInAcceptPlan {
    /// Feature disabled; no-op.
    None,
    /// Apply the accepted auto-mode transition through the caller.
    Accept {
        /// Mode to transition from.
        transition_from_mode: String,
        /// Always `auto`.
        transition_to_mode: String,
        /// Whether the dialog should close.
        dismiss_auto_mode_opt_in: bool,
        /// Whether the stored previous mode should be cleared.
        clear_previous_mode_before_auto: bool,
        /// Whether help should be closed.
        close_help: bool,
    },
}

/// Inputs for the decline callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoModeOptInDeclineInput {
    /// Whether the transcript classifier feature is enabled.
    pub auto_mode_feature_enabled: bool,
    /// Stored previous mode, if any.
    pub previous_mode_before_auto: Option<String>,
    /// Whether a debounce timeout is pending.
    pub auto_mode_opt_in_timeout_pending: bool,
}

/// Outcome of [`plan_auto_mode_opt_in_decline`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoModeOptInDeclinePlan {
    /// Feature disabled; no-op.
    None,
    /// Dismiss the dialog/timeout without a revert payload.
    DismissOnly {
        /// Whether to dismiss the dialog.
        dismiss_auto_mode_opt_in: bool,
        /// Whether to clear the pending timeout.
        clear_pending_timeout: bool,
    },
    /// Revert to the previous mode and disable auto for the session.
    Revert {
        /// Mode to restore.
        revert_mode: String,
        /// Whether to dismiss the dialog.
        dismiss_auto_mode_opt_in: bool,
        /// Whether to clear the pending timeout.
        clear_pending_timeout: bool,
        /// Whether auto mode should be marked inactive.
        set_auto_mode_inactive: bool,
        /// Whether auto mode should be marked unavailable for the session.
        disable_auto_mode_availability: bool,
        /// Whether the stored previous mode should be cleared.
        clear_previous_mode_before_auto: bool,
    },
}

/// Decide which branch the mode cycle takes.
pub fn plan_mode_cycle(input: &ModeCycleInput) -> ModeCyclePlan {
    if input.is_viewing_teammate {
        if let (Some(task_id), Some(next_mode)) = (
            input.viewing_agent_task_id.clone(),
            input.teammate_next_mode.clone(),
        ) {
            return ModeCyclePlan::UpdateViewedTeammate {
                task_id,
                next_mode,
                close_help: input.help_open,
            };
        }
        return ModeCyclePlan::None;
    }

    let entering_auto_mode_first_time = input.auto_mode_feature_enabled
        && input.next_mode == "auto"
        && input.current_mode != "auto"
        && !input.has_auto_mode_opt_in
        && !input.viewing_agent_task_id_present_for_leader;

    if entering_auto_mode_first_time {
        return ModeCyclePlan::PreviewAutoModeOptIn {
            previous_mode_before_auto: input.current_mode.clone(),
            preview_mode: String::from("auto"),
            clear_existing_timeout: input.auto_mode_opt_in_timeout_pending,
            schedule_dialog_after_ms: AUTO_MODE_OPT_IN_DELAY_MS,
            close_help: input.help_open,
        };
    }

    let dismissing_auto_dialog = input.auto_mode_feature_enabled
        && (input.show_auto_mode_opt_in || input.auto_mode_opt_in_timeout_pending);

    ModeCyclePlan::ApplyModeCycle {
        next_mode: input.next_mode.clone(),
        dismiss_auto_mode_opt_in: dismissing_auto_dialog,
        clear_pending_timeout: dismissing_auto_dialog && input.auto_mode_opt_in_timeout_pending,
        clear_previous_mode_before_auto: dismissing_auto_dialog,
        log_auto_mode_dialog_decline: dismissing_auto_dialog && input.show_auto_mode_opt_in,
        record_last_plan_mode_use: input.next_mode == "plan",
        sync_teammate_mode_team_name: input.team_name.clone(),
        close_help: input.help_open,
    }
}

/// Plan the auto-mode opt-in acceptance.
pub fn plan_auto_mode_opt_in_accept(input: &AutoModeOptInAcceptInput) -> AutoModeOptInAcceptPlan {
    if !input.auto_mode_feature_enabled {
        return AutoModeOptInAcceptPlan::None;
    }

    AutoModeOptInAcceptPlan::Accept {
        transition_from_mode: input
            .previous_mode_before_auto
            .clone()
            .unwrap_or_else(|| input.current_mode.clone()),
        transition_to_mode: String::from("auto"),
        dismiss_auto_mode_opt_in: true,
        clear_previous_mode_before_auto: true,
        close_help: input.help_open,
    }
}

/// Plan the auto-mode opt-in decline.
pub fn plan_auto_mode_opt_in_decline(
    input: &AutoModeOptInDeclineInput,
) -> AutoModeOptInDeclinePlan {
    if !input.auto_mode_feature_enabled {
        return AutoModeOptInDeclinePlan::None;
    }

    if let Some(revert_mode) = &input.previous_mode_before_auto {
        return AutoModeOptInDeclinePlan::Revert {
            revert_mode: revert_mode.clone(),
            dismiss_auto_mode_opt_in: true,
            clear_pending_timeout: input.auto_mode_opt_in_timeout_pending,
            set_auto_mode_inactive: true,
            disable_auto_mode_availability: true,
            clear_previous_mode_before_auto: true,
        };
    }

    AutoModeOptInDeclinePlan::DismissOnly {
        dismiss_auto_mode_opt_in: true,
        clear_pending_timeout: input.auto_mode_opt_in_timeout_pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_cycle() -> ModeCycleInput {
        ModeCycleInput {
            is_viewing_teammate: false,
            viewing_agent_task_id: None,
            teammate_next_mode: None,
            current_mode: String::from("default"),
            next_mode: String::from("acceptEdits"),
            auto_mode_feature_enabled: false,
            has_auto_mode_opt_in: false,
            viewing_agent_task_id_present_for_leader: false,
            show_auto_mode_opt_in: false,
            auto_mode_opt_in_timeout_pending: false,
            team_name: Some(String::from("team-a")),
            help_open: false,
        }
    }

    #[test]
    fn teammate_cycle_updates_viewed_teammate_only() {
        let mut input = base_cycle();
        input.is_viewing_teammate = true;
        input.viewing_agent_task_id = Some(String::from("task-1"));
        input.teammate_next_mode = Some(String::from("plan"));
        input.help_open = true;
        assert_eq!(
            plan_mode_cycle(&input),
            ModeCyclePlan::UpdateViewedTeammate {
                task_id: String::from("task-1"),
                next_mode: String::from("plan"),
                close_help: true,
            }
        );
    }

    #[test]
    fn first_time_auto_mode_enters_preview_path() {
        let mut input = base_cycle();
        input.auto_mode_feature_enabled = true;
        input.current_mode = String::from("default");
        input.next_mode = String::from("auto");
        input.help_open = true;
        assert_eq!(
            plan_mode_cycle(&input),
            ModeCyclePlan::PreviewAutoModeOptIn {
                previous_mode_before_auto: String::from("default"),
                preview_mode: String::from("auto"),
                clear_existing_timeout: false,
                schedule_dialog_after_ms: AUTO_MODE_OPT_IN_DELAY_MS,
                close_help: true,
            }
        );
    }

    #[test]
    fn cycling_away_from_auto_preview_dismisses_dialog_then_applies_mode() {
        let mut input = base_cycle();
        input.auto_mode_feature_enabled = true;
        input.current_mode = String::from("auto");
        input.next_mode = String::from("default");
        input.show_auto_mode_opt_in = true;
        input.auto_mode_opt_in_timeout_pending = true;
        assert_eq!(
            plan_mode_cycle(&input),
            ModeCyclePlan::ApplyModeCycle {
                next_mode: String::from("default"),
                dismiss_auto_mode_opt_in: true,
                clear_pending_timeout: true,
                clear_previous_mode_before_auto: true,
                log_auto_mode_dialog_decline: true,
                record_last_plan_mode_use: false,
                sync_teammate_mode_team_name: Some(String::from("team-a")),
                close_help: false,
            }
        );
    }

    #[test]
    fn normal_cycle_can_record_plan_mode_use() {
        let mut input = base_cycle();
        input.next_mode = String::from("plan");
        input.help_open = true;
        assert_eq!(
            plan_mode_cycle(&input),
            ModeCyclePlan::ApplyModeCycle {
                next_mode: String::from("plan"),
                dismiss_auto_mode_opt_in: false,
                clear_pending_timeout: false,
                clear_previous_mode_before_auto: false,
                log_auto_mode_dialog_decline: false,
                record_last_plan_mode_use: true,
                sync_teammate_mode_team_name: Some(String::from("team-a")),
                close_help: true,
            }
        );
    }

    #[test]
    fn accept_plan_uses_previous_mode_when_present() {
        assert_eq!(
            plan_auto_mode_opt_in_accept(&AutoModeOptInAcceptInput {
                auto_mode_feature_enabled: true,
                previous_mode_before_auto: Some(String::from("plan")),
                current_mode: String::from("auto"),
                help_open: true,
            }),
            AutoModeOptInAcceptPlan::Accept {
                transition_from_mode: String::from("plan"),
                transition_to_mode: String::from("auto"),
                dismiss_auto_mode_opt_in: true,
                clear_previous_mode_before_auto: true,
                close_help: true,
            }
        );
    }

    #[test]
    fn decline_plan_reverts_when_previous_mode_exists() {
        assert_eq!(
            plan_auto_mode_opt_in_decline(&AutoModeOptInDeclineInput {
                auto_mode_feature_enabled: true,
                previous_mode_before_auto: Some(String::from("default")),
                auto_mode_opt_in_timeout_pending: true,
            }),
            AutoModeOptInDeclinePlan::Revert {
                revert_mode: String::from("default"),
                dismiss_auto_mode_opt_in: true,
                clear_pending_timeout: true,
                set_auto_mode_inactive: true,
                disable_auto_mode_availability: true,
                clear_previous_mode_before_auto: true,
            }
        );
    }

    #[test]
    fn decline_plan_can_dismiss_without_revert() {
        assert_eq!(
            plan_auto_mode_opt_in_decline(&AutoModeOptInDeclineInput {
                auto_mode_feature_enabled: true,
                previous_mode_before_auto: None,
                auto_mode_opt_in_timeout_pending: false,
            }),
            AutoModeOptInDeclinePlan::DismissOnly {
                dismiss_auto_mode_opt_in: true,
                clear_pending_timeout: false,
            }
        );
    }
}
