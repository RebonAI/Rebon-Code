//! The attachments plan mode speaks into the model's history, and the
//! mode switch it performs when `ExitPlanMode` succeeds.
//!
//! These are the three messages a planning session reads between tool
//! rounds:
//!
//! | attachment | trigger |
//! |---|---|
//! | `plan_mode_exit` | the one-shot flag the session record carries out of plan mode |
//! | `plan_mode` | the mode is `plan`, and the throttle has elapsed |
//! | `plan_mode_reentry` | as above, and the session has been out of plan mode before |
//!
//! They run before everything else a turn collects: the producer sits on the
//! kernel's `attachment-producers` seat at [`Order::Transition`], and every
//! seat producer polls before the engine's own session poller. A mode change
//! has to be the first thing a turn hears about, or the messages after it
//! describe a mode that is no longer in force.
//!
//! **What lives elsewhere.** The seven plan-mode fields on
//! `SessionAttachmentState` and the eight `ServerState` methods that write
//! them are still `rebon-session-state`'s, because the TUI's Shift+Tab mode
//! cycle writes them directly and never goes through a tool. This module is
//! their only *reader* on the attachment path, and it reads them the same way
//! the engine did: a snapshot per poll, a delta applied after.
//!
//! Turning the plugin off stops the reminders. It does not take a session out
//! of plan mode — the mode is a property of the record — but nothing will
//! re-tell the model that it is in one, and `ExitPlanMode` is off the seat
//! too, so the pair goes quiet together.

use std::sync::Arc;

#[cfg(test)]
use rebon_api::ContentBlock as ApiContentBlock;
use rebon_api::{make_meta_user_message, Message as ApiMessage};
use rebon_core::attachment_seat::{SeatAttachmentProducer, SessionAttachmentBinding};
use rebon_core::query::{AttachmentPollPhase, AttachmentPollRequest, AttachmentPoller};
use rebon_session_state::{ServerState, SessionAttachmentState};

/// Throttle: only emit a new `plan_mode` attachment once every N
/// iterations within the current turn.
pub const PLAN_MODE_TURNS_BETWEEN_ATTACHMENTS: u64 = 5;

/// Every Nth plan_mode attachment is the "full" reminder instead of
/// the sparse one-liner.
pub const PLAN_MODE_FULL_REMINDER_EVERY_N: u64 = 5;

/// Immutable snapshot of everything the two producers read. Copied in by
/// [`poll_plan_mode_attachments`] so producers don't hold a lock and so tests
/// can drive them with synthetic values.
#[derive(Debug, Clone)]
pub struct PlanModePollInput {
    /// Resolved permission mode wire string (`"plan"`, `"default"`, ...).
    pub permission_mode: String,
    /// Snapshot of the session's transition-flag state. Producers only read
    /// from this; mutations go through the returned [`PlanModeStateDelta`].
    pub attachment_state: SessionAttachmentState,
    /// Current tool-round iteration (0-based). Used by the `plan_mode`
    /// throttle.
    pub iteration: u64,
}

/// Side-effect bundle a poll wants applied to the session record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanModeStateDelta {
    /// Clear the one-shot `needs_plan_mode_exit_attachment` flag.
    pub clear_plan_mode_exit_flag: bool,
    /// Clear the `has_exited_plan_mode` flag (consumed by
    /// `plan_mode_reentry`).
    pub clear_has_exited_plan_mode: bool,
    /// When `Some`, record a `plan_mode` attachment at this iteration
    /// (increments counter + updates throttle marker).
    pub record_plan_mode_iteration: Option<u64>,
}

impl PlanModeStateDelta {
    /// Apply `other`'s changes on top of self, mutating in place.
    pub fn merge(&mut self, other: PlanModeStateDelta) {
        if other.clear_plan_mode_exit_flag {
            self.clear_plan_mode_exit_flag = true;
        }
        if other.clear_has_exited_plan_mode {
            self.clear_has_exited_plan_mode = true;
        }
        if other.record_plan_mode_iteration.is_some() {
            self.record_plan_mode_iteration = other.record_plan_mode_iteration;
        }
    }
}

/// Aggregate result of one poll.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanModePollOutput {
    /// Messages to push, in order.
    pub messages: Vec<ApiMessage>,
    /// State delta to apply to the session record.
    pub state_delta: PlanModeStateDelta,
}

/// Run both producers in order and fold their outputs.
///
/// Ordering matters — the exit attachment fires before re-entry, and
/// `plan_mode_exit` comes before the general `plan_mode` producer so a
/// transition out of plan mode in the same poll emits the exit attachment
/// exclusively.
pub fn poll_plan_mode_attachments(input: &PlanModePollInput) -> PlanModePollOutput {
    let mut output = PlanModePollOutput::default();
    append(&mut output, plan_mode_exit(input));
    append(&mut output, plan_mode(input));
    output
}

fn append(out: &mut PlanModePollOutput, piece: PlanModePollOutput) {
    out.messages.extend(piece.messages);
    out.state_delta.merge(piece.state_delta);
}

// ── plan_mode_exit ───────────────────────────────────────────────

