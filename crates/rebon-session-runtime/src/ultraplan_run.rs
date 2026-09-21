//! Writing down where an ultraplan run got to.
//!
//! A run's phase, its final gate record, the draft it submitted and the
//! evidence a reviewer cited all live on disk beside the session, not on
//! a screen: the terminal writes them while a person answers a prompt,
//! and nothing here needs to know that a person was watching.
//!
//! The modal that asks the question, and the status the terminal draws
//! from what is written here, stay in the binary's `tui`.

use std::path::{Path, PathBuf};

use rebon_core::permission::OutboundPermissionQuery;
use rebon_types::{
    analyze_ultraplan_plan, parse_markdown_manifest_items, ultraplan_execution_plan_payload,
    ultraplan_plan_hash_for_profile, ExecutionCard, HashDriftKind, HashDriftRecord,
    PlanStepCoverageInput, PolicyMode, ReviewerVerdictRecord, RunPhase, StructuredReview,
    UltraplanContext, UltraplanDiagnostic, UltraplanDiagnosticClass, UltraplanManifestSnapshot,
    UltraplanProfile, UltraplanRunState, VerdictSource,
};
use serde_json::Value;

use sha2::{Digest, Sha256};

use rebon_plugin_tasks::runtime::TaskSnapshot;

use crate::ultraplan_gate::{
    check_exit_plan_mode_submission, mark_question_answered, ExitPlanGateDecision,
};
use crate::ultraplan_review::reviewer_failure_fingerprint;
use crate::EngineSession;

/// Session-local phase for the local `/ultraplan` workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UltraplanPhase {
    PlanModeActive,
    Orchestrating,
    Researching,
    Reviewing,
    Synthesizing,
    AwaitingPlanApproval,
    Executing,
}

/// Visible, session-local status for the local `/ultraplan` workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UltraplanStatus {
    pub run_id: String,
    pub phase: UltraplanPhase,
    pub task_title: String,
    pub started_at_ms: Option<u64>,
    pub worker_count: Option<usize>,
    pub context: Option<UltraplanContext>,
    pub round: u32,
    pub last_verdict: Option<rebon_types::ReviewerVerdictRecord>,
    pub last_coverage: Option<rebon_types::PlanCoverageResult>,
    pub execution_reexploration_count: u32,
}

pub fn status_phase_from_run_phase(phase: RunPhase) -> Option<UltraplanPhase> {
    match phase {
        RunPhase::PlanModeActive => Some(UltraplanPhase::PlanModeActive),
        RunPhase::Orchestrating => Some(UltraplanPhase::Orchestrating),
        RunPhase::Researching => Some(UltraplanPhase::Researching),
        RunPhase::Reviewing => Some(UltraplanPhase::Reviewing),
        RunPhase::Synthesizing => Some(UltraplanPhase::Synthesizing),
        RunPhase::AwaitingPlanApproval => Some(UltraplanPhase::AwaitingPlanApproval),
        RunPhase::Executing => Some(UltraplanPhase::Executing),
        RunPhase::Done | RunPhase::Abandoned => None,
    }
}

/// Builds the runtime policy context for one planning turn.
///
/// The context is always built on the single (Standard) profile, including for
/// a run persisted before the profile fork was removed: the tool layer still
/// carries Grill-shaped checks keyed on `context.profile`, and reviving them
/// for a restored run would put back the interview protocol this workflow no
/// longer has. The persisted `state.profile` stays as a historical record.
pub fn ultraplan_context_for_phase(
    run_id: &str,
    phase: UltraplanPhase,
    manifest: Option<UltraplanManifestSnapshot>,
) -> Option<UltraplanContext> {
    if !ultraplan_runtime_enabled() {
        return None;
    }
    let mut context = UltraplanContext::planning_turn(
        run_id,
        format!("{phase:?}").to_ascii_lowercase(),
        ultraplan_policy_mode_from_env(),
    )
    .with_profile(UltraplanProfile::Standard);
    if let Some(manifest) = manifest {
        context = context.with_manifest(manifest);
    }
    Some(context)
}

pub(crate) fn capture_execution_artifact_hashes(cwd: &str, state: &mut UltraplanRunState) {
    state.file_hashes.clear();
    for file_ref in state
        .execution_cards
        .iter()
        .flat_map(|card| card.files.iter())
    {
        if let Some(path) = resolve_execution_card_file(cwd, file_ref) {
            if let Ok(bytes) = std::fs::read(&path) {
                state.file_hashes.insert(
                    path.display().to_string(),
                    format!("{:x}", Sha256::digest(&bytes)),
                );
            }
        }
    }
}

pub(crate) fn ultraplan_runtime_enabled() -> bool {
    !matches!(
        std::env::var("REBON_ULTRAPLAN_RUNTIME"),
        Ok(value)
            if matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
    )
}

pub(crate) fn ultraplan_policy_mode_from_env() -> PolicyMode {
    match std::env::var("REBON_ULTRAPLAN_POLICY_ENFORCE") {
        Ok(value)
            if matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no" | "observe"
            ) =>
        {
            PolicyMode::Observe
        }
        _ => PolicyMode::Enforce,
    }
}

