//! The turn-completion gate — whether the model's "I'm done" is accepted.
//!
//! Not to be confused with the `Stop` hook, which despite the name asks the
//! opposite question: that one fires when *someone tries to cancel* a running
//! turn, and `HookEffect::BlockStop` refuses the cancel. Nothing in rebon has
//! ever asked whether a turn that ended *on its own* should have.
//!
//! It should. Observable on a long-budget benchmark: left ungated the model
//! calls the task done well inside its budget and hands in work it only believes
//! is correct, because believing the work is right is not the same as having
//! checked it. With three audit rounds the pass rate went 9.3% -> 23.1% and the
//! median run 31 min -> 97 min. The failure is not capability, it is termination.
//!
//! ## Where this sits
//!
//! [`PromptExecutor`] is the one seam every front end shares — a headless
//! run, the ACP server, the terminal UI, and the background worker all end a
//! turn by returning from `execute`. So the gate is a decorator over that
//! trait, the same shape as [`crate::BackendPromptExecutor`], rather than
//! anything reaching into the engine. Wrap once at construction and every
//! caller inherits it; wrap nowhere and nothing changes.
//!
//! ## Why it cannot spin
//!
//! Three independent brakes, in the order they are checked:
//!
//! 1. **Not an `EndTurn`.** A turn that stopped for `MaxTokens`, `Refusal`,
//!    `Cancelled`, or `MaxTurnRequests` is not a claim of completion, and
//!    pushing on it would only burn the same wall the turn just hit.
//! 2. **Cancelled.** The moment the user asks to stop, the gate is out of the
//!    way. An audit round the user is waiting to kill is worse than no gate.
//! 3. **Rounds and wall clock.** Both are hard caps, so the worst case is
//!    bounded even with no progress signal at all.
//!
//! On top of those, an optional [`TurnProgressProbe`] ends the gate the moment
//! a round changes nothing — the difference between "check again" and "keep
//! nagging". Without a probe the gate still terminates, it just pays for every
//! round it was allowed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rebon_types::{ContentBlock, StopReason, TextContent, Usage};

use crate::prompt_executor::{PromptExecutor, PromptExecutorError, PromptOutcome, PromptRequest};

/// The audit prompt used when a policy does not supply one.
///
/// It names the two things the model got wrong when it stopped early: that
/// something else will check this work against inputs it has not seen, and
/// that believing the work is correct is not the same as having run anything.
pub const DEFAULT_AUDIT_PROMPT: &str = "\
Checkpoint: you ended your turn, but you still have budget left and this work \
will be checked by something other than you. Before finishing, do a real \
verification pass: (1) re-read the original request and list every explicit \
requirement, output path/format, tolerance, and edge case; (2) for each one, \
run a concrete check — write and run a test where none exists, and install any \
tool you need rather than approximating; (3) fix what fails and re-run the \
checks; (4) only then finish, saying exactly what you verified and how. If \
everything already passes checks you actually ran, say so briefly and stop.";

/// "Did that round do anything?", asked of whatever owns the answer.
///
/// The gate deliberately does not define what counts. The engine knows which
/// tools ran; a bench harness can only count lines in a transcript mirror.
/// Both can produce a number that changes when work happened, and that is the
/// entire contract: equal snapshots across a round mean the round was talk.
pub trait TurnProgressProbe: Send + Sync {
    /// A value that changes when the session did something worth continuing
    /// for. Monotonic is convenient but not required — only equality is read.
    fn snapshot(&self) -> u64;
}

/// How hard to push, and for how long.
#[derive(Debug, Clone)]
pub struct TurnGatePolicy {
    /// Audit rounds to allow after the first `EndTurn`. `0` disables the gate
    /// entirely, which is what every caller gets until one opts in.
    pub max_rounds: u32,
    /// Wall-clock ceiling measured from the start of the first turn. Once
    /// passed, no *new* round opens; a round already running is left alone.
    /// `None` means only `max_rounds` bounds the gate.
    pub budget: Option<Duration>,
    /// What to say when re-opening the turn.
    pub prompt: String,
}

