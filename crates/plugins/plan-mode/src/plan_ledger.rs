use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    PermissionDecision, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use rebon_types::{
    is_valid_manifest_id, ledger_to_manifest_snapshot, RequirementLedgerEntry, RequirementSource,
    RunRevisionMutation, RunTransition, RunTransitionKind, UltraplanProfile,
};
use std::sync::atomic::{AtomicU64, Ordering};

pub use rebon_tool::plan_mode::PLAN_LEDGER_TOOL_NAME;

const INVALID_INPUT_CODE: i64 = 400;
static NEXT_FORK_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Default)]
pub struct PlanLedgerTool;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum PlanLedgerInput {
    SetRequirements {
        items: Vec<PlanLedgerItem>,
        expected_revision: u64,
    },
    Add {
        items: Vec<PlanLedgerItem>,
        expected_revision: u64,
    },
    Replace {
        items: Vec<PlanLedgerItem>,
        expected_revision: u64,
    },
    List,
    SealUnderstanding {
        expected_revision: u64,
    },
    Rollback {
        target_revision: u64,
        expected_revision: u64,
    },
    Reset {
        expected_revision: u64,
    },
    Fork {
        #[serde(default)]
        target_revision: Option<u64>,
        expected_revision: u64,
    },
    Resume {
        run_id: String,
        expected_revision: u64,
    },
}

#[derive(Debug, Clone, Deserialize)]
struct PlanLedgerItem {
    id: String,
    title: String,
}

