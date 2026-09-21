//! The job record on disk, and everything that describes one.
//!
//! This is the half of the crate whose shape other builds of Rebon read out of
//! `~/.rebon`, so every field name here is a compatibility name.
//! `wire_contract_tests` pins the serialized form byte for byte.

use std::collections::{HashMap, HashSet};

use crate::*;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundRuntimeFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub development_channels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "rebon_types::AgentCapabilityMode::is_normal"
    )]
    pub capability_mode: rebon_types::AgentCapabilityMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add_dirs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugin_dirs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_configs: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub strict_mcp_config: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum BackgroundJobStatus {
    Queued,
    Running,
    NeedsInput,
    Idle,
    Succeeded,
    Failed,
    Stopped,
}

impl BackgroundJobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::NeedsInput => "needs_input",
            Self::Idle => "idle",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingPrompt {
    pub id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<BackgroundImageAttachment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coordinator_report_paths: Vec<String>,
    pub enqueued_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_turn_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_turn_generation: Option<u64>,
}

impl PendingPrompt {
    pub fn new(
        id: String,
        text: String,
        images: Vec<BackgroundImageAttachment>,
        enqueued_at_ms: u64,
    ) -> anyhow::Result<Self> {
        let text = text.trim().to_string();
        if text.is_empty() {
            anyhow::bail!("background reply is empty");
        }
        validate_pending_prompt_id(&id)?;
        Ok(Self {
            id,
            text,
            images,
            coordinator_report_paths: Vec::new(),
            enqueued_at_ms,
            claimed_turn_generation: None,
            completed_turn_generation: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPromptAcceptance {
    pub prompt: PendingPrompt,
    pub appended: bool,
    pub queue_depth: usize,
}

fn deserialize_pending_prompts<'de, D>(deserializer: D) -> Result<Vec<PendingPrompt>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Array(items) => Ok(items
            .into_iter()
            .enumerate()
            .filter_map(|(index, item)| recover_pending_prompt_item(item, index))
            .collect()),
        serde_json::Value::Null => Ok(Vec::new()),
        other => Err(serde::de::Error::custom(format!(
            "pendingPrompts must be an array or null, not {other}"
        ))),
    }
}

fn recover_pending_prompt_item(value: serde_json::Value, index: usize) -> Option<PendingPrompt> {
    let mut object = match value {
        serde_json::Value::Object(object) => object,
        serde_json::Value::String(text) => {
            tracing::warn!(
                pending_prompt_index = index,
                "recovering string item in pendingPrompts"
            );
            return recover_pending_prompt_text(text, index);
        }
        _ => {
            tracing::warn!(
                pending_prompt_index = index,
                "skipping pendingPrompts item without recoverable text"
            );
            return None;
        }
    };

    let text = match object.remove("text") {
        Some(serde_json::Value::String(text)) if !text.trim().is_empty() => text,
        _ => {
            tracing::warn!(
                pending_prompt_index = index,
                "skipping pendingPrompts item without recoverable text"
            );
            return None;
        }
    };
    let id = match object.remove("id") {
        Some(serde_json::Value::String(id)) => id,
        Some(_) => {
            tracing::warn!(
                pending_prompt_index = index,
                "pending prompt id has an invalid type; it will be repaired"
            );
            String::new()
        }
        None => String::new(),
    };
    let images = match object.remove("images") {
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .enumerate()
            .filter_map(|(image_index, value)| {
                serde_json::from_value(value)
                    .map_err(|error| {
                        tracing::warn!(
                            pending_prompt_index = index,
                            pending_image_index = image_index,
                            %error,
                            "skipping malformed pending prompt image"
                        );
                    })
                    .ok()
            })
            .collect(),
        Some(_) => {
            tracing::warn!(
                pending_prompt_index = index,
                "pending prompt images must be an array; dropping malformed value"
            );
            Vec::new()
        }
        None => Vec::new(),
    };
    let coordinator_report_paths = match object.remove("coordinatorReportPaths") {
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .enumerate()
            .filter_map(|(path_index, value)| match value {
                serde_json::Value::String(path) if !path.trim().is_empty() => Some(path),
                _ => {
                    tracing::warn!(
                        pending_prompt_index = index,
                        pending_report_path_index = path_index,
                        "skipping malformed pending prompt coordinator report path"
                    );
                    None
                }
            })
            .collect(),
        Some(_) => {
            tracing::warn!(
                pending_prompt_index = index,
                "pending prompt coordinator report paths must be an array; dropping malformed value"
            );
            Vec::new()
        }
        None => Vec::new(),
    };
    let enqueued_at_ms = match object.remove("enqueuedAtMs") {
        Some(serde_json::Value::Number(value)) => value.as_u64().unwrap_or_else(|| {
            tracing::warn!(
                pending_prompt_index = index,
                "pending prompt enqueue timestamp is invalid; it will be repaired"
            );
            0
        }),
        Some(_) => {
            tracing::warn!(
                pending_prompt_index = index,
                "pending prompt enqueue timestamp is invalid; it will be repaired"
            );
            0
        }
        None => 0,
    };
    let claimed_turn_generation = match object.remove("claimedTurnGeneration") {
        Some(serde_json::Value::Number(value)) => value.as_u64().or_else(|| {
            tracing::warn!(
                pending_prompt_index = index,
                "pending prompt claim is invalid; dropping it"
            );
            None
        }),
        Some(serde_json::Value::Null) | None => None,
        Some(_) => {
            tracing::warn!(
                pending_prompt_index = index,
                "pending prompt claim is invalid; dropping it"
            );
            None
        }
    };
    let completed_turn_generation = match object.remove("completedTurnGeneration") {
        Some(serde_json::Value::Number(value)) => value.as_u64().or_else(|| {
            tracing::warn!(
                pending_prompt_index = index,
                "pending prompt completion generation is invalid; dropping it"
            );
            None
        }),
        Some(serde_json::Value::Null) | None => None,
        Some(_) => {
            tracing::warn!(
                pending_prompt_index = index,
                "pending prompt completion generation is invalid; dropping it"
            );
            None
        }
    };

    Some(PendingPrompt {
        id,
        text,
        images,
        coordinator_report_paths,
        enqueued_at_ms,
        claimed_turn_generation,
        completed_turn_generation,
    })
}

fn recover_pending_prompt_text(text: String, index: usize) -> Option<PendingPrompt> {
    if text.trim().is_empty() {
        tracing::warn!(
            pending_prompt_index = index,
            "skipping pendingPrompts item without recoverable text"
        );
        return None;
    }
    Some(PendingPrompt {
        id: String::new(),
        text,
        images: Vec::new(),
        coordinator_report_paths: Vec::new(),
        enqueued_at_ms: 0,
        claimed_turn_generation: None,
        completed_turn_generation: None,
    })
}

pub(crate) fn validate_pending_prompt_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty()
        || id.len() > 160
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '.'))
    {
        anyhow::bail!("invalid pending prompt id `{id}`");
    }
    Ok(())
}

/// A request the model client is retrying, while it is retrying it.
///
/// Published because a turn that is waiting out a rate limit looks exactly like
/// a turn that has hung: the spinner turns and nothing arrives. A client that
/// can say "Retry 2/10" turns a mystery into a wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundRetryProgress {
    /// 1-based attempt now being made.
    pub attempt: u32,
    /// Total attempts this request gets, the first one included.
    pub max_retries: u32,
}

impl std::fmt::Display for BackgroundRetryProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Retry {}/{}", self.attempt, self.max_retries)
    }
}

