use async_trait::async_trait;
use serde_json::{json, Value};

use rebon_tool::plan_mode::EXIT_PLAN_MODE_TOOL_NAME;
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    validation_outcome_from, PermissionDecision, PermissionRequest, ToolError, ToolId,
    ToolInputSchema, ToolResult, ValidationOutcome,
};
use rebon_types::{
    analyze_ultraplan_plan, ultraplan_execution_plan_payload, ultraplan_plan_hash_for_profile,
    PlanStepCoverageInput, PolicyMode, UltraplanProfile,
};

const INVALID_INPUT_CODE: i64 = 400;

#[derive(Debug, Clone, Default)]
pub struct ExitPlanModeTool;

#[derive(Debug, Clone)]
struct ExitPlanModeInput {
    plan: String,
    ledger_revision: Option<u64>,
    step_coverage: Vec<PlanStepCoverageInput>,
}

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn id(&self) -> ToolId {
        ToolId::new(EXIT_PLAN_MODE_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Submit a completed plan for approval and, once approved, allow the teammate to proceed with implementation.\n\
         \n\
         Teammate workflow:\n\
         - Call this directly after research and any necessary clarification are complete and you have written a concrete implementation plan.\n\
         - Do not use AskUserQuestion to announce readiness, request final approval, or replace plan submission.\n\
         - The `plan` field contains the Markdown presented to the leader.\n\
         - During /ultraplan, pass the current `ledger_revision` and typed `step_coverage`; the system validates and renders coverage, so do not hand-write `[COVERS:...]` markers.\n\
         - When approval is required, the tool sends a plan_approval_request to the team leader and pauses work until a response arrives."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "The plan content to submit for approval."
                },
                "ledger_revision": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Current /ultraplan ledger revision. Required while an /ultraplan policy is active."
                },
                "step_coverage": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "step_id": { "type": "string" },
                            "requirement_ids": {
                                "type": "array",
                                "items": { "type": "string" }
                            }
                        },
                        "required": ["step_id", "requirement_ids"],
                        "additionalProperties": false
                    },
                    "description": "Typed step-to-requirement mapping used to generate coverage. Do not put COVERS markers in plan Markdown."
                }
            },
            "required": ["plan"],
            "additionalProperties": false
        })
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        // Leader path uses the permission dialog to show the plan for
        // approval. Teammate path bypasses permission (auto-allow)
        // and sends approval via TeamManager instead.
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        // Teammate path: require plan mode to be active.
        if let Some(identity) = context.team_identity() {
            if identity.permission_mode.as_deref() != Some("plan") {
                return Ok(ValidationOutcome::invalid(
                    "ExitPlanMode requires the teammate to currently be in plan mode",
                    INVALID_INPUT_CODE,
                ));
            }
        }
        // Both leader and teammate need a valid `plan` field.
        match parse_input(input) {
            Ok(parsed) => match validate_ultraplan_plan(&parsed, context) {
                Some(outcome) => Ok(outcome),
                None => Ok(ValidationOutcome::valid()),
            },
            refused => validation_outcome_from(refused),
        }
    }

    async fn check_permissions(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        if context.team_identity().is_some() {
            // Teammate path: auto-allow — plan approval is sent via
            // TeamManager in `call()`, not the permission dialog.
            return Ok(PermissionDecision::allow(input.clone()));
        }
        // Leader path: show the plan for approval via the permission dialog.
        Ok(PermissionDecision::ask(
            PermissionRequest::new(
                "Plan ready for review",
                "Review the proposed plan and choose how to proceed.",
            )
            .with_options([
                "yes_auto",
                "yes_accept_edits",
                "yes_default",
                "reject_once",
            ]),
            Some(input.clone()),
        ))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed = parse_input(&input)?;
        validate_grill_final_release(&parsed, context).map_err(|reason| {
            ToolError::InvalidInput {
                tool: self.id(),
                reason,
                error_code: Some(INVALID_INPUT_CODE),
            }
        })?;
        validate_standard_final_release(&parsed, context).map_err(|reason| {
            ToolError::InvalidInput {
                tool: self.id(),
                reason,
                error_code: Some(INVALID_INPUT_CODE),
            }
        })?;

        // Leader path: the permission dialog already showed the plan
        // and the user approved. Just return success with the plan
        // and a signal to exit plan mode.
        //
        // The permission broker may inject `clearContext: true` into
        // the input when the user selects a clear-context option.
        // Propagate it to the output so TurnControlPlugin can perform the
        // context reset.
        if context.team_identity().is_none() {
            let clear_context = input
                .get("clearContext")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let permission_mode = input
                .get("permissionMode")
                .and_then(Value::as_str)
                .unwrap_or("default");
            if !matches!(permission_mode, "auto" | "acceptEdits" | "default") {
                return Err(ToolError::InvalidInput {
                    tool: self.id(),
                    reason: format!(
                        "ExitPlanMode received invalid permissionMode `{permission_mode}`; expected auto, acceptEdits, or default"
                    ),
                    error_code: Some(INVALID_INPUT_CODE),
                });
            }
            if clear_context && permission_mode != "auto" {
                return Err(ToolError::InvalidInput {
                    tool: self.id(),
                    reason: "ExitPlanMode clearContext requires permissionMode `auto`".into(),
                    error_code: Some(INVALID_INPUT_CODE),
                });
            }
            return Ok(json!({
                "exitedPlanMode": true,
                "permissionMode": permission_mode,
                "mode": permission_mode,
                "plan": parsed.plan,
                "ledgerRevision": parsed.ledger_revision,
                "stepCoverage": parsed.step_coverage,
                "clearContext": clear_context,
                "message": "Exited plan mode. You may now implement the plan.",
            }));
        }

        // Teammate path: send plan approval request to leader.
        let identity = context.team_identity().unwrap().clone();
        let manager = context
            .team_manager()
            .ok_or_else(|| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!("ExitPlanMode requires a TeamManager"),
            })?
            .clone();
        let request_id = manager
            .request_plan_approval(
                &identity.team_name,
                &identity.agent_name,
                parsed.plan.clone(),
            )
            .await
            .map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(err),
            })?;

        Ok(json!({
            "plan": parsed.plan,
            "ledgerRevision": parsed.ledger_revision,
            "stepCoverage": parsed.step_coverage,
            "awaitingLeaderApproval": true,
            "requestId": request_id,
        }))
    }
}