/// One-shot `plan_mode_exit` attachment.
pub fn plan_mode_exit(input: &PlanModePollInput) -> PlanModePollOutput {
    if !input.attachment_state.needs_plan_mode_exit_attachment {
        return PlanModePollOutput::default();
    }
    // Guard: if the mode is currently plan, clear the flag without
    // emitting — a back-to-back enter/exit should not announce the
    // exit.
    if input.permission_mode == "plan" {
        return PlanModePollOutput {
            messages: Vec::new(),
            state_delta: PlanModeStateDelta {
                clear_plan_mode_exit_flag: true,
                ..Default::default()
            },
        };
    }

    let content = match &input.attachment_state.pending_exit_plan_text {
        Some(plan) => format!(
            "## Exited Plan Mode\n\n\
             You have exited plan mode. You can now make edits, run tools, and take actions.\n\n\
             ## Approved Plan\n\n{plan}"
        ),
        None => "## Exited Plan Mode\n\n\
             You have exited plan mode. You can now make edits, run tools, and take actions."
            .to_string(),
    };

    // The tool result was written before the mode was applied, so a mode the
    // session could not grant is only visible to the model here.
    let content = match &input.attachment_state.pending_exit_mode_note {
        Some(note) => format!("{content}\n\n{note}"),
        None => content,
    };

    PlanModePollOutput {
        messages: vec![make_meta_user_message(&content)],
        state_delta: PlanModeStateDelta {
            clear_plan_mode_exit_flag: true,
            ..Default::default()
        },
    }
}

// ── plan_mode / plan_mode_reentry ────────────────────────────────

/// Recurring `plan_mode` reminder attachment. Emits a full
/// reminder on the 1st, (N+1)th, (2N+1)th… attachment and a sparse
/// reminder in between — except on a re-entry, which is always sparse
/// however the cadence falls, because the session has read the full
/// workflow already.
pub fn plan_mode(input: &PlanModePollInput) -> PlanModePollOutput {
    if input.permission_mode != "plan" {
        return PlanModePollOutput::default();
    }

    // Throttle: if a plan_mode attachment was already emitted within
    // the last PLAN_MODE_TURNS_BETWEEN_ATTACHMENTS iterations, skip.
    // The first-ever attachment always fires (last_plan_mode_iteration
    // is None).
    if let Some(last) = input.attachment_state.last_plan_mode_iteration {
        let gap = input.iteration.saturating_sub(last);
        if gap < PLAN_MODE_TURNS_BETWEEN_ATTACHMENTS {
            return PlanModePollOutput::default();
        }
    }

    let mut output = PlanModePollOutput::default();
    let mut delta = PlanModeStateDelta::default();

    // Re-entry prelude if the session previously exited plan mode.
    let reentering = input.attachment_state.has_exited_plan_mode;
    if reentering {
        output
            .messages
            .push(make_meta_user_message(PLAN_MODE_REENTRY_CONTENT));
        delta.clear_has_exited_plan_mode = true;
    }

    // Full vs sparse based on cumulative count since last exit.
    // `next_count % PLAN_MODE_FULL_REMINDER_EVERY_N == 1` picks the full
    // reminder.
    //
    // A re-entry is sparse whatever the count says. Leaving plan mode resets
    // the counter, so every return used to re-send the whole four-phase
    // workflow to a model that had read it earlier in the same conversation
    // — which is exactly what the sparse reminder's "see full instructions
    // earlier in conversation" points at. Both branches key on the one flag
    // the prelude above consumes, so the prelude and the reminder can never
    // disagree about whether this is a return.
    let next_count = input
        .attachment_state
        .plan_mode_attachment_count
        .saturating_add(1);
    let is_full = !reentering && next_count % PLAN_MODE_FULL_REMINDER_EVERY_N == 1;

    let content = if is_full {
        PLAN_MODE_FULL_CONTENT
    } else {
        PLAN_MODE_SPARSE_CONTENT
    };
    output.messages.push(make_meta_user_message(content));

    delta.record_plan_mode_iteration = Some(input.iteration);
    output.state_delta = delta;
    output
}

/// Full plan-mode reminder text (the non-interview path), trimmed to
/// the parts that don't depend on runtime plan-file state. We
/// deliberately keep this as a static string so the output
/// is deterministic and cache-friendly.
const PLAN_MODE_FULL_CONTENT: &str = "Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits, run any non-readonly tools (including changing configs or making commits), or otherwise make any changes to the system. This supercedes any other instructions you have received.\n\n\
## Plan Workflow\n\n\
### Phase 1: Initial Understanding\n\
Goal: Gain the understanding needed for the user's request by reading through code and asking them questions. Critical: when codebase exploration is needed in this phase you should only use the Explore subagent type.\n\n\
1. Focus on understanding the user's request and the code associated with their request. Actively search for existing functions, utilities, and patterns that can be reused; avoid proposing new code when suitable implementations already exist.\n\n\
2. Launch up to 3 Explore agents IN PARALLEL only when exploration is needed to efficiently explore the codebase.\n\
   - Use 1 agent when the task is isolated to known files, the user provided specific file paths, or you're making a small targeted change.\n\
   - Use multiple agents when the scope is uncertain, multiple areas of the codebase are involved, or you need to understand existing patterns before planning.\n\
   - Quality over quantity: 3 agents maximum, but use the minimum number necessary.\n\
   - If using multiple agents, give each one a specific search focus or area to explore.\n\n\
Do not perform exploratory Read, Glob, Grep, or Bash calls yourself in Phase 1. Wait for any Explore report, then synthesize findings. Use AskUserQuestion only when unresolved decisions that only the user can answer block the plan, and combine all foreseeable independent blocking decisions into one call.\n\n\
Do not invoke `subagent_type=Plan` at any phase. Plan mode assigns planning responsibility to you, the parent agent. You must synthesize the research, evaluate trade-offs, and produce the final plan yourself; delegate only codebase exploration to Explore when needed.\n\n\
### Phase 2: Design\n\
Design the implementation approach yourself. Consider multiple approaches and their trade-offs. Think through edge cases, failure modes, and ordering constraints.\n\n\
### Phase 3: Review\n\
Review the plan and ensure alignment with the user's intentions. If the user's answers and your research are sufficient, stop asking questions. Ask only about remaining blocking user decisions; never use AskUserQuestion for notifications, operation instructions, thanks, restatements, acknowledgements, or closing remarks.\n\n\
### Phase 4: Call ExitPlanMode\n\
After research and any necessary clarification are complete, call the existing ExitPlanMode tool directly to submit the plan for approval. Do not ask about plan approval via text, and do not use an extra AskUserQuestion to announce readiness or replace plan submission.\n\n\
Remember: DO NOT write or edit any files. This is a read-only exploration and planning phase.";