/// Who this job is and what it was asked to do.
///
/// Written once at creation and then only by the prompt transactions: a job's
/// identity does not change because a worker started, and its queue does not
/// change because a lease expired. Everything a launcher decides lives here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobIdentityIntent {
    pub job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The job this one was started from, when it was started from a session
    /// that was already running in a worker.
    ///
    /// Ownership, not bookkeeping: what a worker starts belongs to it and is
    /// released with it. Agents that run *inside* a worker get that for free
    /// — they die with the process — and this field extends the same rule to
    /// the ones that were given a worker of their own. Clearing it (an
    /// explicit `/bg` from inside the child) is how one opts out and becomes
    /// independent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub respawned_job_id: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    pub cwd: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_images: Vec<BackgroundImageAttachment>,
    #[serde(
        default,
        deserialize_with = "deserialize_pending_prompts",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub pending_prompts: Vec<PendingPrompt>,
    /// Every worker report path this job's coordinator has been granted,
    /// kept for the life of the job. A pending prompt is drained once it
    /// is claimed, so deriving the Read allowlist from the queue alone
    /// revoked each report the moment the turn that announced it ended —
    /// and a coordinator routinely re-reads an earlier worker's findings
    /// several turns later.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coordinator_report_grants: Vec<String>,
    pub runtime: BackgroundRuntimeFields,
    #[serde(default, skip_serializing_if = "is_false")]
    pub resume_only: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub queue_session: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub sort_order: i64,
}

/// Who is running this job right now, and how to reach them.
///
/// The block the fences live in. Every field here is part of one answer — this
/// pid, with this identity, listening on this endpoint, at this generation —
/// and they have to move together or a stale client reaches a replacement
/// owner. [`BackgroundJobState::recorded_owner`] is that whole answer as one
/// value, which is why ownership transitions go through it rather than
/// assigning fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessOwnership {
    pub status: BackgroundJobStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_identity: Option<String>,
    /// Whether `pid` is a process **Rebon spawned into a session of its
    /// own** (`detach_background_command`), and therefore leads a process
    /// group that contains only its own children.
    ///
    /// This is the provenance that licenses signalling the whole group when
    /// the job is stopped. A recorded identity is not that proof: a session
    /// running in-process records the *interactive* pid here, and a
    /// terminal-launched Rebon is normally its shell job's group leader — so
    /// inferring the group from "leads its own group" would signal the
    /// user's pipeline (`rebon | tee`, `time rebon`, …). Absent or false,
    /// only the single pid is signalled. Reset wherever a pid is recorded
    /// for an in-process owner, because a job can be adopted back from a
    /// worker.
    #[serde(default, skip_serializing_if = "is_false")]
    pub owner_detached_group: bool,
    /// The supervisor has atomically admitted a spawn but has not yet published
    /// the child owner. While set, no other supervisor may spawn and Stop waits
    /// for the admission to resolve instead of declaring a PID-less job stopped.
    #[serde(default, skip_serializing_if = "is_false")]
    pub spawn_admitted: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub process_owner_fenced: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub removal_reserved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipc_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipc_token: Option<String>,
    #[serde(default)]
    pub turn_generation: u64,
    #[doc(hidden)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_path: Option<String>,
    /// The Node runtime the plugin plane must use, captured from
    /// [`rebon_node_runtime::NODE_EXECUTABLE_ENV`] when the job was created.
    ///
    /// A worker is a separate process, and the one that created the job may be
    /// a desktop app whose `PATH` the worker does not share — the same reason
    /// `process_path` exists. Recording the resolved runtime keeps a session
    /// running on the runtime the app vetted instead of whatever the worker
    /// would find on its own. Absent when nobody had resolved one, in which
    /// case the worker walks the resolution ladder itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_runtime_path: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
}

/// Where the job's files are, and whether that is negotiable.
///
/// Separate from the identity block because isolation is a contract the
/// worktree transactions keep, not a property of the prompt: a job that asked
/// for a worktree and could not get one fails rather than quietly running
/// against the origin checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceIsolation {
    #[serde(default, skip_serializing_if = "is_false")]
    pub isolate_in_worktree: bool,
    /// Isolation is a contract for this job, not best effort: a worktree that
    /// cannot be created or reopened fails the turn instead of silently
    /// running the model against the origin checkout.
    #[serde(default, skip_serializing_if = "is_false")]
    pub require_worktree: bool,
    /// A successful turn keeps the worktree + branch instead of committing,
    /// merging and removing them. The launcher owns the result from there.
    #[serde(default, skip_serializing_if = "is_false")]
    pub preserve_worktree_on_success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
}

/// Who is holding this session open, and how long its host waits once
/// nobody is.
///
/// Only the lease state machine writes here. An endpoint that set
/// `client_leases` or `linger_ms` itself would be deciding the owner's
/// lifetime from outside the owner, which is the failure this block exists to
/// make impossible to write by accident.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseLifecycle {
    /// Whether a client is sitting in front of this job. Decides how long its
    /// host lingers once nobody is.
    #[serde(default, skip_serializing_if = "JobPlacement::is_background")]
    pub placement: JobPlacement,
    /// The clients currently holding this session open. While any lease is
    /// live the owner stays up; when the last one expires it lingers and then
    /// exits. Empty for a job nobody is watching — including every job created
    /// before leases existed, which is why it defaults rather than being
    /// required.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_leases: Vec<ClientLease>,
    /// How long the owner stays up after the last lease expires. `None` uses
    /// the placement's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linger_ms: Option<u64>,
    /// The last client left on purpose, so there is nobody to wait for.
    ///
    /// Set when a deliberate `ReleaseLease` takes the last lease off a
    /// `Foreground` job, cleared the moment anybody takes a lease again. It
    /// zeroes the linger for that one idle stretch without touching
    /// [`Self::linger_ms`], which is configuration: a job may legitimately
    /// name its own linger, and a shutdown signal that borrowed that field
    /// would erase the setting it borrowed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exit_when_idle: bool,
}

/// What happened, and what it cost.
///
/// Written by the owner's event and finalizer transactions. A client reads
/// these; it never writes them, because it is not the process that saw the
/// turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutcomeUsage {
    /// Set while the model client is retrying, cleared when it stops.
    ///
    /// Optional and skipped when absent, so a state file written by an older
    /// build reads fine and one written by this build is read fine by an older
    /// client — it simply shows the spinner it always showed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<BackgroundRetryProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_updated_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pull_requests: Vec<BackgroundPullRequestStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_permission: Option<BackgroundPermissionQuerySnapshot>,
    #[serde(default)]
    pub event_count: u64,
    /// Token spend so far, accumulated across the session's turns.
    ///
    /// Published by the owner because it is the only process that sees a
    /// turn's usage; a client that tried to derive it from the transcript
    /// would be re-implementing the accounting and getting cache reads wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<SessionUsageSnapshot>,
}

/// One job's whole record.
///
/// Five blocks rather than 49 loose fields. The split is a Rust
/// one only: `#[serde(flatten)]` keeps the JSON exactly as flat as it was, so
/// a record written by an older build reads here and a record written here
/// reads there. `wire_contract_tests` pins that byte for byte.
///
/// The blocks are grouped by *who writes them*, which is the invariant that was
/// impossible to see when they were one list: identity is the launcher's,
/// process ownership is the supervisor's and the host's, the workspace is the
/// worktree transactions', the lease is the lease state machine's, and the
/// outcome is the finalizer's. A change that needs two blocks at once is a
/// transaction with a name, not two assignments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundJobState {
    #[serde(flatten)]
    pub identity: JobIdentityIntent,
    #[serde(flatten)]
    pub process: ProcessOwnership,
    #[serde(flatten)]
    pub workspace: WorkspaceIsolation,
    #[serde(flatten)]
    pub lease: LeaseLifecycle,
    #[serde(flatten)]
    pub outcome: OutcomeUsage,
}

/// Reading a job record from outside this crate.
///
/// Rule for the block split: the top level offers block accessors and semantic
/// methods, and endpoint code does not reach through the blocks. These are the
/// read half of that. An endpoint asking "which job is this" should not have to
/// know that the answer is filed under identity rather than process — that
/// grouping is about *who writes* each field, which is the host's business and
/// not the caller's.
///
/// Reads only, and deliberately: a write has to name a transaction, so there is
/// no `set_status`. What an endpoint may change, it changes by asking the owner.
impl BackgroundJobState {
    pub fn job_id(&self) -> &str {
        &self.identity.job_id
    }

    pub fn session_id(&self) -> Option<&str> {
        self.identity.session_id.as_deref()
    }

    pub fn cwd(&self) -> &str {
        &self.identity.cwd
    }