pub(crate) fn resolve_execution_card_file(cwd: &str, file_ref: &str) -> Option<PathBuf> {
    let file = file_ref.trim().split_whitespace().next()?.trim();
    let file = file.split(':').next().unwrap_or(file).trim();
    if file.is_empty() || file.starts_with('`') && file.ends_with('`') && file.len() <= 2 {
        return None;
    }
    let file = file.trim_matches('`');
    let path = PathBuf::from(file);
    Some(if path.is_absolute() {
        path
    } else {
        PathBuf::from(cwd).join(path)
    })
}

/// Guards the execution handoff against a plan the user never saw.
///
/// This is an identity check, not a process gate: the plan handed to CEO or
/// ultrawork must be exactly the draft the run persisted and the user
/// approved. Whether that draft came from a reviewed, ledgered, multi-round
/// run or from one free-form turn is the model's business, not the runtime's.
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub fn validate_ultraplan_execution_approval(
    state: &UltraplanRunState,
    plan: &str,
) -> Result<(), String> {
    let draft = state
        .last_plan_draft
        .as_deref()
        .ok_or_else(|| "approved RunState has no persisted plan draft".to_string())?;
    let plan_hash = ultraplan_plan_hash_for_profile(state.profile, draft);
    if state.plan_hash.as_deref() != Some(plan_hash.as_str()) {
        return Err(format!(
            "approved plan no longer matches RunState `{}`",
            state.run_id
        ));
    }
    if ultraplan_execution_plan_payload(state).as_deref() != Some(plan) {
        return Err("approved plan does not match the persisted draft".into());
    }
    Ok(())
}

pub fn mutate_ultraplan_run_cas<F>(
    session: &EngineSession,
    run_id: &str,
    mut mutation: F,
) -> Result<UltraplanRunState, String>
where
    F: FnMut(&mut UltraplanRunState) -> Result<(), String>,
{
    for attempt in 0..=1 {
        let Some(mut state) =
            rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
        else {
            return Err(format!("run state `{run_id}` is missing or unreadable"));
        };
        let expected_revision = state.state_revision;
        mutation(&mut state)?;
        state.updated_at_ms =
            (rebon_types::wall_clock_ms_u128() as u64).max(state.updated_at_ms.saturating_add(1));
        // Every CAS write must advance state_revision, even for state-only
        // field edits, or a concurrent writer could pass the same expected
        // revision and silently clobber this write.
        state.state_revision = state
            .state_revision
            .max(expected_revision.saturating_add(1));
        state.prepare_for_persist();
        match rebon_session::save_ultraplan_run_cas(
            &session.projects_root,
            &session.cwd,
            expected_revision,
            &state,
        ) {
            Ok(()) => return Ok(state),
            Err(rebon_session::UltraplanRunStoreError::StaleRevision { .. }) if attempt == 0 => {}
            Err(err) => return Err(err.to_string()),
        }
    }
    Err(format!(
        "run state `{run_id}` remained stale after one reload"
    ))
}

pub(crate) fn set_exit_plan_revision(outbound: &mut OutboundPermissionQuery, revision: u64) {
    if let Some(Value::Object(input)) = outbound.tool_input.as_mut() {
        input.insert("ledger_revision".into(), Value::Number(revision.into()));
    }
}

pub(crate) fn persist_final_gate_record(
    session: &EngineSession,
    run_id: &str,
    plan_hash: &str,
    outcome: rebon_types::FinalGateOutcome,
    diagnostics: Vec<rebon_types::UltraplanDiagnostic>,
) -> Result<UltraplanRunState, String> {
    for attempt in 0..=1 {
        let Some(mut state) =
            rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
        else {
            return Err(format!(
                "ULTRAPLAN gate: run state `{run_id}` disappeared before final-gate persistence"
            ));
        };
        if state.plan_hash.as_deref() != Some(plan_hash) {
            return Err(format!(
                "ULTRAPLAN gate: active plan changed before final-gate persistence; expected {plan_hash}, current {:?}. Reload and revalidate once.",
                state.plan_hash
            ));
        }
        let expected_revision = state.state_revision;
        let mut changed = false;
        // Recording the final gate is itself what moves the derived stage to
        // FinalGate; the plan-hash guard above is the actual protection.
        for diagnostic in &diagnostics {
            let already_recorded = state.diagnostics.iter().any(|existing| {
                existing.class == diagnostic.class
                    && existing.message == diagnostic.message
                    && existing.plan_hash == diagnostic.plan_hash
            });
            if !already_recorded {
                state.record_diagnostic(diagnostic.clone());
                changed = true;
            }
        }
        let gate_matches = state.final_gate.as_ref().is_some_and(|gate| {
            gate.outcome == outcome
                && gate.plan_hash == plan_hash
                && gate.diagnostics == diagnostics
        });
        if !gate_matches {
            state.record_final_gate(outcome, plan_hash.to_string(), diagnostics.clone());
            changed = true;
        }
        if changed {
            state.write_checkpoint(
                plan_evidence_references(&state),
                match outcome {
                    rebon_types::FinalGateOutcome::Pass => {
                        "await explicit user approval of the verified final plan"
                    }
                    rebon_types::FinalGateOutcome::Degraded => {
                        "await explicit user approval of the best plan and diagnostics"
                    }
                    rebon_types::FinalGateOutcome::Revalidate => {
                        "reload the current RunState and rerun final validation once"
                    }
                    rebon_types::FinalGateOutcome::Blocked => "resolve the hard final-gate blocker",
                },
            );
            match rebon_session::save_ultraplan_run_cas(
                &session.projects_root,
                &session.cwd,
                expected_revision,
                &state,
            ) {
                Ok(()) => {
                    return Ok(rebon_session::load_ultraplan_run(
                        &session.projects_root,
                        &session.cwd,
                        run_id,
                    )
                    .unwrap_or(state));
                }
                Err(rebon_session::UltraplanRunStoreError::StaleRevision { .. })
                    if attempt == 0 =>
                {
                    continue;
                }
                Err(err) => {
                    return Err(format!(
                        "ULTRAPLAN gate: failed to persist final-gate outcome: {err}"
                    ));
                }
            }
        }
        return Ok(state);
    }
    Err("ULTRAPLAN gate: final-gate persistence remained stale after one reload".into())
}