/// Sparse plan-mode reminder (interleaved with full reminders to
/// keep the prompt lean over long sessions).
const PLAN_MODE_SPARSE_CONTENT: &str = "Plan mode still active (see full instructions earlier in conversation). Read-only; follow the Plan workflow. Never invoke `subagent_type=Plan` while Plan Mode is active; the parent agent owns synthesis and the final plan, and only codebase exploration should be delegated to Explore when needed. AskUserQuestion is only for unresolved blocking user decisions; batch foreseeable questions and stop asking once the answers are sufficient. After research and necessary clarification, call the existing ExitPlanMode tool directly for plan approval. Never replace plan submission or a normal notification or closing remark with AskUserQuestion.";

/// Re-entry reminder. There is no runtime `planFilePath`
/// interpolation because the
/// engine doesn't track per-session plan files yet.
const PLAN_MODE_REENTRY_CONTENT: &str = "## Re-entering Plan Mode\n\n\
You are returning to plan mode after having previously exited it. A plan from your previous planning session may exist.\n\n\
**Before proceeding with any new planning, you should:**\n\
1. Read any existing plan to understand what was previously planned\n\
2. Evaluate the user's current request against that plan\n\
3. Decide how to proceed:\n   \
- **Different task**: Start fresh by overwriting the existing plan\n   \
- **Same task, continuing**: Modify the existing plan while cleaning up outdated sections\n\n\
Treat this as a fresh planning session. Do not assume the existing plan is relevant without evaluating it first.";

// ── the unattended escalation gate ───────────────────────────────

/// Whether a *tool-driven* move to `mode` must be refused because
/// nobody is there to have made it.
///
/// This is the one place a permission mode widens without a person
/// choosing it. `ExitPlanMode` asks "how should I proceed", the
/// unattended approver answers `yes_auto` because that is the most
/// permissive allow option on offer, and the answer lands in the
/// session record and drives every later permission decision — an
/// authorization nobody granted. All 27 unattended trials on Terminal-Bench 4.0
/// that entered plan mode came out of it in auto mode (RFC-0006
/// §2.1), and then lost 22 tool calls to the classifier that auto
/// mode had just switched on.
///
/// Gated here rather than in `ServerState::set_permission_mode`
/// because the record cannot tell the two kinds of write apart. A
/// host that was *told* `--permission-mode auto` has someone behind
/// it and must still be obeyed; a mode that arrives out of a tool
/// result has nobody behind it by construction. Every host — exec,
/// background jobs, `serve` — reaches the record through this one
/// call, so one gate covers all of them, which
/// `select_unattended_allow_option` (exec only) could not.
///
/// The test is `!is_interactive()`, not `is_unattended()`, and the
/// difference is a background job. It *can* be asked — its prompts
/// queue on IPC until someone opens the job — so its tools stay
/// (`tool_exposure` asks the other question). But a mode it handed
/// itself out of a tool result was still authorized by nobody, and
/// `rebon_config::ensure_background_permission_mode_allowed` already
/// refuses to *launch* a job in these two modes without a prior
/// interactive acceptance. Without this, that rule held at the front
/// door while `ExitPlanMode` walked in the back.
///
/// The set of modes matches
/// `rebon_config::background_permission_mode_requires_interactive_acceptance`
/// — background jobs already refuse to *start* in these two without a
/// prior interactive acceptance, so the judgment "these are not for an
/// empty room" is the repo's, not this function's. It is restated
/// rather than imported because this plugin does not depend on
/// `rebon-config`; if one grows a third mode, so must the other.
///
/// `plan` and `default` are not widenings and pass through.
/// `acceptEdits` — the third mode `ExitPlanMode` can hand back — also
/// passes, deliberately: it is the one that does *not* switch the
/// model classifier on, and an unattended approver already answers
/// every edit prompt with an allow, so it grants nothing the run did
/// not already have. `bypassPermissions` cannot reach here today
/// (`exit_plan_mode_selection` never produces it) and is listed to
/// keep the two predicates literally the same set.
/// `preauthorized` is the one way past this short of an interactive
/// room, and it is what makes the desktop app work: every app chat runs
/// as a background job, so the person reading the plan and picking "Yes,
/// run with auto mode" is answering from a `Detached` room. Their answer
/// used to be dropped here — `ExitPlanMode` still reported success, the
/// session stayed in `plan`, the reminder producer re-injected "Plan
/// mode is active", and the model called the tool again, forever. A job
/// already refuses to *launch* in these modes without an interactive
/// acceptance (`ensure_background_permission_mode_allowed`); the host
/// hands that same acceptance to `rebon_tool::set_escalation_preauthorized`,
/// and this gate honors it. An `Unattended` room ignores it: nobody
/// answered the dialog there, the auto-approver did.
///
/// A refusal is never the end of plan mode — the caller applies
/// [`ESCALATION_FALLBACK_MODE`] instead and tells the model, because a
/// session that cannot leave plan mode is worse than one that leaves it
/// narrower.
fn escalation_refused(
    surface: rebon_tool::ExecutionSurface,
    mode: &str,
    preauthorized: bool,
) -> bool {
    if surface.is_interactive() || !matches!(mode, "auto" | "bypassPermissions") {
        return false;
    }
    // A room with a door can carry an authorization a person made ahead of
    // time. A room with nobody in it never can.
    !(preauthorized && !surface.is_unattended())
}

/// What a session leaves plan mode in when the mode `ExitPlanMode` handed
/// back is refused. The widest mode the gate lets through everywhere: it
/// never switches the model classifier on, and it is already what an
/// approver in a room without a person answers to every edit prompt.
const ESCALATION_FALLBACK_MODE: &str = "acceptEdits";