#[async_trait]
impl Tool for PlanLedgerTool {
    fn id(&self) -> ToolId {
        ToolId::new(PLAN_LEDGER_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Maintain the versioned /ultraplan requirement ledger. Every mutation is bound to an expected ledger revision. Use set_requirements for initial criteria, add for additive feedback, replace for an atomic replacement, rollback/reset for recovery, fork/resume for explicit run transitions, list to inspect the current head, and seal_understanding after a Grill interview."
    }

    fn model_description(&self) -> &str {
        "Manage the versioned /ultraplan requirement ledger. Mutations require expected_revision; use list to inspect the current head."
    }

    // Providers such as the Codex Responses backend reject function
    // schemas that carry `oneOf`/`anyOf`/`allOf`/`enum`/`const`/`not`
    // at the top level, so the per-operation variants are flattened
    // into a plain object; `validate_input` enforces operation-specific fields.
    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["set_requirements", "add", "replace", "list", "seal_understanding", "rollback", "reset", "fork", "resume"]
                },
                "items": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "title": { "type": "string" }
                        },
                        "required": ["id", "title"],
                        "additionalProperties": false
                    }
                },
                "expected_revision": {
                    "type": "integer",
                    "minimum": 1
                },
                "target_revision": {
                    "type": "integer",
                    "minimum": 1
                },
                "run_id": {
                    "type": "string",
                    "minLength": 1
                }
            },
            "required": ["operation"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        let parsed = match parse_input(input) {
            Ok(parsed) => parsed,
            Err(ToolError::InvalidInput { reason, .. }) => {
                return Ok(ValidationOutcome::invalid(reason, INVALID_INPUT_CODE));
            }
            Err(err) => return Err(err),
        };
        match parsed {
            PlanLedgerInput::SetRequirements { items, .. }
            | PlanLedgerInput::Add { items, .. }
            | PlanLedgerInput::Replace { items, .. } => validate_items(&items),
            PlanLedgerInput::Resume { ref run_id, .. } if run_id.trim().is_empty() => {
                Ok(ValidationOutcome::invalid(
                    "resume requires a non-empty run_id",
                    INVALID_INPUT_CODE,
                ))
            }
            PlanLedgerInput::List
            | PlanLedgerInput::SealUnderstanding { .. }
            | PlanLedgerInput::Rollback { .. }
            | PlanLedgerInput::Reset { .. }
            | PlanLedgerInput::Fork { .. }
            | PlanLedgerInput::Resume { .. } => Ok(ValidationOutcome::valid()),
        }
    }

    async fn check_permissions(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        if context.ultraplan_context().is_none() {
            return Ok(PermissionDecision::deny(
                "PlanLedger is only available while an /ultraplan execution policy is active",
            ));
        }
        Ok(PermissionDecision::allow(input.clone()))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed = parse_input(&input)?;
        if matches!(parsed, PlanLedgerInput::List) {
            let state = load_state(context, self.id())?;
            return Ok(ledger_output(&state, None, None, None));
        }
        if let PlanLedgerInput::Resume {
            run_id,
            expected_revision,
        } = &parsed
        {
            let mut source = load_state(context, self.id())?;
            source
                .verify_expected_revision(*expected_revision)
                .map_err(|err| invalid_input(self.id(), err.to_string()))?;
            if source.run_id == *run_id {
                return Err(invalid_input(
                    self.id(),
                    format!("run `{run_id}` is already active"),
                ));
            }
            let mut target = context
                .load_ultraplan_run_by_id(run_id)
                .map_err(|err| repository_tool_error(self.id(), err))?
                .ok_or_else(|| invalid_input(self.id(), format!("run `{run_id}` was not found")))?;
            if target.profile != source.profile {
                return Err(invalid_input(
                    self.id(),
                    format!(
                        "cannot resume {} profile run `{run_id}` from an active {} profile run",
                        target.profile.as_str(),
                        source.profile.as_str()
                    ),
                ));
            }
            let session_id = context
                .session_id()
                .unwrap_or(target.identity.attached_session_id.as_str())
                .to_string();
            let expected_target_state_revision = target.state_revision;
            target.attach_session(session_id);
            context
                .compare_and_swap_ultraplan_run(expected_target_state_revision, &target)
                .map_err(|err| repository_tool_error(self.id(), err))?;
            let expected_source_state_revision = source.state_revision;
            source.queue_transition(RunTransitionKind::Resume, target.run_id.clone());
            context
                .compare_and_swap_ultraplan_run(expected_source_state_revision, &source)
                .map_err(|err| repository_tool_error(self.id(), err))?;
            context
                .switch_current_ultraplan_run(&target.run_id)
                .map_err(|err| repository_tool_error(self.id(), err))?;
            let transition = RunTransition {
                kind: RunTransitionKind::Resume,
                target_run_id: target.run_id.clone(),
            };
            return Ok(ledger_output(&target, None, Some(transition), None));
        }
        if let PlanLedgerInput::Fork {
            target_revision,
            expected_revision,
        } = &parsed
        {
            let mut state = load_state(context, self.id())?;
            state
                .verify_expected_revision(*expected_revision)
                .map_err(|err| invalid_input(self.id(), err.to_string()))?;
            let target_revision = target_revision.unwrap_or(state.ledger_revision);
            let session_id = context
                .session_id()
                .unwrap_or(state.identity.attached_session_id.as_str())
                .to_string();
            let fork_run_id = new_fork_run_id(&state.run_id);
            let fork = state
                .fork_from_revision(target_revision, fork_run_id.clone(), session_id)
                .map_err(|err| invalid_input(self.id(), err.to_string()))?;
            context
                .create_ultraplan_run(&fork)
                .map_err(|err| repository_tool_error(self.id(), err))?;
            let expected_state_revision = state.state_revision;
            state.queue_transition(RunTransitionKind::Fork, fork_run_id.clone());
            context
                .compare_and_swap_ultraplan_run(expected_state_revision, &state)
                .map_err(|err| repository_tool_error(self.id(), err))?;
            context
                .switch_current_ultraplan_run(&fork_run_id)
                .map_err(|err| repository_tool_error(self.id(), err))?;
            return Ok(ledger_output(
                &state,
                None,
                Some(RunTransition {
                    kind: RunTransitionKind::Fork,
                    target_run_id: fork_run_id,
                }),
                None,
            ));
        }

        let requested_revision = expected_revision(&parsed).expect("mutating operation revision");
        let rebasable = matches!(
            parsed,
            PlanLedgerInput::Add { .. } | PlanLedgerInput::SealUnderstanding { .. }
        );
        let mut reloaded_from = None;
        for attempt in 0..=1 {
            let mut state = load_state(context, self.id())?;
            let current_revision = state.ledger_revision;
            let current_state_revision = state.state_revision;
            if requested_revision != current_revision {
                if !rebasable {
                    return Err(invalid_input(
                        self.id(),
                        format!(
                            "stale ledger revision: expected {requested_revision}, current revision is {current_revision}; reload and retry"
                        ),
                    ));
                }
                reloaded_from.get_or_insert(requested_revision);
            }
            let seal = apply_mutation(self.id(), &parsed, &mut state)?;
            match context.compare_and_swap_ultraplan_run(current_state_revision, &state) {
                Ok(()) => return Ok(ledger_output(&state, seal, None, reloaded_from)),
                Err(rebon_tool::UltraplanRepositoryError::StaleRevision { .. }) if attempt == 0 => {
                    reloaded_from.get_or_insert(current_revision);
                }
                Err(err) => return Err(repository_tool_error(self.id(), err)),
            }
        }
        Err(ToolError::Execution {
            tool: self.id(),
            source: anyhow::anyhow!("ultraplan mutation retry budget exhausted"),
        })
    }
}