pub fn build_ultraplan_rejection_extra_text(
    feedback: Option<&str>,
    rejection_state: Option<&UltraplanRunState>,
) -> String {
    let mut text = format!(
        "{}: user rejected the /ultraplan plan. ",
        rebon_core::permission::ULTRAPLAN_REJECTION_FEEDBACK_PREFIX
    );
    match feedback.map(str::trim).filter(|text| !text.is_empty()) {
        Some(feedback) => {
            text.push_str("Feedback: ");
            text.push_str(feedback);
            text.push_str(". Act on it and submit one materially revised plan; the identical plan cannot be resubmitted. Decide for yourself whether that needs more research, another question, or only an edit to the draft.");
        }
        None => {
            text.push_str("No feedback was provided. Use AskUserQuestion to ask what requirement the plan failed to satisfy before revising; do not blindly resubmit the same plan.");
        }
    }
    if let Some(state) = rejection_state {
        text.push_str("\n\nAUTHORITATIVE UPDATED /ultraplan LOOP STATE AFTER REJECTION\n");
        text.push_str(&format!("run_id: {}\n", state.run_id));
        text.push_str(&format!("current_round: {}\n", state.round));
        text.push_str(&format!("workflow_stage: {:?}\n", state.stage()));
        text.push_str(&format!("legacy_phase: {:?}\n", state.phase));
        text.push_str(&format!(
            "budget: research {}/{}, plan_revisions {}/{}, adversarial_reviews {}/{}\n",
            state.budget.research_agents_used,
            state.budget.max_research_agents,
            state.budget.plan_revisions_used,
            state.budget.max_plan_revisions,
            state.budget.adversarial_reviews_used,
            state.budget.max_adversarial_reviews,
        ));
        if let Some(verdict) = state.reviewer_verdicts.last() {
            text.push_str("latest_verdict:\n");
            text.push_str(&format!("  round: {}\n", verdict.round));
            text.push_str(&format!("  source: {:?}\n", verdict.source));
            text.push_str(&format!("  verdict: {}\n", verdict.verdict));
            text.push_str(&format!("  blocking_gaps: {}\n", verdict.blocking_gaps));
        } else {
            text.push_str("latest_verdict: none\n");
        }
        text.push_str("This is state, not a gate: nothing here blocks the next submission except the rejected plan itself.\n");
    }
    text
}

pub fn persist_ultraplan_phase(
    session: &EngineSession,
    run_id: &str,
    plan: &str,
    phase: RunPhase,
) -> Result<UltraplanRunState, String> {
    mutate_ultraplan_run_cas(session, run_id, |state| {
        if phase == RunPhase::Executing {
            validate_ultraplan_execution_approval(state, plan)?;
        }
        state.phase = phase;
        if phase == RunPhase::Executing {
            state.write_checkpoint(
                plan_evidence_references(state),
                "execute only the explicitly approved plan",
            );
        }
        Ok(())
    })
    .map_err(|err| format!("failed to persist ultraplan execution phase: {err}"))
}

pub(crate) fn plan_evidence_references(
    state: &UltraplanRunState,
) -> Vec<rebon_types::UltraplanEvidenceReference> {
    state
        .execution_cards
        .iter()
        .flat_map(|card| {
            card.files
                .iter()
                .map(move |location| rebon_types::UltraplanEvidenceReference {
                    location: location.clone(),
                    claim: format!("{}: {}", card.step, card.change),
                })
        })
        .collect()
}