/// What the model is told when its requested mode was not the one applied.
/// Carried by the `plan_mode_exit` attachment: the tool result is written
/// before the mode is applied, so it already claimed the requested one.
fn escalation_downgrade_note(requested: &str, applied: &str) -> String {
    format!(
        "Note: this session could not grant permission mode `{requested}` from a tool result \
         — nobody here authorized it — so it left plan mode in `{applied}` instead. Continue \
         with the plan; expect permission prompts for anything `{applied}` does not cover."
    )
}

// ── the poller and its seat producer ─────────────────────────────

/// [`AttachmentPoller`] for one session's plan-mode attachments.
///
/// Each poll reads the session's `permission_mode` and `attachment_state`,
/// runs the two producers, and applies the resulting delta through
/// [`ServerState`] setters. The state takes the session mutex for the read
/// and the writes separately — a racing `session/set_config_option` that
/// flips the mode between them is possible, but the outcome (an extra
/// `plan_mode` attachment or a slightly late exit attachment) is benign, and
/// tolerated rather than locked against: one lock across both would hold the
/// session mutex for the whole poll.
pub struct PlanModeAttachmentPoller {
    state: Arc<ServerState>,
    session_id: String,
}

impl PlanModeAttachmentPoller {
    pub fn new(state: Arc<ServerState>, session_id: impl Into<String>) -> Self {
        Self {
            state,
            session_id: session_id.into(),
        }
    }
}

impl AttachmentPoller for PlanModeAttachmentPoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        if request.phase == AttachmentPollPhase::Eager {
            return Vec::new();
        }
        let next_iteration = request.next_iteration;
        let Some(record) = self.state.attachment_session_snapshot(&self.session_id) else {
            return Vec::new();
        };

        let input = PlanModePollInput {
            permission_mode: record.permission_mode.clone(),
            attachment_state: record.attachment_state.clone(),
            iteration: next_iteration,
        };

        let output = poll_plan_mode_attachments(&input);

        let delta = output.state_delta;
        if delta.clear_plan_mode_exit_flag {
            let _ = self.state.clear_plan_mode_exit_flag(&self.session_id);
        }
        if delta.clear_has_exited_plan_mode {
            let _ = self.state.clear_plan_mode_exited_flag(&self.session_id);
        }
        if let Some(iter) = delta.record_plan_mode_iteration {
            let _ = self
                .state
                .record_plan_mode_attachment(&self.session_id, iter);
        }

        output.messages
    }

    fn notify_plan_mode_tool(
        &self,
        tool_name: &str,
        succeeded: bool,
        tool_result: Option<&serde_json::Value>,
    ) {
        if !succeeded {
            return;
        }
        let mode = match tool_name {
            "EnterPlanMode" => "plan",
            "ExitPlanMode" => {
                let Some(result) = tool_result else {
                    return;
                };
                match result
                    .get("permissionMode")
                    .or_else(|| result.get("mode"))
                    .and_then(|value| value.as_str())
                {
                    Some(mode @ ("auto" | "acceptEdits" | "default")) => mode,
                    _ => return,
                }
            }
            _ => return,
        };
        let surface = rebon_tool::execution_surface();
        let applied =
            if escalation_refused(surface, mode, rebon_tool::escalation_preauthorized(mode)) {
                tracing::warn!(
                    session_id = %self.session_id,
                    tool = tool_name,
                    requested_mode = mode,
                    applied_mode = ESCALATION_FALLBACK_MODE,
                    %surface,
                    "nobody here authorized this permission mode from a tool result; \
                     leaving plan mode in the narrower one"
                );
                ESCALATION_FALLBACK_MODE
            } else {
                mode
            };
        self.state.set_permission_mode(&self.session_id, applied);
        if applied != mode {
            self.state.set_pending_exit_mode_note(
                &self.session_id,
                escalation_downgrade_note(mode, applied),
            );
        }

        // Store the plan text so it survives as regular text in the
        // conversation (either as the context-reset base message or
        // embedded in the plan_mode_exit attachment).
        if tool_name == "ExitPlanMode" {
            if let Some(result) = tool_result {
                let is_clear = result
                    .get("clearContext")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if let Some(plan) = result.get("plan").and_then(|v| v.as_str()) {
                    if is_clear {
                        self.state
                            .set_pending_context_reset(&self.session_id, plan.to_string());
                    } else {
                        self.state
                            .set_pending_exit_plan_text(&self.session_id, plan.to_string());
                    }
                }
            }
        }
    }

    fn take_context_reset(&self) -> Option<Vec<ApiMessage>> {
        self.state
            .take_pending_context_reset(&self.session_id)
            .map(|plan| {
                let content = format!("Implement the following plan:\n\n{plan}");
                vec![ApiMessage::user_text(&content)]
            })
    }
}

/// The seat producer: one poller per turn, bound to that turn's session.
///
/// It answers for every session, not only those in plan mode. The mode can
/// change mid-turn (Shift+Tab writes the record while the stream is in
/// flight), and the exit attachment exists precisely to report a change the
/// turn did not start with — so deciding at bind time whether this session
/// "is a plan-mode session" would drop exactly the case the producer is for.
pub struct PlanModeAttachmentProducer;