impl Default for TurnGatePolicy {
    fn default() -> Self {
        Self {
            max_rounds: 0,
            budget: None,
            prompt: DEFAULT_AUDIT_PROMPT.to_string(),
        }
    }
}

impl TurnGatePolicy {
    /// A policy that audits up to `rounds` times. Off when `rounds` is 0.
    pub fn with_rounds(rounds: u32) -> Self {
        Self {
            max_rounds: rounds,
            ..Self::default()
        }
    }

    /// Stop opening new rounds once the turn has run this long.
    pub fn budget(mut self, budget: Duration) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Override the audit prompt.
    pub fn prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = prompt.into();
        self
    }

    /// Whether this policy would ever open a round.
    pub fn is_enabled(&self) -> bool {
        self.max_rounds > 0
    }
}

/// Why the gate stopped pushing. Recorded on the tracing span so a run that
/// scored badly can be told apart from a run that was never allowed to try.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateExit {
    Disabled,
    NotEndTurn,
    Cancelled,
    BudgetSpent,
    RoundsSpent,
    NoProgress,
}

impl GateExit {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NotEndTurn => "not_end_turn",
            Self::Cancelled => "cancelled",
            Self::BudgetSpent => "budget_spent",
            Self::RoundsSpent => "rounds_spent",
            Self::NoProgress => "no_progress",
        }
    }
}

/// A [`PromptExecutor`] that re-opens a turn the model ended on its own.
pub struct TurnCompletionGate {
    inner: Arc<dyn PromptExecutor>,
    policy: TurnGatePolicy,
    progress: Option<Arc<dyn TurnProgressProbe>>,
}

impl TurnCompletionGate {
    /// Wrap `inner`. With a disabled policy this is still a pass-through, so
    /// callers may wrap unconditionally and decide with the policy.
    pub fn new(inner: Arc<dyn PromptExecutor>, policy: TurnGatePolicy) -> Self {
        Self {
            inner,
            policy,
            progress: None,
        }
    }

    /// Attach the "did anything happen" signal. Without one the gate spends
    /// every round it is allowed.
    pub fn with_progress(mut self, probe: Arc<dyn TurnProgressProbe>) -> Self {
        self.progress = Some(probe);
        self
    }

    /// Wrap only when the policy would do something, so a disabled gate does
    /// not even appear in the executor chain.
    pub fn wrap_if_enabled(
        inner: Arc<dyn PromptExecutor>,
        policy: TurnGatePolicy,
    ) -> Arc<dyn PromptExecutor> {
        if policy.is_enabled() {
            Arc::new(Self::new(inner, policy))
        } else {
            inner
        }
    }

    /// The follow-up turn: same session, same publishers, same cancel handle,
    /// different prompt.
    ///
    /// Three fields are deliberately dropped. `user_message_uuid` names the
    /// transcript row the *user's* prompt created and must not be reused by a
    /// turn the user did not write. `replay_requests` and `skill_invocations`
    /// are one-shot intents already carried out by the first turn; replaying
    /// them would re-run a denied tool or re-load a skill once per round.
    fn audit_request(&self, base: &PromptRequest) -> PromptRequest {
        PromptRequest {
            user_prompt: None,
            effort_is_session_default: false,
            prompt: vec![ContentBlock::Text(TextContent {
                text: self.policy.prompt.clone(),
                annotations: None,
            })],
            user_message_uuid: None,
            replay_requests: Vec::new(),
            skill_invocations: Vec::new(),
            ..base.clone()
        }
    }
}