/// The message the terminal should put in the transcript, if any. `Some`
/// also means "scroll to the bottom": the user needs to read it.
pub fn persist_ultraplan_ask_user_question_gate(
    ultraplan_status: &mut Option<UltraplanStatus>,
    session: Option<&EngineSession>,
    updated_input: &Value,
) -> Option<String> {
    let (Some(session), Some(status)) = (session, ultraplan_status.as_ref()) else {
        return None;
    };
    let run_id = status.run_id.clone();
    // Every answered question is recorded the same way. There is no interview
    // protocol to enforce (single-question shape, sealed understanding, typed
    // confirmation hashes): the model chooses how and how often to ask, and
    // the recorded turns exist so a resumed run knows what is already settled.
    let state = match mutate_ultraplan_run_cas(session, &run_id, |state| {
        let was_scope_confirm = state.stage() == rebon_types::UltraplanStage::ScopeConfirm;
        if let (Some(questions), Some(answers)) = (
            updated_input.get("questions").and_then(Value::as_array),
            updated_input.get("answers").and_then(Value::as_object),
        ) {
            for question in questions {
                let Some(question_text) = question.get("question").and_then(Value::as_str) else {
                    continue;
                };
                let Some(answer) = answers.get(question_text).and_then(Value::as_str) else {
                    continue;
                };
                let recommended_answer = question
                    .get("options")
                    .and_then(Value::as_array)
                    .and_then(|options| options.first())
                    .and_then(|option| option.get("label"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                state.record_interview_turn(
                    question_text.to_string(),
                    recommended_answer,
                    answer.to_string(),
                );
            }
        }
        mark_question_answered(state);
        // The stage is derived, so an answered question leaves ScopeConfirm on
        // its own; the milestone checkpoint is what anchors crash-resume
        // guidance.
        if was_scope_confirm && state.stage() != rebon_types::UltraplanStage::ScopeConfirm {
            state.write_checkpoint(Vec::new(), "continue planning from the answered decision");
        }
        Ok(())
    }) {
        Ok(state) => state,
        Err(err) => {
            tracing::warn!(error = %err, run_id = %run_id, "failed to persist ultraplan question answer");
            // The user answered but the run state did not record it; a silent
            // drop leaves the model believing the confirmation/interview turn
            // took effect and looping on the gate.
            return Some(format!(
                "Ultraplan did not record the last AskUserQuestion answer for run `{run_id}`: {err}. The confirmation/interview state is unchanged; re-ask the question after resolving the mismatch."
            ));
        }
    };
    if let Some(status) = ultraplan_status.as_mut() {
        status.phase = status_phase_from_run_phase(state.phase).unwrap_or(status.phase);
        status.context =
            ultraplan_context_for_phase(&state.run_id, status.phase, state.manifest.clone());
    }
    None
}

/// What the ultraplan gate decided about an `ExitPlanMode` call.
///
/// A refusal carries the query back out rather than answering it here: the
/// answer goes on the query's own channel, which is a `oneshot::Sender`
/// that can only be sent once, and clearing the modal afterwards is the
/// terminal's business.
pub enum ExitPlanGateOutcome {
    /// The call may go on to the permission prompt.
    Proceed(OutboundPermissionQuery),
    /// The gate refused, with the feedback the model should read.
    Reject {
        outbound: OutboundPermissionQuery,
        feedback: String,
    },
}

pub fn maybe_gate_ultraplan_exit_plan_mode(
    ultraplan_status: &mut Option<UltraplanStatus>,
    task_snapshots: &[rebon_plugin_tasks::runtime::TaskSnapshot],
    session: &EngineSession,
    mut outbound: OutboundPermissionQuery,
) -> ExitPlanGateOutcome {
    if outbound.tool_name != "ExitPlanMode" {
        return ExitPlanGateOutcome::Proceed(outbound);
    }
    let Some(status) = ultraplan_status.as_ref() else {
        return ExitPlanGateOutcome::Proceed(outbound);
    };
    if matches!(
        status.phase,
        crate::ultraplan_run::UltraplanPhase::Executing
    ) {
        return ExitPlanGateOutcome::Proceed(outbound);
    }
    let plan = outbound
        .tool_input
        .as_ref()
        .and_then(|value| value.get("plan"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if plan.trim().is_empty() {
        return ExitPlanGateOutcome::Proceed(outbound);
    }
    // Typed coverage stays supported for runs that keep a PlanLedger, but it
    // is now purely informational: a plan without it is submitted normally.
    let step_coverage = outbound
        .tool_input
        .as_ref()
        .and_then(|value| value.get("step_coverage"))
        .cloned()
        .map(serde_json::from_value::<Vec<PlanStepCoverageInput>>)
        .transpose()
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "invalid typed ultraplan step coverage reached permission gate");
            None
        })
        .unwrap_or_default();

    let state = match persist_ultraplan_exit_plan_draft_submission(
        ultraplan_status,
        task_snapshots,
        Some(session),
        &plan,
        &step_coverage,
    ) {
        Ok(state) => state,
        Err(err) => {
            return ExitPlanGateOutcome::Reject {
                outbound,
                feedback: format!("ULTRAPLAN gate: {err}"),
            };
        }
    };
    // The model no longer has to carry a revision; the runtime stamps the
    // current one so the approved payload stays bound to this run head.
    set_exit_plan_revision(&mut outbound, state.ledger_revision);

    match check_exit_plan_mode_submission(&state, &plan) {
        ExitPlanGateDecision::Allow => {
            match persist_final_gate_record(
                session,
                &state.run_id,
                &ultraplan_plan_hash_for_profile(state.profile, &plan),
                rebon_types::FinalGateOutcome::Pass,
                Vec::new(),
            ) {
                Ok(saved) => {
                    set_exit_plan_revision(&mut outbound, saved.ledger_revision);
                    ExitPlanGateOutcome::Proceed(outbound)
                }
                Err(err) => ExitPlanGateOutcome::Reject {
                    outbound,
                    feedback: err,
                },
            }
        }
        ExitPlanGateDecision::Reject(feedback) => {
            ExitPlanGateOutcome::Reject { outbound, feedback }
        }
    }
}

pub fn persist_ultraplan_rejection_feedback(
    ultraplan_status: &mut Option<UltraplanStatus>,
    session: Option<&EngineSession>,
    feedback: Option<&str>,
) -> Option<UltraplanRunState> {
    let (Some(session), Some(status)) = (session, ultraplan_status.as_ref()) else {
        return None;
    };
    let run_id = status.run_id.clone();
    let state = match mutate_ultraplan_run_cas(session, &run_id, |state| {
        state.round = state.round.saturating_add(1).max(2);
        state.phase = RunPhase::Synthesizing;
        let rejected_plan_hash = state
            .last_plan_draft
            .as_deref()
            .map(|draft| ultraplan_plan_hash_for_profile(state.profile, draft));
        // Recording the rejected hash is what drops the derived stage out of
        // FinalGate; no explicit restart is needed.
        state.final_gate = None;
        state.user_rejected_plan_hash = rejected_plan_hash;
        state.reviewer_verdicts.push(ReviewerVerdictRecord {
            round: state.round,
            verdict: feedback
                .filter(|text| !text.trim().is_empty())
                .map(|text| format!("USER_REJECTED: {text}"))
                .unwrap_or_else(|| "USER_REJECTED".into()),
            blocking_gaps: 1,
            source: VerdictSource::UserRejection,
        });
        Ok(())
    }) {
        Ok(state) => state,
        Err(err) => {
            tracing::warn!(
                error = %err,
                run_id = %run_id,
                "failed to persist ExitPlanMode ultraplan rejection feedback"
            );
            return None;
        }
    };
    if let Some(status) = ultraplan_status.as_mut() {
        status.round = state.round.max(1);
        status.phase = crate::ultraplan_run::UltraplanPhase::Synthesizing;
        status.last_verdict = state.reviewer_verdicts.last().cloned();
        status.last_coverage = state.last_coverage.clone();
    }
    Some(state)
}

/// Records the submitted draft on the run so the approval dialog, the
/// execution handoff, and any resume all refer to the same plan.
///
/// This is persistence, not validation: a stale or absent `ledger_revision`
/// and missing coverage are no longer reasons to refuse a submission.
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub fn persist_ultraplan_exit_plan_draft_submission(
    ultraplan_status: &mut Option<UltraplanStatus>,
    task_snapshots: &[rebon_plugin_tasks::runtime::TaskSnapshot],
    session: Option<&EngineSession>,
    plan: &str,
    step_coverage: &[PlanStepCoverageInput],
) -> Result<UltraplanRunState, String> {
    let (Some(session), Some(status)) = (session, ultraplan_status.as_ref()) else {
        return Err("active ultraplan session is unavailable".into());
    };
    let run_id = status.run_id.clone();
    for attempt in 0..=1 {
        let Some(mut state) =
            rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, &run_id)
        else {
            return Err(format!("run state `{run_id}` is missing or unreadable"));
        };
        let current_revision = state.state_revision;
        let snapshots = task_snapshots;
        let research_agents_used = snapshots
            .iter()
            .filter(|snapshot| {
                snapshot.ultraplan_id() == Some(run_id.as_str())
                    && snapshot.ultraplan_role() == Some("researcher")
            })
            .count() as u32;
        let adversarial_reviews_used = snapshots
            .iter()
            .filter(|snapshot| {
                snapshot.ultraplan_id() == Some(run_id.as_str())
                    && snapshot.ultraplan_role() == Some("reviewer")
                    && snapshot
                        .metadata
                        .get("ultraplan_retry_attempt")
                        .and_then(Value::as_bool)
                        != Some(true)
            })
            .count() as u32;
        let budget_changed =
            state.synchronize_worker_budget_usage(research_agents_used, adversarial_reviews_used);
        let analysis = analyze_ultraplan_plan(plan, &state.requirement_ledger, step_coverage);
        let cards = analysis
            .steps
            .iter()
            .map(|step| step.card.clone())
            .collect::<Vec<_>>();
        // Setting the plan artifacts re-derives the stage on its own: a
        // changed hash drops any stale review/gate bindings.
        let plan_changed =
            state.set_plan_artifacts(plan.to_string(), analysis.coverage.clone(), cards);
        if budget_changed || plan_changed {
            state.write_checkpoint(
                plan_evidence_references(&state),
                "await the user's approval of the submitted plan",
            );
        }
        capture_execution_artifact_hashes(&session.cwd, &mut state);
        // CAS writes always advance state_revision (see mutate_ultraplan_run_cas).
        state.state_revision = state.state_revision.max(current_revision.saturating_add(1));
        match rebon_session::save_ultraplan_run_cas(
            &session.projects_root,
            &session.cwd,
            current_revision,
            &state,
        ) {
            Ok(()) => {
                let saved = rebon_session::load_ultraplan_run(
                    &session.projects_root,
                    &session.cwd,
                    &run_id,
                )
                .unwrap_or(state);
                if let Some(status) = ultraplan_status.as_mut() {
                    status.last_coverage = saved.last_coverage.clone();
                    if let Some(context) = status.context.as_mut() {
                        *context = context
                            .clone()
                            .with_run_head(&saved.head())
                            .with_execution_cards(saved.execution_cards.clone());
                    }
                }
                return Ok(saved);
            }
            Err(rebon_session::UltraplanRunStoreError::StaleRevision { .. }) if attempt == 0 => {}
            Err(err) => {
                return Err(format!(
                    "failed to persist draft at state revision {current_revision}: {err}"
                ));
            }
        }
    }
    Err("failed to persist the submitted draft: run state remained stale after one reload".into())
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn persist_ultraplan_exit_plan_draft(
    ultraplan_status: &mut Option<UltraplanStatus>,
    session: Option<&EngineSession>,
    plan: &str,
) {
    let (Some(session), Some(status)) = (session, ultraplan_status.as_ref()) else {
        return;
    };
    let Some(state) =
        rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, &status.run_id)
    else {
        return;
    };
    let first_step = analyze_ultraplan_plan(plan, &state.requirement_ledger, &[])
        .steps
        .first()
        .map(|step| step.step_id.clone());
    let coverage = first_step
        .map(|step_id| {
            vec![PlanStepCoverageInput {
                step_id,
                requirement_ids: state
                    .requirement_ledger
                    .iter()
                    .map(|entry| entry.id.clone())
                    .collect(),
            }]
        })
        .unwrap_or_default();
    let _ = persist_ultraplan_exit_plan_draft_submission(
        ultraplan_status,
        &[],
        Some(session),
        plan,
        &coverage,
    );
}