impl SeatAttachmentProducer for PlanModeAttachmentProducer {
    fn poller_for_session(
        &self,
        binding: &SessionAttachmentBinding,
    ) -> Option<Arc<dyn AttachmentPoller>> {
        Some(Arc::new(PlanModeAttachmentPoller::new(
            binding.state.clone(),
            binding.session_id.clone(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(next_iteration: u64) -> AttachmentPollRequest<'static> {
        AttachmentPollRequest::new(
            "session",
            "turn",
            next_iteration,
            AttachmentPollPhase::Regular,
        )
    }

    fn base_input() -> PlanModePollInput {
        PlanModePollInput {
            permission_mode: "default".into(),
            attachment_state: SessionAttachmentState::default(),
            iteration: 0,
        }
    }

    fn only_text(messages: &[ApiMessage]) -> String {
        messages
            .iter()
            .flat_map(|m| {
                m.content.iter().filter_map(|b| match b {
                    ApiContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
            })
            .collect::<Vec<_>>()
            .join("\n---\n")
    }

    fn make_session_with_mode(mode: &str) -> (Arc<ServerState>, String) {
        let state = Arc::new(ServerState::new());
        let record = state.create_session("/tmp/test".into(), Vec::new());
        let sid = record.id.clone();
        state.set_permission_mode(&sid, mode);
        (state, sid)
    }

    // ── plan_mode_exit ────────────────────────────────────────────

    #[test]
    fn plan_mode_exit_no_op_when_flag_clear() {
        let out = plan_mode_exit(&base_input());
        assert!(out.messages.is_empty());
        assert_eq!(out.state_delta, PlanModeStateDelta::default());
    }

    #[test]
    fn plan_mode_exit_emits_when_flag_set_and_mode_not_plan() {
        let mut input = base_input();
        input.attachment_state.needs_plan_mode_exit_attachment = true;
        input.permission_mode = "default".into();
        let out = plan_mode_exit(&input);
        assert_eq!(out.messages.len(), 1);
        let text = only_text(&out.messages);
        assert!(text.contains("<system-reminder>"));
        assert!(text.contains("Exited Plan Mode"));
        assert!(out.state_delta.clear_plan_mode_exit_flag);
    }

    #[test]
    fn plan_mode_exit_includes_plan_text_when_present() {
        let mut input = base_input();
        input.attachment_state.needs_plan_mode_exit_attachment = true;
        input.attachment_state.pending_exit_plan_text = Some("1. fix the bug\n2. add tests".into());
        input.permission_mode = "default".into();
        let out = plan_mode_exit(&input);
        assert_eq!(out.messages.len(), 1);
        let text = only_text(&out.messages);
        assert!(text.contains("Exited Plan Mode"));
        assert!(text.contains("Approved Plan"));
        assert!(text.contains("1. fix the bug"));
        assert!(text.contains("2. add tests"));
        assert!(out.state_delta.clear_plan_mode_exit_flag);
    }

    #[test]
    fn plan_mode_exit_clears_flag_without_emitting_when_still_in_plan() {
        let mut input = base_input();
        input.attachment_state.needs_plan_mode_exit_attachment = true;
        input.permission_mode = "plan".into();
        let out = plan_mode_exit(&input);
        assert!(out.messages.is_empty());
        assert!(out.state_delta.clear_plan_mode_exit_flag);
    }

    // ── plan_mode ─────────────────────────────────────────────────

    #[test]
    fn plan_mode_no_op_outside_plan() {
        let out = plan_mode(&base_input());
        assert!(out.messages.is_empty());
        assert_eq!(out.state_delta, PlanModeStateDelta::default());
    }

    #[test]
    fn plan_mode_first_injection_is_full_reminder() {
        let mut input = base_input();
        input.permission_mode = "plan".into();
        let out = plan_mode(&input);
        assert_eq!(out.messages.len(), 1);
        let text = only_text(&out.messages);
        assert!(text.contains("Plan mode is active"));
        assert!(text.contains("Phase 4: Call ExitPlanMode"));
        assert_eq!(out.state_delta.record_plan_mode_iteration, Some(0));
    }

    /// These reminders are the only place that tells the model how
    /// `AskUserQuestion` behaves while planning. The tool's own description
    /// used to repeat it, which put plan vocabulary in front of every session
    /// that carries the tool; that copy is gone, so this one has to hold.
    #[test]
    fn plan_mode_prompt_stops_questioning_and_submits_when_answers_are_sufficient() {
        let mut input = base_input();
        input.permission_mode = "plan".into();
        let full = only_text(&plan_mode(&input).messages);
        for needle in [
            "combine all foreseeable independent blocking decisions into one call",
            "If the user's answers and your research are sufficient, stop asking questions",
            "never use AskUserQuestion for notifications",
            "call the existing ExitPlanMode tool directly",
            "do not use an extra AskUserQuestion",
            "Do not invoke `subagent_type=Plan` at any phase",
            "You must synthesize the research, evaluate trade-offs, and produce the final plan yourself",
        ] {
            assert!(full.contains(needle), "missing {needle:?} in:\n{full}");
        }

        input.attachment_state.plan_mode_attachment_count = 1;
        let sparse = only_text(&plan_mode(&input).messages);
        assert!(sparse.contains("stop asking once the answers are sufficient"));
        assert!(sparse.contains("call the existing ExitPlanMode tool directly"));
        assert!(sparse.contains("Never invoke `subagent_type=Plan`"));
        assert!(sparse.contains("parent agent owns synthesis and the final plan"));
        assert!(sparse.contains("normal notification or closing remark"));
    }

    #[test]
    fn plan_mode_second_injection_is_sparse_reminder() {
        let mut input = base_input();
        input.permission_mode = "plan".into();
        input.attachment_state.plan_mode_attachment_count = 1;
        input.attachment_state.last_plan_mode_iteration = None;
        let out = plan_mode(&input);
        assert_eq!(out.messages.len(), 1);
        let text = only_text(&out.messages);
        assert!(text.contains("Plan mode still active"));
        assert!(!text.contains("preserve and validate"));
        assert!(!text.contains("Phase 4"));
    }

    #[test]
    fn plan_mode_is_throttled_within_turn_gap() {
        let mut input = base_input();
        input.permission_mode = "plan".into();
        input.iteration = 3;
        input.attachment_state.last_plan_mode_iteration = Some(1);
        // gap == 2, below threshold of 5 → no emission.
        let out = plan_mode(&input);
        assert!(out.messages.is_empty());
    }

    #[test]
    fn plan_mode_fires_again_after_throttle_gap_passes() {
        let mut input = base_input();
        input.permission_mode = "plan".into();
        input.iteration = 7;
        input.attachment_state.last_plan_mode_iteration = Some(1);
        let out = plan_mode(&input);
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.state_delta.record_plan_mode_iteration, Some(7));
    }

    #[test]
    fn plan_mode_full_reminder_cadence_every_n() {
        let mut input = base_input();
        input.permission_mode = "plan".into();
        // After 5 attachments → next_count == 6, 6 % 5 == 1 → full.
        input.attachment_state.plan_mode_attachment_count = 5;
        let out = plan_mode(&input);
        let text = only_text(&out.messages);
        assert!(text.contains("Phase 4: Call ExitPlanMode"));
    }

    #[test]
    fn plan_mode_reentry_prepends_when_flag_set() {
        let mut input = base_input();
        input.permission_mode = "plan".into();
        input.attachment_state.has_exited_plan_mode = true;
        let out = plan_mode(&input);
        assert_eq!(out.messages.len(), 2);
        let first = out.messages[0].content[0].as_text().unwrap();
        assert!(first.contains("Re-entering Plan Mode"));
        assert!(out.state_delta.clear_has_exited_plan_mode);
    }

    // ── composition ───────────────────────────────────────────────

    /// The exit attachment fires alone: `plan_mode` short-circuits on a
    /// mode that is no longer `plan`.
    #[test]
    fn poll_skips_plan_mode_reminder_when_exit_fires_and_mode_already_default() {
        let mut input = base_input();
        input.attachment_state.needs_plan_mode_exit_attachment = true;
        let out = poll_plan_mode_attachments(&input);
        assert_eq!(out.messages.len(), 1);
        let text = only_text(&out.messages);
        assert!(text.contains("Exited Plan Mode"));
        assert!(out.state_delta.clear_plan_mode_exit_flag);
    }

    #[test]
    fn poll_is_empty_on_idle_steady_state() {
        let out = poll_plan_mode_attachments(&base_input());
        assert!(out.messages.is_empty());
        assert_eq!(out.state_delta, PlanModeStateDelta::default());
    }

    #[test]
    fn delta_merge_folds_flags_and_keeps_the_latest_iteration() {
        let mut base = PlanModeStateDelta {
            clear_plan_mode_exit_flag: false,
            clear_has_exited_plan_mode: false,
            record_plan_mode_iteration: Some(3),
        };
        base.merge(PlanModeStateDelta {
            clear_plan_mode_exit_flag: true,
            clear_has_exited_plan_mode: true,
            record_plan_mode_iteration: Some(5),
        });
        assert!(base.clear_plan_mode_exit_flag);
        assert!(base.clear_has_exited_plan_mode);
        assert_eq!(base.record_plan_mode_iteration, Some(5));
    }

    // ── PlanModeAttachmentPoller end-to-end ───────────────────────

    #[test]
    fn session_poller_emits_plan_mode_attachment_and_records_state() {
        let (state, sid) = make_session_with_mode("plan");
        let poller = PlanModeAttachmentPoller::new(state.clone(), sid.clone());
        let messages = poller.poll(request(0));
        // First poll in plan mode: full reminder.
        assert_eq!(messages.len(), 1);
        let text = messages[0].content[0].as_text().unwrap();
        assert!(text.contains("Plan mode is active"));
        assert!(text.contains("Phase 4: Call ExitPlanMode"));

        let after = state.get_session(&sid).unwrap();
        assert_eq!(after.attachment_state.plan_mode_attachment_count, 1);
        assert_eq!(after.attachment_state.last_plan_mode_iteration, Some(0));
    }

    #[test]
    fn session_poller_emits_plan_mode_exit_after_transition() {
        let (state, sid) = make_session_with_mode("plan");
        // Simulate a mid-turn exit: TUI cycles the mode back to default.
        state.set_permission_mode(&sid, "default");
        let poller = PlanModeAttachmentPoller::new(state.clone(), sid.clone());
        let messages = poller.poll(request(1));
        assert_eq!(messages.len(), 1);
        let text = messages[0].content[0].as_text().unwrap();
        assert!(text.contains("Exited Plan Mode"));
        let after = state.get_session(&sid).unwrap();
        assert!(!after.attachment_state.needs_plan_mode_exit_attachment);
        // has_exited_plan_mode stays true so the NEXT plan_mode entry
        // triggers a reentry preamble.
        assert!(after.attachment_state.has_exited_plan_mode);
    }

    #[test]
    fn session_poller_is_no_op_for_unknown_session() {
        let state = Arc::new(ServerState::new());
        let poller = PlanModeAttachmentPoller::new(state, "sess-ghost");
        assert!(poller.poll(request(0)).is_empty());
    }

    #[test]
    fn session_poller_plan_mode_reentry_flag_fires_once() {
        let (state, sid) = make_session_with_mode("plan");
        state.set_permission_mode(&sid, "default");
        state.set_permission_mode(&sid, "plan");
        let poller = PlanModeAttachmentPoller::new(state.clone(), sid.clone());
        let first = poller.poll(request(0));
        // Two messages: re-entry prelude + sparse reminder.
        assert_eq!(first.len(), 2);
        let first_text = first[0].content[0].as_text().unwrap();
        assert!(first_text.contains("Re-entering Plan Mode"));
        let after = state.get_session(&sid).unwrap();
        assert!(!after.attachment_state.has_exited_plan_mode);
        assert_eq!(after.attachment_state.plan_mode_attachment_count, 1);
    }

    /// Enter, leave, return: the four-phase workflow is sent once.
    ///
    /// Leaving plan mode resets the full/sparse counter, so a return landed
    /// on "count 1" and re-sent the entire workflow to a model that had
    /// already read it. A session that enters for the first time still gets
    /// it — that is the case the reset was written for, and it is unchanged.
    #[test]
    fn returning_to_plan_mode_sends_the_sparse_reminder_not_the_workflow_again() {
        let (state, sid) = make_session_with_mode("plan");

        let entered =
            only_text(&PlanModeAttachmentPoller::new(state.clone(), sid.clone()).poll(request(0)));
        assert!(entered.contains("Plan mode is active"), "{entered}");
        assert!(entered.contains("Phase 4: Call ExitPlanMode"), "{entered}");

        state.set_permission_mode(&sid, "default");
        state.set_permission_mode(&sid, "plan");

        let returned = only_text(&PlanModeAttachmentPoller::new(state, sid).poll(request(0)));
        assert!(returned.contains("Re-entering Plan Mode"), "{returned}");
        assert!(returned.contains("Plan mode still active"), "{returned}");
        assert!(
            !returned.contains("## Plan Workflow"),
            "the workflow was sent twice:\n{returned}"
        );
        assert!(
            !returned.contains("Phase 4: Call ExitPlanMode"),
            "the workflow was sent twice:\n{returned}"
        );
    }

    /// The seat producer hands out a poller for any session, because the
    /// mode can change after the turn has started.
    #[test]
    fn the_seat_producer_binds_a_poller_to_the_session_it_is_given() {
        let (state, sid) = make_session_with_mode("plan");
        let binding = SessionAttachmentBinding::new(state.clone(), sid.clone());
        let poller = PlanModeAttachmentProducer
            .poller_for_session(&binding)
            .expect("plan-mode produces for every session");
        let text = only_text(&poller.poll(request(0)));
        assert!(text.contains("Plan mode is active"), "{text}");
    }

    // ── unattended escalation gate ────────────────────────────────

    /// The one that was measured: `ExitPlanMode` handing back `auto`
    /// in a room with nobody in it, 27 times out of 27.
    ///
    /// A background job is the same refusal for a different reason —
    /// someone can be asked there, eventually, but nobody chose this.
    /// `ensure_background_permission_mode_allowed` says so at launch;
    /// this is the same rule at the other door.
    #[test]
    fn no_room_without_a_person_gets_the_two_modes_no_one_chose() {
        use rebon_tool::ExecutionSurface::{Detached, Unattended};

        for surface in [Unattended, Detached] {
            assert!(escalation_refused(surface, "auto", false), "{surface}");
            assert!(
                escalation_refused(surface, "bypassPermissions", false),
                "{surface}"
            );
        }
    }

    /// Leaving plan mode, and the edit-accepting mode that never turns
    /// the classifier on, are not widenings — refusing them would
    /// strand a turn in plan mode with no way out.
    #[test]
    fn the_modes_that_are_not_widenings_pass_through_everywhere() {
        use rebon_tool::ExecutionSurface::{Detached, Unattended};

        for surface in [Unattended, Detached] {
            for mode in ["default", "plan", "acceptEdits"] {
                assert!(
                    !escalation_refused(surface, mode, false),
                    "{surface}/{mode}"
                );
            }
        }
    }

    /// An attended session is exactly where these choices belong: a
    /// person read the plan and picked the option.
    #[test]
    fn an_attended_room_refuses_nothing() {
        use rebon_tool::ExecutionSurface::Interactive;

        for mode in [
            "auto",
            "bypassPermissions",
            "acceptEdits",
            "default",
            "plan",
        ] {
            assert!(!escalation_refused(Interactive, mode, false), "{mode}");
        }
    }

    /// The desktop app's case: every app chat is a background job, so the
    /// person who accepted `auto` for background jobs is the same person
    /// answering the plan dialog from a `Detached` room. Refusing them left
    /// the session in plan mode with the tool reporting success.
    #[test]
    fn a_room_with_a_door_honors_what_a_person_authorized_ahead_of_time() {
        use rebon_tool::ExecutionSurface::{Detached, Unattended};

        for mode in ["auto", "bypassPermissions"] {
            assert!(!escalation_refused(Detached, mode, true), "{mode}");
            // Nobody answered the dialog in an empty room — the auto-approver
            // did — so a prior acceptance says nothing about this choice.
            assert!(escalation_refused(Unattended, mode, true), "{mode}");
        }
    }

    /// The guarantee that makes the gate safe to have at all: whatever it
    /// refuses, plan mode still ends.
    #[test]
    fn the_fallback_mode_is_one_no_room_refuses() {
        use rebon_tool::ExecutionSurface::{Detached, Interactive, Unattended};

        for surface in [Interactive, Detached, Unattended] {
            assert!(
                !escalation_refused(surface, ESCALATION_FALLBACK_MODE, false),
                "{surface}"
            );
        }
        assert_ne!(ESCALATION_FALLBACK_MODE, "plan");
    }

    /// The tool result claimed the requested mode before the gate ran, so the
    /// correction has to reach the model somewhere. It rides the exit
    /// attachment, which is the next thing the model reads.
    #[test]
    fn the_exit_attachment_reports_the_mode_that_was_actually_applied() {
        let mut input = base_input();
        input.permission_mode = "acceptEdits".into();
        input.attachment_state.needs_plan_mode_exit_attachment = true;
        input.attachment_state.pending_exit_plan_text = Some("1. do the thing".into());
        input.attachment_state.pending_exit_mode_note =
            Some(escalation_downgrade_note("auto", ESCALATION_FALLBACK_MODE));

        let out = plan_mode_exit(&input);
        let text = only_text(&out.messages);
        assert!(text.contains("Exited Plan Mode"));
        assert!(text.contains("1. do the thing"));
        assert!(text.contains("`auto`"), "{text}");
        assert!(text.contains("`acceptEdits`"), "{text}");
        assert!(out.state_delta.clear_plan_mode_exit_flag);
    }

    // ── notify_plan_mode_tool ─────────────────────────────────────

    #[test]
    fn notify_plan_mode_tool_updates_server_state_before_poll() {
        let (state, sid) = make_session_with_mode("plan");
        let poller = PlanModeAttachmentPoller::new(state.clone(), sid.clone());

        // Simulate ExitPlanMode success (no clearContext): the poller
        // updates the state synchronously, so the next poll sees the
        // selected execution mode.
        let result = serde_json::json!({
            "exitedPlanMode": true,
            "permissionMode": "default",
            "mode": "default",
            "clearContext": false,
        });
        poller.notify_plan_mode_tool("ExitPlanMode", true, Some(&result));

        let session = state.get_session(&sid).unwrap();
        assert_eq!(session.permission_mode, "default");
        assert!(session.attachment_state.needs_plan_mode_exit_attachment);

        // Now poll — should emit the plan_mode_exit message, not a
        // plan_mode reminder.
        let messages = poller.poll(request(1));
        assert!(!messages.is_empty());
        let text = only_text(&messages);
        assert!(
            text.contains("Exited Plan Mode"),
            "expected exit message, got: {text}"
        );
    }

    #[test]
    fn notify_plan_mode_tool_noop_on_failure() {
        let (state, sid) = make_session_with_mode("plan");
        let poller = PlanModeAttachmentPoller::new(state.clone(), sid.clone());

        // Failed tool call should not change mode.
        poller.notify_plan_mode_tool("ExitPlanMode", false, None);
        let session = state.get_session(&sid).unwrap();
        assert_eq!(session.permission_mode, "plan");
    }

    #[test]
    fn notify_plan_mode_tool_with_clear_context_sets_pending_reset() {
        let (state, sid) = make_session_with_mode("plan");
        let poller = PlanModeAttachmentPoller::new(state.clone(), sid.clone());

        // ExitPlanMode with clearContext=true: should set pending reset.
        let result = serde_json::json!({
            "exitedPlanMode": true,
            "permissionMode": "auto",
            "mode": "auto",
            "clearContext": true,
            "plan": "1. fix the bug\n2. add tests",
        });
        poller.notify_plan_mode_tool("ExitPlanMode", true, Some(&result));

        // Mode should match the selected execution mode.
        let session = state.get_session(&sid).unwrap();
        assert_eq!(session.permission_mode, "auto");

        // take_context_reset should return the plan.
        let reset = poller.take_context_reset();
        assert!(reset.is_some());
        let msgs = reset.unwrap();
        assert_eq!(msgs.len(), 1);
        let text = match &msgs[0].content[0] {
            ApiContentBlock::Text(tb) => &tb.text,
            _ => panic!("expected text block"),
        };
        assert!(text.contains("Implement the following plan"));
        assert!(text.contains("1. fix the bug"));

        // Second call should return None (consumed).
        assert!(poller.take_context_reset().is_none());
    }

    #[test]
    fn notify_plan_mode_tool_without_clear_context_stores_plan_text() {
        let (state, sid) = make_session_with_mode("plan");
        let poller = PlanModeAttachmentPoller::new(state.clone(), sid.clone());

        // ExitPlanMode without clearContext: no pending reset, but
        // plan text should be stored for the exit attachment.
        let result = serde_json::json!({
            "exitedPlanMode": true,
            "permissionMode": "default",
            "mode": "default",
            "clearContext": false,
            "plan": "my plan",
        });
        poller.notify_plan_mode_tool("ExitPlanMode", true, Some(&result));

        assert!(poller.take_context_reset().is_none());

        // Plan text should be stored in attachment state.
        let session = state.get_session(&sid).unwrap();
        assert_eq!(session.permission_mode, "default");
        assert_eq!(
            session.attachment_state.pending_exit_plan_text.as_deref(),
            Some("my plan"),
        );

        // Poll should include the plan in the exit attachment.
        let messages = poller.poll(request(1));
        let text = only_text(&messages);
        assert!(text.contains("Approved Plan"));
        assert!(text.contains("my plan"));

        // After poll, the plan text should be cleared.
        let after = state.get_session(&sid).unwrap();
        assert!(after.attachment_state.pending_exit_plan_text.is_none());
    }

    #[test]
    fn notify_plan_mode_tool_preserves_nonclear_execution_modes() {
        for mode in ["auto", "acceptEdits"] {
            let (state, sid) = make_session_with_mode("plan");
            let poller = PlanModeAttachmentPoller::new(state.clone(), sid.clone());
            let result = serde_json::json!({
                "exitedPlanMode": true,
                "permissionMode": mode,
                "mode": mode,
                "clearContext": false,
                "plan": "my plan",
            });

            poller.notify_plan_mode_tool("ExitPlanMode", true, Some(&result));

            let session = state.get_session(&sid).unwrap();
            assert_eq!(session.permission_mode, mode);
            assert_eq!(
                session.attachment_state.pending_exit_plan_text.as_deref(),
                Some("my plan")
            );
            assert!(poller.take_context_reset().is_none());
        }
    }

    #[test]
    fn rejected_exit_plan_mode_keeps_plan_mode_and_context() {
        let (state, sid) = make_session_with_mode("plan");
        let poller = PlanModeAttachmentPoller::new(state.clone(), sid.clone());

        poller.notify_plan_mode_tool("ExitPlanMode", false, None);

        let session = state.get_session(&sid).unwrap();
        assert_eq!(session.permission_mode, "plan");
        assert!(session.attachment_state.pending_exit_plan_text.is_none());
        assert!(poller.take_context_reset().is_none());
    }
}