fn apply_mutation(
    tool: ToolId,
    input: &PlanLedgerInput,
    state: &mut rebon_types::UltraplanRunState,
) -> ToolResult<Option<rebon_types::UltraplanUnderstandingSeal>> {
    let round = state.round.max(1);
    match input {
        PlanLedgerInput::SetRequirements { items, .. } => {
            if state
                .manifest
                .as_ref()
                .is_some_and(|manifest| manifest.source_path != "ledger")
                || state
                    .requirement_ledger
                    .iter()
                    .any(|entry| entry.source == RequirementSource::Manifest)
            {
                return Err(invalid_input(
                    tool,
                    "set_requirements cannot overwrite manifest-sourced requirements",
                ));
            }
            if !state.requirement_ledger.is_empty() {
                return Err(invalid_input(
                    tool,
                    "set_requirements can only be used before the requirement ledger is initialized",
                ));
            }
            state.requirement_ledger = ledger_entries(items, RequirementSource::Question, round);
            state.manifest = ledger_to_manifest_snapshot(&state.requirement_ledger);
            state.mark_requirements_changed(RunRevisionMutation::RequirementsSet);
            Ok(None)
        }
        PlanLedgerInput::Add { items, .. } => {
            for item in items {
                if state
                    .requirement_ledger
                    .iter()
                    .any(|entry| entry.id == item.id)
                {
                    return Err(invalid_input(
                        tool,
                        format!("requirement id `{}` already exists", item.id),
                    ));
                }
            }
            state.requirement_ledger.extend(ledger_entries(
                items,
                RequirementSource::UserFeedback,
                round,
            ));
            if state
                .manifest
                .as_ref()
                .is_none_or(|manifest| manifest.source_path == "ledger")
            {
                state.manifest = ledger_to_manifest_snapshot(&state.requirement_ledger);
            }
            state.mark_requirements_changed(RunRevisionMutation::RequirementsAdded);
            Ok(None)
        }
        PlanLedgerInput::Replace { items, .. } => {
            state.replace_requirements(ledger_entries(
                items,
                RequirementSource::UserFeedback,
                round,
            ));
            Ok(None)
        }
        PlanLedgerInput::SealUnderstanding { .. } => {
            if state.profile != UltraplanProfile::Grill {
                return Err(invalid_input(
                    tool,
                    "seal_understanding is only available for Grill-profile ultraplan runs",
                ));
            }
            state
                .seal_grill_understanding()
                .ok_or_else(|| {
                    invalid_input(
                        tool,
                        "seal_understanding requires a non-empty requirement ledger",
                    )
                })
                .map(Some)
        }
        PlanLedgerInput::Rollback {
            target_revision, ..
        } => {
            state
                .rollback_to_revision(*target_revision)
                .map_err(|err| invalid_input(tool, err.to_string()))?;
            Ok(None)
        }
        PlanLedgerInput::Reset { .. } => {
            state
                .reset_to_baseline()
                .map_err(|err| invalid_input(tool, err.to_string()))?;
            Ok(None)
        }
        PlanLedgerInput::List | PlanLedgerInput::Fork { .. } | PlanLedgerInput::Resume { .. } => {
            unreachable!("handled before mutation")
        }
    }
}