/// A manifest larger than this is refused rather than read into memory.
const ULTRAPLAN_MANIFEST_MAX_BYTES: u64 = 1024 * 1024;

/// Write the run state beside the session, complaining in the log rather
/// than to the caller: the run has already moved on by the time this fails.
pub fn persist_ultraplan_run(session: &EngineSession, state: &UltraplanRunState) {
    if let Err(err) = rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, state)
    {
        tracing::warn!(
            error = %err,
            session_id = %session.session_id,
            run_id = %state.run_id,
            "failed to persist ultraplan run state"
        );
    }
}

/// Read a Markdown manifest or custom plan from disk and record what it
/// said, together with the hash that later tells us whether it drifted.
pub fn load_ultraplan_manifest_snapshot(
    requested_path: &Path,
    cwd: &str,
) -> Result<UltraplanManifestSnapshot, String> {
    let resolved_path = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        PathBuf::from(cwd).join(requested_path)
    };
    let metadata = std::fs::metadata(&resolved_path)
        .map_err(|err| format!("cannot read {}: {err}", resolved_path.display()))?;
    if metadata.is_dir() {
        return Err(format!("{} is a directory", resolved_path.display()));
    }
    if metadata.len() > ULTRAPLAN_MANIFEST_MAX_BYTES {
        return Err(format!(
            "{} is too large ({} bytes; maximum is {} bytes)",
            resolved_path.display(),
            metadata.len(),
            ULTRAPLAN_MANIFEST_MAX_BYTES
        ));
    }

    let bytes = std::fs::read(&resolved_path)
        .map_err(|err| format!("cannot read {}: {err}", resolved_path.display()))?;
    let content_sha256 = format!("{:x}", Sha256::digest(&bytes));
    let content = std::str::from_utf8(&bytes)
        .map_err(|err| format!("{} is not valid UTF-8: {err}", resolved_path.display()))?;
    let items = parse_markdown_manifest_items(content).map_err(|err| err.to_string())?;
    if items.is_empty() {
        return Err(format!(
            "{} contains no Markdown manifest/custom plan items. Use checklist rows, bullets, ordered lists, or headings with `ID: title` or `ID - title`.",
            resolved_path.display()
        ));
    }
    let canonical_path =
        std::fs::canonicalize(&resolved_path).unwrap_or_else(|_| resolved_path.clone());
    Ok(UltraplanManifestSnapshot {
        source_path: requested_path.display().to_string(),
        canonical_path: canonical_path.display().to_string(),
        display_path: requested_path.display().to_string(),
        content_sha256,
        items,
    })
}

