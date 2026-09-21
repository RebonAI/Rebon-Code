use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{PlanCoverageResult, UltraplanManifestItem, UltraplanManifestSnapshot};

// Any schema change to `UltraplanRunState` or its nested types MUST bump this
// version and add a migration arm in `decode_ultraplan_run_state`: the
// integrity hash covers the whole serialized state, so an unversioned field
// change silently invalidates every persisted run.
pub const ULTRAPLAN_RUN_STATE_VERSION: u32 = 3;
const ULTRAPLAN_RUN_STATE_VERSION_V2: u32 = 2;
const ULTRAPLAN_RUN_STATE_VERSION_V1: u32 = 1;
pub const ULTRAPLAN_CONFIRM_UNDERSTANDING_INTENT: &str = "confirm_understanding";
pub const ULTRAPLAN_CONFIRM_UNDERSTANDING_LABEL: &str = "Confirm shared understanding";
pub const ULTRAPLAN_REVISE_UNDERSTANDING_LABEL: &str = "Revise the plan";

fn default_max_tool_error_retries() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UltraplanProfile {
    #[default]
    Standard,
    Grill,
}

impl UltraplanProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Grill => "grill",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunIdentity {
    pub run_id: String,
    pub lineage_id: String,
    pub origin_session_id: String,
    pub attached_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<RunRevisionRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRevisionRef {
    pub run_id: String,
    pub ledger_revision: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionSnapshot {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub allowed_roots: Vec<String>,
    #[serde(default)]
    pub read_allowed: bool,
    #[serde(default)]
    pub write_allowed: bool,
    #[serde(default)]
    pub shell_allowed: bool,
    #[serde(default)]
    pub tool_ids: Vec<String>,
    #[serde(default)]
    pub network: NetworkCapability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_dirty: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityContext {
    pub run_id: String,
    pub ledger_revision: u64,
    pub requirements_hash: String,
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub allowed_roots: Vec<String>,
    #[serde(default)]
    pub read_allowed: bool,
    #[serde(default)]
    pub write_allowed: bool,
    #[serde(default)]
    pub shell_allowed: bool,
    #[serde(default)]
    pub tool_ids: Vec<String>,
    #[serde(default)]
    pub network: NetworkCapability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_dirty: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub sub_agent_available: bool,
    #[serde(default)]
    pub max_research_agents: u32,
    #[serde(default)]
    pub research_agents_used: u32,
    #[serde(default)]
    pub max_adversarial_reviews: u32,
    #[serde(default)]
    pub adversarial_reviews_used: u32,
    #[serde(default = "default_max_tool_error_retries")]
    pub max_tool_error_retries: u32,
    #[serde(default)]
    pub capability_hash: String,
}

impl CapabilityContext {
    pub fn refresh_hash(&mut self) {
        self.allowed_roots.sort();
        self.allowed_roots.dedup();
        self.tool_ids.sort();
        self.tool_ids.dedup();
        if let NetworkCapability::ToolScoped(tool_ids) = &mut self.network {
            tool_ids.sort();
            tool_ids.dedup();
        }
        self.capability_hash.clear();
        self.capability_hash = hash_serializable(self);
    }

    pub fn hash_is_valid(&self) -> bool {
        let expected = self.capability_hash.clone();
        let mut refreshed = self.clone();
        refreshed.refresh_hash();
        !expected.is_empty() && refreshed.capability_hash == expected
    }

    pub fn permission_snapshot(&self) -> PermissionSnapshot {
        PermissionSnapshot {
            session_id: self.session_id.clone(),
            allowed_roots: self.allowed_roots.clone(),
            read_allowed: self.read_allowed,
            write_allowed: self.write_allowed,
            shell_allowed: self.shell_allowed,
            tool_ids: self.tool_ids.clone(),
            network: self.network.clone(),
            workspace_head: self.workspace_head.clone(),
            workspace_dirty: self.workspace_dirty,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityDiagnosticClass {
    MissingRoot,
    ToolUnavailable,
    ReadDenied,
    SessionScopeMismatch,
    CapabilityDrift,
    StaleRevision,
    CircuitOpen,
    BudgetExhausted,
}

impl CapabilityDiagnosticClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingRoot => "missing_root",
            Self::ToolUnavailable => "tool_unavailable",
            Self::ReadDenied => "read_denied",
            Self::SessionScopeMismatch => "session_scope_mismatch",
            Self::CapabilityDrift => "capability_drift",
            Self::StaleRevision => "stale_revision",
            Self::CircuitOpen => "circuit_open",
            Self::BudgetExhausted => "budget_exhausted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityDiagnostic {
    pub class: CapabilityDiagnosticClass,
    pub message: String,
    pub run_id: String,
    pub ledger_revision: u64,
    pub capability_hash: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub fallback_to_parent: bool,
}

impl std::fmt::Display for CapabilityDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match serde_json::to_string(self) {
            Ok(value) => f.write_str(&value),
            Err(_) => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for CapabilityDiagnostic {}

impl CapabilityDiagnostic {
    pub fn fingerprint(&self) -> String {
        let details = serde_json::to_vec(&(
            self.class,
            self.root.as_deref(),
            self.capability.as_deref(),
            self.message.as_str(),
        ))
        .unwrap_or_default();
        format!(
            "{}:{}:{}:{:x}",
            self.run_id,
            self.capability_hash,
            self.role,
            Sha256::digest(details)
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UltraplanStage {
    #[default]
    ScopeConfirm,
    EvidenceVerify,
    AdversarialReview,
    FinalGate,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UltraplanBudget {
    pub max_research_agents: u32,
    pub max_plan_revisions: u32,
    pub max_adversarial_reviews: u32,
    pub max_tool_error_retries: u32,
    #[serde(default)]
    pub research_agents_used: u32,
    #[serde(default)]
    pub plan_revisions_used: u32,
    #[serde(default)]
    pub adversarial_reviews_used: u32,
}

impl Default for UltraplanBudget {
    fn default() -> Self {
        Self::standard()
    }
}

impl UltraplanBudget {
    pub fn standard() -> Self {
        Self {
            max_research_agents: 6,
            max_plan_revisions: 2,
            max_adversarial_reviews: 1,
            max_tool_error_retries: 1,
            research_agents_used: 0,
            plan_revisions_used: 0,
            adversarial_reviews_used: 0,
        }
    }

    pub fn strict() -> Self {
        Self::standard()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerFindingClass {
    Blocker,
    Advisory,
    OutOfScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewerFinding {
    pub classification: ReviewerFindingClass,
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementPatchItem {
    pub id: String,
    pub title: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementsPatch {
    pub base_revision: u64,
    #[serde(default)]
    pub items: Vec<RequirementPatchItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredReview {
    pub plan_hash: String,
    pub base_revision: u64,
    pub verdict: String,
    #[serde(default)]
    pub findings: Vec<ReviewerFinding>,
    #[serde(default)]
    pub step_coverage: Vec<ReviewStepCoverage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirements_patch: Option<RequirementsPatch>,
}

impl StructuredReview {
    pub fn blockers(&self) -> impl Iterator<Item = &ReviewerFinding> {
        self.findings
            .iter()
            .filter(|finding| finding.classification == ReviewerFindingClass::Blocker)
    }

    pub fn is_pass(&self) -> bool {
        self.blockers().next().is_none()
            && !self.step_coverage.is_empty()
            && self.step_coverage.iter().all(|coverage| coverage.ok)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UltraplanEvidenceReference {
    pub location: String,
    pub claim: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UltraplanCheckpoint {
    pub run_id: String,
    pub ledger_revision: u64,
    pub stage: UltraplanStage,
    pub requirements_hash: String,
    #[serde(default)]
    pub user_decisions: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<UltraplanEvidenceReference>,
    #[serde(default)]
    pub unresolved_blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_hash: Option<String>,
    #[serde(default)]
    pub capability_hash: String,
    pub budget: UltraplanBudget,
    pub next_action: String,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalGateOutcome {
    Pass,
    Revalidate,
    Degraded,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UltraplanDiagnosticClass {
    ReviewerUnavailable,
    ReviewerMalformed,
    BudgetExhausted,
    GateConflict,
    StaleGate,
    ManifestDrift,
    StateCorrupt,
    CapabilityFailure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UltraplanDiagnostic {
    pub class: UltraplanDiagnosticClass,
    pub message: String,
    pub ledger_revision: u64,
    pub stage: UltraplanStage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_hash: Option<String>,
    #[serde(default)]
    pub details: serde_json::Value,
}

impl std::fmt::Display for UltraplanDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match serde_json::to_string(self) {
            Ok(value) => f.write_str(&value),
            Err(_) => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for UltraplanDiagnostic {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalGateRecord {
    pub outcome: FinalGateOutcome,
    pub plan_hash: String,
    pub ledger_revision: u64,
    #[serde(default)]
    pub diagnostics: Vec<UltraplanDiagnostic>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkCapability {
    #[default]
    Denied,
    ToolScoped(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunHead {
    pub run_id: String,
    pub ledger_revision: u64,
    pub requirements_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seal_hash: Option<String>,
    pub permission_snapshot: PermissionSnapshot,
    pub permission_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_hash: Option<String>,
}

/// A point-in-time snapshot of the requirement ledger. Rollback, reset, and
/// fork restore exactly this — plan/review bindings are re-derived and the
/// run budget is monotonic, so neither travels back in time. V2 files
/// persisted full-state snapshots with many more fields; serde ignores the
/// extras, which is what makes the v2 → v3 migration a plain deserialize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerRevision {
    pub revision: u64,
    pub mutation: RunRevisionMutation,
    #[serde(default)]
    pub requirement_ledger: Vec<RequirementLedgerEntry>,
    #[serde(default)]
    pub interview: UltraplanInterviewState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<UltraplanManifestSnapshot>,
    #[serde(default)]
    pub requirements_hash: String,
    #[serde(default)]
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RunRevisionMutation {
    Initial,
    MigratedV1,
    RequirementsSet,
    RequirementsAdded,
    RequirementsReplaced,
    InterviewRecorded,
    UnderstandingSealed,
    Rollback {
        target_revision: u64,
    },
    Reset,
    PlanUpdated,
    ReviewUpdated,
    ApprovalConfirmed,
    CapabilityChanged,
    /// Legacy (v2): the stage machine is gone; kept so old files deserialize.
    StageAdvanced,
    /// Legacy (v2): the stage machine is gone; kept so old files deserialize.
    StageRestarted,
    BudgetConsumed,
    BudgetSynchronized,
    CheckpointWritten,
    DiagnosticRecorded,
    ReviewPatchApplied,
    FinalGateRecorded,
    StateSynchronized,
    Resume {
        session_id: String,
    },
    Fork {
        source_run_id: String,
        source_revision: u64,
    },
}

impl RunRevisionMutation {
    fn changes_ledger(&self) -> bool {
        matches!(
            self,
            Self::Initial
                | Self::MigratedV1
                | Self::RequirementsSet
                | Self::RequirementsAdded
                | Self::RequirementsReplaced
                | Self::InterviewRecorded
                | Self::Rollback { .. }
                | Self::Reset
                | Self::ReviewPatchApplied
                | Self::Fork { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunTransition {
    pub kind: RunTransitionKind,
    pub target_run_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTransitionKind {
    Fork,
    Resume,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UltraplanStateError {
    RevisionNotFound(u64),
    StaleRevision { expected: u64, actual: u64 },
    RunIdMismatch { expected: String, actual: String },
    BudgetExhausted { resource: String, limit: u32 },
    RequirementsPatchConflict(String),
}

impl std::fmt::Display for UltraplanStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RevisionNotFound(revision) => {
                write!(f, "ultraplan revision {revision} was not found")
            }
            Self::StaleRevision { expected, actual } => write!(
                f,
                "stale ultraplan revision: expected {expected}, current revision is {actual}"
            ),
            Self::RunIdMismatch { expected, actual } => write!(
                f,
                "ultraplan run id mismatch: expected `{expected}`, got `{actual}`"
            ),
            Self::BudgetExhausted { resource, limit } => {
                write!(f, "ultraplan {resource} budget exhausted (limit {limit})")
            }
            Self::RequirementsPatchConflict(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for UltraplanStateError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UltraplanRunState {
    pub version: u32,
    #[serde(default)]
    pub integrity_hash: String,
    pub run_id: String,
    pub session_id: String,
    #[serde(default)]
    pub identity: RunIdentity,
    #[serde(default)]
    pub ledger_revision: u64,
    #[serde(default)]
    pub state_revision: u64,
    #[serde(default)]
    pub requirements_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seal_hash: Option<String>,
    #[serde(default)]
    pub permission_snapshot: PermissionSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_context: Option<CapabilityContext>,
    #[serde(default)]
    pub permission_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_hash: Option<String>,
    #[serde(default)]
    pub revision_history: Vec<LedgerRevision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_transition: Option<RunTransition>,
    #[serde(default)]
    pub budget: UltraplanBudget,
    #[serde(default)]
    pub tool_error_attempts: BTreeMap<String, u32>,
    #[serde(default)]
    pub checkpoints: Vec<UltraplanCheckpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_review: Option<StructuredReview>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_requirements_patch: Option<RequirementsPatch>,
    #[serde(default)]
    pub diagnostics: Vec<UltraplanDiagnostic>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_gate: Option<FinalGateRecord>,
    /// Plan hash of an adversarial review that has been reserved (budget
    /// consumed) but whose result has not been ingested yet. Ephemeral
    /// coordination state: not part of revision snapshots, cleared on fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_review_plan_hash: Option<String>,
    pub task: String,
    #[serde(default)]
    pub profile: UltraplanProfile,
    pub phase: RunPhase,
    pub round: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<UltraplanManifestSnapshot>,
    #[serde(default)]
    pub requirement_ledger: Vec<RequirementLedgerEntry>,
    #[serde(default)]
    pub interview: UltraplanInterviewState,
    #[serde(default)]
    pub asked_user_once: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_plan_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_review_passed_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_review_failed_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_rejected_plan_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_review_summary: Option<ReviewSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_plan_draft: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_coverage: Option<PlanCoverageResult>,
    #[serde(default)]
    pub reviewer_verdicts: Vec<ReviewerVerdictRecord>,
    #[serde(default)]
    pub execution_cards: Vec<ExecutionCard>,
    #[serde(default)]
    pub file_hashes: std::collections::BTreeMap<String, String>,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
}

impl UltraplanRunState {
    pub fn new(
        run_id: String,
        session_id: String,
        task: String,
        manifest: Option<UltraplanManifestSnapshot>,
        now_ms: u64,
    ) -> Self {
        let requirement_ledger = manifest
            .as_ref()
            .map(|manifest| {
                manifest
                    .items
                    .iter()
                    .map(|item| RequirementLedgerEntry {
                        id: item.id.clone(),
                        title: item.title.clone(),
                        source: RequirementSource::Manifest,
                        round_added: 1,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let identity = RunIdentity {
            run_id: run_id.clone(),
            lineage_id: run_id.clone(),
            origin_session_id: session_id.clone(),
            attached_session_id: session_id.clone(),
            parent: None,
        };
        let permission_snapshot = PermissionSnapshot {
            session_id: session_id.clone(),
            ..PermissionSnapshot::default()
        };
        let mut state = Self {
            version: ULTRAPLAN_RUN_STATE_VERSION,
            integrity_hash: String::new(),
            run_id,
            session_id,
            identity,
            ledger_revision: 1,
            state_revision: 1,
            requirements_hash: String::new(),
            seal_hash: None,
            permission_snapshot,
            capability_context: None,
            permission_hash: String::new(),
            plan_hash: None,
            revision_history: Vec::new(),
            pending_transition: None,
            budget: UltraplanBudget::standard(),
            tool_error_attempts: BTreeMap::new(),
            checkpoints: Vec::new(),
            structured_review: None,
            pending_requirements_patch: None,
            diagnostics: Vec::new(),
            final_gate: None,
            pending_review_plan_hash: None,
            task,
            profile: UltraplanProfile::Standard,
            phase: RunPhase::PlanModeActive,
            round: 1,
            manifest,
            requirement_ledger,
            interview: UltraplanInterviewState {
                revision: 1,
                ..UltraplanInterviewState::default()
            },
            asked_user_once: false,
            released_plan_hash: None,
            auto_review_passed_hash: None,
            manual_review_failed_hash: None,
            user_rejected_plan_hash: None,
            last_review_summary: None,
            last_plan_draft: None,
            last_coverage: None,
            reviewer_verdicts: Vec::new(),
            execution_cards: Vec::new(),
            file_hashes: BTreeMap::new(),
            started_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        state.refresh_integrity();
        state.record_ledger_revision(RunRevisionMutation::Initial, now_ms);
        state
    }

    pub fn with_profile(mut self, profile: UltraplanProfile) -> Self {
        self.profile = profile;
        self.budget = if profile == UltraplanProfile::Grill {
            UltraplanBudget::strict()
        } else {
            UltraplanBudget::standard()
        };
        self.refresh_integrity();
        self.record_ledger_revision(RunRevisionMutation::Initial, self.updated_at_ms);
        self
    }

    /// The workflow stage is derived from the run's data instead of being
    /// stored: it can never disagree with the bindings the gates actually
    /// check (the class of bug behind the grill early-stop and review
    /// deadlock incidents).
    pub fn stage(&self) -> UltraplanStage {
        if matches!(self.phase, RunPhase::Executing | RunPhase::Done) {
            return UltraplanStage::Completed;
        }
        if let Some(plan_hash) = self.plan_hash.as_deref() {
            // A user-rejected plan must fall back to drafting even though
            // its review artifacts are still recorded.
            if self.user_rejected_plan_hash.as_deref() != Some(plan_hash) {
                let gate_bound = self
                    .final_gate
                    .as_ref()
                    .is_some_and(|gate| gate.plan_hash == plan_hash);
                let review_bound = self
                    .structured_review
                    .as_ref()
                    .is_some_and(|review| review.plan_hash == plan_hash)
                    || self.auto_review_passed_hash.as_deref() == Some(plan_hash);
                if gate_bound || review_bound {
                    return UltraplanStage::FinalGate;
                }
            }
            if self.pending_review_plan_hash.as_deref() == Some(plan_hash) {
                return UltraplanStage::AdversarialReview;
            }
        }
        if self.scope_confirmed() {
            return UltraplanStage::EvidenceVerify;
        }
        UltraplanStage::ScopeConfirm
    }

    /// Scope confirmation ends the interview stage: Grill requires the sealed
    /// understanding, Standard requires at least one answered question.
    pub fn scope_confirmed(&self) -> bool {
        match self.profile {
            UltraplanProfile::Grill => self.grill_understanding_is_sealed(),
            _ => self.asked_user_once,
        }
    }

    pub fn head(&self) -> RunHead {
        RunHead {
            run_id: self.run_id.clone(),
            ledger_revision: self.ledger_revision,
            requirements_hash: self.requirements_hash.clone(),
            seal_hash: self.seal_hash.clone(),
            permission_snapshot: self.permission_snapshot.clone(),
            permission_hash: self.permission_hash.clone(),
            plan_hash: self.plan_hash.clone(),
        }
    }

    pub fn verify_expected_revision(&self, expected: u64) -> Result<(), UltraplanStateError> {
        if expected == self.ledger_revision {
            Ok(())
        } else {
            Err(UltraplanStateError::StaleRevision {
                expected,
                actual: self.ledger_revision,
            })
        }
    }

    pub fn synchronize_worker_budget_usage(
        &mut self,
        research_agents_used: u32,
        adversarial_reviews_used: u32,
    ) -> bool {
        let research_agents_used = self.budget.research_agents_used.max(research_agents_used);
        let adversarial_reviews_used = self
            .budget
            .adversarial_reviews_used
            .max(adversarial_reviews_used);
        if self.budget.research_agents_used == research_agents_used
            && self.budget.adversarial_reviews_used == adversarial_reviews_used
        {
            return false;
        }
        self.budget.research_agents_used = research_agents_used;
        self.budget.adversarial_reviews_used = adversarial_reviews_used;
        self.advance_revision(RunRevisionMutation::BudgetSynchronized);
        true
    }

    pub fn tool_error_attempt_count(&self, fingerprint: &str) -> u32 {
        self.tool_error_attempts
            .get(fingerprint)
            .copied()
            .unwrap_or(0)
    }

    pub fn record_tool_error_attempts(&mut self, fingerprint: String, attempts: u32) -> u32 {
        if attempts == 0 {
            return self.tool_error_attempt_count(&fingerprint);
        }
        let count = self.tool_error_attempts.entry(fingerprint).or_default();
        *count = count.saturating_add(attempts);
        let count = *count;
        self.advance_revision(RunRevisionMutation::BudgetConsumed);
        count
    }

    pub fn clear_tool_error_attempts(&mut self, prefix: &str) -> bool {
        let before = self.tool_error_attempts.len();
        self.tool_error_attempts
            .retain(|fingerprint, _| !fingerprint.starts_with(prefix));
        if self.tool_error_attempts.len() == before {
            return false;
        }
        self.advance_revision(RunRevisionMutation::BudgetSynchronized);
        true
    }

    pub fn consume_research_agent(&mut self) -> Result<(), UltraplanStateError> {
        if self.budget.research_agents_used >= self.budget.max_research_agents {
            return Err(UltraplanStateError::BudgetExhausted {
                resource: "research agent".into(),
                limit: self.budget.max_research_agents,
            });
        }
        self.budget.research_agents_used += 1;
        self.advance_revision(RunRevisionMutation::BudgetConsumed);
        Ok(())
    }

    pub fn consume_adversarial_review(&mut self) -> Result<(), UltraplanStateError> {
        if self.budget.adversarial_reviews_used >= self.budget.max_adversarial_reviews {
            return Err(UltraplanStateError::BudgetExhausted {
                resource: "adversarial review".into(),
                limit: self.budget.max_adversarial_reviews,
            });
        }
        self.budget.adversarial_reviews_used += 1;
        self.advance_revision(RunRevisionMutation::BudgetConsumed);
        Ok(())
    }

    pub fn set_plan_artifacts(
        &mut self,
        plan: String,
        coverage: PlanCoverageResult,
        execution_cards: Vec<ExecutionCard>,
    ) -> bool {
        let next_hash = ultraplan_plan_hash_for_profile(self.profile, &plan);
        if self.plan_hash.as_deref() == Some(next_hash.as_str())
            && self.last_coverage.as_ref() == Some(&coverage)
            && self.execution_cards == execution_cards
        {
            return false;
        }
        if self.plan_hash.is_some() && self.plan_hash.as_deref() != Some(next_hash.as_str()) {
            // Material revisions are counted but never hard-blocked: loop
            // prevention lives in the single-review reservation and the
            // rejected-hash bindings, and a hard error here could only strand
            // the run. The prompt still advertises the soft cap.
            self.budget.plan_revisions_used = self.budget.plan_revisions_used.saturating_add(1);
        }
        self.last_plan_draft = Some(plan);
        self.last_coverage = Some(coverage);
        self.execution_cards = execution_cards;
        self.interview.confirmation = None;
        self.released_plan_hash = None;
        self.auto_review_passed_hash = None;
        self.manual_review_failed_hash = None;
        self.last_review_summary = None;
        self.structured_review = None;
        self.pending_requirements_patch = None;
        self.final_gate = None;
        self.advance_revision(RunRevisionMutation::PlanUpdated);
        true
    }

    pub fn record_structured_review(&mut self, review: StructuredReview) {
        self.pending_requirements_patch = review.requirements_patch.clone();
        self.structured_review = Some(review);
        self.advance_revision(RunRevisionMutation::ReviewUpdated);
    }

    pub fn apply_pending_requirements_patch(&mut self) -> Result<bool, UltraplanStateError> {
        let Some(patch) = self.pending_requirements_patch.clone() else {
            return Ok(false);
        };
        if patch.base_revision != self.ledger_revision {
            return Err(UltraplanStateError::StaleRevision {
                expected: patch.base_revision,
                actual: self.ledger_revision,
            });
        }
        if patch.items.is_empty() {
            self.pending_requirements_patch = None;
            return Ok(false);
        }
        let mut seen = BTreeSet::new();
        for item in &patch.items {
            if !seen.insert(item.id.clone())
                || self
                    .requirement_ledger
                    .iter()
                    .any(|entry| entry.id == item.id)
            {
                return Err(UltraplanStateError::RequirementsPatchConflict(format!(
                    "requirements patch contains duplicate or existing id `{}`",
                    item.id
                )));
            }
        }
        self.requirement_ledger
            .extend(patch.items.into_iter().map(|item| RequirementLedgerEntry {
                id: item.id,
                title: item.title,
                source: RequirementSource::UserFeedback,
                round_added: self.round.max(1),
            }));
        if self
            .manifest
            .as_ref()
            .is_none_or(|manifest| manifest.source_path == "ledger")
        {
            self.manifest = ledger_to_manifest_snapshot(&self.requirement_ledger);
        }
        self.pending_requirements_patch = None;
        self.invalidate_material_bindings();
        self.advance_revision(RunRevisionMutation::ReviewPatchApplied);
        Ok(true)
    }

    pub fn write_checkpoint(
        &mut self,
        evidence: Vec<UltraplanEvidenceReference>,
        next_action: impl Into<String>,
    ) {
        let checkpoint_revision = self.ledger_revision.max(1);
        let checkpoint_created_at_ms = next_state_timestamp(self.updated_at_ms);
        let review_hash = self.structured_review.as_ref().map(hash_serializable);
        let unresolved_blockers = self
            .structured_review
            .as_ref()
            .map(|review| {
                review
                    .blockers()
                    .map(|finding| finding.message.clone())
                    .collect()
            })
            .unwrap_or_default();
        // Checkpoints are a bounded resume aid, not an audit log.
        const MAX_CHECKPOINTS: usize = 8;
        if self.checkpoints.len() >= MAX_CHECKPOINTS {
            self.checkpoints
                .drain(..=self.checkpoints.len() - MAX_CHECKPOINTS);
        }
        self.checkpoints.push(UltraplanCheckpoint {
            run_id: self.run_id.clone(),
            ledger_revision: checkpoint_revision,
            stage: self.stage(),
            requirements_hash: self.requirements_hash.clone(),
            user_decisions: self
                .interview
                .turns
                .iter()
                .map(|turn| format!("{} => {}", turn.question, turn.answer))
                .collect(),
            evidence,
            unresolved_blockers,
            plan_hash: self.plan_hash.clone(),
            review_hash,
            capability_hash: self
                .capability_context
                .as_ref()
                .map(|context| context.capability_hash.clone())
                .unwrap_or_default(),
            budget: self.budget.clone(),
            next_action: next_action.into(),
            created_at_ms: checkpoint_created_at_ms,
        });
        self.advance_revision(RunRevisionMutation::CheckpointWritten);
    }

    pub fn record_diagnostic(&mut self, mut diagnostic: UltraplanDiagnostic) {
        diagnostic.ledger_revision = self.ledger_revision.max(1);
        diagnostic.stage = self.stage();
        self.diagnostics.push(diagnostic);
        self.advance_revision(RunRevisionMutation::DiagnosticRecorded);
    }

    pub fn record_final_gate(
        &mut self,
        outcome: FinalGateOutcome,
        plan_hash: String,
        diagnostics: Vec<UltraplanDiagnostic>,
    ) {
        self.final_gate = Some(FinalGateRecord {
            outcome,
            plan_hash,
            ledger_revision: self.ledger_revision.max(1),
            diagnostics,
        });
        self.advance_revision(RunRevisionMutation::FinalGateRecorded);
    }

    pub fn latest_checkpoint(&self) -> Option<&UltraplanCheckpoint> {
        self.checkpoints.last()
    }

    pub fn record_interview_turn(
        &mut self,
        question: String,
        recommended_answer: Option<String>,
        answer: String,
    ) {
        self.interview.turns.push(UltraplanInterviewTurn {
            question,
            recommended_answer,
            answer,
            round: self.round.max(1),
        });
        self.asked_user_once = true;
        self.advance_revision(RunRevisionMutation::InterviewRecorded);
    }

    pub fn record_grill_interview_turn(
        &mut self,
        question: String,
        recommended_answer: Option<String>,
        answer: String,
    ) -> bool {
        if self.profile != UltraplanProfile::Grill {
            return false;
        }
        self.invalidate_material_bindings();
        self.record_interview_turn(question, recommended_answer, answer);
        true
    }

    pub fn mark_requirements_changed(&mut self, mutation: RunRevisionMutation) {
        self.invalidate_material_bindings();
        self.advance_revision(mutation);
    }

    pub fn mark_grill_requirements_changed(&mut self) -> bool {
        if self.profile != UltraplanProfile::Grill {
            return false;
        }
        self.mark_requirements_changed(RunRevisionMutation::RequirementsAdded);
        true
    }

    pub fn replace_requirements(&mut self, requirements: Vec<RequirementLedgerEntry>) {
        self.requirement_ledger = requirements;
        if self
            .manifest
            .as_ref()
            .is_none_or(|manifest| manifest.source_path == "ledger")
        {
            self.manifest = ledger_to_manifest_snapshot(&self.requirement_ledger);
        }
        self.mark_requirements_changed(RunRevisionMutation::RequirementsReplaced);
    }

    /// Rollback restores the requirement ledger, interview, and manifest of
    /// the target revision. The budget stays monotonic and diagnostics stay
    /// recorded — a rolled-back run that re-submits an already-diagnosed plan
    /// hash may therefore reach degraded delivery directly; that is the
    /// intended price of not letting rollbacks refresh consumed reviews.
    pub fn rollback_to_revision(
        &mut self,
        target_revision: u64,
    ) -> Result<(), UltraplanStateError> {
        let entry = self
            .revision_history
            .iter()
            .find(|entry| entry.revision == target_revision)
            .cloned()
            .ok_or(UltraplanStateError::RevisionNotFound(target_revision))?;
        self.apply_ledger_revision(&entry);
        self.invalidate_material_bindings();
        self.advance_revision(RunRevisionMutation::Rollback { target_revision });
        Ok(())
    }

    pub fn reset_to_baseline(&mut self) -> Result<(), UltraplanStateError> {
        let entry = self
            .revision_history
            .first()
            .cloned()
            .ok_or(UltraplanStateError::RevisionNotFound(1))?;
        self.apply_ledger_revision(&entry);
        self.invalidate_material_bindings();
        self.advance_revision(RunRevisionMutation::Reset);
        Ok(())
    }

    /// Fork starts a fresh run from a past ledger revision: new identity,
    /// fresh budget, and no inherited diagnostics/checkpoints. This is also
    /// the sanctioned escape hatch when the source run's single review was
    /// consumed by infrastructure failure.
    pub fn fork_from_revision(
        &self,
        target_revision: u64,
        new_run_id: String,
        attached_session_id: String,
    ) -> Result<Self, UltraplanStateError> {
        let entry = self
            .revision_history
            .iter()
            .find(|entry| entry.revision == target_revision)
            .cloned()
            .ok_or(UltraplanStateError::RevisionNotFound(target_revision))?;
        let now_ms = next_state_timestamp(self.updated_at_ms);
        let mut fork = self.clone();
        fork.run_id = new_run_id.clone();
        fork.session_id = attached_session_id.clone();
        fork.identity = RunIdentity {
            run_id: new_run_id,
            lineage_id: if self.identity.lineage_id.is_empty() {
                self.run_id.clone()
            } else {
                self.identity.lineage_id.clone()
            },
            origin_session_id: attached_session_id.clone(),
            attached_session_id: attached_session_id.clone(),
            parent: Some(RunRevisionRef {
                run_id: self.run_id.clone(),
                ledger_revision: target_revision,
            }),
        };
        fork.permission_snapshot.session_id = attached_session_id;
        fork.revision_history.clear();
        fork.pending_transition = None;
        fork.pending_review_plan_hash = None;
        fork.budget = if fork.profile == UltraplanProfile::Grill {
            UltraplanBudget::strict()
        } else {
            UltraplanBudget::standard()
        };
        fork.tool_error_attempts.clear();
        fork.diagnostics.clear();
        fork.checkpoints.clear();
        fork.ledger_revision = 1;
        fork.state_revision = 1;
        fork.apply_ledger_revision(&entry);
        fork.invalidate_material_bindings();
        fork.interview.revision = 1;
        fork.started_at_ms = now_ms;
        fork.updated_at_ms = now_ms;
        fork.refresh_integrity();
        fork.record_ledger_revision(
            RunRevisionMutation::Fork {
                source_run_id: self.run_id.clone(),
                source_revision: target_revision,
            },
            now_ms,
        );
        Ok(fork)
    }

    pub fn queue_transition(&mut self, kind: RunTransitionKind, target_run_id: String) {
        self.pending_transition = Some(RunTransition {
            kind,
            target_run_id,
        });
        self.advance_revision(RunRevisionMutation::StateSynchronized);
    }

    pub fn attach_session(&mut self, session_id: String) {
        self.session_id = session_id.clone();
        self.identity.attached_session_id = session_id.clone();
        self.permission_snapshot.session_id = session_id.clone();
        self.advance_revision(RunRevisionMutation::Resume { session_id });
    }

    pub fn set_permission_snapshot(&mut self, snapshot: PermissionSnapshot) {
        if self.permission_snapshot == snapshot && self.capability_context.is_none() {
            return;
        }
        self.permission_snapshot = snapshot;
        self.capability_context = None;
        self.advance_revision(RunRevisionMutation::CapabilityChanged);
    }

    pub fn set_capability_context(&mut self, mut context: CapabilityContext) {
        context.run_id = self.run_id.clone();
        context.session_id = self.identity.attached_session_id.clone();
        context.ledger_revision = self.ledger_revision;
        context.requirements_hash = self.requirements_hash.clone();
        context.refresh_hash();
        if self
            .capability_context
            .as_ref()
            .is_some_and(|current| capabilities_equivalent(current, &context))
        {
            return;
        }
        self.permission_snapshot = context.permission_snapshot();
        self.capability_context = Some(context);
        self.advance_revision(RunRevisionMutation::CapabilityChanged);
    }

    pub fn seal_grill_understanding(&mut self) -> Option<UltraplanUnderstandingSeal> {
        if self.profile != UltraplanProfile::Grill || self.requirement_ledger.is_empty() {
            return None;
        }
        self.invalidate_plan_bindings();
        self.state_revision = self.state_revision.max(1).saturating_add(1);
        self.interview.revision = self.ledger_revision;
        let seal = UltraplanUnderstandingSeal {
            revision: self.ledger_revision,
            requirements_sha256: self.requirements_sha256(),
        };
        self.interview.seal = Some(seal.clone());
        self.updated_at_ms = next_state_timestamp(self.updated_at_ms);
        self.refresh_integrity();
        self.record_ledger_revision(RunRevisionMutation::UnderstandingSealed, self.updated_at_ms);
        // The seal is what ends the Grill scope-confirmation interview; the
        // derived stage() reflects that immediately.
        Some(seal)
    }

    pub fn grill_understanding_is_sealed(&self) -> bool {
        self.interview.seal.as_ref().is_some_and(|seal| {
            seal.revision == self.ledger_revision
                && seal.revision == self.interview.revision
                && seal.requirements_sha256 == self.requirements_sha256()
        })
    }

    /// True when the plan hash is release-eligible from the review side:
    /// either the adversarial review passed, or the final gate recorded a
    /// degraded completion for exactly this plan (reviewer infrastructure
    /// failed and the user still gets to confirm the best verified draft).
    pub fn grill_plan_review_satisfied(&self, plan_hash: &str) -> bool {
        self.auto_review_passed_hash.as_deref() == Some(plan_hash)
            || self.final_gate.as_ref().is_some_and(|gate| {
                gate.outcome == FinalGateOutcome::Degraded && gate.plan_hash == plan_hash
            })
    }

    pub fn confirm_grill_understanding(&mut self, revision: u64, plan_hash: &str) -> bool {
        self.refresh_integrity();
        if self.profile != UltraplanProfile::Grill
            || revision != self.ledger_revision
            || !self.grill_understanding_is_sealed()
            || !self.grill_plan_review_satisfied(plan_hash)
            || self.user_rejected_plan_hash.as_deref() == Some(plan_hash)
            || self.plan_hash.as_deref() != Some(plan_hash)
        {
            return false;
        }
        self.state_revision = self.state_revision.max(1).saturating_add(1);
        self.interview.revision = self.ledger_revision;
        if let Some(seal) = self.interview.seal.as_mut() {
            seal.revision = self.ledger_revision;
        }
        self.interview.confirmation = Some(UltraplanUnderstandingConfirmation {
            revision: self.ledger_revision,
            plan_hash: plan_hash.to_string(),
        });
        self.released_plan_hash = Some(plan_hash.to_string());
        self.updated_at_ms = next_state_timestamp(self.updated_at_ms);
        self.refresh_integrity();
        self.record_ledger_revision(RunRevisionMutation::ApprovalConfirmed, self.updated_at_ms);
        true
    }

    pub fn grill_confirmation_matches(&self, plan_hash: &str) -> bool {
        self.interview
            .confirmation
            .as_ref()
            .is_some_and(|confirmation| {
                confirmation.revision == self.ledger_revision
                    && confirmation.revision == self.interview.revision
                    && confirmation.plan_hash == plan_hash
                    && self.grill_understanding_is_sealed()
            })
    }

    pub fn invalidate_grill_plan_bindings(&mut self) {
        if self.profile == UltraplanProfile::Grill {
            self.invalidate_plan_bindings();
        }
    }

    pub fn invalidate_grill_understanding(&mut self) {
        if self.profile == UltraplanProfile::Grill {
            self.interview.seal = None;
            self.invalidate_plan_bindings();
            self.refresh_integrity();
        }
    }

    pub fn requirements_sha256(&self) -> String {
        requirements_sha256_for(&self.task, &self.requirement_ledger, &self.interview.turns)
    }

    pub fn validate_persisted_integrity(&self) -> Result<(), String> {
        if self.integrity_hash.is_empty() || self.integrity_hash != self.computed_integrity_hash() {
            return Err("ultraplan RunState integrity hash mismatch".into());
        }
        if self.run_id.is_empty()
            || self.session_id.is_empty()
            || self.identity.run_id != self.run_id
            || self.identity.lineage_id.is_empty()
            || self.identity.origin_session_id.is_empty()
            || self.identity.attached_session_id != self.session_id
        {
            return Err("ultraplan RunState identity is invalid".into());
        }
        if self.ledger_revision == 0
            || self.state_revision == 0
            || self.state_revision < self.ledger_revision
            || self.interview.revision != self.ledger_revision
        {
            return Err("ultraplan RunState revisions are inconsistent".into());
        }
        validate_budget(&self.budget)?;
        let requirements_hash = self.requirements_sha256();
        if self.requirements_hash != requirements_hash {
            return Err("ultraplan RunState requirements hash mismatch".into());
        }
        let plan_hash = self
            .last_plan_draft
            .as_deref()
            .map(|plan| ultraplan_plan_hash_for_profile(self.profile, plan));
        if self.plan_hash != plan_hash {
            return Err("ultraplan RunState plan hash mismatch".into());
        }
        validate_seal(
            self.interview.seal.as_ref(),
            self.ledger_revision,
            &self.requirements_hash,
            self.seal_hash.as_deref(),
        )?;
        if self.permission_hash
            != hash_serializable(&canonical_permission_snapshot(&self.permission_snapshot))
        {
            return Err("ultraplan RunState permission hash mismatch".into());
        }
        validate_capability_context(
            self.capability_context.as_ref(),
            &self.run_id,
            self.ledger_revision,
            &self.requirements_hash,
            &self.session_id,
            &self.budget,
            &self.permission_snapshot,
        )?;
        if self.revision_history.is_empty() {
            return Err("ultraplan RunState revision history is empty".into());
        }
        // Head-only validation: ledger entries are checked for monotonicity
        // and requirement-hash consistency, nothing else. The expensive
        // per-snapshot plan/seal/capability revalidation of v2 is gone with
        // the full-state snapshots themselves.
        let mut previous_revision = 0;
        let mut previous_created_at_ms = 0;
        for entry in &self.revision_history {
            if entry.revision <= previous_revision
                || entry.revision == 0
                || entry.created_at_ms < previous_created_at_ms
            {
                return Err("ultraplan RunState ledger history is not monotonic".into());
            }
            let entry_requirements_hash = requirements_sha256_for(
                &self.task,
                &entry.requirement_ledger,
                &entry.interview.turns,
            );
            if entry.requirements_hash != entry_requirements_hash {
                return Err(format!(
                    "ultraplan ledger revision {} requirements hash mismatch",
                    entry.revision
                ));
            }
            previous_revision = entry.revision;
            previous_created_at_ms = entry.created_at_ms;
        }
        if self
            .revision_history
            .last()
            .is_none_or(|entry| !self.ledger_matches_head(entry))
        {
            return Err("ultraplan RunState head does not match its latest ledger revision".into());
        }
        Ok(())
    }

    fn computed_integrity_hash(&self) -> String {
        let mut state = self.clone();
        state.integrity_hash.clear();
        hash_serializable(&state)
    }

    pub fn prepare_for_persist(&mut self) {
        self.refresh_integrity();
        // Self-heal: if a caller mutated the ledger without going through
        // advance_revision, record the head as-is (same-revision entries are
        // replaced, so this never bumps state_revision).
        if self
            .revision_history
            .last()
            .is_none_or(|entry| !self.ledger_matches_head(entry))
        {
            self.record_ledger_revision(RunRevisionMutation::StateSynchronized, self.updated_at_ms);
        }
        self.integrity_hash = self.computed_integrity_hash();
    }

    pub fn refresh_integrity(&mut self) {
        self.integrity_hash.clear();
        if self.identity.run_id.is_empty() {
            self.identity.run_id = self.run_id.clone();
        }
        if self.identity.lineage_id.is_empty() {
            self.identity.lineage_id = self.run_id.clone();
        }
        if self.identity.origin_session_id.is_empty() {
            self.identity.origin_session_id = self.session_id.clone();
        }
        if self.identity.attached_session_id.is_empty() {
            self.identity.attached_session_id = self.session_id.clone();
        }
        self.identity.run_id = self.run_id.clone();
        self.session_id = self.identity.attached_session_id.clone();
        self.ledger_revision = self.ledger_revision.max(1);
        self.state_revision = self.state_revision.max(1);
        self.interview.revision = self.ledger_revision;
        self.requirements_hash = self.requirements_sha256();
        self.plan_hash = self
            .last_plan_draft
            .as_deref()
            .map(|plan| ultraplan_plan_hash_for_profile(self.profile, plan));
        self.seal_hash = self.interview.seal.as_ref().and_then(|seal| {
            (seal.revision == self.ledger_revision
                && seal.requirements_sha256 == self.requirements_hash)
                .then(|| hash_serializable(seal))
        });
        if let Some(context) = self.capability_context.as_mut() {
            context.run_id = self.run_id.clone();
            context.ledger_revision = self.ledger_revision;
            context.requirements_hash = self.requirements_hash.clone();
            context.session_id = self.identity.attached_session_id.clone();
            context.max_research_agents = self.budget.max_research_agents;
            context.research_agents_used = self.budget.research_agents_used;
            context.max_adversarial_reviews = self.budget.max_adversarial_reviews;
            context.adversarial_reviews_used = self.budget.adversarial_reviews_used;
            context.max_tool_error_retries = self.budget.max_tool_error_retries;
            context.refresh_hash();
            self.permission_snapshot = context.permission_snapshot();
        }
        self.permission_hash =
            hash_serializable(&canonical_permission_snapshot(&self.permission_snapshot));
    }

    fn advance_revision(&mut self, mutation: RunRevisionMutation) {
        let carry_seal = self.grill_understanding_is_sealed();
        let carried_confirmation = self.interview.confirmation.clone().filter(|confirmation| {
            carry_seal
                && self.grill_plan_review_satisfied(confirmation.plan_hash.as_str())
                && self.plan_hash.as_deref() == Some(confirmation.plan_hash.as_str())
        });
        let carried_release = if self.profile == UltraplanProfile::Grill {
            carried_confirmation
                .as_ref()
                .map(|confirmation| confirmation.plan_hash.clone())
        } else {
            self.released_plan_hash.clone().filter(|plan_hash| {
                self.auto_review_passed_hash.as_deref() == Some(plan_hash.as_str())
                    && self.plan_hash.as_deref() == Some(plan_hash.as_str())
                    && self.user_rejected_plan_hash.as_deref() != Some(plan_hash.as_str())
            })
        };
        self.state_revision = self.state_revision.max(1).saturating_add(1);
        if mutation.changes_ledger() {
            self.ledger_revision = self.ledger_revision.max(1).saturating_add(1);
        }
        self.interview.revision = self.ledger_revision;
        if carry_seal {
            if let Some(seal) = self.interview.seal.as_mut() {
                seal.revision = self.ledger_revision;
            }
        }
        match carried_confirmation {
            Some(mut confirmation) => {
                confirmation.revision = self.ledger_revision;
                self.interview.confirmation = Some(confirmation);
            }
            None => {
                self.interview.confirmation = None;
            }
        }
        self.released_plan_hash = carried_release;
        self.updated_at_ms = next_state_timestamp(self.updated_at_ms);
        self.refresh_integrity();
        if mutation.changes_ledger() {
            self.record_ledger_revision(mutation, self.updated_at_ms);
        }
    }

    fn invalidate_material_bindings(&mut self) {
        self.interview.seal = None;
        self.invalidate_plan_bindings();
        self.last_plan_draft = None;
        self.execution_cards.clear();
        self.plan_hash = None;
    }

    fn invalidate_plan_bindings(&mut self) {
        self.interview.confirmation = None;
        self.released_plan_hash = None;
        self.auto_review_passed_hash = None;
        self.manual_review_failed_hash = None;
        self.user_rejected_plan_hash = None;
        self.last_review_summary = None;
        self.last_coverage = None;
        self.structured_review = None;
        self.pending_requirements_patch = None;
        self.final_gate = None;
    }

    fn ledger_matches_head(&self, entry: &LedgerRevision) -> bool {
        entry.revision == self.ledger_revision
            && entry.requirements_hash == self.requirements_hash
            && entry.requirement_ledger == self.requirement_ledger
            && entry.interview == self.interview
            && entry.manifest == self.manifest
    }

    fn apply_ledger_revision(&mut self, entry: &LedgerRevision) {
        self.requirement_ledger = entry.requirement_ledger.clone();
        self.interview = entry.interview.clone();
        self.manifest = entry.manifest.clone();
        // asked_user_once is evidence-derived so a fork to baseline cannot
        // bypass the ask-at-least-once gate.
        self.asked_user_once = !self.interview.turns.is_empty();
    }

    fn record_ledger_revision(&mut self, mutation: RunRevisionMutation, created_at_ms: u64) {
        self.refresh_integrity();
        let entry = LedgerRevision {
            revision: self.ledger_revision,
            mutation,
            requirement_ledger: self.requirement_ledger.clone(),
            interview: self.interview.clone(),
            manifest: self.manifest.clone(),
            requirements_hash: self.requirements_hash.clone(),
            created_at_ms,
        };
        if self
            .revision_history
            .last()
            .is_some_and(|current| current.revision == self.ledger_revision)
        {
            if let Some(current) = self.revision_history.last_mut() {
                *current = entry;
            }
        } else {
            self.revision_history.push(entry);
        }
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.phase, RunPhase::Done | RunPhase::Abandoned)
    }
}

fn capabilities_equivalent(left: &CapabilityContext, right: &CapabilityContext) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    left.ledger_revision = 0;
    right.ledger_revision = 0;
    left.capability_hash.clear();
    right.capability_hash.clear();
    left == right
}

fn requirements_sha256_for(
    task: &str,
    requirements: &[RequirementLedgerEntry],
    interview_turns: &[UltraplanInterviewTurn],
) -> String {
    let canonical = serde_json::json!({
        "task": task,
        "requirements": requirements,
        "interview_turns": interview_turns,
    });
    let digest_input = serde_json::to_vec(&canonical).unwrap_or_default();
    format!("{:x}", Sha256::digest(digest_input))
}

fn validate_budget(budget: &UltraplanBudget) -> Result<(), String> {
    // plan_revisions_used is deliberately unchecked: it is a saturating
    // counter that may exceed its soft cap, and a load-time bound here would
    // brick the run on the third material revision.
    if budget.research_agents_used > budget.max_research_agents
        || budget.adversarial_reviews_used > budget.max_adversarial_reviews
    {
        return Err("ultraplan RunState budget usage exceeds its limits".into());
    }
    Ok(())
}

fn validate_seal(
    seal: Option<&UltraplanUnderstandingSeal>,
    ledger_revision: u64,
    requirements_hash: &str,
    stored_hash: Option<&str>,
) -> Result<(), String> {
    match seal {
        Some(seal)
            if seal.revision == ledger_revision
                && seal.requirements_sha256 == requirements_hash
                && stored_hash == Some(hash_serializable(seal).as_str()) =>
        {
            Ok(())
        }
        None if stored_hash.is_none() => Ok(()),
        _ => Err("ultraplan RunState understanding seal is inconsistent".into()),
    }
}

fn validate_capability_context(
    context: Option<&CapabilityContext>,
    run_id: &str,
    ledger_revision: u64,
    requirements_hash: &str,
    session_id: &str,
    budget: &UltraplanBudget,
    permission_snapshot: &PermissionSnapshot,
) -> Result<(), String> {
    let Some(context) = context else {
        return Ok(());
    };
    // The max_* limits feed the coordinator spawner's slot reservation and
    // retry circuit breaker, so they must match the budget exactly. The used
    // counters are mirrors that only need to be self-consistent — requiring
    // strict equality here made any missed sync invalidate every persisted
    // state.
    if !context.hash_is_valid()
        || context.run_id != run_id
        || context.ledger_revision != ledger_revision
        || context.requirements_hash != requirements_hash
        || context.session_id != session_id
        || context.max_research_agents != budget.max_research_agents
        || context.max_adversarial_reviews != budget.max_adversarial_reviews
        || context.max_tool_error_retries != budget.max_tool_error_retries
        || context.research_agents_used > context.max_research_agents
        || context.adversarial_reviews_used > context.max_adversarial_reviews
        || context.permission_snapshot() != *permission_snapshot
    {
        return Err("ultraplan RunState CapabilityContext is inconsistent".into());
    }
    Ok(())
}

fn canonical_permission_snapshot(snapshot: &PermissionSnapshot) -> PermissionSnapshot {
    let mut canonical = snapshot.clone();
    canonical.allowed_roots = canonical
        .allowed_roots
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    canonical.tool_ids = canonical
        .tool_ids
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if let NetworkCapability::ToolScoped(tool_ids) = &mut canonical.network {
        *tool_ids = std::mem::take(tool_ids)
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
    }
    canonical
}

fn hash_serializable<T: Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    format!("{:x}", Sha256::digest(bytes))
}

fn next_state_timestamp(previous: u64) -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    now.max(previous.saturating_add(1))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UltraplanInterviewState {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub turns: Vec<UltraplanInterviewTurn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seal: Option<UltraplanUnderstandingSeal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<UltraplanUnderstandingConfirmation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UltraplanInterviewTurn {
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_answer: Option<String>,
    pub answer: String,
    pub round: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UltraplanUnderstandingSeal {
    pub revision: u64,
    pub requirements_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UltraplanUnderstandingConfirmation {
    pub revision: u64,
    pub plan_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementLedgerEntry {
    pub id: String,
    pub title: String,
    pub source: RequirementSource,
    pub round_added: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub plan_hash: String,
    pub verdict: String,
    pub source: VerdictSource,
    #[serde(default)]
    pub blockers: Vec<String>,
    #[serde(default)]
    pub coverage: Vec<ReviewStepCoverage>,
}

impl ReviewSummary {
    pub fn is_pass(&self) -> bool {
        self.verdict.eq_ignore_ascii_case("PASS")
            && self.blockers.is_empty()
            && !self.coverage.is_empty()
            && self.coverage.iter().all(|item| item.ok)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewStepCoverage {
    pub step_id: String,
    pub ok: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionCard {
    pub step: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covers: Option<String>,
    #[serde(default)]
    pub files: Vec<String>,
    pub change: String,
    pub verify: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HashDriftKind {
    Changed,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashDriftRecord {
    pub path: String,
    pub stored_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_sha256: Option<String>,
    pub kind: HashDriftKind,
}

impl ExecutionCard {
    pub fn is_complete(&self) -> bool {
        !self.files.is_empty() && !self.change.trim().is_empty() && !self.verify.trim().is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementSource {
    Manifest,
    UserFeedback,
    Question,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewerVerdictRecord {
    pub round: u32,
    pub verdict: String,
    pub blocking_gaps: u32,
    pub source: VerdictSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictSource {
    Agent,
    PlanHunter,
    UserRejection,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    #[default]
    PlanModeActive,
    Orchestrating,
    Researching,
    Reviewing,
    Synthesizing,
    AwaitingPlanApproval,
    Executing,
    Done,
    Abandoned,
}

/// True for the self-referential `ULTRAPLAN_DRAFT_HASH:` marker line that the
/// ExitPlanMode gate asks the model to echo back into its own draft.
pub fn is_ultraplan_draft_hash_marker(line: &str) -> bool {
    line.trim_start().starts_with("ULTRAPLAN_DRAFT_HASH:")
}

pub fn ultraplan_execution_plan_payload(state: &UltraplanRunState) -> Option<String> {
    let plan = state.last_plan_draft.as_deref()?;
    let Some(gate) = state
        .final_gate
        .as_ref()
        .filter(|gate| gate.outcome == FinalGateOutcome::Degraded)
    else {
        return Some(plan.to_string());
    };
    let payload = serde_json::json!({
        "ultraplan_final_gate": {
            "outcome": "degraded",
            "run_id": state.run_id,
            "ledger_revision": state.ledger_revision,
            "requirements_hash": state.requirements_hash,
            "plan_hash": gate.plan_hash,
            "diagnostics": gate.diagnostics,
        }
    });
    let diagnostics =
        serde_json::to_string_pretty(&payload).expect("serde_json::Value always serializes");
    Some(format!(
        "{plan}\n\n## Ultraplan degraded-completion diagnostics\n\n```json\n{diagnostics}\n```"
    ))
}

pub fn ultraplan_plan_hash(plan: &str) -> String {
    let normalized = plan
        .lines()
        .map(str::trim_end)
        // The `ULTRAPLAN_DRAFT_HASH:` marker is self-referential: the gate asks
        // the model to echo the plan's own hash back into the draft. Hashing
        // that line would make the hash depend on itself — no fixed point
        // exists, so the model can never produce a draft whose embedded hash
        // matches its computed hash, and the gate livelocks. Exclude it so the
        // identity hash is a function of plan *content* only and stays stable
        // no matter which hash literal the model embeds.
        .filter(|line| !is_ultraplan_draft_hash_marker(line))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{:x}", Sha256::digest(normalized.as_bytes()))
}

pub fn ultraplan_plan_hash_for_profile(_profile: UltraplanProfile, plan: &str) -> String {
    ultraplan_plan_hash(plan)
}

pub fn decode_ultraplan_run_state(
    bytes: &[u8],
) -> Result<Option<UltraplanRunState>, serde_json::Error> {
    #[derive(Deserialize)]
    struct VersionOnly {
        version: u32,
    }
    let version = serde_json::from_slice::<VersionOnly>(bytes)?.version;
    if !matches!(
        version,
        ULTRAPLAN_RUN_STATE_VERSION
            | ULTRAPLAN_RUN_STATE_VERSION_V2
            | ULTRAPLAN_RUN_STATE_VERSION_V1
    ) {
        return Ok(None);
    }
    // V2 full-state snapshots and the removed `stage` field deserialize
    // cleanly into the v3 shapes because serde ignores unknown fields and all
    // legacy `RunRevisionMutation` variants are still declared.
    let mut state = serde_json::from_slice::<UltraplanRunState>(bytes)?;
    if version == ULTRAPLAN_RUN_STATE_VERSION {
        state
            .validate_persisted_integrity()
            .map_err(<serde_json::Error as serde::de::Error>::custom)?;
        return Ok(Some(state));
    }

    if version == ULTRAPLAN_RUN_STATE_VERSION_V2 {
        // No integrity check before migration: the v2 hash covered the v2
        // shape and can never match a v3 re-serialization. The migration
        // must not move `state_revision` either — CAS keys on it, and the
        // file may be re-migrated in place by several processes.
        state.version = ULTRAPLAN_RUN_STATE_VERSION;
        state.integrity_hash.clear();
        // v2 recorded one full snapshot per mutation, so a ledger revision
        // can appear multiple times; keep the last entry per revision.
        let mut deduped: Vec<LedgerRevision> = Vec::with_capacity(state.revision_history.len());
        for entry in state.revision_history.drain(..) {
            match deduped.last_mut() {
                Some(last) if last.revision == entry.revision => *last = entry,
                Some(last) if last.revision > entry.revision => {}
                _ => deduped.push(entry),
            }
        }
        state.revision_history = deduped;
        let keep_from = state.checkpoints.len().saturating_sub(8);
        state.checkpoints.drain(..keep_from);
        state.refresh_integrity();
        if state
            .revision_history
            .last()
            .is_none_or(|entry| !state.ledger_matches_head(entry))
        {
            state.record_ledger_revision(
                RunRevisionMutation::StateSynchronized,
                state.updated_at_ms,
            );
        }
        state.integrity_hash = state.computed_integrity_hash();
        return Ok(Some(state));
    }

    state.version = ULTRAPLAN_RUN_STATE_VERSION;
    state.integrity_hash.clear();
    if state.ledger_revision == 0 {
        state.ledger_revision = state.interview.revision.max(1);
    }
    if state.state_revision == 0 {
        state.state_revision = state.ledger_revision;
    }
    // The draft's coverage is not rebuilt. A V1 draft's `[COVERS:ID]` markers
    // were hand-written text, and rebuilding a coverage result out of them
    // presented "R1 is covered" as a finding when all it ever meant was that
    // somebody typed the marker. V2 coverage is the typed `step_coverage` the
    // plan tool submits; the migrated run reports none until the next plan is
    // analysed, which is what "not analysed yet" should look like.
    state.last_coverage = None;
    state.auto_review_passed_hash = None;
    state.released_plan_hash = None;
    state.manual_review_failed_hash = None;
    state.interview.confirmation = None;
    state.last_review_summary = None;
    state.revision_history.clear();
    state.refresh_integrity();
    state.record_ledger_revision(RunRevisionMutation::MigratedV1, state.updated_at_ms);
    state.prepare_for_persist();
    Ok(Some(state))
}

pub fn ledger_to_manifest_snapshot(
    ledger: &[RequirementLedgerEntry],
) -> Option<UltraplanManifestSnapshot> {
    if ledger.is_empty() {
        return None;
    }
    let canonical = ledger
        .iter()
        .map(|entry| {
            serde_json::json!({
                "id": entry.id,
                "title": entry.title,
                "source": entry.source,
                "round_added": entry.round_added,
            })
        })
        .collect::<Vec<_>>();
    let digest_input = serde_json::to_vec(&canonical).unwrap_or_default();
    let content_sha256 = format!("{:x}", Sha256::digest(&digest_input));
    Some(UltraplanManifestSnapshot {
        source_path: "ledger".into(),
        canonical_path: "ledger".into(),
        display_path: "Requirement ledger".into(),
        content_sha256,
        items: ledger
            .iter()
            .enumerate()
            .map(|(idx, entry)| UltraplanManifestItem {
                id: entry.id.clone(),
                title: entry.title.clone(),
                line: idx + 1,
                required: true,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_state_roundtrips_with_version_two() {
        let mut state = UltraplanRunState::new(
            "ultraplan-1-session-0".into(),
            "session-id".into(),
            "do the work".into(),
            None,
            10,
        )
        .with_profile(UltraplanProfile::Grill);
        state.last_coverage = Some(PlanCoverageResult {
            covered: vec!["R1".into()],
            missing: vec!["R2".into()],
            unknown_ids: vec!["X".into()],
        });
        state.requirement_ledger.push(RequirementLedgerEntry {
            id: "R1".into(),
            title: "Requirement".into(),
            source: RequirementSource::Manifest,
            round_added: 1,
        });
        state.asked_user_once = true;
        let hash = ultraplan_plan_hash("P1. Update src.rs");
        state.released_plan_hash = Some(hash.clone());
        state.auto_review_passed_hash = Some(hash.clone());
        state.last_review_summary = Some(ReviewSummary {
            plan_hash: hash,
            verdict: "PASS".into(),
            source: VerdictSource::Agent,
            blockers: Vec::new(),
            coverage: vec![ReviewStepCoverage {
                step_id: "P1".into(),
                ok: true,
                reason: "Evidence supports the step".into(),
            }],
        });
        state.prepare_for_persist();

        let bytes = serde_json::to_vec(&state).unwrap();
        let decoded = decode_ultraplan_run_state(&bytes).unwrap().unwrap();

        assert_eq!(decoded, state);
    }

    #[test]
    fn current_run_state_rejects_rehashed_identity_corruption() {
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1);
        state.prepare_for_persist();
        state.identity.run_id = "other".into();
        state.integrity_hash = state.computed_integrity_hash();

        let error = decode_ultraplan_run_state(&serde_json::to_vec(&state).unwrap()).unwrap_err();

        assert!(error.to_string().contains("identity"));
    }

    #[test]
    fn current_run_state_rejects_rehashed_capability_corruption() {
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1);
        let mut capability = CapabilityContext {
            run_id: state.run_id.clone(),
            ledger_revision: state.ledger_revision,
            requirements_hash: state.requirements_hash.clone(),
            session_id: state.session_id.clone(),
            cwd: "/repo".into(),
            allowed_roots: vec!["/repo".into()],
            read_allowed: true,
            write_allowed: false,
            shell_allowed: false,
            tool_ids: vec!["Read".into()],
            network: NetworkCapability::Denied,
            workspace_head: None,
            workspace_dirty: None,
            provider: None,
            model: None,
            sub_agent_available: true,
            max_research_agents: state.budget.max_research_agents,
            research_agents_used: state.budget.research_agents_used,
            max_adversarial_reviews: state.budget.max_adversarial_reviews,
            adversarial_reviews_used: state.budget.adversarial_reviews_used,
            max_tool_error_retries: state.budget.max_tool_error_retries,
            capability_hash: String::new(),
        };
        capability.refresh_hash();
        state.set_capability_context(capability);
        state.prepare_for_persist();
        state.capability_context.as_mut().unwrap().cwd = "/tampered".into();
        state.integrity_hash = state.computed_integrity_hash();

        let error = decode_ultraplan_run_state(&serde_json::to_vec(&state).unwrap()).unwrap_err();

        assert!(error.to_string().contains("CapabilityContext"));
    }

    #[test]
    fn current_run_state_rejects_rehashed_snapshot_corruption() {
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1);
        state.prepare_for_persist();
        state.revision_history[0].requirements_hash = "tampered".into();
        state.integrity_hash = state.computed_integrity_hash();

        let error = decode_ultraplan_run_state(&serde_json::to_vec(&state).unwrap()).unwrap_err();

        assert!(error.to_string().contains("revision"));
    }

    #[test]
    fn run_state_seeds_manifest_items_into_requirement_ledger() {
        let manifest = UltraplanManifestSnapshot {
            source_path: "requirements.md".into(),
            canonical_path: "/repo/requirements.md".into(),
            display_path: "requirements.md".into(),
            content_sha256: "abc".into(),
            items: vec![UltraplanManifestItem {
                id: "M1".into(),
                title: "manifest item".into(),
                line: 3,
                required: true,
            }],
        };
        let state = UltraplanRunState::new(
            "ultraplan-1-session-0".into(),
            "session-id".into(),
            "do the work".into(),
            Some(manifest),
            10,
        );

        assert_eq!(state.requirement_ledger.len(), 1);
        assert_eq!(state.requirement_ledger[0].id, "M1");
        assert_eq!(state.requirement_ledger[0].title, "manifest item");
        assert_eq!(
            state.requirement_ledger[0].source,
            RequirementSource::Manifest
        );
        assert_eq!(state.requirement_ledger[0].round_added, 1);
    }

    #[test]
    fn grill_interview_turns_and_ledger_changes_invalidate_then_seal_understanding() {
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 10)
                .with_profile(UltraplanProfile::Grill);
        state.requirement_ledger.push(RequirementLedgerEntry {
            id: "R1".into(),
            title: "Requirement".into(),
            source: RequirementSource::Question,
            round_added: 1,
        });
        state.auto_review_passed_hash = Some("old".into());
        state.released_plan_hash = Some("old".into());

        assert!(state.record_grill_interview_turn(
            "Which scope?".into(),
            Some("Narrow (Recommended)".into()),
            "Narrow (Recommended)".into(),
        ));
        assert_eq!(state.interview.revision, 2);
        assert_eq!(state.ledger_revision, 2);
        assert_eq!(state.interview.turns.len(), 1);
        assert!(state.auto_review_passed_hash.is_none());
        assert!(state.released_plan_hash.is_none());
        assert!(!state.grill_understanding_is_sealed());

        assert!(state.mark_grill_requirements_changed());
        assert_eq!(state.interview.revision, 3);
        let seal = state.seal_grill_understanding().expect("seal");
        assert_eq!(seal.revision, 3);
        assert_eq!(state.state_revision, 4);
        assert_eq!(seal.requirements_sha256, state.requirements_sha256());
        assert!(state.grill_understanding_is_sealed());
        assert_eq!(state.stage(), UltraplanStage::EvidenceVerify);

        state.requirement_ledger[0].title = "Changed".into();
        assert!(!state.grill_understanding_is_sealed());
    }

    #[test]
    fn grill_confirmation_binds_current_seal_and_reviewed_plan() {
        let plan = "P1. Ship it";
        let plan_hash = ultraplan_plan_hash_for_profile(UltraplanProfile::Grill, plan);
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 10)
                .with_profile(UltraplanProfile::Grill);
        state.requirement_ledger.push(RequirementLedgerEntry {
            id: "R1".into(),
            title: "Requirement".into(),
            source: RequirementSource::Question,
            round_added: 1,
        });
        state.record_grill_interview_turn(
            "Which scope?".into(),
            Some("Narrow".into()),
            "Narrow".into(),
        );
        state.seal_grill_understanding().unwrap();
        state.last_plan_draft = Some(plan.into());
        state.auto_review_passed_hash = Some(plan_hash.clone());

        let sealed_revision = state.ledger_revision;
        assert!(!state.confirm_grill_understanding(0, &plan_hash));
        assert!(state.confirm_grill_understanding(sealed_revision, &plan_hash));
        assert!(state.grill_confirmation_matches(&plan_hash));
        assert_eq!(
            state.released_plan_hash.as_deref(),
            Some(plan_hash.as_str())
        );

        state.invalidate_grill_plan_bindings();
        assert!(!state.grill_confirmation_matches(&plan_hash));
        assert!(state.grill_understanding_is_sealed());
    }

    #[test]
    fn review_summary_pass_requires_non_empty_coverage() {
        let summary = ReviewSummary {
            plan_hash: "hash".into(),
            verdict: "PASS".into(),
            source: VerdictSource::Agent,
            blockers: Vec::new(),
            coverage: Vec::new(),
        };

        assert!(!summary.is_pass());
    }

    #[test]
    fn standard_review_release_survives_state_only_revision() {
        let plan = "P1. Ship it";
        let plan_hash = ultraplan_plan_hash(plan);
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 10);
        state.set_plan_artifacts(
            plan.into(),
            PlanCoverageResult {
                covered: Vec::new(),
                missing: Vec::new(),
                unknown_ids: Vec::new(),
            },
            Vec::new(),
        );
        state.auto_review_passed_hash = Some(plan_hash.clone());
        state.released_plan_hash = Some(plan_hash.clone());

        state.write_checkpoint(Vec::new(), "run the final gate");

        assert_eq!(
            state.released_plan_hash.as_deref(),
            Some(plan_hash.as_str())
        );
    }

    #[test]
    fn standard_profile_does_not_record_grill_interview_state() {
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 10);

        assert!(!state.record_grill_interview_turn(
            "Question?".into(),
            Some("Answer".into()),
            "Answer".into(),
        ));
        assert!(!state.mark_grill_requirements_changed());
        assert!(state.seal_grill_understanding().is_none());
        assert_eq!(state.interview.revision, 1);
        assert!(state.interview.turns.is_empty());
        assert!(state.interview.seal.is_none());
        assert!(state.interview.confirmation.is_none());
    }

    #[test]
    fn requirement_revisions_can_expand_then_rollback_without_losing_history() {
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 10);
        let requirements = |end: u32| {
            (1..=end)
                .map(|index| RequirementLedgerEntry {
                    id: format!("R{index}"),
                    title: format!("Requirement {index}"),
                    source: RequirementSource::Question,
                    round_added: 1,
                })
                .collect::<Vec<_>>()
        };

        state.replace_requirements(requirements(12));
        let twelve_revision = state.ledger_revision;
        state.replace_requirements(requirements(17));
        let expanded_revision = state.ledger_revision;

        assert_eq!(state.requirement_ledger.len(), 17);
        assert!(state
            .revision_history
            .iter()
            .any(|snapshot| snapshot.revision == twelve_revision));
        state.rollback_to_revision(twelve_revision).unwrap();

        assert_eq!(state.requirement_ledger.len(), 12);
        assert!(state.ledger_revision > expanded_revision);
        assert!(matches!(
            state.revision_history.last().map(|snapshot| &snapshot.mutation),
            Some(RunRevisionMutation::Rollback { target_revision })
                if *target_revision == twelve_revision
        ));
    }

    #[test]
    fn reset_restores_manifest_baseline_and_fork_preserves_lineage() {
        let manifest = UltraplanManifestSnapshot {
            source_path: "requirements.md".into(),
            canonical_path: "/repo/requirements.md".into(),
            display_path: "requirements.md".into(),
            content_sha256: "baseline".into(),
            items: vec![UltraplanManifestItem {
                id: "R1".into(),
                title: "Baseline".into(),
                line: 1,
                required: true,
            }],
        };
        let mut state = UltraplanRunState::new(
            "run".into(),
            "session-a".into(),
            "task".into(),
            Some(manifest),
            10,
        );
        let baseline_revision = state.ledger_revision;
        state.requirement_ledger.push(RequirementLedgerEntry {
            id: "R2".into(),
            title: "Added".into(),
            source: RequirementSource::UserFeedback,
            round_added: 1,
        });
        state.mark_requirements_changed(RunRevisionMutation::RequirementsAdded);

        let fork = state
            .fork_from_revision(baseline_revision, "fork".into(), "session-b".into())
            .unwrap();
        assert_eq!(fork.requirement_ledger.len(), 1);
        assert_eq!(fork.identity.lineage_id, "run");
        assert_eq!(
            fork.identity.parent,
            Some(RunRevisionRef {
                run_id: "run".into(),
                ledger_revision: baseline_revision,
            })
        );
        assert_eq!(fork.identity.attached_session_id, "session-b");

        state.reset_to_baseline().unwrap();
        assert_eq!(state.requirement_ledger.len(), 1);
        assert_eq!(state.requirement_ledger[0].id, "R1");
    }

    #[test]
    fn non_material_revision_carries_current_seal_forward() {
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 10)
                .with_profile(UltraplanProfile::Grill);
        state.replace_requirements(vec![RequirementLedgerEntry {
            id: "R1".into(),
            title: "Requirement".into(),
            source: RequirementSource::Question,
            round_added: 1,
        }]);
        state.record_grill_interview_turn("Scope?".into(), None, "Narrow".into());
        state.seal_grill_understanding().unwrap();

        state.set_plan_artifacts(
            "P1. Work\n- files: src/a.rs\n- change: change\n- verify: test".into(),
            PlanCoverageResult {
                covered: vec!["R1".into()],
                missing: Vec::new(),
                unknown_ids: Vec::new(),
            },
            vec![ExecutionCard {
                step: "P1".into(),
                covers: Some("R1".into()),
                files: vec!["src/a.rs".into()],
                change: "change".into(),
                verify: "test".into(),
            }],
        );

        assert!(state.grill_understanding_is_sealed());
        assert_eq!(
            state.interview.seal.as_ref().map(|seal| seal.revision),
            Some(state.ledger_revision)
        );
    }

    #[test]
    fn derived_stage_follows_plan_review_and_rejection_bindings() {
        let plan = "P1. Do work";
        let plan_hash = ultraplan_plan_hash(plan);
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1);
        assert_eq!(state.stage(), UltraplanStage::ScopeConfirm);

        state.record_interview_turn("Scope?".into(), None, "Narrow".into());
        assert_eq!(state.stage(), UltraplanStage::EvidenceVerify);

        state.set_plan_artifacts(
            plan.into(),
            PlanCoverageResult {
                covered: Vec::new(),
                missing: Vec::new(),
                unknown_ids: Vec::new(),
            },
            Vec::new(),
        );
        assert_eq!(state.stage(), UltraplanStage::EvidenceVerify);

        state.pending_review_plan_hash = Some(plan_hash.clone());
        assert_eq!(state.stage(), UltraplanStage::AdversarialReview);

        state.pending_review_plan_hash = None;
        state.auto_review_passed_hash = Some(plan_hash.clone());
        assert_eq!(state.stage(), UltraplanStage::FinalGate);

        // A user rejection forces the derived stage back to drafting even
        // though the review artifacts stay recorded.
        state.user_rejected_plan_hash = Some(plan_hash.clone());
        assert_eq!(state.stage(), UltraplanStage::EvidenceVerify);
        state.user_rejected_plan_hash = None;

        // A pending review bound to a different plan does not hold the stage.
        state.auto_review_passed_hash = None;
        state.pending_review_plan_hash = Some("stale-hash".into());
        assert_eq!(state.stage(), UltraplanStage::EvidenceVerify);

        state.phase = RunPhase::Executing;
        assert_eq!(state.stage(), UltraplanStage::Completed);
        state.phase = RunPhase::Done;
        assert_eq!(state.stage(), UltraplanStage::Completed);
    }

    #[test]
    fn standard_budget_caps_agents_and_reviews_but_only_counts_plan_revisions() {
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1);
        let ledger_revision = state.ledger_revision;
        for _ in 0..state.budget.max_research_agents {
            state.consume_research_agent().unwrap();
        }
        assert!(matches!(
            state.consume_research_agent(),
            Err(UltraplanStateError::BudgetExhausted { resource, limit: 6 })
                if resource == "research agent"
        ));
        state.consume_adversarial_review().unwrap();
        assert!(state.consume_adversarial_review().is_err());

        // Material plan revisions are a saturating counter, never a hard
        // error: the third revision must not strand the run.
        for index in 1..=4 {
            assert!(state.set_plan_artifacts(
                format!("P1. Version {index}"),
                PlanCoverageResult {
                    covered: Vec::new(),
                    missing: Vec::new(),
                    unknown_ids: Vec::new(),
                },
                Vec::new(),
            ));
        }
        assert_eq!(state.budget.plan_revisions_used, 3);
        assert_eq!(state.ledger_revision, ledger_revision);
        assert!(state.state_revision > ledger_revision);
    }

    #[test]
    fn rollback_restores_the_ledger_but_keeps_budget_and_diagnostics() {
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1);
        state.replace_requirements(vec![RequirementLedgerEntry {
            id: "R1".into(),
            title: "Baseline".into(),
            source: RequirementSource::Question,
            round_added: 1,
        }]);
        let target_revision = state.ledger_revision;
        state.replace_requirements(vec![RequirementLedgerEntry {
            id: "R2".into(),
            title: "Replacement".into(),
            source: RequirementSource::UserFeedback,
            round_added: 1,
        }]);
        state.consume_research_agent().unwrap();
        state.record_diagnostic(UltraplanDiagnostic {
            class: UltraplanDiagnosticClass::ReviewerMalformed,
            message: "bad output".into(),
            ledger_revision: 0,
            stage: UltraplanStage::EvidenceVerify,
            plan_hash: None,
            details: serde_json::Value::Null,
        });

        state.rollback_to_revision(target_revision).unwrap();

        assert_eq!(state.requirement_ledger.len(), 1);
        assert_eq!(state.requirement_ledger[0].id, "R1");
        // Budget stays monotonic and diagnostics stay recorded: rollback is
        // a requirements operation, not a way to refresh consumed reviews.
        assert_eq!(state.budget.research_agents_used, 1);
        assert_eq!(state.diagnostics.len(), 1);
        assert!(state.plan_hash.is_none());
        assert!(state.ledger_revision > target_revision);
    }

    #[test]
    fn advisory_only_structured_review_is_non_blocking() {
        let review = StructuredReview {
            plan_hash: "plan".into(),
            base_revision: 1,
            verdict: "FAIL".into(),
            findings: vec![ReviewerFinding {
                classification: ReviewerFindingClass::Advisory,
                code: "style".into(),
                message: "Optional cleanup".into(),
                evidence: Vec::new(),
            }],
            step_coverage: vec![ReviewStepCoverage {
                step_id: "P1".into(),
                ok: true,
                reason: "covered".into(),
            }],
            requirements_patch: None,
        };

        assert!(review.is_pass());
    }

    #[test]
    fn pending_requirements_patch_rejects_a_stale_active_ledger() {
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1);
        state.record_structured_review(StructuredReview {
            plan_hash: "plan".into(),
            base_revision: 1,
            verdict: "FAIL".into(),
            findings: vec![ReviewerFinding {
                classification: ReviewerFindingClass::Blocker,
                code: "missing".into(),
                message: "Missing acceptance criterion".into(),
                evidence: Vec::new(),
            }],
            step_coverage: vec![ReviewStepCoverage {
                step_id: "P1".into(),
                ok: false,
                reason: "Missing acceptance criterion".into(),
            }],
            requirements_patch: Some(RequirementsPatch {
                base_revision: 1,
                items: vec![RequirementPatchItem {
                    id: "R2".into(),
                    title: "New criterion".into(),
                    reason: "Reviewer blocker".into(),
                }],
            }),
        });
        state.ledger_revision = 2;

        assert!(matches!(
            state.apply_pending_requirements_patch(),
            Err(UltraplanStateError::StaleRevision {
                expected: 1,
                actual: 2
            })
        ));
        assert!(state.requirement_ledger.is_empty());
    }

    #[test]
    fn checkpoint_records_current_decisions_hashes_and_budget() {
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1);
        state.consume_research_agent().unwrap();
        state.write_checkpoint(
            vec![UltraplanEvidenceReference {
                location: "src/lib.rs:10".into(),
                claim: "state owner".into(),
            }],
            "review the draft",
        );

        let checkpoint = state.latest_checkpoint().unwrap();
        assert_eq!(checkpoint.ledger_revision, state.ledger_revision);
        assert_eq!(checkpoint.budget.research_agents_used, 1);
        assert_eq!(checkpoint.evidence[0].location, "src/lib.rs:10");
        assert_eq!(checkpoint.next_action, "review the draft");
    }

    /// A migrated V1 run reports no coverage and no standing approvals.
    ///
    /// The `[COVERS:R1]` marker in the draft below is exactly what is no
    /// longer read: it was text somebody typed, and turning it back into a
    /// coverage result made "R1 covered, R2 missing" look like an analysis.
    #[test]
    fn version_one_migration_drops_marker_coverage_and_invalidates_approvals() {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "run_id": "r",
            "session_id": "s",
            "task": "t",
            "phase": "reviewing",
            "round": 1,
            "requirement_ledger": [
                {"id":"R1","title":"First","source":"question","round_added":1},
                {"id":"R2","title":"Second","source":"question","round_added":1}
            ],
            "last_plan_draft": "P1. Work [COVERS:R1]\n- files: src/a.rs\n- change: change\n- verify: test",
            "auto_review_passed_hash": "old-pass",
            "released_plan_hash": "old-release",
            "last_review_summary": {
                "plan_hash": "old-pass",
                "verdict": "PASS",
                "source": "agent",
                "blockers": [],
                "coverage": [{"step_id":"P1","ok":true,"reason":"covered"}]
            },
            "started_at_ms": 1,
            "updated_at_ms": 2
        }))
        .unwrap();

        let state = decode_ultraplan_run_state(&bytes).unwrap().unwrap();

        assert!(
            state.last_coverage.is_none(),
            "a hand-written marker must not come back as a coverage result: {:?}",
            state.last_coverage
        );
        assert!(state.auto_review_passed_hash.is_none());
        assert!(state.released_plan_hash.is_none());
        assert!(state.last_review_summary.is_none());
        assert!(matches!(
            state
                .revision_history
                .last()
                .map(|snapshot| &snapshot.mutation),
            Some(RunRevisionMutation::MigratedV1)
        ));
    }

    #[test]
    fn capability_fingerprint_distinguishes_failure_details() {
        let diagnostic = CapabilityDiagnostic {
            class: CapabilityDiagnosticClass::ToolUnavailable,
            message: "missing Read".into(),
            run_id: "run".into(),
            ledger_revision: 1,
            capability_hash: "cap".into(),
            role: "reviewer".into(),
            root: None,
            capability: Some("Read".into()),
            retryable: true,
            fallback_to_parent: true,
        };
        let mut other = diagnostic.clone();
        other.capability = Some("Grep".into());
        other.message = "missing Grep".into();

        assert_ne!(diagnostic.fingerprint(), other.fingerprint());
    }

    #[test]
    fn tool_error_attempts_are_versioned_and_clearable_by_scope() {
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1);
        let initial_state_revision = state.state_revision;

        assert_eq!(
            state.record_tool_error_attempts("run:cap:role:a".into(), 2),
            2
        );
        state.record_tool_error_attempts("run:other:role:b".into(), 1);
        assert!(state.state_revision > initial_state_revision);
        assert!(state.clear_tool_error_attempts("run:cap:role:"));
        assert_eq!(state.tool_error_attempt_count("run:cap:role:a"), 0);
        assert_eq!(state.tool_error_attempt_count("run:other:role:b"), 1);
    }

    #[test]
    fn capability_context_is_canonical_and_tracks_the_run_head() {
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 10);
        let mut capability = CapabilityContext {
            run_id: "run".into(),
            ledger_revision: 1,
            requirements_hash: state.requirements_hash.clone(),
            session_id: "session".into(),
            cwd: "/repo".into(),
            allowed_roots: vec!["/repo/b".into(), "/repo/a".into(), "/repo/a".into()],
            read_allowed: true,
            write_allowed: false,
            shell_allowed: false,
            tool_ids: vec!["Read".into(), "Glob".into(), "Read".into()],
            network: NetworkCapability::Denied,
            workspace_head: Some("head".into()),
            workspace_dirty: Some(true),
            provider: Some("provider".into()),
            model: Some("model".into()),
            sub_agent_available: true,
            max_research_agents: 6,
            research_agents_used: 0,
            max_adversarial_reviews: 1,
            adversarial_reviews_used: 0,
            max_tool_error_retries: 1,
            capability_hash: String::new(),
        };
        capability.refresh_hash();
        let canonical_hash = capability.capability_hash.clone();
        capability.allowed_roots.reverse();
        capability.tool_ids.reverse();
        capability.refresh_hash();
        assert_eq!(capability.capability_hash, canonical_hash);

        state.set_capability_context(capability);
        let stored = state.capability_context.as_ref().unwrap();
        assert_eq!(stored.ledger_revision, state.ledger_revision);
        assert_eq!(stored.requirements_hash, state.requirements_hash);
        assert!(stored.hash_is_valid());
        assert_eq!(
            state.permission_snapshot.allowed_roots,
            stored.allowed_roots
        );
        let old_hash = stored.capability_hash.clone();

        state.replace_requirements(vec![RequirementLedgerEntry {
            id: "R1".into(),
            title: "Requirement".into(),
            source: RequirementSource::Question,
            round_added: 1,
        }]);
        let stored = state.capability_context.as_ref().unwrap();
        assert_eq!(stored.ledger_revision, state.ledger_revision);
        assert_ne!(stored.capability_hash, old_hash);
        assert!(stored.hash_is_valid());
    }

    #[test]
    fn unknown_run_state_version_decodes_to_none() {
        let bytes = br#"{"version":999,"run_id":"x"}"#;

        let decoded = decode_ultraplan_run_state(bytes).unwrap();

        assert!(decoded.is_none());
    }

    #[test]
    fn legacy_run_state_defaults_new_gate_fields() {
        let bytes = br#"{"version":1,"run_id":"r","session_id":"s","task":"t","phase":"researching","round":1,"started_at_ms":1,"updated_at_ms":2}"#;

        let decoded = decode_ultraplan_run_state(bytes).unwrap().unwrap();

        assert_eq!(decoded.profile, UltraplanProfile::Standard);
        assert_eq!(decoded.version, ULTRAPLAN_RUN_STATE_VERSION);
        assert_eq!(decoded.ledger_revision, 1);
        assert_eq!(decoded.interview.revision, 1);
        assert_eq!(decoded.identity.run_id, "r");
        assert_eq!(decoded.identity.attached_session_id, "s");
        assert!(decoded.user_rejected_plan_hash.is_none());
        assert!(!decoded.asked_user_once);
        assert!(decoded.released_plan_hash.is_none());
        assert!(decoded.auto_review_passed_hash.is_none());
        assert!(decoded.manual_review_failed_hash.is_none());
        assert!(decoded.last_review_summary.is_none());
    }

    #[test]
    fn version_two_migration_dedupes_history_and_keeps_state_revision() {
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1);
        state.record_interview_turn("Scope?".into(), None, "Narrow".into());
        state.prepare_for_persist();
        let state_revision = state.state_revision;
        let ledger_revision = state.ledger_revision;
        let mut value = serde_json::to_value(&state).unwrap();
        value["version"] = serde_json::json!(2);
        // v2 persisted a stored stage field and one full-state snapshot per
        // mutation, including state-only mutations at the same ledger
        // revision with legacy mutation kinds.
        value["stage"] = serde_json::json!("evidence_verify");
        let history = value["revision_history"].as_array().cloned().unwrap();
        let mut duplicated = history.clone();
        let mut extra = history.last().cloned().unwrap();
        extra["mutation"] = serde_json::json!({"kind": "stage_advanced"});
        extra["state_revision"] = serde_json::json!(state_revision + 7);
        duplicated.push(extra);
        value["revision_history"] = serde_json::Value::Array(duplicated);
        let bytes = serde_json::to_vec(&value).unwrap();

        let migrated = decode_ultraplan_run_state(&bytes).unwrap().unwrap();

        assert_eq!(migrated.version, ULTRAPLAN_RUN_STATE_VERSION);
        assert_eq!(migrated.state_revision, state_revision);
        assert_eq!(migrated.ledger_revision, ledger_revision);
        assert_eq!(migrated.revision_history.len(), 2);
        assert!(matches!(
            migrated
                .revision_history
                .last()
                .map(|entry| &entry.mutation),
            Some(RunRevisionMutation::StageAdvanced)
        ));
        migrated.validate_persisted_integrity().unwrap();
    }

    #[test]
    fn degraded_execution_payload_is_canonical_and_bound_to_the_gate() {
        let plan = "P1. Ship it";
        let mut state =
            UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1);
        state.set_plan_artifacts(
            plan.into(),
            PlanCoverageResult {
                covered: Vec::new(),
                missing: Vec::new(),
                unknown_ids: Vec::new(),
            },
            Vec::new(),
        );
        state.record_final_gate(
            FinalGateOutcome::Degraded,
            state.plan_hash.clone().unwrap(),
            vec![UltraplanDiagnostic {
                class: UltraplanDiagnosticClass::ReviewerUnavailable,
                message: "reviewer unavailable".into(),
                ledger_revision: state.ledger_revision,
                stage: UltraplanStage::FinalGate,
                plan_hash: state.plan_hash.clone(),
                details: serde_json::json!({"attempts": 2}),
            }],
        );
        assert_eq!(state.stage(), UltraplanStage::FinalGate);

        let payload = ultraplan_execution_plan_payload(&state).unwrap();

        assert!(payload.starts_with(plan));
        assert!(payload.contains("Ultraplan degraded-completion diagnostics"));
        assert!(payload.contains("\"plan_hash\""));
        assert!(payload.contains("\"reviewer_unavailable\""));
    }

    #[test]
    fn plan_hash_is_stable_across_trailing_whitespace() {
        assert_eq!(ultraplan_plan_hash("A  \nB"), ultraplan_plan_hash("A\nB"));
        assert_ne!(ultraplan_plan_hash("A\nB"), ultraplan_plan_hash("A\nC"));
    }

    #[test]
    fn plan_hash_ignores_self_referential_draft_hash_marker() {
        // The embedded marker line must not affect identity: a placeholder, the
        // real hash, or a stale hash all hash to the same value as no marker.
        let base = "## Plan\nP1. Do work";
        let bare = ultraplan_plan_hash(base);
        let pending = ultraplan_plan_hash(&format!("ULTRAPLAN_DRAFT_HASH: REVIEW-PENDING\n{base}"));
        let echoed = ultraplan_plan_hash(&format!("ULTRAPLAN_DRAFT_HASH: {bare}\n{base}"));
        let stale = ultraplan_plan_hash(&format!("ULTRAPLAN_DRAFT_HASH: deadbeef\n{base}"));
        assert_eq!(bare, pending);
        assert_eq!(bare, echoed);
        assert_eq!(bare, stale);
        // Real plan content still changes the hash.
        assert_ne!(bare, ultraplan_plan_hash(&format!("{base}\nP2. More")));
    }

    #[test]
    fn grill_plan_hash_also_ignores_legacy_draft_hash_marker_lines() {
        let base = "## Plan\nP1. Do work";
        let bare = ultraplan_plan_hash_for_profile(UltraplanProfile::Grill, base);
        let marked = ultraplan_plan_hash_for_profile(
            UltraplanProfile::Grill,
            &format!("ULTRAPLAN_DRAFT_HASH: arbitrary\n{base}"),
        );

        assert_eq!(bare, marked);
        assert_eq!(
            ultraplan_plan_hash_for_profile(UltraplanProfile::Standard, base),
            ultraplan_plan_hash(base)
        );
    }

    #[test]
    fn review_summary_pass_requires_no_blockers_and_ok_coverage() {
        let mut summary = ReviewSummary {
            plan_hash: ultraplan_plan_hash("plan"),
            verdict: "PASS".into(),
            source: VerdictSource::Agent,
            blockers: Vec::new(),
            coverage: vec![ReviewStepCoverage {
                step_id: "P1".into(),
                ok: true,
                reason: "covered".into(),
            }],
        };
        assert!(summary.is_pass());

        summary.coverage[0].ok = false;
        assert!(!summary.is_pass());
        summary.coverage[0].ok = true;
        summary.blockers.push("missing test".into());
        assert!(!summary.is_pass());
    }

    #[test]
    fn ledger_manifest_hash_changes_with_content() {
        let left = ledger_to_manifest_snapshot(&[RequirementLedgerEntry {
            id: "R1".into(),
            title: "First".into(),
            source: RequirementSource::Question,
            round_added: 1,
        }])
        .unwrap();
        let right = ledger_to_manifest_snapshot(&[RequirementLedgerEntry {
            id: "R2".into(),
            title: "Second".into(),
            source: RequirementSource::UserFeedback,
            round_added: 2,
        }])
        .unwrap();

        assert_eq!(left.content_sha256.len(), 64);
        assert_ne!(left.content_sha256, right.content_sha256);
    }
}