fn parse_input(input: &Value) -> ToolResult<ExitPlanModeInput> {
    let tool = ToolId::new(EXIT_PLAN_MODE_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "ExitPlanMode input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;
    let plan = object
        .get("plan")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
        .ok_or_else(|| ToolError::InvalidInput {
            tool,
            reason: "ExitPlanMode requires a non-empty string `plan`".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?
        .to_string();
    let ledger_revision = match object.get("ledger_revision") {
        Some(value) => Some(value.as_u64().filter(|value| *value > 0).ok_or_else(|| {
            ToolError::InvalidInput {
                tool: ToolId::new(EXIT_PLAN_MODE_TOOL_NAME),
                reason: "ExitPlanMode ledger_revision must be a positive integer".into(),
                error_code: Some(INVALID_INPUT_CODE),
            }
        })?),
        None => None,
    };
    let step_coverage = object
        .get("step_coverage")
        .cloned()
        .map(serde_json::from_value::<Vec<PlanStepCoverageInput>>)
        .transpose()
        .map_err(|err| ToolError::InvalidInput {
            tool: ToolId::new(EXIT_PLAN_MODE_TOOL_NAME),
            reason: format!("invalid ExitPlanMode step_coverage: {err}"),
            error_code: Some(INVALID_INPUT_CODE),
        })?
        .unwrap_or_default();
    Ok(ExitPlanModeInput {
        plan,
        ledger_revision,
        step_coverage,
    })
}

fn validate_standard_final_release(
    input: &ExitPlanModeInput,
    context: &ToolContext,
) -> Result<(), String> {
    let Some(ultraplan) = context.effective_ultraplan_context() else {
        return Ok(());
    };
    if ultraplan.profile != UltraplanProfile::Standard || ultraplan.mode != PolicyMode::Enforce {
        return Ok(());
    }
    let state = context
        .load_ultraplan_run_state()
        .map_err(|err| err.to_string())?
        .ok_or_else(|| {
            "Standard-profile ExitPlanMode requires readable persisted run state".to_string()
        })?;
    if state.profile != UltraplanProfile::Standard || state.run_id != ultraplan.run_id {
        return Err("ExitPlanMode run state does not match the active /ultraplan run".into());
    }
    if !state.is_active() {
        return Err("ExitPlanMode run state is no longer active".into());
    }
    // What is still checked is plan identity, not planning process: the plan
    // the user approved must be the draft this run persisted, and a plan the
    // user already rejected must not slip through.
    let draft = state
        .last_plan_draft
        .as_deref()
        .ok_or_else(|| "ExitPlanMode has no persisted /ultraplan draft".to_string())?;
    let plan_hash = ultraplan_plan_hash_for_profile(UltraplanProfile::Standard, draft);
    if state.plan_hash.as_deref() != Some(plan_hash.as_str())
        || ultraplan_execution_plan_payload(&state).as_deref() != Some(input.plan.as_str())
    {
        return Err("ExitPlanMode plan does not match the persisted /ultraplan draft".into());
    }
    if state.user_rejected_plan_hash.as_deref() == Some(plan_hash.as_str()) {
        return Err("ExitPlanMode cannot release a plan the user already rejected".into());
    }
    Ok(())
}

fn validate_grill_final_release(
    input: &ExitPlanModeInput,
    context: &ToolContext,
) -> Result<(), String> {
    let plan = input.plan.as_str();
    let Some(ultraplan) = context.effective_ultraplan_context() else {
        return Ok(());
    };
    if ultraplan.profile != UltraplanProfile::Grill {
        return Ok(());
    }
    if ultraplan.mode != PolicyMode::Enforce {
        return Err("Grill-profile ExitPlanMode requires strict runtime enforcement".into());
    }
    let state = context
        .load_ultraplan_run_state()
        .map_err(|err| err.to_string())?
        .ok_or_else(|| {
            "Grill-profile ExitPlanMode requires readable persisted run state".to_string()
        })?;
    if state.profile != UltraplanProfile::Grill || state.run_id != ultraplan.run_id {
        return Err("Grill-profile ExitPlanMode run state does not match the active run".into());
    }
    if !state.is_active() {
        return Err("Grill-profile ExitPlanMode run state is no longer active".into());
    }
    if input.ledger_revision != Some(state.ledger_revision) {
        return Err(format!(
            "Grill-profile ExitPlanMode references stale ledger revision {:?}; current revision is {}",
            input.ledger_revision, state.ledger_revision
        ));
    }
    let analysis = analyze_ultraplan_plan(plan, &state.requirement_ledger, &input.step_coverage);
    if analysis.steps.is_empty() {
        return Err(
            "Grill-profile ExitPlanMode requires at least one stable Pn implementation step".into(),
        );
    }
    if state.interview.turns.is_empty() {
        return Err(
            "Grill-profile ExitPlanMode requires at least one completed interview turn".into(),
        );
    }
    if state.requirement_ledger.is_empty() {
        return Err("Grill-profile ExitPlanMode requires a non-empty requirement ledger".into());
    }
    if !state.grill_understanding_is_sealed() {
        return Err(
            "Grill-profile ExitPlanMode requires the current interview revision to be sealed"
                .into(),
        );
    }

    let draft = state
        .last_plan_draft
        .as_deref()
        .ok_or_else(|| "Grill-profile ExitPlanMode has no persisted draft".to_string())?;
    let plan_hash = ultraplan_plan_hash_for_profile(UltraplanProfile::Grill, draft);
    if state.plan_hash.as_deref() != Some(plan_hash.as_str())
        || ultraplan_execution_plan_payload(&state).as_deref() != Some(plan)
    {
        return Err("Grill-profile ExitPlanMode plan does not match the persisted draft".into());
    }
    let gate = state.final_gate.as_ref().ok_or_else(|| {
        "Grill-profile ExitPlanMode requires a persisted final-gate record".to_string()
    })?;
    if !matches!(
        state.stage(),
        rebon_types::UltraplanStage::FinalGate | rebon_types::UltraplanStage::Completed
    ) || gate.plan_hash != plan_hash
        || gate.ledger_revision != state.ledger_revision
        || !matches!(
            gate.outcome,
            rebon_types::FinalGateOutcome::Pass | rebon_types::FinalGateOutcome::Degraded
        )
    {
        return Err("Grill-profile ExitPlanMode final-gate record is stale".into());
    }
    // A degraded final gate exempts the auto-review PASS requirement (the
    // reviewer never produced one), but never the explicit hash-bound user
    // confirmation below — otherwise a reviewer outage would permanently
    // brick the run because the budget is monotonic.
    if !state.grill_plan_review_satisfied(&plan_hash) {
        return Err(
            "Grill-profile ExitPlanMode requires an auto-review PASS for the exact plan".into(),
        );
    }
    if !state.grill_confirmation_matches(&plan_hash)
        || state.released_plan_hash.as_deref() != Some(plan_hash.as_str())
    {
        return Err("Grill-profile ExitPlanMode requires explicit user confirmation of the exact reviewed plan".into());
    }
    if state.user_rejected_plan_hash.as_deref() == Some(plan_hash.as_str())
        || state.manual_review_failed_hash.as_deref() == Some(plan_hash.as_str())
    {
        return Err("Grill-profile ExitPlanMode cannot release a rejected plan hash".into());
    }

    if !analysis.is_structurally_valid() {
        return Err(format!(
            "Grill-profile ExitPlanMode plan analysis failed at ledger revision {}: missing requirements {:?}; unknown requirements {:?}; unknown steps {:?}; duplicate steps {:?}; incomplete steps {:?}",
            state.ledger_revision,
            analysis.coverage.missing,
            analysis.coverage.unknown_ids,
            analysis.unknown_step_ids,
            analysis.duplicate_step_ids,
            analysis.incomplete_step_ids,
        ));
    }
    if context.team_identity().is_none() && !context.exit_plan_mode_approved() {
        return Err(
            "Grill-profile ExitPlanMode requires a separate explicit final approval from the interactive permission broker"
                .into(),
        );
    }
    Ok(())
}

fn validate_ultraplan_plan(
    _input: &ExitPlanModeInput,
    context: &ToolContext,
) -> Option<ValidationOutcome> {
    context.ultraplan_context()?;
    // A `/ultraplan` submission is refused here only when the run it belongs
    // to cannot be read at all. Ledger-revision staleness and requirement
    // coverage are no longer submission gates: the plan goes to the user, who
    // is the only hard gate in the planning workflow.
    match context.load_ultraplan_run_state() {
        Ok(Some(_)) => None,
        Ok(None) => Some(ValidationOutcome::invalid(
            "ExitPlanMode requires readable persisted /ultraplan run state",
            INVALID_INPUT_CODE,
        )),
        Err(err) => Some(ValidationOutcome::invalid(
            format!("ExitPlanMode could not load /ultraplan run state: {err}"),
            INVALID_INPUT_CODE,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::{TeamIdentityContext, TeamManager, TeammateSpawnResult, TeammateSpawnSpec};
    use rebon_tools_core::PermissionBehavior;
    use rebon_types::{
        ExecutionPolicy, PolicyMode, RequirementLedgerEntry, RequirementSource, UltraplanContext,
        UltraplanManifestItem, UltraplanManifestSnapshot, UltraplanRunState,
    };
    use std::sync::{Arc, Mutex};

    struct ScriptedTeamManager {
        plans: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait]
    impl TeamManager for ScriptedTeamManager {
        async fn spawn_teammate(
            &self,
            _spec: TeammateSpawnSpec,
        ) -> Result<TeammateSpawnResult, String> {
            unreachable!()
        }

        async fn send_message(
            &self,
            _team_name: &str,
            _recipient: &str,
            _message: String,
        ) -> Result<(), rebon_tools_core::ToolErrorPresentation> {
            Ok(())
        }

        async fn request_shutdown(
            &self,
            _team_name: &str,
            _recipient: &str,
            _reason: Option<String>,
        ) -> Result<String, String> {
            Ok("req".into())
        }

        async fn request_plan_approval(
            &self,
            team_name: &str,
            agent_name: &str,
            plan_content: String,
        ) -> Result<String, String> {
            self.plans
                .lock()
                .unwrap()
                .push((team_name.into(), agent_name.into(), plan_content));
            Ok("plan-1".into())
        }

        async fn delete_team(&self, _team_name: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn manifest_context() -> ToolContext {
        let manifest = UltraplanManifestSnapshot {
            source_path: "tasks.md".into(),
            canonical_path: "/repo/tasks.md".into(),
            display_path: "tasks.md".into(),
            content_sha256: "abc".into(),
            items: vec![
                UltraplanManifestItem {
                    id: "T1".into(),
                    title: "one".into(),
                    line: 1,
                    required: true,
                },
                UltraplanManifestItem {
                    id: "T2".into(),
                    title: "two".into(),
                    line: 2,
                    required: true,
                },
            ],
        };
        let state = Arc::new(Mutex::new(UltraplanRunState::new(
            "run".into(),
            "session".into(),
            "task".into(),
            Some(manifest.clone()),
            1,
        )));
        let head = state.lock().unwrap().head();
        ToolContext::new()
            .with_execution_policy(ExecutionPolicy::ultraplan(
                UltraplanContext::planning_turn("run", "plan", PolicyMode::Enforce)
                    .with_manifest(manifest)
                    .with_run_head(&head),
            ))
            .with_ultraplan_run_handle(state)
    }

    fn grill_context_with_final_gate(
        plan: &str,
        confirmed: bool,
        include_final_gate: bool,
    ) -> ToolContext {
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1)
                .with_profile(UltraplanProfile::Grill);
        state.requirement_ledger.push(RequirementLedgerEntry {
            id: "R1".into(),
            title: "Ship safely".into(),
            source: RequirementSource::Question,
            round_added: 1,
        });
        state.record_grill_interview_turn(
            "Which rollout?".into(),
            Some("Gradual".into()),
            "Gradual".into(),
        );
        state.seal_grill_understanding().unwrap();
        state.set_plan_artifacts(
            plan.into(),
            rebon_types::PlanCoverageResult {
                covered: vec!["R1".into()],
                missing: Vec::new(),
                unknown_ids: Vec::new(),
            },
            Vec::new(),
        );
        let plan_hash = ultraplan_plan_hash_for_profile(UltraplanProfile::Grill, plan);
        state.auto_review_passed_hash = Some(plan_hash.clone());
        if confirmed {
            assert!(state.confirm_grill_understanding(state.interview.revision, &plan_hash));
        }
        if include_final_gate {
            state.record_final_gate(rebon_types::FinalGateOutcome::Pass, plan_hash, Vec::new());
        }
        let head = state.head();
        ToolContext::new()
            .with_execution_policy(ExecutionPolicy::ultraplan(
                UltraplanContext::planning_turn("run", "synthesizing", PolicyMode::Enforce)
                    .with_profile(UltraplanProfile::Grill)
                    .with_run_head(&head),
            ))
            .with_ultraplan_run_handle(Arc::new(Mutex::new(state)))
    }

    fn grill_context_without_syncer(plan: &str, confirmed: bool) -> ToolContext {
        grill_context_with_final_gate(plan, confirmed, true)
    }

    fn grill_context(plan: &str, confirmed: bool) -> ToolContext {
        grill_context_without_syncer(plan, confirmed).with_ultraplan_run_syncer(Arc::new(|_| true))
    }

    fn standard_context_with_final_gate(plan: &str, include_final_gate: bool) -> ToolContext {
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1);
        state.set_plan_artifacts(
            plan.into(),
            rebon_types::PlanCoverageResult {
                covered: Vec::new(),
                missing: Vec::new(),
                unknown_ids: Vec::new(),
            },
            Vec::new(),
        );
        if include_final_gate {
            state.record_final_gate(
                rebon_types::FinalGateOutcome::Pass,
                state.plan_hash.clone().unwrap(),
                Vec::new(),
            );
        }
        let head = state.head();
        ToolContext::new()
            .with_execution_policy(ExecutionPolicy::ultraplan(
                UltraplanContext::planning_turn("run", "plan", PolicyMode::Enforce)
                    .with_run_head(&head),
            ))
            .with_ultraplan_run_handle(Arc::new(Mutex::new(state)))
            .with_exit_plan_mode_approval()
    }

    fn standard_context(plan: &str) -> ToolContext {
        standard_context_with_final_gate(plan, true)
    }

    fn implementation_plan() -> &'static str {
        "P1. Ship gradually\n- files: src/lib.rs\n- change: ship safely\n- verify: cargo test"
    }

    fn typed_input(context: &ToolContext, plan: &str, requirement_ids: &[&str]) -> Value {
        let state = context
            .load_ultraplan_run_state()
            .unwrap()
            .expect("run state");
        json!({
            "plan": plan,
            "ledger_revision": state.ledger_revision,
            "step_coverage": [{
                "step_id": "P1",
                "requirement_ids": requirement_ids,
            }],
        })
    }

    #[tokio::test]
    async fn standard_call_revalidates_readable_current_run_state() {
        let plan = implementation_plan();
        let context = standard_context(plan);

        let output = ExitPlanModeTool
            .call(typed_input(&context, plan, &[]), &context)
            .await
            .unwrap();

        assert_eq!(output["exitedPlanMode"], json!(true));
    }

    #[tokio::test]
    async fn standard_call_accepts_an_approved_plan_without_a_final_gate_record() {
        // The final gate is bookkeeping, not a precondition: the user's
        // approval of this exact draft is what authorizes the exit.
        let plan = implementation_plan();
        let context = standard_context_with_final_gate(plan, false);

        let output = ExitPlanModeTool
            .call(typed_input(&context, plan, &[]), &context)
            .await
            .unwrap();

        assert_eq!(output["exitedPlanMode"], json!(true));
    }

    #[tokio::test]
    async fn standard_call_accepts_the_canonical_degraded_payload_after_approval() {
        let plan = implementation_plan();
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1);
        state.set_plan_artifacts(
            plan.into(),
            rebon_types::PlanCoverageResult {
                covered: Vec::new(),
                missing: Vec::new(),
                unknown_ids: Vec::new(),
            },
            Vec::new(),
        );
        state.record_final_gate(
            rebon_types::FinalGateOutcome::Degraded,
            state.plan_hash.clone().unwrap(),
            vec![rebon_types::UltraplanDiagnostic {
                class: rebon_types::UltraplanDiagnosticClass::ReviewerUnavailable,
                message: "reviewer unavailable".into(),
                ledger_revision: state.ledger_revision,
                stage: rebon_types::UltraplanStage::FinalGate,
                plan_hash: state.plan_hash.clone(),
                details: json!({"attempts": 2}),
            }],
        );
        let payload = ultraplan_execution_plan_payload(&state).unwrap();
        state.phase = rebon_types::RunPhase::Executing;
        assert_eq!(state.stage(), rebon_types::UltraplanStage::Completed);
        let head = state.head();
        let context = ToolContext::new()
            .with_execution_policy(ExecutionPolicy::ultraplan(
                UltraplanContext::planning_turn("run", "plan", PolicyMode::Enforce)
                    .with_run_head(&head),
            ))
            .with_ultraplan_run_handle(Arc::new(Mutex::new(state)))
            .with_exit_plan_mode_approval();

        let output = ExitPlanModeTool
            .call(typed_input(&context, &payload, &[]), &context)
            .await
            .unwrap();

        assert_eq!(output["plan"], json!(payload));
    }

    #[tokio::test]
    async fn standard_call_fails_closed_when_run_state_disappears() {
        let plan = implementation_plan();
        let context = ToolContext::new()
            .with_execution_policy(ExecutionPolicy::ultraplan(UltraplanContext::planning_turn(
                "run",
                "plan",
                PolicyMode::Enforce,
            )))
            .with_exit_plan_mode_approval();
        let input = json!({
            "plan": plan,
            "ledger_revision": 1,
            "step_coverage": [],
        });

        let error = ExitPlanModeTool.call(input, &context).await.unwrap_err();

        assert!(matches!(
            error,
            ToolError::InvalidInput { reason, .. }
                if reason.contains("readable persisted run state")
        ));
    }

    #[tokio::test]
    async fn grill_call_rejects_direct_release_without_exact_confirmation() {
        let plan = implementation_plan();
        let context = grill_context(plan, false);
        let err = ExitPlanModeTool
            .call(typed_input(&context, plan, &["R1"]), &context)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ToolError::InvalidInput { reason, .. }
                if reason.contains("explicit user confirmation")
        ));
    }

    #[tokio::test]
    async fn grill_call_requires_separate_final_permission_approval() {
        let plan = implementation_plan();
        let context = grill_context(plan, true);
        let err = ExitPlanModeTool
            .call(typed_input(&context, plan, &["R1"]), &context)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ToolError::InvalidInput { reason, .. }
                if reason.contains("separate explicit final approval")
        ));

        let approved_context = context.with_exit_plan_mode_approval();
        let out = ExitPlanModeTool
            .call(
                typed_input(&approved_context, plan, &["R1"]),
                &approved_context,
            )
            .await
            .unwrap();
        assert_eq!(out["exitedPlanMode"], json!(true));
        assert_eq!(out["plan"], json!(plan));
    }

    #[tokio::test]
    async fn grill_call_rejects_missing_final_gate() {
        let plan = implementation_plan();
        let context =
            grill_context_with_final_gate(plan, true, false).with_exit_plan_mode_approval();

        let err = ExitPlanModeTool
            .call(typed_input(&context, plan, &["R1"]), &context)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            ToolError::InvalidInput { reason, .. }
                if reason.contains("persisted final-gate record")
        ));
    }

    #[tokio::test]
    async fn auto_approve_broker_cannot_release_confirmed_grill_plan() {
        use rebon_tool::{AutoApprovePermissionBroker, PermissionBroker};

        let plan = implementation_plan();
        let context = grill_context(plan, true);
        let tool = ExitPlanModeTool;
        let input = typed_input(&context, plan, &["R1"]);
        let decision = tool.check_permissions(&input, &context).await.unwrap();

        let err = AutoApprovePermissionBroker
            .resolve(&tool, input, &context, decision)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            ToolError::InvalidInput { reason, .. }
                if reason.contains("separate explicit final approval")
        ));
    }

    #[tokio::test]
    async fn grill_call_rejects_observe_policy_even_with_final_approval() {
        let plan = implementation_plan();
        let context = grill_context(plan, true)
            .with_execution_policy(ExecutionPolicy::ultraplan(
                UltraplanContext::planning_turn("run", "synthesizing", PolicyMode::Observe)
                    .with_profile(UltraplanProfile::Grill),
            ))
            .with_exit_plan_mode_approval();

        let err = ExitPlanModeTool
            .call(typed_input(&context, plan, &["R1"]), &context)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            ToolError::InvalidInput { reason, .. }
                if reason.contains("strict runtime enforcement")
        ));
    }

    #[tokio::test]
    async fn grill_call_accepts_legacy_in_memory_context_without_sync_hook() {
        let plan = implementation_plan();
        let context = grill_context_without_syncer(plan, true).with_exit_plan_mode_approval();

        let output = ExitPlanModeTool
            .call(typed_input(&context, plan, &["R1"]), &context)
            .await
            .unwrap();

        assert_eq!(output["exitedPlanMode"], true);
    }

    #[tokio::test]
    async fn legacy_failed_sync_reports_missing_state() {
        let plan = implementation_plan();
        let base_context = grill_context(plan, true);
        let input = typed_input(&base_context, plan, &["R1"]);
        let context = base_context
            .with_ultraplan_run_syncer(Arc::new(|_| false))
            .with_exit_plan_mode_approval();

        let err = ExitPlanModeTool.call(input, &context).await.unwrap_err();

        assert!(matches!(
            err,
            ToolError::InvalidInput { reason, .. }
                if reason.contains("readable persisted run state")
        ));
    }

    #[tokio::test]
    async fn validate_input_no_longer_gates_manifest_coverage() {
        // Requirement coverage is a planning judgement, not a submission
        // gate: an incomplete or unknown mapping still reaches the user.
        let context = manifest_context();
        let tool = ExitPlanModeTool;

        for coverage in [vec!["T1"], vec!["T1", "T2", "BAD"]] {
            let outcome = tool
                .validate_input(
                    &typed_input(&context, implementation_plan(), &coverage),
                    &context,
                )
                .await
                .unwrap();
            assert!(outcome.result, "coverage {coverage:?} should be accepted");
        }
    }

    #[tokio::test]
    async fn validate_input_accepts_complete_manifest_coverage() {
        let context = manifest_context();
        let tool = ExitPlanModeTool;
        let outcome = tool
            .validate_input(
                &typed_input(&context, implementation_plan(), &["T1", "T2"]),
                &context,
            )
            .await
            .unwrap();
        assert!(outcome.result);
    }

    #[tokio::test]
    async fn validate_input_preserves_legacy_without_manifest() {
        let context = ToolContext::new();
        let tool = ExitPlanModeTool;
        let outcome = tool
            .validate_input(&json!({"plan": "plain legacy plan"}), &context)
            .await
            .unwrap();
        assert!(outcome.result);
    }

    #[tokio::test]
    async fn leader_check_permissions_omits_clear_context_option() {
        let context = ToolContext::new(); // no team identity → leader path
        let tool = ExitPlanModeTool;
        let input = json!({"plan": "1. Do stuff"});
        let decision = tool.check_permissions(&input, &context).await.unwrap();
        assert_eq!(decision.behavior, PermissionBehavior::Ask);
        let request = decision.request.unwrap();
        assert_eq!(
            request.options,
            vec!["yes_auto", "yes_accept_edits", "yes_default", "reject_once",]
        );
    }

    #[tokio::test]
    async fn leader_call_returns_exited_plan_mode() {
        let context = ToolContext::new();
        let tool = ExitPlanModeTool;
        let out = tool
            .call(json!({"plan": "my plan"}), &context)
            .await
            .unwrap();
        assert_eq!(out["exitedPlanMode"], json!(true));
        assert_eq!(out["permissionMode"], json!("default"));
        assert_eq!(out["mode"], json!("default"));
        assert_eq!(out["plan"], json!("my plan"));
        assert_eq!(out["clearContext"], json!(false));
    }

    #[tokio::test]
    async fn leader_call_propagates_clear_context() {
        let context = ToolContext::new();
        let tool = ExitPlanModeTool;
        let out = tool
            .call(
                json!({
                    "plan": "my plan",
                    "permissionMode": "auto",
                    "clearContext": true
                }),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(out["exitedPlanMode"], json!(true));
        assert_eq!(out["permissionMode"], json!("auto"));
        assert_eq!(out["mode"], json!("auto"));
        assert_eq!(out["clearContext"], json!(true));
        assert_eq!(out["plan"], json!("my plan"));
    }

    #[tokio::test]
    async fn leader_call_reports_each_supported_permission_mode() {
        let context = ToolContext::new();
        let tool = ExitPlanModeTool;
        for mode in ["auto", "acceptEdits", "default"] {
            let out = tool
                .call(json!({"plan": "my plan", "permissionMode": mode}), &context)
                .await
                .unwrap();
            assert_eq!(out["permissionMode"], json!(mode));
            assert_eq!(out["mode"], json!(mode));
            assert_eq!(out["clearContext"], json!(false));
        }
    }

    #[tokio::test]
    async fn leader_call_rejects_invalid_permission_mode() {
        let context = ToolContext::new();
        let tool = ExitPlanModeTool;
        let err = tool
            .call(
                json!({"plan": "my plan", "permissionMode": "bypassPermissions"}),
                &context,
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput { reason, .. } => {
                assert!(reason.contains("invalid permissionMode"));
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn leader_call_rejects_clear_context_without_auto_mode() {
        let context = ToolContext::new();
        let tool = ExitPlanModeTool;
        let err = tool
            .call(
                json!({
                    "plan": "my plan",
                    "permissionMode": "default",
                    "clearContext": true
                }),
                &context,
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput { reason, .. } => {
                assert!(reason.contains("clearContext requires permissionMode `auto`"));
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exit_plan_mode_requests_plan_approval_for_teammate() {
        let manager = Arc::new(ScriptedTeamManager {
            plans: Mutex::new(Vec::new()),
        });
        let context = ToolContext::new()
            .with_team_manager(manager.clone() as Arc<dyn TeamManager>)
            .with_team_identity(TeamIdentityContext {
                agent_id: "alice@alpha".into(),
                agent_name: "alice".into(),
                team_name: "alpha".into(),
                permission_mode: Some("plan".into()),
            });
        let tool = ExitPlanModeTool;
        let out = tool
            .call(json!({"plan":"1. inspect\n2. patch\n3. test"}), &context)
            .await
            .unwrap();
        assert_eq!(out["awaitingLeaderApproval"], json!(true));
        assert_eq!(out["requestId"], json!("plan-1"));
        let plans = manager.plans.lock().unwrap();
        assert_eq!(plans[0].0, "alpha");
        assert_eq!(plans[0].1, "alice");
    }
}