/// Whether the manifest a run was planned against still hashes the same.
pub fn detect_manifest_drift_warning(state: &UltraplanRunState) -> Option<String> {
    let manifest = state.manifest.as_ref()?;
    if manifest.canonical_path == "ledger" || manifest.source_path == "ledger" {
        return None;
    }
    match std::fs::read(&manifest.canonical_path) {
        Ok(bytes) => {
            let current_sha256 = format!("{:x}", Sha256::digest(&bytes));
            if current_sha256 == manifest.content_sha256 {
                None
            } else {
                Some(format!(
                    "Ultraplan manifest/custom plan file drift detected for run {}: {} hash changed (stored {}, current {}). Reconcile the file before continuing planning.",
                    state.run_id, manifest.display_path, manifest.content_sha256, current_sha256
                ))
            }
        }
        Err(err) => Some(format!(
            "Ultraplan manifest/custom plan file for run {} could not be read at {}: {err}. Reconcile the file before continuing planning.",
            state.run_id, manifest.canonical_path
        )),
    }
}

pub fn parse_execution_cards(plan: &str) -> Vec<ExecutionCard> {
    analyze_ultraplan_plan(plan, &[], &[])
        .steps
        .into_iter()
        .map(|step| step.card)
        .collect()
}

/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub fn capture_execution_artifacts(cwd: &str, plan: &str, state: &mut UltraplanRunState) {
    if state.execution_cards.is_empty() {
        state.execution_cards = parse_execution_cards(plan);
    }
    capture_execution_artifact_hashes(cwd, state);
}