/// Add `add` into `total`, saturating rather than wrapping.
///
/// Usage is reported per turn, so without this the caller would see only the
/// last audit round and read a three-round turn as costing one. Saturating
/// because a wrapped token count reads as a cheap turn, which is the most
/// expensive possible lie for a number people budget against.
fn accumulate(total: &mut Usage, add: &Usage) {
    total.input_tokens = total.input_tokens.saturating_add(add.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(add.output_tokens);
    total.cache_read_input_tokens = total
        .cache_read_input_tokens
        .saturating_add(add.cache_read_input_tokens);
    total.cache_creation_input_tokens = total
        .cache_creation_input_tokens
        .saturating_add(add.cache_creation_input_tokens);
    total.prompt_cache_hit_tokens = total
        .prompt_cache_hit_tokens
        .saturating_add(add.prompt_cache_hit_tokens);
    total.prompt_cache_miss_tokens = total
        .prompt_cache_miss_tokens
        .saturating_add(add.prompt_cache_miss_tokens);
    total.total_input_tokens = total
        .total_input_tokens
        .saturating_add(add.total_input_tokens);
    total.total_output_tokens = total
        .total_output_tokens
        .saturating_add(add.total_output_tokens);
    total.reasoning_tokens = total.reasoning_tokens.saturating_add(add.reasoning_tokens);
}

#[async_trait]
impl PromptExecutor for TurnCompletionGate {
    async fn execute(&self, request: PromptRequest) -> Result<PromptOutcome, PromptExecutorError> {
        let started = Instant::now();
        let mut outcome = self.inner.execute(request.clone()).await?;
        let mut usage = outcome.usage;

        if !self.policy.is_enabled() {
            // Recorded, not just returned: "the gate was off" is the case the
            // exit reason exists to distinguish, and a reader comparing two
            // runs needs one `turn gate: finished` line either way.
            tracing::debug!(
                session_id = %request.session_id,
                rounds_used = 0,
                exit = GateExit::Disabled.as_str(),
                elapsed_s = started.elapsed().as_secs(),
                "turn gate: finished"
            );
            return Ok(outcome);
        }

        let mut round = 0;
        let exit = loop {
            if outcome.stop_reason != StopReason::EndTurn {
                break GateExit::NotEndTurn;
            }
            if request.cancel.is_cancelled() {
                break GateExit::Cancelled;
            }
            if round >= self.policy.max_rounds {
                break GateExit::RoundsSpent;
            }
            if self
                .policy
                .budget
                .is_some_and(|budget| started.elapsed() >= budget)
            {
                break GateExit::BudgetSpent;
            }

            round += 1;
            let before = self.progress.as_ref().map(|probe| probe.snapshot());
            tracing::debug!(
                session_id = %request.session_id,
                round,
                max_rounds = self.policy.max_rounds,
                elapsed_s = started.elapsed().as_secs(),
                "turn gate: opening audit round"
            );

            outcome = self.inner.execute(self.audit_request(&request)).await?;
            accumulate(&mut usage, &outcome.usage);

            // Checked after the round, not before the next one: a round that
            // edited nothing has answered the question, and asking again would
            // be the nagging this gate exists to avoid.
            if let (Some(before), Some(probe)) = (before, self.progress.as_ref()) {
                if probe.snapshot() == before {
                    break GateExit::NoProgress;
                }
            }
        };

        tracing::debug!(
            session_id = %request.session_id,
            rounds_used = round,
            exit = exit.as_str(),
            elapsed_s = started.elapsed().as_secs(),
            "turn gate: finished"
        );

        Ok(PromptOutcome {
            stop_reason: outcome.stop_reason,
            usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    /// Inner executor that replays a script and records what it was asked.
    struct ScriptedExecutor {
        script: Mutex<Vec<PromptOutcome>>,
        seen: Mutex<Vec<String>>,
        /// Bumped on every call, so a probe reading it sees "work happened".
        calls: Arc<AtomicU64>,
        /// When set, the probe stops moving after this many calls.
        freeze_after: Option<u64>,
    }

    impl ScriptedExecutor {
        fn new(script: Vec<PromptOutcome>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script),
                seen: Mutex::new(Vec::new()),
                calls: Arc::new(AtomicU64::new(0)),
                freeze_after: None,
            })
        }

        fn freezing_after(script: Vec<PromptOutcome>, calls: u64) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script),
                seen: Mutex::new(Vec::new()),
                calls: Arc::new(AtomicU64::new(0)),
                freeze_after: Some(calls),
            })
        }

        fn prompts(&self) -> Vec<String> {
            self.seen.lock().expect("poisoned").clone()
        }
    }

    #[async_trait]
    impl PromptExecutor for ScriptedExecutor {
        async fn execute(
            &self,
            request: PromptRequest,
        ) -> Result<PromptOutcome, PromptExecutorError> {
            let text = match request.prompt.first() {
                Some(ContentBlock::Text(t)) => t.text.clone(),
                _ => String::new(),
            };
            self.seen.lock().expect("poisoned").push(text);
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.freeze_after.is_none_or(|limit| n <= limit) {
                // Progress: the probe below reads `calls`.
            }
            let mut script = self.script.lock().expect("poisoned");
            Ok(if script.is_empty() {
                PromptOutcome::end_turn()
            } else {
                script.remove(0)
            })
        }
    }

    /// Reads the executor's call counter, frozen once it stops moving.
    struct CallProbe {
        calls: Arc<AtomicU64>,
        freeze_after: Option<u64>,
    }

    impl TurnProgressProbe for CallProbe {
        fn snapshot(&self) -> u64 {
            let n = self.calls.load(Ordering::SeqCst);
            match self.freeze_after {
                Some(limit) => n.min(limit),
                None => n,
            }
        }
    }

    fn outcome(stop_reason: StopReason, output_tokens: u32) -> PromptOutcome {
        PromptOutcome {
            stop_reason,
            usage: Usage {
                output_tokens,
                ..Usage::default()
            },
        }
    }

    fn request() -> PromptRequest {
        PromptRequest {
            user_prompt: None,
            effort_is_session_default: false,
            session_id: "gate-test".into(),
            cwd: ".".into(),
            prompt: vec![ContentBlock::Text(TextContent {
                text: "do the thing".into(),
                annotations: None,
            })],
            mcp_servers: Vec::new(),
            update_publisher: None,
            permission_publisher: None,
            cancel: rebon_types::PromptCancel::default(),
            thinking_budget: None,
            max_tokens: None,
            reasoning_effort_ordinal: None,
            additional_working_directories: Vec::new(),
            coordinator_mode: None,
            coordinator_report_paths: Vec::new(),
            user_message_uuid: Some("user-row-1".into()),
            background_agent_system: None,
            background_agent_tool_filter: None,
            execution_policy: None,
            replay_requests: Vec::new(),
            skill_invocations: Vec::new(),
        }
    }

    #[tokio::test]
    async fn disabled_policy_is_a_pass_through() {
        let inner = ScriptedExecutor::new(vec![outcome(StopReason::EndTurn, 10)]);
        let gate = TurnCompletionGate::new(inner.clone(), TurnGatePolicy::default());
        let out = gate.execute(request()).await.unwrap();
        assert_eq!(out.stop_reason, StopReason::EndTurn);
        assert_eq!(inner.prompts(), vec!["do the thing".to_string()]);
    }

    #[tokio::test]
    async fn wrap_if_enabled_does_not_wrap_a_disabled_policy() {
        let inner = ScriptedExecutor::new(vec![]);
        let wrapped = TurnCompletionGate::wrap_if_enabled(inner.clone(), TurnGatePolicy::default());
        assert!(Arc::ptr_eq(&(inner as Arc<dyn PromptExecutor>), &wrapped));
    }

    #[tokio::test]
    async fn end_turn_is_audited_up_to_max_rounds() {
        let inner = ScriptedExecutor::new(vec![
            outcome(StopReason::EndTurn, 10),
            outcome(StopReason::EndTurn, 20),
            outcome(StopReason::EndTurn, 30),
        ]);
        let gate = TurnCompletionGate::new(inner.clone(), TurnGatePolicy::with_rounds(2));
        let out = gate.execute(request()).await.unwrap();

        let prompts = inner.prompts();
        assert_eq!(prompts.len(), 3, "one real turn plus two audit rounds");
        assert_eq!(prompts[0], "do the thing");
        assert!(prompts[1].starts_with("Checkpoint:"));
        assert_eq!(prompts[1], prompts[2]);
        // 10 + 20 + 30: a three-round turn must not report as a one-round turn.
        assert_eq!(out.usage.output_tokens, 60);
    }

    #[tokio::test]
    async fn a_turn_that_did_not_end_on_its_own_is_left_alone() {
        for stop in [
            StopReason::MaxTokens,
            StopReason::Refusal,
            StopReason::Cancelled,
            StopReason::MaxTurnRequests,
        ] {
            let inner = ScriptedExecutor::new(vec![outcome(stop, 10)]);
            let gate = TurnCompletionGate::new(inner.clone(), TurnGatePolicy::with_rounds(3));
            let out = gate.execute(request()).await.unwrap();
            assert_eq!(out.stop_reason, stop);
            assert_eq!(inner.prompts().len(), 1, "{stop:?} must not be audited");
        }
    }

    #[tokio::test]
    async fn a_cancelled_session_gets_no_audit_round() {
        let inner = ScriptedExecutor::new(vec![outcome(StopReason::EndTurn, 10)]);
        let gate = TurnCompletionGate::new(inner.clone(), TurnGatePolicy::with_rounds(3));
        let req = request();
        req.cancel.cancel();
        gate.execute(req).await.unwrap();
        assert_eq!(inner.prompts().len(), 1);
    }

    #[tokio::test]
    async fn a_spent_budget_opens_no_new_round() {
        let inner = ScriptedExecutor::new(vec![outcome(StopReason::EndTurn, 10)]);
        let gate = TurnCompletionGate::new(
            inner.clone(),
            TurnGatePolicy::with_rounds(3).budget(Duration::ZERO),
        );
        gate.execute(request()).await.unwrap();
        assert_eq!(inner.prompts().len(), 1);
    }

    #[tokio::test]
    async fn a_round_that_changes_nothing_ends_the_gate() {
        // The probe stops moving after the second call, so audit round 1 shows
        // no progress and round 2 is never opened -- even though 5 are allowed.
        let inner = ScriptedExecutor::freezing_after(vec![], 1);
        let probe = Arc::new(CallProbe {
            calls: Arc::clone(&inner.calls),
            freeze_after: Some(1),
        });
        let gate = TurnCompletionGate::new(inner.clone(), TurnGatePolicy::with_rounds(5))
            .with_progress(probe);
        gate.execute(request()).await.unwrap();
        assert_eq!(
            inner.prompts().len(),
            2,
            "the real turn plus exactly one audit round"
        );
    }

    #[tokio::test]
    async fn a_round_that_changes_something_keeps_going() {
        let inner = ScriptedExecutor::new(vec![]);
        let probe = Arc::new(CallProbe {
            calls: Arc::clone(&inner.calls),
            freeze_after: None,
        });
        let gate = TurnCompletionGate::new(inner.clone(), TurnGatePolicy::with_rounds(3))
            .with_progress(probe);
        gate.execute(request()).await.unwrap();
        assert_eq!(inner.prompts().len(), 4, "the real turn plus three rounds");
    }

    #[tokio::test]
    async fn audit_rounds_drop_one_shot_fields() {
        // A resumed turn must not re-own the user's transcript row, re-run a
        // denied tool, or re-load a skill once per round.
        struct FieldSpy {
            uuids: Mutex<Vec<Option<String>>>,
        }
        #[async_trait]
        impl PromptExecutor for FieldSpy {
            async fn execute(
                &self,
                request: PromptRequest,
            ) -> Result<PromptOutcome, PromptExecutorError> {
                self.uuids
                    .lock()
                    .expect("poisoned")
                    .push(request.user_message_uuid.clone());
                Ok(PromptOutcome::end_turn())
            }
        }
        let spy = Arc::new(FieldSpy {
            uuids: Mutex::new(Vec::new()),
        });
        let gate = TurnCompletionGate::new(spy.clone(), TurnGatePolicy::with_rounds(2));
        gate.execute(request()).await.unwrap();
        let uuids = spy.uuids.lock().expect("poisoned").clone();
        assert_eq!(
            uuids,
            vec![Some("user-row-1".to_string()), None, None],
            "only the user's own turn carries their transcript row"
        );
    }
}