    pub fn name(&self) -> &str {
        &self.identity.name
    }

    pub fn prompt(&self) -> &str {
        &self.identity.prompt
    }

    pub fn runtime(&self) -> &BackgroundRuntimeFields {
        &self.identity.runtime
    }

    pub fn status(&self) -> BackgroundJobStatus {
        self.process.status
    }

    pub fn pid(&self) -> Option<u32> {
        self.process.pid
    }

    pub fn pid_identity(&self) -> Option<&str> {
        self.process.pid_identity.as_deref()
    }

    pub fn turn_generation(&self) -> u64 {
        self.process.turn_generation
    }

    pub fn created_at_ms(&self) -> u64 {
        self.process.created_at_ms
    }

    pub fn updated_at_ms(&self) -> u64 {
        self.process.updated_at_ms
    }

    pub fn worktree_path(&self) -> Option<&str> {
        self.workspace.worktree_path.as_deref()
    }

    pub fn placement(&self) -> JobPlacement {
        self.lease.placement
    }

    pub fn event_count(&self) -> u64 {
        self.outcome.event_count
    }

    pub fn parent_job_id(&self) -> Option<&str> {
        self.identity.parent_job_id.as_deref()
    }

    pub fn agent_type(&self) -> Option<&str> {
        self.identity.agent_type.as_deref()
    }

    pub fn started_at_ms(&self) -> Option<u64> {
        self.process.started_at_ms
    }

    pub fn completed_at_ms(&self) -> Option<u64> {
        self.process.completed_at_ms
    }

    pub fn spawn_admitted(&self) -> bool {
        self.process.spawn_admitted
    }

    pub fn owner_detached_group(&self) -> bool {
        self.process.owner_detached_group
    }

    pub fn process_owner_fenced(&self) -> bool {
        self.process.process_owner_fenced
    }

    pub fn ipc_port(&self) -> Option<u16> {
        self.process.ipc_port
    }

    pub fn ipc_token(&self) -> Option<&str> {
        self.process.ipc_token.as_deref()
    }

    pub fn isolate_in_worktree(&self) -> bool {
        self.workspace.isolate_in_worktree
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.outcome.exit_code
    }

    pub fn error(&self) -> Option<&str> {
        self.outcome.error.as_deref()
    }

    pub fn summary(&self) -> Option<&str> {
        self.outcome.summary.as_deref()
    }

    pub fn retry(&self) -> Option<BackgroundRetryProgress> {
        self.outcome.retry
    }

    /// The question or permission prompt the job's turn is parked on.
    pub fn pending_permission(&self) -> Option<&BackgroundPermissionQuerySnapshot> {
        self.outcome.pending_permission.as_ref()
    }

    pub fn linger_ms(&self) -> Option<u64> {
        self.lease.linger_ms
    }

    pub fn exit_when_idle(&self) -> bool {
        self.lease.exit_when_idle
    }

    pub fn pinned(&self) -> bool {
        self.identity.pinned
    }

    pub fn sort_order(&self) -> i64 {
        self.identity.sort_order
    }
}

/// The changes an endpoint is allowed to make to a job record, each with a
/// name that says what it means.
///
/// Second rule of the block split: an action that needs more than one field to move together
/// is one transaction, not several assignments. These are the ones a launcher
/// or a client legitimately performs; everything else about a running job is
/// the owner's, and reaches it as a request rather than as a write.
///
/// Every one of them stamps `updated_at_ms`, because a record that changed and
/// does not say when is a record other processes will treat as stale — or,
/// worse, as fresh.
impl BackgroundJobState {
    /// Say whether somebody is sitting in front of this job.
    ///
    /// Placement decides how long the host lingers once its last client
    /// leaves, so it is a launch-time policy rather than a field: an endpoint
    /// that set it mid-flight would be changing the owner's lifetime from
    /// outside the owner.
    pub fn place_in(&mut self, placement: JobPlacement, now_ms: u64) {
        self.lease.placement = placement;
        self.process.updated_at_ms = now_ms;
    }

    /// How long this job's host waits after its last client leaves.
    ///
    /// `None` restores the placement's own default. Separate from
    /// [`Self::place_in`] because a job may legitimately name a linger that its
    /// placement would not have chosen.
    pub fn set_linger(&mut self, linger_ms: Option<u64>, now_ms: u64) {
        self.lease.linger_ms = linger_ms;
        self.process.updated_at_ms = now_ms;
    }

    /// Record that this job was seen, without claiming anything else changed.
    pub fn touch(&mut self, now_ms: u64) {
        self.process.updated_at_ms = now_ms;
    }

    /// Publish an endpoint for a host running inside this process.
    ///
    /// One call rather than six assignments, because the six fields are one
    /// answer: this pid, with this identity, listening on this port with this
    /// token. Assigning them separately is how a record ends up naming a pid
    /// from one generation and a port from the next, which is exactly the
    /// half-state [`RecordedOwnerSnapshot`] exists to prevent.
    ///
    /// The pid identity is read here rather than passed in: it is derived from
    /// the pid, and a caller that computed it separately could hand over an
    /// identity belonging to a different process than the one it named.
    pub fn publish_local_endpoint(&mut self, pid: u32, port: u16, token: String, now_ms: u64) {
        self.set_recorded_owner(RecordedOwnerSnapshot::owned(
            pid,
            crate::process_identity(pid),
            // In-process, so it leads no process group of its own: signalling
            // the group would reach whatever shell or app started us.
            false,
            false,
            Some(port),
            Some(token),
            self.process.turn_generation,
        ));
        self.process.status = BackgroundJobStatus::Idle;
        self.lease.placement = JobPlacement::Foreground;
        self.process.updated_at_ms = now_ms;
    }
}

impl BackgroundJobState {
    /// Add or renew `lease`, dropping any that have gone stale.
    ///
    /// Renewal is keyed on `client_id`, so a client that reconnects with the
    /// same id renews rather than accumulating a second lease.
    pub fn touch_client_lease(&mut self, lease: ClientLease, now_ms: u64) {
        self.expire_client_leases(now_ms);
        match self
            .lease
            .client_leases
            .iter_mut()
            .find(|existing| existing.client_id == lease.client_id)
        {
            Some(existing) => *existing = lease,
            None => self.lease.client_leases.push(lease),
        }
    }

    /// Drop `client_id`'s lease. Returns whether one was there.
    pub fn release_client_lease(&mut self, client_id: &str, now_ms: u64) -> bool {
        let before = self.lease.client_leases.len();
        self.lease
            .client_leases
            .retain(|lease| lease.client_id != client_id);
        let removed = self.lease.client_leases.len() != before;
        self.expire_client_leases(now_ms);
        removed
    }

    /// Forget leases nobody has renewed inside [`CLIENT_LEASE_TTL_MS`].
    ///
    /// A lease stamped in the future (a clock change, or a client on a
    /// different machine's clock through a shared home) is kept rather than
    /// expired: treating it as stale would hand the session away from a client
    /// that is still there.
    pub fn expire_client_leases(&mut self, now_ms: u64) {
        self.lease
            .client_leases
            .retain(|lease| now_ms.saturating_sub(lease.updated_at_ms) < CLIENT_LEASE_TTL_MS);
    }

    /// Whether any client is still holding this session open.
    pub fn has_live_client_lease(&self, now_ms: u64) -> bool {
        self.lease
            .client_leases
            .iter()
            .any(|lease| now_ms.saturating_sub(lease.updated_at_ms) < CLIENT_LEASE_TTL_MS)
    }

    /// When the owner may exit, having gone idle at `idle_since_ms`.
    ///
    /// A session someone is watching does not age: every lease pushes the
    /// deadline out to its own expiry plus the linger, so a client renewing
    /// every few seconds keeps the owner up for as long as it is there, and
    /// the clock only starts once the last one stops renewing. Without that,
    /// a terminal sitting on an idle session would watch its own host exit.
    ///
    /// The linger itself comes from the job when it named one, and from its
    /// placement otherwise — one policy, in one place.
    pub fn linger_deadline_ms(&self, idle_since_ms: u64) -> u64 {
        // Nobody to wait for. The linger buys time for a client that might
        // come back; a client that said it was done on its way out has
        // answered that question, so there is nothing left to buy.
        let linger = if self.lease.exit_when_idle {
            0
        } else {
            self.lease
                .linger_ms
                .unwrap_or_else(|| self.lease.placement.default_linger_ms())
        };
        let last_lease_expiry = self
            .lease
            .client_leases
            .iter()
            .map(|lease| lease.updated_at_ms.saturating_add(CLIENT_LEASE_TTL_MS))
            .max()
            .unwrap_or(0);
        idle_since_ms.max(last_lease_expiry).saturating_add(linger)
    }
}