/// Which of the files a run recorded have changed or gone missing.
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub fn compare_hash_drift(
    file_hashes: &std::collections::BTreeMap<String, String>,
) -> Vec<HashDriftRecord> {
    let mut drift = Vec::new();
    for (path, stored_sha256) in file_hashes {
        match std::fs::read(path) {
            Ok(bytes) => {
                let current_sha256 = format!("{:x}", Sha256::digest(&bytes));
                if current_sha256 != *stored_sha256 {
                    drift.push(HashDriftRecord {
                        path: path.clone(),
                        stored_sha256: stored_sha256.clone(),
                        current_sha256: Some(current_sha256),
                        kind: HashDriftKind::Changed,
                    });
                }
            }
            Err(_) => drift.push(HashDriftRecord {
                path: path.clone(),
                stored_sha256: stored_sha256.clone(),
                current_sha256: None,
                kind: HashDriftKind::Missing,
            }),
        }
    }
    drift
}

/// Check the approval, record what the plan touches, and report the drift
/// found while doing it into the context the next phase carries.
pub fn prepare_execution_context_from_state(
    cwd: &str,
    plan: &str,
    state: &mut UltraplanRunState,
    context: &mut UltraplanContext,
) -> Result<(), String> {
    validate_ultraplan_execution_approval(state, plan)?;
    let drift = compare_hash_drift(&state.file_hashes);
    capture_execution_artifacts(cwd, plan, state);
    context.execution_cards = state.execution_cards.clone();
    context.hash_drift = drift;
    Ok(())
}

/// Record a failed reviewer round against the run, retrying once when
/// another writer wins the compare-and-swap first.
pub fn persist_reviewer_retry_failure(
    session: &EngineSession,
    snapshot: &TaskSnapshot,
    run_id: &str,
    diagnostic: UltraplanDiagnostic,
) -> Option<rebon_types::UltraplanRunState> {
    for attempt in 0..=1 {
        let mut state =
            rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)?;
        let expected_revision = state.state_revision;
        if let Some(plan_hash) = diagnostic.plan_hash.as_deref() {
            state.record_tool_error_attempts(
                reviewer_failure_fingerprint(
                    run_id,
                    plan_hash,
                    diagnostic.class,
                    &diagnostic.message,
                ),
                1,
            );
        }
        ingest_reviewer_failure(
            snapshot,
            &mut state,
            diagnostic.class,
            diagnostic.message.clone(),
        );
        match rebon_session::save_ultraplan_run_cas(
            &session.projects_root,
            &session.cwd,
            expected_revision,
            &state,
        ) {
            Ok(()) => {
                return Some(
                    rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
                        .unwrap_or(state),
                );
            }
            Err(rebon_session::UltraplanRunStoreError::StaleRevision { .. }) if attempt == 0 => {}
            Err(_) => return None,
        }
    }
    None
}

/// Bring the run's recorded worker budget back in line with what the
/// tasks actually used. Bookkeeping, so a lost race is only logged.
pub fn synchronize_ultraplan_task_budget(
    session: &EngineSession,
    run_id: &str,
    research_agents_used: u32,
    adversarial_reviews_used: u32,
) {
    for attempt in 0..=1 {
        let Some(mut state) =
            rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
        else {
            return;
        };
        let expected_revision = state.state_revision;
        if !state.synchronize_worker_budget_usage(research_agents_used, adversarial_reviews_used) {
            return;
        }
        // Budget synchronization is bookkeeping, not a milestone; it no
        // longer writes a checkpoint.
        match rebon_session::save_ultraplan_run_cas(
            &session.projects_root,
            &session.cwd,
            expected_revision,
            &state,
        ) {
            Ok(()) => return,
            Err(rebon_session::UltraplanRunStoreError::StaleRevision { .. }) if attempt == 0 => {
                continue;
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    run_id,
                    "failed to persist ultraplan task budget"
                );
                return;
            }
        }
    }
}