fn expected_revision(input: &PlanLedgerInput) -> Option<u64> {
    match input {
        PlanLedgerInput::SetRequirements {
            expected_revision, ..
        }
        | PlanLedgerInput::Add {
            expected_revision, ..
        }
        | PlanLedgerInput::Replace {
            expected_revision, ..
        }
        | PlanLedgerInput::SealUnderstanding { expected_revision }
        | PlanLedgerInput::Rollback {
            expected_revision, ..
        }
        | PlanLedgerInput::Reset { expected_revision }
        | PlanLedgerInput::Fork {
            expected_revision, ..
        }
        | PlanLedgerInput::Resume {
            expected_revision, ..
        } => Some(*expected_revision),
        PlanLedgerInput::List => None,
    }
}

fn ledger_entries(
    items: &[PlanLedgerItem],
    source: RequirementSource,
    round: u32,
) -> Vec<RequirementLedgerEntry> {
    items
        .iter()
        .map(|item| RequirementLedgerEntry {
            id: item.id.clone(),
            title: item.title.clone(),
            source,
            round_added: round,
        })
        .collect()
}

fn load_state(context: &ToolContext, tool: ToolId) -> ToolResult<rebon_types::UltraplanRunState> {
    context
        .load_ultraplan_run_state()
        .map_err(|err| repository_tool_error(tool.clone(), err))?
        .ok_or_else(|| ToolError::Execution {
            tool,
            source: anyhow::anyhow!("PlanLedger requires an active /ultraplan run state"),
        })
}

fn ledger_output(
    state: &rebon_types::UltraplanRunState,
    seal: Option<rebon_types::UltraplanUnderstandingSeal>,
    transition: Option<RunTransition>,
    reloaded_from_revision: Option<u64>,
) -> Value {
    json!({
        "runId": state.run_id,
        "ledgerRevision": state.ledger_revision,
        "requirementsHash": state.requirements_hash,
        "sealHash": state.seal_hash,
        "permissionHash": state.permission_hash,
        "planHash": state.plan_hash,
        "requirements": state.requirement_ledger,
        "manifest": state.manifest,
        "understandingSeal": seal.or_else(|| state.interview.seal.clone()),
        "availableRevisions": state.revision_history.iter().map(|item| item.revision).collect::<Vec<_>>(),
        "reloadedFromRevision": reloaded_from_revision,
        "pendingTransition": transition.or_else(|| state.pending_transition.clone()),
    })
}

fn invalid_input(tool: ToolId, reason: impl Into<String>) -> ToolError {
    ToolError::InvalidInput {
        tool,
        reason: reason.into(),
        error_code: Some(INVALID_INPUT_CODE),
    }
}

fn repository_tool_error(tool: ToolId, error: rebon_tool::UltraplanRepositoryError) -> ToolError {
    match error {
        rebon_tool::UltraplanRepositoryError::StaleRevision { expected, actual } => invalid_input(
            tool,
            format!("stale ledger revision: expected {expected}, current revision is {actual}"),
        ),
        other => ToolError::Execution {
            tool,
            source: anyhow::anyhow!(other.to_string()),
        },
    }
}

fn new_fork_run_id(source_run_id: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = NEXT_FORK_ID.fetch_add(1, Ordering::Relaxed);
    format!("{source_run_id}-fork-{timestamp}-{sequence}")
}

fn parse_input(input: &Value) -> ToolResult<PlanLedgerInput> {
    serde_json::from_value(input.clone()).map_err(|err| ToolError::InvalidInput {
        tool: ToolId::new(PLAN_LEDGER_TOOL_NAME),
        reason: format!("invalid PlanLedger input: {err}"),
        error_code: Some(INVALID_INPUT_CODE),
    })
}