/// One coherent snapshot of every field that identifies or gates a recorded
/// process owner. Ownership transitions must compare and publish this value as
/// a unit so a PID can never inherit another generation's identity, endpoint,
/// detached-group provenance, or fence.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedOwnerSnapshot {
    pub pid: Option<u32>,
    pub pid_identity: Option<String>,
    pub owner_detached_group: bool,
    pub process_owner_fenced: bool,
    pub ipc_port: Option<u16>,
    pub ipc_token: Option<String>,
    pub turn_generation: u64,
}

impl RecordedOwnerSnapshot {
    pub fn unowned(turn_generation: u64) -> Self {
        Self {
            pid: None,
            pid_identity: None,
            owner_detached_group: false,
            process_owner_fenced: false,
            ipc_port: None,
            ipc_token: None,
            turn_generation,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn owned(
        pid: u32,
        pid_identity: Option<String>,
        owner_detached_group: bool,
        process_owner_fenced: bool,
        ipc_port: Option<u16>,
        ipc_token: Option<String>,
        turn_generation: u64,
    ) -> Self {
        Self {
            pid: Some(pid),
            pid_identity,
            owner_detached_group,
            process_owner_fenced,
            ipc_port,
            ipc_token,
            turn_generation,
        }
    }

    pub fn fenced(mut self, fenced: bool) -> Self {
        self.process_owner_fenced = fenced;
        self.ipc_port = None;
        self.ipc_token = None;
        self
    }
}

impl BackgroundJobState {
    #[doc(hidden)]
    pub fn recorded_owner(&self) -> RecordedOwnerSnapshot {
        RecordedOwnerSnapshot {
            pid: self.process.pid,
            pid_identity: self.process.pid_identity.clone(),
            owner_detached_group: self.process.owner_detached_group,
            process_owner_fenced: self.process.process_owner_fenced,
            ipc_port: self.process.ipc_port,
            ipc_token: self.process.ipc_token.clone(),
            turn_generation: self.process.turn_generation,
        }
    }

    #[doc(hidden)]
    pub fn set_recorded_owner(&mut self, owner: RecordedOwnerSnapshot) {
        self.process.pid = owner.pid;
        self.process.pid_identity = owner.pid_identity;
        self.process.owner_detached_group = owner.owner_detached_group;
        self.process.process_owner_fenced = owner.process_owner_fenced;
        self.process.ipc_port = owner.ipc_port;
        self.process.ipc_token = owner.ipc_token;
        self.process.turn_generation = owner.turn_generation;
    }

    #[doc(hidden)]
    pub fn clear_recorded_owner(&mut self) {
        self.set_recorded_owner(RecordedOwnerSnapshot::unowned(self.process.turn_generation));
    }
}

impl BackgroundJobState {
    pub fn new(
        prompt: String,
        cwd: String,
        runtime: BackgroundRuntimeFields,
        name: Option<String>,
    ) -> Self {
        let now = now_ms();
        let name = name
            .and_then(|name| non_empty_trimmed(&name))
            .unwrap_or_else(|| job_name_from_prompt(&prompt));
        Self {
            identity: JobIdentityIntent {
                job_id: generate_job_id(),
                session_id: None,
                parent_job_id: None,
                respawned_job_id: None,
                name,
                agent_type: None,
                cwd,
                prompt,
                prompt_images: Vec::new(),
                pending_prompts: Vec::new(),
                coordinator_report_grants: Vec::new(),
                runtime,
                resume_only: false,
                queue_session: false,
                pinned: false,
                sort_order: now as i64,
            },
            process: ProcessOwnership {
                status: BackgroundJobStatus::Queued,
                pid: None,
                pid_identity: None,
                owner_detached_group: false,
                spawn_admitted: false,
                process_owner_fenced: false,
                removal_reserved: false,
                ipc_port: None,
                ipc_token: None,
                turn_generation: 0,
                process_path: std::env::var("PATH").ok(),
                node_runtime_path: std::env::var(rebon_node_runtime::NODE_EXECUTABLE_ENV)
                    .ok()
                    .filter(|path| !path.is_empty()),
                created_at_ms: now,
                updated_at_ms: now,
                started_at_ms: None,
                completed_at_ms: None,
            },
            workspace: WorkspaceIsolation {
                isolate_in_worktree: false,
                require_worktree: false,
                preserve_worktree_on_success: false,
                worktree_path: None,
            },
            lease: LeaseLifecycle {
                placement: JobPlacement::default(),
                client_leases: Vec::new(),
                linger_ms: None,
                exit_when_idle: false,
            },
            outcome: OutcomeUsage {
                retry: None,
                exit_code: None,
                error: None,
                summary: None,
                summary_updated_at_ms: None,
                pull_requests: Vec::new(),
                pending_permission: None,
                event_count: 0,
                usage: None,
            },
        }
    }

    pub fn normalize_pending_prompts(&mut self) -> anyhow::Result<()> {
        let mut reserved_ids = self
            .identity
            .pending_prompts
            .iter()
            .filter_map(|prompt| {
                validate_pending_prompt_id(&prompt.id)
                    .is_ok()
                    .then_some(prompt.id.clone())
            })
            .collect::<HashSet<_>>();
        let mut used_ids = HashSet::new();
        let mut recovered =
            Vec::with_capacity(self.identity.pending_prompts.len().min(MAX_PENDING_PROMPTS));
        let mut claimed_prefix_generation = None;
        let mut claimed_prefix_ended = false;
        for (index, mut prompt) in std::mem::take(&mut self.identity.pending_prompts)
            .into_iter()
            .enumerate()
        {
            prompt.text = prompt.text.trim().to_string();
            if prompt.text.is_empty() {
                tracing::warn!(
                    job_id = %self.identity.job_id,
                    pending_prompt_index = index,
                    "skipping pending prompt without recoverable text"
                );
                continue;
            }
            if recovered.len() == MAX_PENDING_PROMPTS {
                tracing::warn!(
                    job_id = %self.identity.job_id,
                    pending_prompt_index = index,
                    max_pending_prompts = MAX_PENDING_PROMPTS,
                    "skipping recoverable pending prompt beyond queue limit"
                );
                continue;
            }

            let valid_unique_id = validate_pending_prompt_id(&prompt.id).is_ok()
                && used_ids.insert(prompt.id.clone());
            if !valid_unique_id {
                let old_id = prompt.id.clone();
                let mut suffix = 0usize;
                loop {
                    let candidate = format!(
                        "pp-recovered-{}-{}-{}-{suffix}",
                        self.process.turn_generation, self.process.updated_at_ms, index
                    );
                    if !reserved_ids.contains(&candidate) && used_ids.insert(candidate.clone()) {
                        prompt.id = candidate;
                        break;
                    }
                    suffix = suffix.saturating_add(1);
                }
                tracing::warn!(
                    job_id = %self.identity.job_id,
                    pending_prompt_index = index,
                    pending_prompt_id = %old_id,
                    repaired_pending_prompt_id = %prompt.id,
                    "repairing invalid or duplicate pending prompt id"
                );
            }
            reserved_ids.insert(prompt.id.clone());

            if prompt.enqueued_at_ms == 0 {
                prompt.enqueued_at_ms = self.process.updated_at_ms;
            }
            if let Some(claimed_generation) = prompt.claimed_turn_generation {
                let invalid_claim = claimed_generation == 0
                    || claimed_generation > self.process.turn_generation
                    || claimed_prefix_ended
                    || claimed_prefix_generation
                        .is_some_and(|generation| generation != claimed_generation);
                if invalid_claim {
                    tracing::warn!(
                        job_id = %self.identity.job_id,
                        pending_prompt_id = %prompt.id,
                        "dropping invalid pending prompt claim"
                    );
                    prompt.claimed_turn_generation = None;
                    claimed_prefix_ended = true;
                } else {
                    claimed_prefix_generation.get_or_insert(claimed_generation);
                }
            } else {
                claimed_prefix_ended = true;
            }
            if let Some(completed_generation) = prompt.completed_turn_generation {
                let invalid_completion = completed_generation == 0
                    || completed_generation > self.process.turn_generation
                    || prompt
                        .claimed_turn_generation
                        .map(|claimed_generation| completed_generation > claimed_generation)
                        .unwrap_or(true);
                if invalid_completion {
                    tracing::warn!(
                        job_id = %self.identity.job_id,
                        pending_prompt_id = %prompt.id,
                        "dropping invalid pending prompt completion generation"
                    );
                    prompt.completed_turn_generation = None;
                }
            }
            recovered.push(prompt);
        }
        self.identity.pending_prompts = recovered;
        self.validate_pending_prompts()
    }

    pub fn validate_pending_prompts(&self) -> anyhow::Result<()> {
        if self.identity.pending_prompts.len() > MAX_PENDING_PROMPTS {
            anyhow::bail!(
                "background job {} has {} pending prompts; maximum is {}",
                self.identity.job_id,
                self.identity.pending_prompts.len(),
                MAX_PENDING_PROMPTS
            );
        }
        let mut ids = HashSet::new();
        let mut claimed_prefix_generation = None;
        let mut claimed_prefix_ended = false;
        for prompt in &self.identity.pending_prompts {
            validate_pending_prompt_id(&prompt.id)?;
            if prompt.text.trim().is_empty() {
                anyhow::bail!("pending prompt {} is empty", prompt.id);
            }
            if prompt
                .coordinator_report_paths
                .iter()
                .any(|path| path.trim().is_empty())
            {
                anyhow::bail!(
                    "pending prompt {} has an empty coordinator report path",
                    prompt.id
                );
            }
            if !ids.insert(prompt.id.as_str()) {
                anyhow::bail!("duplicate pending prompt id `{}`", prompt.id);
            }
            if let Some(claimed_generation) = prompt.claimed_turn_generation {
                if claimed_prefix_ended {
                    anyhow::bail!("claimed pending prompts must form a contiguous prefix");
                }
                if claimed_generation == 0 {
                    anyhow::bail!(
                        "pending prompt {} has invalid claim generation 0",
                        prompt.id
                    );
                }
                if claimed_generation > self.process.turn_generation {
                    anyhow::bail!(
                        "pending prompt {} has claim generation newer than the job",
                        prompt.id
                    );
                }
                if let Some(prefix_generation) = claimed_prefix_generation {
                    if claimed_generation != prefix_generation {
                        anyhow::bail!(
                            "claimed pending prompts must belong to the same turn generation"
                        );
                    }
                } else {
                    claimed_prefix_generation = Some(claimed_generation);
                }
            } else {
                claimed_prefix_ended = true;
            }
            if let Some(completed_generation) = prompt.completed_turn_generation {
                if completed_generation == 0 {
                    anyhow::bail!(
                        "pending prompt {} has invalid completion generation 0",
                        prompt.id
                    );
                }
                if completed_generation > self.process.turn_generation {
                    anyhow::bail!(
                        "pending prompt {} has completion generation newer than the job",
                        prompt.id
                    );
                }
                let Some(claimed_generation) = prompt.claimed_turn_generation else {
                    anyhow::bail!(
                        "pending prompt {} has a completion generation without a claim",
                        prompt.id
                    );
                };
                if completed_generation > claimed_generation {
                    anyhow::bail!(
                        "pending prompt {} completed after its claimed generation",
                        prompt.id
                    );
                }
            }
        }
        Ok(())
    }

    pub fn pending_prompt(&self) -> Option<&PendingPrompt> {
        self.identity.pending_prompts.first()
    }

    pub fn has_pending_prompts(&self) -> bool {
        !self.identity.pending_prompts.is_empty()
    }

    pub fn append_pending_prompt(
        &mut self,
        prompt: PendingPrompt,
    ) -> anyhow::Result<PendingPromptAcceptance> {
        self.validate_pending_prompts()?;
        validate_pending_prompt_id(&prompt.id)?;
        if prompt.text.trim().is_empty() {
            anyhow::bail!("pending prompt {} is empty", prompt.id);
        }
        if prompt
            .coordinator_report_paths
            .iter()
            .any(|path| path.trim().is_empty())
        {
            anyhow::bail!(
                "pending prompt {} has an empty coordinator report path",
                prompt.id
            );
        }
        if prompt.claimed_turn_generation.is_some() {
            anyhow::bail!("a newly appended pending prompt cannot already be claimed");
        }
        if prompt.completed_turn_generation.is_some() {
            anyhow::bail!("a newly appended pending prompt cannot already be completed");
        }
        if let Some(existing) = self
            .identity
            .pending_prompts
            .iter()
            .find(|existing| existing.id == prompt.id)
        {
            if existing.text != prompt.text
                || existing.images != prompt.images
                || existing.coordinator_report_paths != prompt.coordinator_report_paths
            {
                anyhow::bail!(
                    "pending prompt id `{}` was already used for different content",
                    prompt.id
                );
            }
            return Ok(PendingPromptAcceptance {
                prompt: existing.clone(),
                appended: false,
                queue_depth: self.identity.pending_prompts.len(),
            });
        }
        if self.identity.pending_prompts.len() >= MAX_PENDING_PROMPTS {
            anyhow::bail!(
                "background job {} already has the maximum of {} pending prompts",
                self.identity.job_id,
                MAX_PENDING_PROMPTS
            );
        }
        self.grant_coordinator_report_paths(&prompt.coordinator_report_paths);
        self.identity.pending_prompts.push(prompt.clone());
        Ok(PendingPromptAcceptance {
            prompt,
            appended: true,
            queue_depth: self.identity.pending_prompts.len(),
        })
    }

    /// Record report paths as readable for the rest of the job. Bounded
    /// so a very long coordinator run cannot grow the state file without
    /// limit; the oldest grants fall off first.
    pub fn grant_coordinator_report_paths<I, S>(&mut self, paths: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for path in paths {
            let path = path.as_ref().trim();
            if path.is_empty()
                || self
                    .identity
                    .coordinator_report_grants
                    .iter()
                    .any(|q| q == path)
            {
                continue;
            }
            self.identity
                .coordinator_report_grants
                .push(path.to_string());
        }
        if self.identity.coordinator_report_grants.len() > MAX_COORDINATOR_REPORT_GRANTS {
            let excess =
                self.identity.coordinator_report_grants.len() - MAX_COORDINATOR_REPORT_GRANTS;
            self.identity.coordinator_report_grants.drain(..excess);
        }
    }

    pub fn clear_pending_prompts(&mut self) {
        self.identity.pending_prompts.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundPullRequestDotStatus {
    Waiting,
    Ready,
    Merged,
    Inactive,
}

impl BackgroundPullRequestDotStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Ready => "ready",
            Self::Merged => "merged",
            Self::Inactive => "inactive",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundPullRequestStatus {
    pub url: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dot: Option<BackgroundPullRequestDotStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundJobEvent {
    pub timestamp_ms: u64,
    pub kind: String,
    #[serde(default)]
    pub data: serde_json::Value,
}

impl BackgroundJobEvent {
    /// Where this event sits in the owner's live stream, if the owner said.
    ///
    /// A `session_update` the owner published to its stream before writing
    /// it here carries the stream's numbering, so a client reading the file
    /// to fill a gap in the stream can tell which lines it has already seen
    /// arrive live. Events from an owner that does not stamp — an older
    /// worker, or a line that is not a session update — have none.
    pub fn stream_stamp(&self) -> Option<StreamStamp> {
        let epoch = self.data.get("streamEpoch")?.as_u64()?;
        let cursor = self.data.get("streamCursor")?.as_u64()?;
        Some(StreamStamp { epoch, cursor })
    }
}

/// One position in an owner's event stream, as written into its event log.
///
/// The stream and the log carry the same session updates in the same order,
/// but the stream numbers them and the log does not — and a client that takes
/// its deltas from the stream needs to read the log only for what the stream
/// dropped. Writing the stream's number beside each logged update is the
/// anchor that makes "read the file from here" and "resume the stream from
/// there" the same place. `epoch` is the owner incarnation that numbered it
/// (a restarted worker counts from one again); `cursor` is the number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamStamp {
    pub epoch: u64,
    pub cursor: u64,
}

pub(crate) fn session_update_event_data(
    update: &rebon_types::SessionUpdateParams,
    turn_generation: Option<u64>,
    stamp: Option<StreamStamp>,
) -> serde_json::Value {
    let mut data = serde_json::to_value(update).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(object) = data.as_object_mut() {
        if let Some(turn_generation) = turn_generation {
            object.insert("turnGeneration".into(), turn_generation.into());
        }
        if let Some(stamp) = stamp {
            object.insert("streamEpoch".into(), stamp.epoch.into());
            object.insert("streamCursor".into(), stamp.cursor.into());
        }
    }
    data
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskDescriptor {
    pub task_id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub is_backgrounded: bool,
    pub start_time_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_progress: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BackgroundTaskSnapshot {
    pub task: BackgroundTaskDescriptor,
    pub updated_at_ms: u64,
    pub log_preview: Vec<String>,
    pub transcript: Vec<BackgroundTaskTranscriptEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackgroundTaskTranscriptEntry {
    User {
        text: String,
        #[serde(default)]
        timestamp_ms: u64,
    },
    Thinking {
        text: String,
        #[serde(default)]
        timestamp_ms: u64,
    },
    Assistant {
        text: String,
        #[serde(default)]
        timestamp_ms: u64,
    },
    ToolStart {
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
        activity: String,
        #[serde(default)]
        timestamp_ms: u64,
    },
    ToolProgress {
        tool_use_id: String,
        name: String,
        message: String,
        #[serde(default)]
        timestamp_ms: u64,
    },
    ToolFinish {
        tool_use_id: String,
        name: String,
        output: Option<serde_json::Value>,
        error: Option<String>,
        #[serde(default)]
        timestamp_ms: u64,
    },
}

impl BackgroundTaskTranscriptEntry {
    /// Wall-clock time of the originating task event; 0 when the entry was
    /// deserialized from a peer that predates the field.
    pub fn timestamp_ms(&self) -> u64 {
        match self {
            Self::User { timestamp_ms, .. }
            | Self::Thinking { timestamp_ms, .. }
            | Self::Assistant { timestamp_ms, .. }
            | Self::ToolStart { timestamp_ms, .. }
            | Self::ToolProgress { timestamp_ms, .. }
            | Self::ToolFinish { timestamp_ms, .. } => *timestamp_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BackgroundTaskEventKind {
    Started,
    UserMessage {
        text: String,
    },
    AssistantTextDelta {
        delta: String,
        snapshot: String,
    },
    ThinkingDelta {
        delta: String,
        snapshot: String,
    },
    ThinkingEnd,
    AssistantTurnComplete {
        text: String,
    },
    ToolStart {
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolProgress {
        tool_use_id: String,
        name: String,
        message: String,
    },
    ToolFinish {
        tool_use_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    TerminalOutput {
        stream: String,
        chunk: String,
    },
    Finished {
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskEvent {
    pub cursor: u64,
    pub task_id: String,
    pub timestamp_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<BackgroundTaskDescriptor>,
    pub event: BackgroundTaskEventKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskEventBatch {
    pub schema: u8,
    pub stream_id: String,
    pub from_cursor: u64,
    pub through_cursor: u64,
    pub cursor_was_stale: bool,
    #[serde(default)]
    pub reset_tasks: Vec<BackgroundTaskDescriptor>,
    pub events: Vec<BackgroundTaskEvent>,
}

/// A one-line summary of an event, for the Agents list.
///
/// Every arm funnels through [`shorten_labelled_excerpt`] rather than building
/// `format!("{label}: {}", collapse(text))` first: the inputs here include
/// assistant snapshots that grow with the turn, so summarising each delta the
/// eager way is quadratic in the length of a reply. See that function for the
/// measurements.
pub fn background_task_event_summary(event: &BackgroundTaskEventKind) -> String {
    match event {
        BackgroundTaskEventKind::Started => String::from("agent started"),
        BackgroundTaskEventKind::UserMessage { text } => {
            shorten_labelled_excerpt("User:", text, 180)
        }
        BackgroundTaskEventKind::AssistantTextDelta { snapshot, .. } => {
            shorten_labelled_excerpt("", snapshot, 180)
        }
        BackgroundTaskEventKind::ThinkingDelta { snapshot, .. } => {
            shorten_labelled_excerpt("Thinking:", snapshot, 180)
        }
        BackgroundTaskEventKind::ThinkingEnd => String::new(),
        BackgroundTaskEventKind::AssistantTurnComplete { text } => {
            shorten_labelled_excerpt("", text, 180)
        }
        BackgroundTaskEventKind::ToolStart { name, input, .. } => {
            let detail = input
                .get("file_path")
                .or_else(|| input.get("path"))
                .or_else(|| input.get("pattern"))
                .or_else(|| input.get("command"))
                .and_then(serde_json::Value::as_str)
                .map(|value| format!(" {value}"))
                .unwrap_or_default();
            shorten_excerpt(&format!("{name}{detail}"), 180)
        }
        BackgroundTaskEventKind::ToolProgress { name, message, .. } => {
            shorten_labelled_excerpt(&format!("{name}:"), message, 180)
        }
        BackgroundTaskEventKind::ToolFinish { name, error, .. } => error
            .as_ref()
            .map(|error| shorten_labelled_excerpt(&format!("{name} failed:"), error, 180))
            .unwrap_or_else(|| format!("{name} completed")),
        BackgroundTaskEventKind::TerminalOutput { stream, chunk } => {
            shorten_labelled_excerpt(&format!("{stream}:"), chunk, 180)
        }
        BackgroundTaskEventKind::Finished { status, error } if status == "running" => error
            .as_ref()
            .map(|error| shorten_labelled_excerpt("agent idle after error:", error, 180))
            .unwrap_or_else(|| "agent idle".to_string()),
        BackgroundTaskEventKind::Finished { status, error } => error
            .as_ref()
            .map(|error| shorten_labelled_excerpt(&format!("{status}:"), error, 180))
            .unwrap_or_else(|| format!("agent {status}")),
    }
}

pub(crate) const MAX_BACKGROUND_TASK_TOOL_FIELD_CHARS: usize = 32 * 1024;

fn truncate_background_task_tool_field(text: &str) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.len() <= MAX_BACKGROUND_TASK_TOOL_FIELD_CHARS {
        return text.to_string();
    }

    const MARKER: &str = "\n… [truncated for Agent View] …\n";
    let retained = MAX_BACKGROUND_TASK_TOOL_FIELD_CHARS.saturating_sub(MARKER.chars().count());
    let head_len = retained / 2;
    let tail_len = retained.saturating_sub(head_len);
    let head = chars[..head_len].iter().collect::<String>();
    let tail = chars[chars.len() - tail_len..].iter().collect::<String>();
    format!("{head}{MARKER}{tail}")
}

fn bound_background_task_tool_value(value: &serde_json::Value) -> serde_json::Value {
    let Ok(serialized) = serde_json::to_string(value) else {
        return value.clone();
    };
    if serialized.chars().count() <= MAX_BACKGROUND_TASK_TOOL_FIELD_CHARS {
        return value.clone();
    }
    serde_json::json!({
        "truncated": true,
        "preview": truncate_background_task_tool_field(&serialized),
    })
}

fn background_task_transcript_tool_use_id(entry: &BackgroundTaskTranscriptEntry) -> Option<&str> {
    match entry {
        BackgroundTaskTranscriptEntry::ToolStart { tool_use_id, .. }
        | BackgroundTaskTranscriptEntry::ToolProgress { tool_use_id, .. }
        | BackgroundTaskTranscriptEntry::ToolFinish { tool_use_id, .. } => Some(tool_use_id),
        BackgroundTaskTranscriptEntry::User { .. }
        | BackgroundTaskTranscriptEntry::Thinking { .. }
        | BackgroundTaskTranscriptEntry::Assistant { .. } => None,
    }
}

fn trim_background_task_transcript(
    transcript: &mut Vec<BackgroundTaskTranscriptEntry>,
    max_entries: usize,
) {
    while transcript.len() > max_entries {
        let completed = transcript
            .iter()
            .filter_map(|entry| match entry {
                BackgroundTaskTranscriptEntry::ToolFinish { tool_use_id, .. } => {
                    Some(tool_use_id.as_str())
                }
                _ => None,
            })
            .collect::<HashSet<_>>();
        let completed_tool_use_id = transcript
            .iter()
            .filter_map(background_task_transcript_tool_use_id)
            .find(|tool_use_id| completed.contains(tool_use_id))
            .map(str::to_string);
        if let Some(tool_use_id) = completed_tool_use_id {
            transcript.retain(|entry| {
                background_task_transcript_tool_use_id(entry) != Some(tool_use_id.as_str())
            });
            continue;
        }

        let Some(first) = transcript.first() else {
            break;
        };
        if let Some(tool_use_id) = background_task_transcript_tool_use_id(first).map(str::to_string)
        {
            transcript.retain(|entry| {
                background_task_transcript_tool_use_id(entry) != Some(tool_use_id.as_str())
            });
        } else {
            transcript.remove(0);
        }
    }
}

fn apply_background_task_transcript_event(
    snapshot: &mut BackgroundTaskSnapshot,
    event: &BackgroundTaskEventKind,
    continues_assistant_text: bool,
    continues_thinking: bool,
    summary: &str,
    timestamp_ms: u64,
) {
    match event {
        BackgroundTaskEventKind::UserMessage { text } => {
            if !text.trim().is_empty() {
                snapshot
                    .transcript
                    .push(BackgroundTaskTranscriptEntry::User {
                        text: text.clone(),
                        timestamp_ms,
                    });
            }
        }
        BackgroundTaskEventKind::AssistantTextDelta {
            snapshot: assistant_text,
            ..
        }
        | BackgroundTaskEventKind::AssistantTurnComplete {
            text: assistant_text,
        } => {
            if assistant_text.trim().is_empty() {
                return;
            }
            if continues_assistant_text {
                // A continued stream keeps the timestamp of the message start.
                if let Some(BackgroundTaskTranscriptEntry::Assistant { text, .. }) =
                    snapshot.transcript.last_mut()
                {
                    *text = assistant_text.clone();
                    return;
                }
            }
            snapshot
                .transcript
                .push(BackgroundTaskTranscriptEntry::Assistant {
                    text: assistant_text.clone(),
                    timestamp_ms,
                });
        }
        BackgroundTaskEventKind::ThinkingDelta {
            snapshot: thinking, ..
        } => {
            if thinking.trim().is_empty() {
                return;
            }
            if continues_thinking {
                if let Some(BackgroundTaskTranscriptEntry::Thinking { text, .. }) =
                    snapshot.transcript.last_mut()
                {
                    *text = thinking.clone();
                    return;
                }
            }
            snapshot
                .transcript
                .push(BackgroundTaskTranscriptEntry::Thinking {
                    text: thinking.clone(),
                    timestamp_ms,
                });
        }
        BackgroundTaskEventKind::ToolStart {
            tool_use_id,
            name,
            input,
        } => snapshot
            .transcript
            .push(BackgroundTaskTranscriptEntry::ToolStart {
                tool_use_id: tool_use_id.clone(),
                name: name.clone(),
                input: bound_background_task_tool_value(input),
                activity: summary.to_string(),
                timestamp_ms,
            }),
        BackgroundTaskEventKind::ToolProgress {
            tool_use_id,
            name,
            message,
        } => snapshot
            .transcript
            .push(BackgroundTaskTranscriptEntry::ToolProgress {
                tool_use_id: tool_use_id.clone(),
                name: name.clone(),
                message: truncate_background_task_tool_field(message),
                timestamp_ms,
            }),
        BackgroundTaskEventKind::ToolFinish {
            tool_use_id,
            name,
            output,
            error,
        } => snapshot
            .transcript
            .push(BackgroundTaskTranscriptEntry::ToolFinish {
                tool_use_id: tool_use_id.clone(),
                name: name.clone(),
                output: output.as_ref().map(bound_background_task_tool_value),
                error: error.as_deref().map(truncate_background_task_tool_field),
                timestamp_ms,
            }),
        BackgroundTaskEventKind::Started
        | BackgroundTaskEventKind::ThinkingEnd
        | BackgroundTaskEventKind::TerminalOutput { .. }
        | BackgroundTaskEventKind::Finished { .. } => {}
    }
}

fn initial_background_task_transcript(
    descriptor: &BackgroundTaskDescriptor,
) -> Vec<BackgroundTaskTranscriptEntry> {
    descriptor
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .map(|prompt| {
            vec![BackgroundTaskTranscriptEntry::User {
                text: prompt.to_string(),
                timestamp_ms: descriptor.start_time_ms,
            }]
        })
        .unwrap_or_default()
}

pub fn project_background_task_snapshots(
    events: &[BackgroundJobEvent],
) -> Vec<BackgroundTaskSnapshot> {
    const LOG_CAP: usize = 200;
    const TRANSCRIPT_CAP: usize = LOG_CAP * 3;

    let mut tasks = HashMap::<String, BackgroundTaskSnapshot>::new();
    let mut assistant_streaming_tail = HashSet::<String>::new();
    let mut thinking_streaming_tail = HashSet::<String>::new();
    for outer in events
        .iter()
        .filter(|event| event.kind == "task_live_batch")
    {
        let Ok(batch) = serde_json::from_value::<BackgroundTaskEventBatch>(outer.data.clone())
        else {
            continue;
        };
        for descriptor in batch.reset_tasks {
            let key = descriptor.task_id.clone();
            let (log_preview, transcript) = tasks
                .get(&key)
                .map(|snapshot| (snapshot.log_preview.clone(), snapshot.transcript.clone()))
                .unwrap_or_else(|| (Vec::new(), initial_background_task_transcript(&descriptor)));
            tasks.insert(
                key,
                BackgroundTaskSnapshot {
                    task: descriptor,
                    updated_at_ms: outer.timestamp_ms,
                    log_preview,
                    transcript,
                },
            );
        }
        for event in batch.events {
            let key = event.task_id.clone();
            if let Some(descriptor) = event.task.clone() {
                let (log_preview, transcript) = tasks
                    .get(&key)
                    .map(|snapshot| (snapshot.log_preview.clone(), snapshot.transcript.clone()))
                    .unwrap_or_else(|| {
                        (Vec::new(), initial_background_task_transcript(&descriptor))
                    });
                tasks.insert(
                    key.clone(),
                    BackgroundTaskSnapshot {
                        task: descriptor,
                        updated_at_ms: event.timestamp_ms,
                        log_preview,
                        transcript,
                    },
                );
            }
            let snapshot = tasks
                .entry(key.clone())
                .or_insert_with(|| BackgroundTaskSnapshot {
                    task: BackgroundTaskDescriptor {
                        task_id: key,
                        title: event.task_id.clone(),
                        kind: String::from("local_agent"),
                        status: String::from("running"),
                        is_backgrounded: true,
                        start_time_ms: event.timestamp_ms,
                        end_time_ms: None,
                        last_progress: None,
                        error: None,
                        prompt: None,
                        parent_tool_call_id: None,
                        agent_id: Some(event.task_id.clone()),
                        agent_name: None,
                        agent_type: None,
                        model: None,
                        token_count: None,
                        tool_use_count: None,
                        result: None,
                    },
                    updated_at_ms: event.timestamp_ms,
                    log_preview: Vec::new(),
                    transcript: Vec::new(),
                });
            snapshot.updated_at_ms = event.timestamp_ms;
            let summary = background_task_event_summary(&event.event);
            let is_assistant_stream = matches!(
                &event.event,
                BackgroundTaskEventKind::AssistantTextDelta { .. }
                    | BackgroundTaskEventKind::AssistantTurnComplete { .. }
            );
            let is_thinking_stream =
                matches!(&event.event, BackgroundTaskEventKind::ThinkingDelta { .. });
            let continues_assistant_text =
                is_assistant_stream && assistant_streaming_tail.contains(&event.task_id);
            let continues_thinking =
                is_thinking_stream && thinking_streaming_tail.contains(&event.task_id);
            let continues_log_stream = continues_assistant_text || continues_thinking;
            let has_summary = !summary.trim().is_empty();
            apply_background_task_transcript_event(
                snapshot,
                &event.event,
                continues_assistant_text,
                continues_thinking,
                &summary,
                event.timestamp_ms,
            );
            trim_background_task_transcript(&mut snapshot.transcript, TRANSCRIPT_CAP);
            if has_summary {
                // A user message belongs in the live log but is not
                // agent progress: letting it land in `last_progress`
                // would replace the last thing the agent itself
                // reported with the text the user just typed.
                if !matches!(&event.event, BackgroundTaskEventKind::UserMessage { .. }) {
                    snapshot.task.last_progress = Some(summary.clone());
                }
                if continues_log_stream {
                    if let Some(last) = snapshot.log_preview.last_mut() {
                        *last = summary;
                    }
                } else {
                    snapshot.log_preview.push(summary);
                    if snapshot.log_preview.len() > LOG_CAP {
                        let remove = snapshot.log_preview.len() - LOG_CAP;
                        snapshot.log_preview.drain(..remove);
                    }
                }
            }
            if has_summary
                && matches!(
                    &event.event,
                    BackgroundTaskEventKind::AssistantTextDelta { .. }
                )
            {
                assistant_streaming_tail.insert(event.task_id.clone());
                thinking_streaming_tail.remove(&event.task_id);
            } else if has_summary && is_thinking_stream {
                thinking_streaming_tail.insert(event.task_id.clone());
                assistant_streaming_tail.remove(&event.task_id);
            } else {
                assistant_streaming_tail.remove(&event.task_id);
                thinking_streaming_tail.remove(&event.task_id);
            }
            match event.event {
                BackgroundTaskEventKind::Started => {
                    snapshot.task.status = String::from("running");
                }
                BackgroundTaskEventKind::Finished { status, error } => {
                    let is_running = status == "running";
                    // A resumable worker's Finished event says "running" — the
                    // task is not terminal. The descriptor riding with the
                    // event carries the post-turn state and says "idle" for a
                    // worker parked at its turn boundary; keep that instead of
                    // flattening it back to "running".
                    if !(is_running && snapshot.task.status == "idle") {
                        snapshot.task.status = status;
                    }
                    snapshot.task.error = error;
                    snapshot.task.end_time_ms = (!is_running).then_some(event.timestamp_ms);
                }
                BackgroundTaskEventKind::UserMessage { .. }
                | BackgroundTaskEventKind::ToolStart { .. }
                | BackgroundTaskEventKind::AssistantTextDelta { .. }
                | BackgroundTaskEventKind::ThinkingDelta { .. }
                | BackgroundTaskEventKind::ThinkingEnd
                | BackgroundTaskEventKind::AssistantTurnComplete { .. }
                | BackgroundTaskEventKind::ToolProgress { .. }
                | BackgroundTaskEventKind::ToolFinish { .. }
                | BackgroundTaskEventKind::TerminalOutput { .. } => {}
            }
        }
    }
    let mut snapshots = tasks.into_values().collect::<Vec<_>>();
    snapshots.sort_by(|left, right| {
        left.task
            .start_time_ms
            .cmp(&right.task.start_time_ms)
            .then_with(|| left.task.task_id.cmp(&right.task.task_id))
    });
    snapshots
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundImageAttachment {
    pub id: u32,
    pub data: String,
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

impl BackgroundImageAttachment {
    pub fn from_prompt_paste_content(content: rebon_types::PromptPasteContent) -> Self {
        Self {
            id: content.id,
            data: content.content,
            media_type: content
                .media_type
                .unwrap_or_else(|| String::from("image/png")),
            filename: content.filename,
            source_path: content.source_path,
        }
    }

    pub fn to_prompt_paste_content(&self) -> rebon_types::PromptPasteContent {
        rebon_types::PromptPasteContent {
            id: self.id,
            kind: String::from("image"),
            content: self.data.clone(),
            media_type: Some(self.media_type.clone()),
            filename: self.filename.clone(),
            source_path: self.source_path.clone(),
        }
    }

    pub fn to_content_block(&self) -> rebon_types::ContentBlock {
        rebon_types::ContentBlock::Image(rebon_types::ImageContent {
            mime_type: self.media_type.clone(),
            data: self.data.clone(),
            uri: self.source_path.clone(),
            annotations: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct BackgroundPermissionOptionSnapshot {
    pub option_id: String,
    pub label: String,
    // Read it through `parse_permission_option_kind`, never by comparing
    // strings: two spellings of each kind are legitimate. A plain comment on
    // purpose: a doc comment here becomes the field's description in the
    // 生成的 wire schema（`assets/schemas/acp-wire.json`）。
    pub kind: String,
}

/// Which kind a permission option's `kind` field names, however the side
/// that wrote it spelled it; `None` for a spelling neither side writes.
///
/// The owner records the engine's enum name; ACP spells the same thing in
/// snake_case. Both are accepted, and **nothing else is** -- no lowercasing,
/// no stripping of separators. A kind neither side knows is not quietly bent
/// into one that looks plausible: the caller decides what an unknown kind
/// means for it — a default for drawing it, a refusal for acting on it.
///
/// This is the one table. It used to be two, disagreeing: this one, and one in
/// the terminal mirror that normalised case and separators first, so
/// `"ALLOW_ALWAYS"` was `AllowAlways` there and the default here. Two readings
/// of one field is a bug waiting for a peer that spells it a third way.
pub fn parse_permission_option_kind(kind: &str) -> Option<rebon_proto::PermissionOptionKind> {
    use rebon_proto::PermissionOptionKind;
    match kind {
        "AllowOnce" | "allow_once" => Some(PermissionOptionKind::AllowOnce),
        "AllowAlways" | "allow_always" => Some(PermissionOptionKind::AllowAlways),
        "RejectOnce" | "reject_once" => Some(PermissionOptionKind::RejectOnce),
        "RejectAlways" | "reject_always" => Some(PermissionOptionKind::RejectAlways),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct BackgroundIpcEndpoint {
    pub pid: u32,
    pub port: u16,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct BackgroundPermissionQuerySnapshot {
    pub query_id: u64,
    #[serde(default)]
    pub turn_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<BackgroundIpcEndpoint>,
    pub tool: Option<String>,
    pub tool_call_id: Option<String>,
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub options: Vec<BackgroundPermissionOptionSnapshot>,
}

pub(crate) fn validate_permission_target(
    state: &BackgroundJobState,
    snapshot: &BackgroundPermissionQuerySnapshot,
    expected_turn_generation: Option<u64>,
    expected_endpoint: Option<&BackgroundIpcEndpoint>,
) -> anyhow::Result<()> {
    if let Some(expected_turn_generation) = expected_turn_generation {
        if snapshot.turn_generation != expected_turn_generation {
            anyhow::bail!("background permission belongs to a different turn generation");
        }
    }
    let Some(expected_endpoint) = expected_endpoint else {
        return Ok(());
    };
    if snapshot.endpoint.as_ref() != Some(expected_endpoint) {
        anyhow::bail!("background permission belongs to a different IPC endpoint generation");
    }
    let current_matches = state.process.pid == Some(expected_endpoint.pid)
        && state.process.ipc_port == Some(expected_endpoint.port)
        && state.process.ipc_token.as_deref() == Some(expected_endpoint.token.as_str());
    if !current_matches {
        anyhow::bail!("background IPC endpoint changed before the permission answer was sent");
    }
    Ok(())
}