/// Check a structured review against the draft it claims to review: the
/// plan hash, the ledger revision the requirements were at, and one
/// coverage entry for each plan step, no more and no fewer.
pub fn validate_structured_reviewer_result(
    snapshot: &TaskSnapshot,
    state: &rebon_types::UltraplanRunState,
    mut review: StructuredReview,
) -> Result<StructuredReview, (UltraplanDiagnosticClass, String)> {
    let expected_plan_hash = snapshot.metadata_str("plan_hash").ok_or_else(|| {
        (
            UltraplanDiagnosticClass::ReviewerMalformed,
            "reviewer task is missing its bound plan hash".to_string(),
        )
    })?;
    let expected_revision = snapshot
        .metadata
        .get("ultraplan_ledger_revision")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            (
                UltraplanDiagnosticClass::ReviewerMalformed,
                "reviewer task is missing its bound ledger revision".to_string(),
            )
        })?;
    let expected_requirements_hash = snapshot
        .metadata_str("ultraplan_requirements_hash")
        .ok_or_else(|| {
            (
                UltraplanDiagnosticClass::ReviewerMalformed,
                "reviewer task is missing its bound requirements hash".to_string(),
            )
        })?;
    if review.plan_hash != expected_plan_hash
        || state.plan_hash.as_deref() != Some(expected_plan_hash)
    {
        return Err((
            UltraplanDiagnosticClass::StaleGate,
            format!(
                "review plan hash does not match the active draft: expected {expected_plan_hash}, got {}",
                review.plan_hash
            ),
        ));
    }
    if review.base_revision != expected_revision {
        return Err((
            UltraplanDiagnosticClass::ReviewerMalformed,
            format!(
                "review base revision mismatch: expected {expected_revision}, got {}",
                review.base_revision
            ),
        ));
    }
    if state.ledger_revision != expected_revision {
        return Err((
            UltraplanDiagnosticClass::StaleGate,
            format!(
                "requirements changed while the reviewer was running: expected ledger revision {expected_revision}, current revision is {}",
                state.ledger_revision
            ),
        ));
    }
    if state.requirements_hash != expected_requirements_hash {
        return Err((
            UltraplanDiagnosticClass::StaleGate,
            "requirements changed while the reviewer was running; reload and revalidate the current plan"
                .to_string(),
        ));
    }
    if review
        .findings
        .iter()
        .any(|finding| finding.code.trim().is_empty() || finding.message.trim().is_empty())
    {
        return Err((
            UltraplanDiagnosticClass::ReviewerMalformed,
            "review findings require non-empty code and message fields".to_string(),
        ));
    }

    let plan = state.last_plan_draft.as_deref().unwrap_or_default();
    let expected_steps = crate::ultraplan_gate::extract_plan_step_ids(plan);
    let expected = expected_steps
        .iter()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    let mut counts = std::collections::HashMap::<&str, usize>::new();
    for coverage in &review.step_coverage {
        if !expected.contains(coverage.step_id.as_str()) {
            return Err((
                UltraplanDiagnosticClass::ReviewerMalformed,
                format!(
                    "review coverage references unknown plan step `{}`",
                    coverage.step_id
                ),
            ));
        }
        *counts.entry(coverage.step_id.as_str()).or_default() += 1;
    }
    for step_id in expected_steps {
        match counts.get(step_id.as_str()).copied().unwrap_or(0) {
            1 => {}
            0 => {
                return Err((
                    UltraplanDiagnosticClass::ReviewerMalformed,
                    format!("review coverage is missing plan step `{step_id}`"),
                ));
            }
            count => {
                return Err((
                    UltraplanDiagnosticClass::ReviewerMalformed,
                    format!("review coverage contains plan step `{step_id}` {count} times"),
                ));
            }
        }
    }

    if let Some(patch) = review.requirements_patch.as_ref() {
        if patch.base_revision != review.base_revision {
            return Err((
                UltraplanDiagnosticClass::ReviewerMalformed,
                format!(
                    "requirements patch targets revision {}, but the review targets {}",
                    patch.base_revision, review.base_revision
                ),
            ));
        }
        if review.blockers().next().is_none() {
            review.requirements_patch = None;
        }
    }
    Ok(review)
}

/// Write a reviewer failure into the run: an UNKNOWN verdict, the
/// diagnostic that explains it, and the phase that follows from it.
pub fn ingest_reviewer_failure(
    snapshot: &TaskSnapshot,
    state: &mut rebon_types::UltraplanRunState,
    class: UltraplanDiagnosticClass,
    message: String,
) {
    state.pending_review_plan_hash = None;
    let plan_hash = snapshot
        .metadata_str("plan_hash")
        .map(str::to_string)
        .or_else(|| state.plan_hash.clone());
    state.reviewer_verdicts.push(ReviewerVerdictRecord {
        round: state.round.max(1),
        verdict: "UNKNOWN".into(),
        blocking_gaps: 0,
        source: VerdictSource::Agent,
    });
    state.record_diagnostic(UltraplanDiagnostic {
        class,
        message,
        ledger_revision: state.ledger_revision,
        stage: state.stage(),
        plan_hash,
        details: serde_json::json!({"task_id": snapshot.id.to_string()}),
    });
    state.phase = RunPhase::Synthesizing;
    state.write_checkpoint(
        Vec::new(),
        "deliver the best structurally verified plan with machine-readable diagnostics",
    );
}