fn validate_items(items: &[PlanLedgerItem]) -> ToolResult<ValidationOutcome> {
    let mut seen = std::collections::HashSet::new();
    for item in items {
        let id = item.id.trim();
        let title = item.title.trim();
        if !is_valid_manifest_id(id) {
            return Ok(ValidationOutcome::invalid(
                format!(
                    "invalid requirement id `{}`; use ASCII letters, digits, '_', '-', or '.'",
                    item.id
                ),
                INVALID_INPUT_CODE,
            ));
        }
        if title.is_empty() {
            return Ok(ValidationOutcome::invalid(
                format!("requirement `{}` must have a non-empty title", item.id),
                INVALID_INPUT_CODE,
            ));
        }
        if !seen.insert(id.to_string()) {
            return Ok(ValidationOutcome::invalid(
                format!("duplicate requirement id `{id}`"),
                INVALID_INPUT_CODE,
            ));
        }
    }
    Ok(ValidationOutcome::valid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::{DenyAskPermissionBroker, PermissionBroker};
    use rebon_types::{
        ExecutionPolicy, PolicyMode, UltraplanContext, UltraplanManifestItem,
        UltraplanManifestSnapshot, UltraplanProfile, UltraplanRunState,
    };
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    fn context_with_state() -> (ToolContext, Arc<Mutex<UltraplanRunState>>) {
        let state = Arc::new(Mutex::new(UltraplanRunState::new(
            "run".into(),
            "session".into(),
            "task".into(),
            None,
            1,
        )));
        let context = ToolContext::new()
            .with_execution_policy(ExecutionPolicy::ultraplan(UltraplanContext::planning_turn(
                "run",
                "plan",
                PolicyMode::Enforce,
            )))
            .with_ultraplan_run_handle(state.clone());
        (context, state)
    }

    fn grill_context_with_state() -> (ToolContext, Arc<Mutex<UltraplanRunState>>) {
        let state = Arc::new(Mutex::new(
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1)
                .with_profile(UltraplanProfile::Grill),
        ));
        let policy = UltraplanContext::planning_turn("run", "plan", PolicyMode::Enforce)
            .with_profile(UltraplanProfile::Grill);
        let context = ToolContext::new()
            .with_execution_policy(ExecutionPolicy::ultraplan(policy))
            .with_ultraplan_run_handle(state.clone());
        (context, state)
    }

    #[derive(Clone)]
    struct MultiRunRepository {
        runs: Arc<Mutex<HashMap<String, UltraplanRunState>>>,
        current_run_id: Arc<Mutex<String>>,
    }

    impl MultiRunRepository {
        fn new(states: Vec<UltraplanRunState>, current_run_id: &str) -> Self {
            Self {
                runs: Arc::new(Mutex::new(
                    states
                        .into_iter()
                        .map(|state| (state.run_id.clone(), state))
                        .collect(),
                )),
                current_run_id: Arc::new(Mutex::new(current_run_id.to_string())),
            }
        }

        fn state(&self, run_id: &str) -> UltraplanRunState {
            self.runs.lock().unwrap()[run_id].clone()
        }
    }

    impl rebon_tool::UltraplanRunRepository for MultiRunRepository {
        fn load_current(&self) -> Result<UltraplanRunState, rebon_tool::UltraplanRepositoryError> {
            let run_id = self.current_run_id.lock().unwrap().clone();
            self.load_run(&run_id)?
                .ok_or(rebon_tool::UltraplanRepositoryError::Missing)
        }

        fn load_run(
            &self,
            run_id: &str,
        ) -> Result<Option<UltraplanRunState>, rebon_tool::UltraplanRepositoryError> {
            Ok(self.runs.lock().unwrap().get(run_id).cloned())
        }

        fn compare_and_swap(
            &self,
            expected_revision: u64,
            state: &UltraplanRunState,
        ) -> Result<(), rebon_tool::UltraplanRepositoryError> {
            let mut runs = self.runs.lock().unwrap();
            let current = runs
                .get(&state.run_id)
                .ok_or(rebon_tool::UltraplanRepositoryError::Missing)?;
            if current.state_revision != expected_revision {
                return Err(rebon_tool::UltraplanRepositoryError::StaleRevision {
                    expected: expected_revision,
                    actual: current.state_revision,
                });
            }
            runs.insert(state.run_id.clone(), state.clone());
            Ok(())
        }

        fn create_run(
            &self,
            state: &UltraplanRunState,
        ) -> Result<(), rebon_tool::UltraplanRepositoryError> {
            let mut runs = self.runs.lock().unwrap();
            if runs.contains_key(&state.run_id) {
                return Err(rebon_tool::UltraplanRepositoryError::Storage(format!(
                    "run `{}` already exists",
                    state.run_id
                )));
            }
            runs.insert(state.run_id.clone(), state.clone());
            Ok(())
        }

        fn switch_current(&self, run_id: &str) -> Result<(), rebon_tool::UltraplanRepositoryError> {
            if !self.runs.lock().unwrap().contains_key(run_id) {
                return Err(rebon_tool::UltraplanRepositoryError::Missing);
            }
            *self.current_run_id.lock().unwrap() = run_id.to_string();
            Ok(())
        }
    }

    fn context_with_repository(repository: MultiRunRepository) -> ToolContext {
        ToolContext::new()
            .with_session_id("session")
            .with_execution_policy(ExecutionPolicy::ultraplan(UltraplanContext::planning_turn(
                "source",
                "plan",
                PolicyMode::Enforce,
            )))
            .with_ultraplan_run_repository(Arc::new(repository))
    }

    #[tokio::test]
    async fn set_add_list_and_manifest_conversion() {
        let (context, state) = context_with_state();
        let tool = PlanLedgerTool;
        tool.call(
            json!({"operation":"set_requirements","expected_revision":1,"items":[{"id":"R1","title":"first"}]}),
            &context,
        )
        .await
        .unwrap();
        tool.call(
            json!({"operation":"add","expected_revision":2,"items":[{"id":"R2","title":"second"}]}),
            &context,
        )
        .await
        .unwrap();
        let output = tool
            .call(json!({"operation":"list"}), &context)
            .await
            .unwrap();

        assert_eq!(output["requirements"].as_array().unwrap().len(), 2);
        let state = state.lock().unwrap();
        let manifest = state.manifest.as_ref().unwrap();
        assert_eq!(manifest.source_path, "ledger");
        assert_eq!(manifest.items[0].id, "R1");
        assert_eq!(manifest.items[1].id, "R2");
    }

    #[tokio::test]
    async fn grill_ledger_changes_revision_and_seals_current_understanding() {
        let (context, state) = grill_context_with_state();
        let tool = PlanLedgerTool;

        tool.call(
            json!({"operation":"set_requirements","expected_revision":1,"items":[{"id":"R1","title":"first"}]}),
            &context,
        )
        .await
        .unwrap();
        {
            let state = state.lock().unwrap();
            assert_eq!(state.interview.revision, 2);
            assert!(state.interview.seal.is_none());
        }

        let output = tool
            .call(
                json!({"operation":"seal_understanding","expected_revision":2}),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(output["understandingSeal"]["revision"], 2);
        assert!(state.lock().unwrap().grill_understanding_is_sealed());

        tool.call(
            json!({"operation":"add","expected_revision":2,"items":[{"id":"R2","title":"second"}]}),
            &context,
        )
        .await
        .unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.interview.revision, 3);
        assert!(state.interview.seal.is_none());
    }

    #[tokio::test]
    async fn mutating_ledger_operations_use_host_persister_immediately() {
        let (context, _state) = grill_context_with_state();
        let persisted = Arc::new(Mutex::new(Vec::new()));
        let persisted_for_hook = Arc::clone(&persisted);
        let context = context.with_ultraplan_run_persister(Arc::new(move |handle| {
            persisted_for_hook
                .lock()
                .unwrap()
                .push(handle.lock().unwrap().clone());
        }));
        let tool = PlanLedgerTool;

        tool.call(
            json!({"operation":"set_requirements","expected_revision":1,"items":[{"id":"R1","title":"first"}]}),
            &context,
        )
        .await
        .unwrap();
        tool.call(
            json!({"operation":"seal_understanding","expected_revision":2}),
            &context,
        )
        .await
        .unwrap();
        tool.call(json!({"operation":"list"}), &context)
            .await
            .unwrap();

        let persisted = persisted.lock().unwrap();
        assert_eq!(persisted.len(), 2);
        assert!(persisted.last().unwrap().grill_understanding_is_sealed());
    }

    #[tokio::test]
    async fn seal_understanding_requires_grill_and_non_empty_ledger() {
        let tool = PlanLedgerTool;
        let (standard_context, _) = context_with_state();
        let err = tool
            .call(
                json!({"operation":"seal_understanding","expected_revision":1}),
                &standard_context,
            )
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("only available for Grill"));

        let (grill_context, _) = grill_context_with_state();
        let err = tool
            .call(
                json!({"operation":"seal_understanding","expected_revision":1}),
                &grill_context,
            )
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("non-empty requirement ledger"));
    }

    #[tokio::test]
    async fn schema_is_flat_object_and_missing_items_still_rejected() {
        let tool = PlanLedgerTool;
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        for key in ["oneOf", "anyOf", "allOf", "enum", "const", "not", "$ref"] {
            assert!(
                schema.get(key).is_none(),
                "top-level `{key}` must be absent"
            );
        }

        // The flat schema cannot express operation-specific fields; runtime
        // validation must still require items and expected_revision.
        let (context, _state) = context_with_state();
        for operation in ["set_requirements", "add", "replace"] {
            let outcome = tool
                .validate_input(
                    &json!({"operation": operation, "expected_revision": 1}),
                    &context,
                )
                .await
                .unwrap();
            assert!(!outcome.result, "`{operation}` without items must fail");
        }
        let list = tool
            .validate_input(&json!({"operation": "list"}), &context)
            .await
            .unwrap();
        assert!(list.result);
        let seal_without_revision = tool
            .validate_input(&json!({"operation": "seal_understanding"}), &context)
            .await
            .unwrap();
        assert!(!seal_without_revision.result);
        let seal = tool
            .validate_input(
                &json!({"operation": "seal_understanding", "expected_revision": 1}),
                &context,
            )
            .await
            .unwrap();
        assert!(seal.result);
    }

    #[tokio::test]
    async fn duplicate_ids_are_rejected() {
        let (context, _state) = context_with_state();
        let tool = PlanLedgerTool;
        let outcome = tool
            .validate_input(
                &json!({"operation":"set_requirements","expected_revision":1,"items":[{"id":"R1","title":"a"},{"id":"R1","title":"b"}]}),
                &context,
            )
            .await
            .unwrap();
        assert!(!outcome.result);
        assert!(outcome
            .message
            .unwrap()
            .contains("duplicate requirement id"));
    }

    #[tokio::test]
    async fn denied_without_ultraplan_policy() {
        let tool = PlanLedgerTool;
        let decision = tool
            .check_permissions(&json!({"operation":"list"}), &ToolContext::new())
            .await
            .unwrap();
        assert_eq!(
            decision.behavior,
            rebon_tools_core::PermissionBehavior::Deny
        );
        let err = DenyAskPermissionBroker
            .resolve(
                &tool,
                json!({"operation":"list"}),
                &ToolContext::new(),
                decision,
            )
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("only available"));
    }

    fn manifest_snapshot() -> UltraplanManifestSnapshot {
        UltraplanManifestSnapshot {
            source_path: "requirements.md".into(),
            canonical_path: "/repo/requirements.md".into(),
            display_path: "requirements.md".into(),
            content_sha256: "abc".into(),
            items: vec![UltraplanManifestItem {
                id: "M1".into(),
                title: "manifest item".into(),
                line: 1,
                required: true,
            }],
        }
    }

    #[tokio::test]
    async fn set_rejects_initialized_non_manifest_ledger() {
        let (context, _state) = context_with_state();
        let tool = PlanLedgerTool;
        tool.call(
            json!({"operation":"set_requirements","expected_revision":1,"items":[{"id":"R1","title":"first"}]}),
            &context,
        )
        .await
        .unwrap();

        let err = tool
            .call(
                json!({"operation":"set_requirements","expected_revision":2,"items":[{"id":"R2","title":"second"}]}),
                &context,
            )
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("only be used before"));
    }

    #[tokio::test]
    async fn list_preserves_manifest_backed_state_with_empty_ledger() {
        let (context, state) = context_with_state();
        let manifest = manifest_snapshot();
        state.lock().unwrap().manifest = Some(manifest.clone());

        let output = PlanLedgerTool
            .call(json!({"operation":"list"}), &context)
            .await
            .unwrap();

        assert_eq!(output["requirements"].as_array().unwrap().len(), 0);
        assert_eq!(state.lock().unwrap().manifest, Some(manifest));
    }

    #[tokio::test]
    async fn replace_and_rollback_are_revision_checked_and_reversible() {
        let (context, state) = context_with_state();
        let tool = PlanLedgerTool;
        tool.call(
            json!({"operation":"set_requirements","expected_revision":1,"items":[{"id":"R1","title":"first"}]}),
            &context,
        )
        .await
        .unwrap();
        tool.call(
            json!({"operation":"replace","expected_revision":2,"items":[{"id":"R1","title":"first"},{"id":"R2","title":"second"}]}),
            &context,
        )
        .await
        .unwrap();

        let stale = tool
            .call(
                json!({"operation":"replace","expected_revision":2,"items":[{"id":"R9","title":"stale"}]}),
                &context,
            )
            .await
            .unwrap_err();
        assert!(format!("{stale}").contains("stale ledger revision"));

        tool.call(
            json!({"operation":"rollback","expected_revision":3,"target_revision":2}),
            &context,
        )
        .await
        .unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.ledger_revision, 4);
        assert_eq!(state.requirement_ledger.len(), 1);
        assert_eq!(state.requirement_ledger[0].id, "R1");
    }

    #[tokio::test]
    async fn additive_stale_revision_reloads_once_without_overwriting_new_requirements() {
        let (context, state) = context_with_state();
        let tool = PlanLedgerTool;
        tool.call(
            json!({"operation":"set_requirements","expected_revision":1,"items":[{"id":"R1","title":"first"}]}),
            &context,
        )
        .await
        .unwrap();
        tool.call(
            json!({"operation":"add","expected_revision":2,"items":[{"id":"R2","title":"second"}]}),
            &context,
        )
        .await
        .unwrap();

        let output = tool
            .call(
                json!({"operation":"add","expected_revision":2,"items":[{"id":"R3","title":"third"}]}),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(output["reloadedFromRevision"], 2);
        let state = state.lock().unwrap();
        assert_eq!(state.ledger_revision, 4);
        assert_eq!(
            state
                .requirement_ledger
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec!["R1", "R2", "R3"]
        );
    }

    #[tokio::test]
    async fn fork_switches_the_active_repository_and_queues_host_transition() {
        let source =
            UltraplanRunState::new("source".into(), "session".into(), "task".into(), None, 1);
        let repository = MultiRunRepository::new(vec![source], "source");
        let context = context_with_repository(repository.clone());
        let tool = PlanLedgerTool;

        let output = tool
            .call(json!({"operation":"fork","expected_revision":1}), &context)
            .await
            .unwrap();
        let target_run_id = output["pendingTransition"]["target_run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let listed = tool
            .call(json!({"operation":"list"}), &context)
            .await
            .unwrap();

        assert_eq!(listed["runId"], target_run_id);
        let source = repository.state("source");
        assert_eq!(
            source
                .pending_transition
                .as_ref()
                .map(|transition| transition.target_run_id.as_str()),
            Some(target_run_id.as_str())
        );
    }

    #[tokio::test]
    async fn resume_uses_the_active_revision_and_switches_to_the_target_run() {
        let source = UltraplanRunState::new(
            "source".into(),
            "session".into(),
            "source task".into(),
            None,
            1,
        );
        let target = UltraplanRunState::new(
            "target".into(),
            "other-session".into(),
            "target task".into(),
            None,
            1,
        );
        let repository = MultiRunRepository::new(vec![source, target], "source");
        let context = context_with_repository(repository.clone());
        let tool = PlanLedgerTool;

        tool.call(
            json!({"operation":"resume","run_id":"target","expected_revision":1}),
            &context,
        )
        .await
        .unwrap();
        let listed = tool
            .call(json!({"operation":"list"}), &context)
            .await
            .unwrap();

        assert_eq!(listed["runId"], "target");
        assert_eq!(
            repository.state("target").identity.attached_session_id,
            "session"
        );
        assert_eq!(
            repository
                .state("source")
                .pending_transition
                .as_ref()
                .map(|transition| transition.kind),
            Some(RunTransitionKind::Resume)
        );
    }

    #[tokio::test]
    async fn set_rejects_manifest_backed_state_even_when_ledger_empty() {
        let (context, state) = context_with_state();
        state.lock().unwrap().manifest = Some(manifest_snapshot());

        let err = PlanLedgerTool
            .call(
                json!({"operation":"set_requirements","expected_revision":1,"items":[{"id":"R1","title":"new"}]}),
                &context,
            )
            .await
            .unwrap_err();

        assert!(format!("{err}").contains("cannot overwrite manifest"));
    }
}
