//! Whether a turn nobody asked for may start now.
//!
//! An automatic turn — a wakeup, a schedule, a pull-request event, a crash
//! recovery — waits while too many turns are running, or once its session has
//! used the day's token budget. The limits are the user's; the binary reads
//! them from settings and passes them in. The supervisor asks before it starts
//! a worker, and a parked worker asks before it takes up an automatic turn of
//! its own, so neither way of starting a turn gets around them.

use crate::{local_day_number, BackgroundJobState, BackgroundJobStatus};

/// The user's limits on automatic turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutomatedTurnLimits {
    /// An automatic turn starts only while fewer turns than this are running.
    pub max_running_turns: usize,
    /// Tokens a session may use in a local day before no automatic turn
    /// starts in it. `None` sets no budget.
    pub daily_token_budget: Option<u64>,
}

/// Why an automatic turn waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomatedTurnHold {
    TurnLimit,
    TokenBudget,
}

impl AutomatedTurnHold {
    pub fn reason(self) -> &'static str {
        match self {
            Self::TurnLimit => "turn_limit",
            Self::TokenBudget => "token_budget",
        }
    }
}

/// How many jobs are in the middle of a turn.
pub fn running_turns(jobs: &[BackgroundJobState]) -> usize {
    jobs.iter()
        .filter(|job| {
            matches!(
                job.process.status,
                BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput
            )
        })
        .count()
}

/// Why `job`'s next turn may not start now, if it is automatic and may not.
/// A turn a person asked for is never held.
pub fn automated_turn_hold(
    job: &BackgroundJobState,
    running_turns: usize,
    limits: AutomatedTurnLimits,
    now_ms: u64,
) -> Option<AutomatedTurnHold> {
    if !job.next_turn_is_automated() {
        return None;
    }
    if running_turns >= limits.max_running_turns {
        return Some(AutomatedTurnHold::TurnLimit);
    }
    let budget = limits.daily_token_budget?;
    let used = job.outcome.tokens_on_day(local_day_number(now_ms));
    (used >= budget).then_some(AutomatedTurnHold::TokenBudget)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(prompt: &str) -> BackgroundJobState {
        BackgroundJobState::new(
            prompt.into(),
            ".".into(),
            crate::BackgroundRuntimeFields {
                provider: None,
                model: None,
                fast_mode: None,
                channels: Vec::new(),
                development_channels: Vec::new(),
                provider_format: None,
                ui_mode: None,
                effort_level: None,
                permission_mode: None,
                capability_mode: rebon_types::AgentCapabilityMode::Normal,
                settings: Vec::new(),
                add_dirs: Vec::new(),
                plugin_dirs: Vec::new(),
                mcp_configs: Vec::new(),
                strict_mcp_config: false,
            },
            None,
        )
    }

    fn turn(input: u32, cache_read: u32) -> rebon_types::Usage {
        rebon_types::Usage {
            input_tokens: input,
            cache_read_input_tokens: cache_read,
            ..Default::default()
        }
    }

    const LIMITS: AutomatedTurnLimits = AutomatedTurnLimits {
        max_running_turns: 2,
        daily_token_budget: Some(1_000),
    };

    #[test]
    fn automatic_turns_wait_past_the_turn_limit_and_a_persons_never_do() {
        let automatic = job(&rebon_tool::cron::mark_scheduled_prompt("nightly review"));
        assert_eq!(automated_turn_hold(&automatic, 1, LIMITS, 0), None);
        assert_eq!(
            automated_turn_hold(&automatic, 2, LIMITS, 0),
            Some(AutomatedTurnHold::TurnLimit)
        );
        assert_eq!(automated_turn_hold(&job("fix the bug"), 9, LIMITS, 0), None);
    }

    #[test]
    fn the_budget_counts_todays_tokens_without_cache_reads() {
        let now = 1_700_000_000_000;
        let today = local_day_number(now);
        let mut automatic = job(&rebon_tool::cron::mark_scheduled_prompt("loop"));
        automatic
            .outcome
            .record_turn_usage(&turn(600, 90_000), today - 1);
        automatic
            .outcome
            .record_turn_usage(&turn(999, 90_000), today);
        assert_eq!(
            automatic.outcome.tokens_on_day(today),
            999,
            "yesterday's are not today's"
        );
        assert_eq!(automated_turn_hold(&automatic, 0, LIMITS, now), None);
        automatic.outcome.record_turn_usage(&turn(1, 0), today);
        assert_eq!(
            automated_turn_hold(&automatic, 0, LIMITS, now),
            Some(AutomatedTurnHold::TokenBudget)
        );
        assert_eq!(
            automatic.outcome.usage.as_ref().unwrap().input_tokens,
            1_600,
            "the running total keeps every turn"
        );
        let next_day = now + 24 * 60 * 60 * 1000;
        assert_eq!(automated_turn_hold(&automatic, 0, LIMITS, next_day), None);
    }
}
