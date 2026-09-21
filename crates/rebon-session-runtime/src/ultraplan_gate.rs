use rebon_types::{ultraplan_plan_hash_for_profile, RunPhase, UltraplanRunState};

/// What the runtime does with an `ExitPlanMode` submission during `/ultraplan`.
///
/// The planning workflow has exactly one hard gate: the user approving the
/// final plan. This decision is therefore deliberately thin. The runtime never
/// refuses a submission because the process looks incomplete — no question
/// asked, no requirement ledger, no sealed understanding, no reviewer PASS, no
/// coverage report, no stage reached. Those are semantic judgements the state
/// machine cannot make, and the user makes them when they read the plan.
///
/// The one refusal left honours a user decision rather than a workflow: the
/// model may not put the same rejected plan back in front of the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExitPlanGateDecision {
    Allow,
    Reject(String),
}

pub(crate) fn check_exit_plan_mode_submission(
    state: &UltraplanRunState,
    plan: &str,
) -> ExitPlanGateDecision {
    let plan_hash = ultraplan_plan_hash_for_profile(state.profile, plan);
    if state.user_rejected_plan_hash.as_deref() == Some(plan_hash.as_str()) {
        return ExitPlanGateDecision::Reject(format!(
            "ULTRAPLAN gate: the user already rejected this exact plan (hash {plan_hash}). Act on their feedback and submit a materially changed plan instead of re-asking for approval."
        ));
    }
    ExitPlanGateDecision::Allow
}

pub(crate) fn extract_plan_step_ids(plan: &str) -> Vec<String> {
    rebon_types::ultraplan_plan_step_ids(plan)
}

/// Bookkeeping for an answered `AskUserQuestion` during planning.
///
/// `asked_user_once` is kept as a record that a real decision turn happened —
/// the status line and resume prompt read it — but nothing gates on it.
pub(crate) fn mark_question_answered(state: &mut UltraplanRunState) {
    state.asked_user_once = true;
    state.phase = RunPhase::Researching;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_types::{
        ledger_to_manifest_snapshot, ultraplan_plan_hash, RequirementLedgerEntry, RequirementSource,
    };

    fn state() -> UltraplanRunState {
        UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1)
    }

    #[test]
    fn allows_a_first_plan_with_no_question_ledger_or_review() {
        assert_eq!(
            check_exit_plan_mode_submission(&state(), "P1. Do work"),
            ExitPlanGateDecision::Allow
        );
    }

    #[test]
    fn allows_free_text_plans_without_pn_steps_or_coverage() {
        let mut state = state();
        state.requirement_ledger.push(RequirementLedgerEntry {
            id: "R1".into(),
            title: "Do the work".into(),
            source: RequirementSource::Question,
            round_added: 1,
        });
        state.manifest = ledger_to_manifest_snapshot(&state.requirement_ledger);

        assert_eq!(
            check_exit_plan_mode_submission(&state, "Rewrite the loader, then re-run the suite."),
            ExitPlanGateDecision::Allow
        );
    }

    #[test]
    fn allows_delivery_without_review_pass_or_release() {
        let plan = "P1. Do work";
        let mut state = state();
        state.asked_user_once = true;
        state.pending_review_plan_hash = Some(ultraplan_plan_hash(plan));
        state.budget.adversarial_reviews_used = state.budget.max_adversarial_reviews;

        assert_eq!(
            check_exit_plan_mode_submission(&state, plan),
            ExitPlanGateDecision::Allow
        );
    }

    #[test]
    fn advisory_and_blocking_review_findings_are_both_advisory_to_the_gate() {
        let plan = "P1. Do work";
        let mut state = state();
        state.structured_review = Some(rebon_types::StructuredReview {
            plan_hash: ultraplan_plan_hash(plan),
            base_revision: state.ledger_revision,
            verdict: "FAIL".into(),
            findings: vec![rebon_types::ReviewerFinding {
                classification: rebon_types::ReviewerFindingClass::Blocker,
                code: "missing_test".into(),
                message: "No regression test is named".into(),
                evidence: Vec::new(),
            }],
            step_coverage: vec![rebon_types::ReviewStepCoverage {
                step_id: "P1".into(),
                ok: false,
                reason: "verification is absent".into(),
            }],
            requirements_patch: None,
        });

        // The reviewer informs the model and the user; it never blocks the
        // submission, so the plan still reaches the approval dialog.
        assert_eq!(
            check_exit_plan_mode_submission(&state, plan),
            ExitPlanGateDecision::Allow
        );
    }

    #[test]
    fn rejects_resubmitting_the_exact_plan_the_user_rejected() {
        let plan = "P1. Do work";
        let mut state = state();
        state.user_rejected_plan_hash = Some(ultraplan_plan_hash(plan));

        assert!(matches!(
            check_exit_plan_mode_submission(&state, plan),
            ExitPlanGateDecision::Reject(message) if message.contains("already rejected")
        ));

        assert_eq!(
            check_exit_plan_mode_submission(&state, "P1. Do work differently"),
            ExitPlanGateDecision::Allow
        );
    }

    #[test]
    fn draft_hash_marker_lines_do_not_change_plan_identity() {
        let plan = "## Plan\nP1. Do work";
        let hash = ultraplan_plan_hash(plan);
        let mut state = state();
        state.user_rejected_plan_hash = Some(hash.clone());

        let remarked = format!("ULTRAPLAN_DRAFT_HASH: {hash}\n{plan}");
        assert!(matches!(
            check_exit_plan_mode_submission(&state, &remarked),
            ExitPlanGateDecision::Reject(_)
        ));
    }

    #[test]
    fn answered_question_is_recorded_without_gating_anything() {
        let mut state = state();
        mark_question_answered(&mut state);

        assert!(state.asked_user_once);
        assert_eq!(state.phase, RunPhase::Researching);
    }

    #[test]
    fn extracts_step_ids_from_p_lines() {
        assert_eq!(
            extract_plan_step_ids("P1. One\n- detail\nP2: Two\nP2. dup\n"),
            vec!["P1", "P2"]
        );
    }

    #[test]
    fn extracts_step_ids_from_markdown_headings() {
        assert_eq!(
            extract_plan_step_ids("### P1. One\n#### P2: Two\n# Not a step\nP3) Three"),
            vec!["P1", "P2", "P3"]
        );
    }
}
