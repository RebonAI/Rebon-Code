//! Task execution layer owned by the `tasks` feature plugin.
//!
//! [`crate::ui`] is the state-machine projector/reducer layer for the task
//! surfaces. It intentionally carries no I/O. The **runtime**
//! layer — the piece that tracks background work and owns cancellation —
//! lives here. Coordinator workers publish into this runtime directly.
//!
//! This module implements the execution contract consumed by task views:
//!
//! - [`Task`] — marker for task specs the runtime knows how to spawn.
//! - [`LocalShellTaskSpec`] — direct shell-command execution.
//! - [`TaskRegistry`] — shared slot that owns running tasks, hands
//!   out a cloneable [`TaskSnapshot`] per task, and supports cancel
//!   + listing lookups.
//! - [`TaskObservation`] — stream of lifecycle events a caller can
//!   drain without touching the worker/child process primitives.
//!
//! Backends whose handles live outside this crate (remote agents,
//! coordinator workers, teammates, workflows) publish through the runtime's
//! registration and update APIs. This module owns lifecycle state, event
//! delivery, observation, and cancellation rather than those backend handles.
//!
//! Disk-backed history bootstrap (which tasks were loaded from disk, what
//! to retain, what to evict afterwards) stays
//! in the state-machine projectors under [`crate::ui`]; this runtime feeds those
//! projectors through [`TaskRegistry::snapshots`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rebon_core::Engine;
use rebon_tool::{
    remove_session_default_team, BackgroundShellCompletionStatus, BackgroundShellTaskCompletion,
    BackgroundShellTaskSpec, EscalationId, EscalationRegistry, MonitorEventDisposition,
    MonitorTaskCompletion, MonitorTaskCompletionStatus, MonitorTaskSource, MonitorTaskSpec,
    QuestionEscalationNotification, StopTaskOutcome, TaskRuntimeController, ToolContext,
};
use rebon_tools_core::{ToolError, ToolErrorPresentation, ToolId};
use rebon_types::{xml_escape, PromptCancel};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{mpsc, watch, Notify};
use tokio::time::Instant;

fn empty_metadata() -> Value {
    serde_json::json!({})
}

const MAX_AGENT_TRANSCRIPT_ENTRIES: usize = 512;
const MAX_AGENT_TRANSCRIPT_BYTES: usize = 4 * 1024 * 1024;
const MAX_AGENT_TOOL_FIELD_CHARS: usize = 32 * 1024;
const MAX_PENDING_AGENT_MESSAGES: usize = 64;
const MAX_TASK_TOMBSTONES: usize = 4096;
const MONITOR_EVENT_MAX_CHARS: usize = 500;
const MONITOR_NOTIFICATION_MAX_CHARS: usize = 3000;
const MONITOR_PENDING_HARD_LIMIT: usize = 1024;
const MONITOR_BURST_WINDOW: Duration = Duration::from_secs(15);
const MONITOR_BURST_MAX_DURATION: Duration = Duration::from_secs(60);
const MONITOR_BURST_MAX_KEPT: usize = 20;
const LOCAL_AGENT_IDLE_METADATA_KEY: &str = "_runtime_is_idle";

#[derive(Debug, Clone)]
struct AgentRetentionPolicy {
    /// How long a local agent parked at a turn boundary stays resumable
    /// before it is reclaimed. It is still addressable by `SendMessage`
    /// for this long; past it the coordinator is told to spawn fresh.
    internal_idle_ttl: Duration,
    terminal_ttl: Duration,
    /// Per owner session, the number of parked local agents kept alive.
    /// Past it the longest-idle ones are reclaimed regardless of TTL, so a
    /// wide fan-out cannot pin one runtime per worker indefinitely.
    max_idle_internal_agents_per_session: usize,
}

impl Default for AgentRetentionPolicy {
    fn default() -> Self {
        Self {
            internal_idle_ttl: Duration::from_secs(15 * 60),
            terminal_ttl: Duration::from_secs(5 * 60),
            max_idle_internal_agents_per_session: 16,
        }
    }
}

/// Stable identifier for a running task.
///
/// Carried as an opaque string so callers can mint ids however
/// they like (UUID v7, hash, sequential, …). The registry only
/// uses it for equality + hash lookups.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskId(pub String);

impl TaskId {
    /// Construct from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the inner string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Tagged union of task kinds the runtime knows how to spawn.
///
/// Uses eight task-kind variants so the coordinator's vocabulary is 1:1 with
/// `crate::ui::tasks::common::TaskKind`. `LocalShell`, `LocalAgent`, and `Monitor`
/// have runtime-backed spawn paths; the remaining variants are registered by
/// their owning integrations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
    /// A direct shell command execution via an engine-registered
    /// bash-family tool (`Bash` / `PowerShell`). Uses the persisted
    /// `'local_bash'` discriminant.
    LocalShell,
    /// A sub-agent query driven by the coordinator worker bridge. Uses the persisted
    /// `'local_agent'` discriminant.
    LocalAgent,
    /// A remote cloud session (ultraplan / ultrareview / background PR
    /// autofix / etc.). Uses the persisted `'remote_agent'` discriminant.
    /// The coordinator tracks its state but does not poll the remote
    /// backend; a future backend wires the poller.
    RemoteAgent,
    /// An in-process teammate (swarm member) running an embedded
    /// agent loop. Uses the persisted `'in_process_teammate'` discriminant.
    /// Currently a state-only slot; the actual teammate runtime belongs
    /// in runtime integration.
    InProcessTeammate,
    /// A multi-agent workflow run. Uses the persisted `'local_workflow'`
    /// discriminant. State-only.
    LocalWorkflow,
    /// A built-in command or WebSocket event monitor. Uses the persisted
    /// `'monitor'` discriminant.
    Monitor,
    /// A long-running MCP server monitor. Uses the persisted
    /// `'monitor_mcp'` discriminant. State-only.
    MonitorMcp,
    /// Memory consolidation ("dream") sub-agent. Uses the persisted
    /// `'dream'` discriminant. State-only.
    Dream,
}

impl TaskKind {
    /// String form for persisted literals (`'local_bash'`,
    /// `'local_agent'`, `'remote_agent'`, `'in_process_teammate'`,
    /// `'local_workflow'`, `'monitor'`, `'monitor_mcp'`, `'dream'`).
    ///
    /// Note the `local_bash` label for `LocalShell`: persisted session
    /// state uses that discriminant for backward compatibility, so we
    /// keep it rather than coining our own `'local_shell'` literal.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::LocalShell => "local_bash",
            TaskKind::LocalAgent => "local_agent",
            TaskKind::RemoteAgent => "remote_agent",
            TaskKind::InProcessTeammate => "in_process_teammate",
            TaskKind::LocalWorkflow => "local_workflow",
            TaskKind::Monitor => "monitor",
            TaskKind::MonitorMcp => "monitor_mcp",
            TaskKind::Dream => "dream",
        }
    }

    /// Round-trip parser — inverse of [`Self::as_str`]. Returns `None`
    /// for unknown variants.
    pub fn from_str(s: &str) -> Option<TaskKind> {
        Some(match s {
            "local_bash" | "local_shell" => TaskKind::LocalShell,
            "local_agent" => TaskKind::LocalAgent,
            "remote_agent" => TaskKind::RemoteAgent,
            "in_process_teammate" => TaskKind::InProcessTeammate,
            "local_workflow" => TaskKind::LocalWorkflow,
            "monitor" => TaskKind::Monitor,
            "monitor_mcp" => TaskKind::MonitorMcp,
            "dream" => TaskKind::Dream,
            _ => return None,
        })
    }

    /// Single-character task id prefix.
    pub fn id_prefix(self) -> char {
        match self {
            TaskKind::LocalShell => 'b', // 'b' for backward compatibility
            TaskKind::LocalAgent => 'a',
            TaskKind::RemoteAgent => 'r',
            TaskKind::InProcessTeammate => 't',
            TaskKind::LocalWorkflow => 'w',
            TaskKind::Monitor | TaskKind::MonitorMcp => 'm',
            TaskKind::Dream => 'd',
        }
    }
}

impl std::fmt::Display for TaskKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle status of a task. The runtime's own vocabulary, defined once in
/// [`rebon_types`] and shared with every surface that renders it.
pub use rebon_types::TaskStatus;

/// Clone-friendly snapshot of one registered task.
///
/// The base fields
/// (`id`, `kind`, `status`, `description`, timestamps, `notified`,
/// etc.) live flat on the struct; the per-kind typed fields are
/// bundled into [`TaskData`] under [`Self::data`].
///
/// Produced by [`TaskRegistry::snapshot`] / [`TaskRegistry::snapshots`]
/// so the projector layer (the state-machine
/// [`crate::ui::tasks::task_list`]) can read the runtime's
/// current state without borrowing locks across `.await` points.
#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    /// Task id.
    pub id: TaskId,
    /// Task kind. Duplicated from [`Self::data`]'s discriminant so
    /// callers that only care about kind don't have to match on
    /// `TaskData`.
    pub kind: TaskKind,
    /// Current lifecycle status.
    pub status: TaskStatus,
    /// Human-readable description. For `LocalShell` this is the
    /// command body (or the shell monitor's description label);
    /// for `LocalAgent` / `InProcessTeammate` / `RemoteAgent` it's
    /// the task's title; for `Dream` it's the literal
    /// `"dreaming"`.
    pub title: String,
    /// Last progress blurb — "text" for local_agent turns,
    /// "stdout tail" for local_shell.
    pub last_progress: Option<String>,
    /// Error message if [`TaskStatus::Failed`].
    pub error: Option<String>,
    /// Structured result payload once the task terminates.
    pub result: Option<Value>,
    /// Whether the task has been explicitly backgrounded.
    /// Used to route the task through the background indicator
    /// rather than the inline panel.
    pub is_backgrounded: bool,
    /// Whether the task's terminal-state notification has been
    /// delivered to the user.
    /// `notified` is used to suppress duplicate SDK events and to
    /// gate the "Stop all agents" aggregate notification path.
    pub notified: bool,
    /// Wall-clock milliseconds (since Unix epoch) when the task was
    /// registered. The
    /// projector layer uses this for the
    /// "running first then descending start time" sort order in
    /// [`crate::ui::tasks::tasks_dialog::build_dialog_layout`] and for the
    /// elapsed-time display in the per-kind detail dialogs. Set to
    /// `SystemTime::now()` at spawn time.
    pub start_time_ms: u64,
    /// Wall-clock milliseconds when the task reached a terminal
    /// status (`Completed` / `Killed` / `Failed`). `None` while
    /// the task is still running. Used by the shell detail
    /// dialog so completed tasks display their real final runtime
    /// rather than "now − start".
    pub end_time_ms: Option<u64>,
    /// Passive caller-provided metadata for correlation/display.
    /// Runtime task control semantics intentionally do not inspect this.
    pub metadata: Value,
    /// Per-kind typed state. Each variant carries the fields that
    /// kind needs on top of the base snapshot fields. Set at
    /// spawn/register time and mutated through
    /// [`TaskRegistry::update`] helpers.
    pub data: TaskData,
}

impl TaskSnapshot {
    /// Construct a new base snapshot in `Pending` state with the
    /// given kind-specific [`TaskData`]. The caller is expected to
    /// wrap this in a [`TaskRegistry::insert`] call or hand it to a
    /// `spawn_*` helper.
    pub fn new_pending(id: TaskId, title: String, data: TaskData) -> Self {
        let kind = data.kind();
        Self {
            id,
            kind,
            status: TaskStatus::Pending,
            title,
            last_progress: None,
            error: None,
            result: None,
            is_backgrounded: false,
            notified: false,
            start_time_ms: rebon_types::wall_clock_ms(),
            end_time_ms: None,
            metadata: empty_metadata(),
            data,
        }
    }

    /// Return a string metadata value by key.
    pub fn metadata_str(&self, key: &str) -> Option<&str> {
        self.metadata.get(key).and_then(|value| value.as_str())
    }

    /// Return this task's local ultraplan correlation id, if present.
    pub fn ultraplan_id(&self) -> Option<&str> {
        self.metadata_str("ultraplan_id")
    }

    /// Return this task's local ultraplan role, if present.
    pub fn ultraplan_role(&self) -> Option<&str> {
        self.metadata_str("ultraplan_role")
    }
}

/// Per-kind typed extension fields carried on [`TaskSnapshot`].
///
/// Discriminated-union task state: each variant corresponds to one
/// concrete task kind and holds the fields that kind adds
/// on top of the shared snapshot. Kept as a dedicated enum so callers can
/// pattern-match for per-kind behavior without boxing the whole snapshot.
#[derive(Debug, Clone)]
pub enum TaskData {
    /// `'local_bash'` — shell command execution.
    LocalShell(LocalShellData),
    /// `'local_agent'` — sub-agent query.
    LocalAgent(LocalAgentData),
    /// `'remote_agent'` — remote cloud session (ultraplan / review).
    RemoteAgent(Box<RemoteAgentData>),
    /// `'in_process_teammate'` — swarm member.
    InProcessTeammate(Box<InProcessTeammateData>),
    /// `'local_workflow'` — multi-agent workflow.
    LocalWorkflow(LocalWorkflowData),
    /// `'monitor'` — built-in command or WebSocket event monitor.
    Monitor(MonitorData),
    /// `'monitor_mcp'` — long-running MCP monitor.
    MonitorMcp(MonitorMcpData),
    /// `'dream'` — memory consolidation sub-agent.
    Dream(DreamData),
}

impl TaskData {
    /// Discriminant of this data payload — matches the enclosing
    /// [`TaskSnapshot::kind`] so callers can round-trip through a
    /// borrowed `&TaskData`.
    pub fn kind(&self) -> TaskKind {
        match self {
            TaskData::LocalShell(_) => TaskKind::LocalShell,
            TaskData::LocalAgent(_) => TaskKind::LocalAgent,
            TaskData::RemoteAgent(_) => TaskKind::RemoteAgent,
            TaskData::InProcessTeammate(_) => TaskKind::InProcessTeammate,
            TaskData::LocalWorkflow(_) => TaskKind::LocalWorkflow,
            TaskData::Monitor(_) => TaskKind::Monitor,
            TaskData::MonitorMcp(_) => TaskKind::MonitorMcp,
            TaskData::Dream(_) => TaskKind::Dream,
        }
    }
}

/// Bash task display variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BashTaskKind {
    /// Regular interactive shell command.
    Bash,
    /// Long-running monitor (shows description instead of command).
    Monitor,
}

/// Typed fields for a `local_bash` snapshot.
#[derive(Debug, Clone)]
pub struct LocalShellData {
    /// `command` — the raw shell command.
    pub command: String,
    /// Exit code if the command has completed.
    pub exit_code: Option<i32>,
    /// True if the command was interrupted by a signal before exit.
    pub interrupted: bool,
    /// UI display variant (bash vs monitor).
    pub display_kind: BashTaskKind,
    /// Optional agent id of the sub-agent that spawned this task.
    /// Used by [`kill_shell_tasks_for_agent`] so sub-agent exits
    /// clean up orphaned bash children.
    pub agent_id: Option<String>,
}

/// One visible row in a local-agent foreground transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum LocalAgentTranscriptEntry {
    User {
        text: String,
    },
    Thinking {
        text: String,
    },
    Assistant {
        text: String,
    },
    ToolStart {
        tool_use_id: String,
        name: String,
        input: Value,
        activity: String,
    },
    ToolProgress {
        tool_use_id: String,
        name: String,
        message: String,
    },
    ToolFinish {
        tool_use_id: String,
        name: String,
        ok: bool,
        summary: String,
        outcome: Result<Value, String>,
    },
}

fn transcript_entry_size(entry: &LocalAgentTranscriptEntry) -> usize {
    match entry {
        LocalAgentTranscriptEntry::User { text }
        | LocalAgentTranscriptEntry::Thinking { text }
        | LocalAgentTranscriptEntry::Assistant { text } => text.len(),
        LocalAgentTranscriptEntry::ToolStart {
            tool_use_id,
            name,
            input,
            activity,
        } => {
            tool_use_id.len()
                + name.len()
                + activity.len()
                + serde_json::to_string(input).map_or(0, |value| value.len())
        }
        LocalAgentTranscriptEntry::ToolProgress {
            tool_use_id,
            name,
            message,
        } => tool_use_id.len() + name.len() + message.len(),
        LocalAgentTranscriptEntry::ToolFinish {
            tool_use_id,
            name,
            summary,
            outcome,
            ..
        } => {
            tool_use_id.len()
                + name.len()
                + summary.len()
                + match outcome {
                    Ok(value) => serde_json::to_string(value).map_or(0, |value| value.len()),
                    Err(error) => error.len(),
                }
        }
    }
}

fn truncate_agent_tool_field(text: &str) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.len() <= MAX_AGENT_TOOL_FIELD_CHARS {
        return text.to_string();
    }

    const MARKER: &str = "\n… [truncated for Agent View] …\n";
    let marker_chars = MARKER.chars().count();
    let retained = MAX_AGENT_TOOL_FIELD_CHARS.saturating_sub(marker_chars);
    let head_len = retained / 2;
    let tail_len = retained.saturating_sub(head_len);
    let head = chars[..head_len].iter().collect::<String>();
    let tail = chars[chars.len() - tail_len..].iter().collect::<String>();
    format!("{head}{MARKER}{tail}")
}

fn bound_agent_tool_value(value: Value) -> Value {
    let Ok(serialized) = serde_json::to_string(&value) else {
        return value;
    };
    if serialized.chars().count() <= MAX_AGENT_TOOL_FIELD_CHARS {
        return value;
    }
    serde_json::json!({
        "truncated": true,
        "preview": truncate_agent_tool_field(&serialized),
    })
}

fn bound_agent_transcript_entry(entry: LocalAgentTranscriptEntry) -> LocalAgentTranscriptEntry {
    match entry {
        LocalAgentTranscriptEntry::ToolStart {
            tool_use_id,
            name,
            input,
            activity,
        } => LocalAgentTranscriptEntry::ToolStart {
            tool_use_id,
            name,
            input: bound_agent_tool_value(input),
            activity: truncate_agent_tool_field(&activity),
        },
        LocalAgentTranscriptEntry::ToolProgress {
            tool_use_id,
            name,
            message,
        } => LocalAgentTranscriptEntry::ToolProgress {
            tool_use_id,
            name,
            message: truncate_agent_tool_field(&message),
        },
        LocalAgentTranscriptEntry::ToolFinish {
            tool_use_id,
            name,
            ok,
            summary,
            outcome,
        } => LocalAgentTranscriptEntry::ToolFinish {
            tool_use_id,
            name,
            ok,
            summary: truncate_agent_tool_field(&summary),
            outcome: match outcome {
                Ok(value) => Ok(bound_agent_tool_value(value)),
                Err(error) => Err(truncate_agent_tool_field(&error)),
            },
        },
        entry => entry,
    }
}

fn transcript_tool_use_id(entry: &LocalAgentTranscriptEntry) -> Option<&str> {
    match entry {
        LocalAgentTranscriptEntry::ToolStart { tool_use_id, .. }
        | LocalAgentTranscriptEntry::ToolProgress { tool_use_id, .. }
        | LocalAgentTranscriptEntry::ToolFinish { tool_use_id, .. } => Some(tool_use_id),
        LocalAgentTranscriptEntry::User { .. }
        | LocalAgentTranscriptEntry::Thinking { .. }
        | LocalAgentTranscriptEntry::Assistant { .. } => None,
    }
}

fn evict_oldest_completed_tool_group(transcript: &mut Vec<LocalAgentTranscriptEntry>) -> bool {
    let completed = transcript
        .iter()
        .filter_map(|entry| match entry {
            LocalAgentTranscriptEntry::ToolFinish { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let Some(tool_use_id) = transcript
        .iter()
        .filter_map(transcript_tool_use_id)
        .find(|tool_use_id| completed.contains(tool_use_id))
        .map(str::to_string)
    else {
        return false;
    };
    transcript.retain(|entry| transcript_tool_use_id(entry) != Some(tool_use_id.as_str()));
    true
}

fn evict_oldest_transcript_group(transcript: &mut Vec<LocalAgentTranscriptEntry>) {
    let Some(first) = transcript.first() else {
        return;
    };
    let Some(tool_use_id) = transcript_tool_use_id(first).map(str::to_string) else {
        transcript.remove(0);
        return;
    };
    transcript.retain(|entry| transcript_tool_use_id(entry) != Some(tool_use_id.as_str()));
}

pub fn push_bounded_agent_transcript(
    transcript: &mut Vec<LocalAgentTranscriptEntry>,
    entry: LocalAgentTranscriptEntry,
) {
    transcript.push(bound_agent_transcript_entry(entry));
    let mut total_bytes = transcript.iter().map(transcript_entry_size).sum::<usize>();
    while (transcript.len() > MAX_AGENT_TRANSCRIPT_ENTRIES
        || total_bytes > MAX_AGENT_TRANSCRIPT_BYTES)
        && transcript.len() > 1
    {
        if !evict_oldest_completed_tool_group(transcript) {
            evict_oldest_transcript_group(transcript);
        }
        total_bytes = transcript.iter().map(transcript_entry_size).sum::<usize>();
    }
}

pub fn upsert_bounded_agent_thinking(
    transcript: &mut Vec<LocalAgentTranscriptEntry>,
    text: String,
) {
    if matches!(
        transcript.last(),
        Some(LocalAgentTranscriptEntry::Thinking { .. })
    ) {
        transcript.pop();
    }
    push_bounded_agent_transcript(transcript, LocalAgentTranscriptEntry::Thinking { text });
}

/// Typed fields for a `local_agent` snapshot: the subset
/// the
/// coordinator needs to track for UI projectors.
#[derive(Debug, Clone)]
pub struct LocalAgentData {
    /// Full prompt. The detail
    /// dialog shows the first 300 chars.
    pub prompt: String,
    /// Agent type identifier (e.g. `"general-purpose"`,
    /// `"code-reviewer"`).
    pub agent_type: String,
    /// Model override. `None` means "inherit from session".
    pub model: Option<String>,
    /// Optional system prompt used by this worker.
    pub system: Option<String>,
    /// Allowed-tool filter captured at spawn time.
    pub allowed_tools: Option<Vec<String>>,
    /// Total token count (input + output). Grows as the worker
    /// emits usage events.
    pub token_count: u64,
    /// Count of tool_use blocks seen in the worker stream.
    pub tool_use_count: u64,
    /// Visible foreground transcript for this local agent.
    pub transcript: Vec<LocalAgentTranscriptEntry>,
    /// Current streaming assistant text that has not reached an iteration boundary yet.
    pub streaming_text: Option<String>,
    /// Pending messages queued during the sub-agent run (used for
    /// mid-run user messages; present here as a pass-through for
    /// future task extensions).
    pub pending_messages: Vec<String>,
    /// `retrieved` flag — set to true when the
    /// caller has consumed the final result, gating eviction.
    pub retrieved: bool,
}

/// Typed fields for a `remote_agent` snapshot.
#[derive(Debug, Clone)]
pub struct RemoteAgentData {
    /// Remote task kind — `remote-agent` / `ultraplan` /
    /// `ultrareview` / `autofix-pr` / `background-pr`.
    pub remote_task_type: RemoteTaskType,
    /// Remote session id (used by the teleport API).
    pub session_id: String,
    /// Original command text that kicked off the remote session.
    pub command: String,
    /// User-visible title.
    pub title: String,
    /// Wall-clock ms when local polling started. A `--resume` uses
    /// this instead of `start_time_ms` so the review timeout clock
    /// doesn't immediately trip for resumed long-runners.
    pub poll_started_at_ms: u64,
    /// True when the task was created by a remote `/ultrareview`.
    pub is_remote_review: bool,
    /// True when the task is an ultraplan invocation.
    pub is_ultraplan: bool,
    /// True when this is a long-running task that should NOT be
    /// marked complete after the first `result`.
    pub is_long_running: bool,
    /// Parsed ultraplan phase (`needs_input` / `plan_ready`). `None`
    /// while the task is plain running. Surfaced in the pill badge.
    pub ultraplan_phase: Option<UltraplanPhase>,
    /// Review-progress counts parsed from the orchestrator's
    /// `<remote-review-progress>` heartbeats.
    pub review_progress: Option<ReviewProgress>,
}

/// Remote task discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemoteTaskType {
    /// Generic remote agent.
    RemoteAgent,
    /// Plan-generation task (`/ultraplan`).
    Ultraplan,
    /// Bug-hunt review task (`/ultrareview`).
    Ultrareview,
    /// Autofix a specific PR.
    AutofixPr,
    /// Long-running PR background task.
    BackgroundPr,
}

impl RemoteTaskType {
    /// Wire string for this remote task type.
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteTaskType::RemoteAgent => "remote-agent",
            RemoteTaskType::Ultraplan => "ultraplan",
            RemoteTaskType::Ultrareview => "ultrareview",
            RemoteTaskType::AutofixPr => "autofix-pr",
            RemoteTaskType::BackgroundPr => "background-pr",
        }
    }
}

/// Ultraplan phase. `'running'` is represented by
/// `None` in [`RemoteAgentData::ultraplan_phase`] so the enum here
/// only carries the two attention states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UltraplanPhase {
    /// Remote is waiting for the user to answer a clarifying question.
    NeedsInput,
    /// Remote has produced a plan and is awaiting browser approval.
    PlanReady,
}

/// Review progress counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReviewProgress {
    /// Current stage — `finding` / `verifying` / `synthesizing`.
    /// `None` = pre-stage orchestrator setup.
    pub stage: Option<ReviewStage>,
    /// Number of candidate bugs found in the "finding" stage.
    pub bugs_found: u64,
    /// Number of bugs promoted through the "verifying" stage.
    pub bugs_verified: u64,
    /// Number of verified bugs dropped by the "synthesizing" dedupe.
    pub bugs_refuted: u64,
}

/// Review stage. Uses the persisted `ReviewStage` literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewStage {
    /// Searching for candidate bugs.
    Finding,
    /// Validating candidate bugs.
    Verifying,
    /// Deduping verified bugs into the final report.
    Synthesizing,
}

impl ReviewStage {
    /// Wire string for this remote task type.
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewStage::Finding => "finding",
            ReviewStage::Verifying => "verifying",
            ReviewStage::Synthesizing => "synthesizing",
        }
    }
}

/// Teammate identity.
#[derive(Debug, Clone)]
pub struct TeammateIdentity {
    /// Fully-qualified agent id (`"researcher@my-team"`).
    pub agent_id: String,
    /// Short agent name (`"researcher"`).
    pub agent_name: String,
    /// Parent team name.
    pub team_name: String,
    /// Display color (pre-projected to a design-system color key).
    pub color: Option<String>,
    /// Whether the teammate must approve plan mode.
    pub plan_mode_required: bool,
    /// Parent leader session id.
    pub parent_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeammateRequest {
    pub request_id: String,
    pub message: String,
}

/// Typed fields for an `in_process_teammate` snapshot.
#[derive(Debug, Clone)]
pub struct InProcessTeammateData {
    /// Teammate identity.
    pub identity: TeammateIdentity,
    /// User prompt that spawned the teammate.
    pub prompt: String,
    /// Optional model override.
    pub model: Option<String>,
    /// Optional model profile override.
    pub model_profile: Option<String>,
    /// Current permission mode snapshot.
    pub permission_mode: String,
    /// Plan mode approval pending flag.
    pub awaiting_plan_approval: bool,
    /// True when the teammate is idle (no pending work).
    pub is_idle: bool,
    /// True when a graceful shutdown has been requested via
    /// [`request_teammate_shutdown`]. The runtime picks this up on
    /// its next idle tick.
    pub shutdown_requested: bool,
    /// Queue of requests waiting for the resident teammate loop.
    pub pending_user_messages: Vec<TeammateRequest>,
    /// Running tool-use count — progress hint for the UI.
    pub tool_use_count: u64,
    /// Running token count — progress hint for the UI.
    pub token_count: u64,
    /// Live transcript across turns. Shares the entry type with
    /// `LocalAgentData` so the attach view renders both kinds
    /// through the same projection.
    pub transcript: Vec<LocalAgentTranscriptEntry>,
    /// In-flight assistant text for the current turn (cleared when
    /// the turn completes).
    pub streaming_text: Option<String>,
}

#[derive(Debug, Clone)]
pub enum WorkflowProgressEntry {
    Agent {
        index: u64,
        state: String,
        phase_title: Option<String>,
        /// Stable identity of the phase invocation that owned this agent call.
        /// Older progress streams omit it and remain title-matched by renderers.
        phase_id: Option<String>,
        label: String,
        tokens: u64,
        tool_calls: u64,
        tool_call_details: Vec<Value>,
        duration_ms: Option<u64>,
        error: Option<String>,
        /// Task id of the spawned sub-agent (`agent-<hex>`), when known.
        /// Renderers folding repeated state entries should retain the latest
        /// non-None id for compatibility with older persisted progress.
        agent_id: Option<String>,
    },
    Phase {
        title: String,
        state: String,
        /// Stable identity for one invocation of a phase title. Older progress
        /// streams omit it and remain title-matched by renderers.
        phase_id: Option<String>,
    },
    Log {
        message: String,
    },
}

/// Typed fields for a `local_workflow` snapshot. The workflow runtime keeps
/// incremental progress here so task list/details can render a live run.
#[derive(Debug, Clone)]
pub struct LocalWorkflowData {
    /// Stable workflow run id (`wf_<id>`).
    pub run_id: String,
    /// Human-readable workflow name (`"release-train"`, etc.).
    pub workflow_name: String,
    /// One-line summary shown in place of the name when present.
    pub summary: Option<String>,
    /// Number of sub-agents participating.
    pub agent_count: u64,
    /// Incremental phase/agent/log progress.
    pub progress_entries: Vec<WorkflowProgressEntry>,
    /// Total tokens consumed by child agents.
    pub token_count: u64,
    /// Total child tool calls.
    pub tool_use_count: u64,
    /// Persisted workflow output directory.
    pub output_path: Option<String>,
    /// Persisted script path.
    pub script_path: Option<String>,
    /// Invocation args, if any.
    pub args: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MonitorSourceKind {
    Command,
    WebSocket,
}

impl MonitorSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::WebSocket => "websocket",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MonitorEndReason {
    Exited,
    Closed,
    Failed,
    Stopped,
    TimedOut,
    AutoStopped,
}

impl MonitorEndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::Closed => "closed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::TimedOut => "timed_out",
            Self::AutoStopped => "auto_stopped",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MonitorData {
    pub description: String,
    pub source: MonitorSourceKind,
    pub redacted_target: String,
    pub event_count: u64,
    pub suppressed_count: u64,
    pub end_reason: Option<MonitorEndReason>,
}

/// Typed fields for a `monitor_mcp` snapshot. Same situation as
/// `LocalWorkflowData` — matching the pure projector's input shape.
#[derive(Debug, Clone)]
pub struct MonitorMcpData {
    /// MCP server name.
    pub server_name: String,
    /// Short description.
    pub description: String,
}

/// Single turn captured from the dream agent stream.
#[derive(Debug, Clone)]
pub struct DreamTurn {
    /// Assistant text for the turn.
    pub text: String,
    /// Collapsed tool-use count for the turn.
    pub tool_use_count: u64,
}

/// Dream-task phase. Uses the persisted `DreamPhase` literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DreamPhase {
    /// Waiting for the first Edit/Write tool call.
    Starting,
    /// An Edit/Write landed — actively mutating files.
    Updating,
}

impl DreamPhase {
    /// Wire string for this remote task type.
    pub fn as_str(self) -> &'static str {
        match self {
            DreamPhase::Starting => "starting",
            DreamPhase::Updating => "updating",
        }
    }
}

/// Typed fields for a `dream` snapshot.
#[derive(Debug, Clone)]
pub struct DreamData {
    /// Current phase.
    pub phase: DreamPhase,
    /// Number of session transcripts the dream agent is reviewing.
    pub sessions_reviewing: u64,
    /// Paths observed in Edit/Write tool calls. Treat as "at least
    /// these" — bash-mediated writes can be missed.
    pub files_touched: Vec<String>,
    /// Assistant turns captured from the stream, most-recent last.
    /// Capped at [`DREAM_MAX_TURNS`].
    pub turns: Vec<DreamTurn>,
    /// Consolidation lock mtime captured at spawn time, used by the
    /// kill path to rewind the lock.
    pub prior_mtime: u64,
}

/// Cap on the number of recent dream turns kept in memory.
pub const DREAM_MAX_TURNS: usize = 30;

/// Observation event emitted on a task's lifecycle stream.
///
/// Covers worker lifecycle events plus shell-specific lines. This stream
/// is unbounded and intended for the spawning caller. App/UI consumers should
/// prefer the bounded, cursor-based [`TaskRegistry::task_live_events`] or
/// [`TaskRegistry::session_live_events`] journal APIs.
#[derive(Debug, Clone)]
pub enum TaskObservation {
    /// Task moved from Pending into Running and emitted its first
    /// progress event.
    Started,
    /// Assistant text snapshot (for local_agent) or stdout/stderr
    /// chunk (for local_shell).
    Progress {
        /// Free-form channel tag (`"text"`, `"stdout"`, `"stderr"`,
        /// `"tool_start"`, `"tool_finish"`).
        channel: String,
        /// Textual payload. For the `"text"` channel this is the complete
        /// in-flight assistant text snapshot, not a delta.
        payload: String,
    },
    /// Task finished with a snapshot of its final state.
    Finished(TaskSnapshot),
}

/// Monotonic cursor assigned to a live task event within one [`TaskRegistry`]
/// session. Cursor zero means "before the first event".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskEventCursor(pub u64);

impl TaskEventCursor {
    pub const ZERO: Self = Self(0);

    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// stdout/stderr stream associated with a local terminal task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskTerminalStream {
    Stdout,
    Stderr,
}

/// Event-level live task payload retained by the bounded journal.
///
/// Unlike the worker's cumulative assistant-text snapshots,
/// `AssistantTextDelta::delta` contains
/// only the newly arrived text. `snapshot` is carried alongside it so callers
/// migrating from snapshot-based rendering do not need to reconstruct state.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskLiveEventKind {
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
        input: Value,
    },
    ToolProgress {
        tool_use_id: String,
        name: String,
        message: String,
    },
    ToolFinish {
        tool_use_id: String,
        name: String,
        outcome: Result<Value, String>,
    },
    TerminalOutput {
        stream: TaskTerminalStream,
        chunk: String,
    },
    Finished {
        status: TaskStatus,
        error: Option<String>,
    },
}

/// One retained event. Cursors are registry-session scoped and can therefore
/// also be used to merge events from multiple tasks in session order.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskLiveEvent {
    pub cursor: TaskEventCursor,
    pub task_id: TaskId,
    pub timestamp_ms: u64,
    pub kind: TaskLiveEventKind,
}

/// Result of a cursor read from a task or session journal.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskLiveEventBatch {
    /// Retained events strictly newer than the requested cursor. Passing
    /// `None` to the read API returns all currently retained events.
    pub events: Vec<TaskLiveEvent>,
    /// Cursor to pass to the next read. It advances past evicted events even
    /// when no retained event is returned.
    pub next_cursor: TaskEventCursor,
    /// Oldest event currently available in this journal.
    pub oldest_available_cursor: Option<TaskEventCursor>,
    /// Newest cursor assigned to this journal, including events since evicted.
    pub latest_cursor: TaskEventCursor,
    /// True when the supplied cursor predates retained history. The caller
    /// should resync from [`TaskRegistry::snapshot`] / `snapshots`, then apply
    /// the returned retained events and continue from `next_cursor`.
    pub cursor_was_stale: bool,
}

/// Default maximum retained events for both each task journal and the merged
/// registry-session journal. Slow consumers never exert unbounded memory
/// pressure; they observe `cursor_was_stale` after eviction.
pub const DEFAULT_TASK_EVENT_JOURNAL_CAPACITY: usize = 512;

#[derive(Debug)]
struct BoundedTaskEventJournal {
    capacity: usize,
    events: VecDeque<TaskLiveEvent>,
    dropped_through: TaskEventCursor,
    latest_cursor: TaskEventCursor,
}

impl BoundedTaskEventJournal {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            events: VecDeque::with_capacity(capacity),
            dropped_through: TaskEventCursor::ZERO,
            latest_cursor: TaskEventCursor::ZERO,
        }
    }

    fn push(&mut self, event: TaskLiveEvent) {
        self.latest_cursor = event.cursor;
        if self.capacity == 0 {
            self.dropped_through = event.cursor;
            return;
        }
        if self.events.len() == self.capacity {
            if let Some(dropped) = self.events.pop_front() {
                self.dropped_through = dropped.cursor;
            }
        }
        self.events.push_back(event);
    }

    fn read(&self, after: Option<TaskEventCursor>) -> TaskLiveEventBatch {
        let cursor_was_stale = after.is_some_and(|cursor| cursor < self.dropped_through);
        let events = self
            .events
            .iter()
            .filter(|event| after.is_none_or(|cursor| event.cursor > cursor))
            .cloned()
            .collect::<Vec<_>>();
        let requested = after.unwrap_or(TaskEventCursor::ZERO);
        let next_cursor = events
            .last()
            .map(|event| event.cursor)
            .unwrap_or(requested.max(self.dropped_through));
        TaskLiveEventBatch {
            events,
            next_cursor,
            oldest_available_cursor: self.events.front().map(|event| event.cursor),
            latest_cursor: self.latest_cursor,
            cursor_was_stale,
        }
    }
}

#[derive(Debug)]
struct TaskEventJournals {
    capacity: usize,
    next_cursor: u64,
    session: BoundedTaskEventJournal,
    tasks: HashMap<TaskId, BoundedTaskEventJournal>,
}

impl Default for TaskEventJournals {
    fn default() -> Self {
        Self::new(DEFAULT_TASK_EVENT_JOURNAL_CAPACITY)
    }
}

impl TaskEventJournals {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            next_cursor: 0,
            session: BoundedTaskEventJournal::new(capacity),
            tasks: HashMap::new(),
        }
    }

    fn ensure_task(&mut self, task_id: TaskId) {
        let capacity = self.capacity;
        self.tasks
            .entry(task_id)
            .or_insert_with(|| BoundedTaskEventJournal::new(capacity));
    }

    fn record(&mut self, task_id: &TaskId, kind: TaskLiveEventKind) {
        self.ensure_task(task_id.clone());
        self.next_cursor = self.next_cursor.saturating_add(1);
        let event = TaskLiveEvent {
            cursor: TaskEventCursor(self.next_cursor),
            task_id: task_id.clone(),
            timestamp_ms: rebon_types::wall_clock_ms(),
            kind,
        };
        self.session.push(event.clone());
        if let Some(task) = self.tasks.get_mut(task_id) {
            task.push(event);
        }
    }
}

pub fn assistant_text_delta(previous: &str, snapshot: &str) -> String {
    snapshot
        .strip_prefix(previous)
        .unwrap_or(snapshot)
        .to_string()
}

pub fn describe_worker_after_tool_activity(name: &str) -> String {
    format!("processing {name} result")
}

pub fn describe_worker_tool_activity(name: &str, input: &Value) -> String {
    match name {
        "Read" | "FileReadTool" => input_first_str(input, &["file_path", "path"])
            .map(|path| format!("reading {}", compact_activity_text(path, 96)))
            .unwrap_or_else(|| format!("running {name}")),
        "Grep" | "GrepTool" => {
            let pattern = input_first_str(input, &["pattern", "query"])
                .map(|value| compact_activity_text(value, 64));
            let path = input_first_str(input, &["path", "include"])
                .map(|value| compact_activity_text(value, 64));
            match (pattern, path) {
                (Some(pattern), Some(path)) => format!("searching {pattern} in {path}"),
                (Some(pattern), None) => format!("searching {pattern}"),
                _ => format!("running {name}"),
            }
        }
        "Glob" | "GlobTool" => input_first_str(input, &["pattern"])
            .map(|pattern| format!("matching {}", compact_activity_text(pattern, 96)))
            .unwrap_or_else(|| format!("running {name}")),
        "Bash" | "PowerShell" | "BashTool" | "PowerShellTool" => {
            input_first_str(input, &["command"])
                .map(|command| format!("running {}", compact_activity_text(command, 120)))
                .unwrap_or_else(|| format!("running {name}"))
        }
        // Whoever writes a file gets the same line, so the arm asks the kind
        // instead of naming the writers.
        name if rebon_tools_core::tool_kind_for_name(name)
            == rebon_tools_core::ToolKind::FileEdit =>
        {
            let verb = if name == "Write" || name == "FileWriteTool" {
                "writing"
            } else {
                "editing"
            };
            input_first_str(input, &["file_path", "notebook_path", "path"])
                .map(|path| format!("{verb} {}", compact_activity_text(path, 96)))
                .unwrap_or_else(|| format!("running {name}"))
        }
        "WebFetch" | "WebFetchTool" => input_first_str(input, &["url"])
            .map(|url| format!("fetching {}", compact_activity_text(url, 96)))
            .unwrap_or_else(|| format!("running {name}")),
        "WebSearch" | "WebSearchTool" => input_first_str(input, &["query"])
            .map(|query| format!("web searching {}", compact_activity_text(query, 96)))
            .unwrap_or_else(|| format!("running {name}")),
        "Agent" | "AgentTool" => input_first_str(input, &["description", "prompt"])
            .map(|task| format!("delegating {}", compact_activity_text(task, 96)))
            .unwrap_or_else(|| format!("running {name}")),
        "Skill" | "SkillTool" => input_first_str(input, &["skill"])
            .map(|skill| format!("using skill {}", compact_activity_text(skill, 64)))
            .unwrap_or_else(|| format!("running {name}")),
        _ => format!("running {name}"),
    }
}

fn input_first_str<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
        .filter(|value| !value.trim().is_empty())
}

fn compact_activity_text(text: &str, max_chars: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = one_line.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskTurnToken {
    task_id: TaskId,
    generation: u64,
    turn_id: u64,
}

pub struct LocalAgentMessageLease {
    registry: TaskRegistry,
    task_id: TaskId,
    messages: Vec<String>,
    message_generation: u64,
    settled: bool,
}

impl LocalAgentMessageLease {
    pub fn messages(&self) -> &[String] {
        &self.messages
    }

    pub fn message_generation(&self) -> u64 {
        self.message_generation
    }

    pub fn commit(mut self) {
        self.settle(false);
    }

    fn settle(&mut self, restore: bool) {
        if self.settled {
            return;
        }
        let messages = std::mem::take(&mut self.messages);
        self.registry
            .settle_local_agent_message_lease(&self.task_id, messages, restore);
        self.settled = true;
    }
}

impl Drop for LocalAgentMessageLease {
    fn drop(&mut self) {
        self.settle(true);
    }
}

#[derive(Debug)]
struct TaskRuntimeState {
    wake: Arc<Notify>,
    owner_session_id: Option<String>,
    generation: u64,
    notification_generation: u64,
    message_generation: u64,
    leased_local_agent_messages: usize,
    next_turn_id: u64,
    current_turn_id: Option<u64>,
    idle_since: Option<Instant>,
}

impl TaskRuntimeState {
    fn new(snapshot: &TaskSnapshot) -> Self {
        Self {
            wake: Arc::new(Notify::new()),
            owner_session_id: task_owner_session_id(snapshot),
            generation: 1,
            notification_generation: 0,
            message_generation: 0,
            leased_local_agent_messages: 0,
            next_turn_id: 1,
            current_turn_id: None,
            idle_since: task_is_idle(snapshot).then(Instant::now),
        }
    }

    fn begin_turn(&mut self, task_id: &TaskId) -> TaskTurnToken {
        let turn_id = self.next_turn_id;
        self.next_turn_id = self.next_turn_id.saturating_add(1);
        self.notification_generation = self.notification_generation.saturating_add(1);
        self.current_turn_id = Some(turn_id);
        self.idle_since = None;
        TaskTurnToken {
            task_id: task_id.clone(),
            generation: self.generation,
            turn_id,
        }
    }

    fn matches(&self, token: &TaskTurnToken) -> bool {
        self.generation == token.generation && self.current_turn_id == Some(token.turn_id)
    }

    fn invalidate(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.current_turn_id = None;
        self.idle_since = None;
        self.wake.notify_waiters();
    }
}

fn task_owner_session_id(snapshot: &TaskSnapshot) -> Option<String> {
    snapshot
        .metadata
        .get("parent_session_id")
        .or_else(|| snapshot.metadata.get("session_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| match &snapshot.data {
            TaskData::InProcessTeammate(data) => {
                let session_id = data.identity.parent_session_id.trim();
                (!session_id.is_empty()).then(|| session_id.to_string())
            }
            _ => None,
        })
}

fn task_is_idle(snapshot: &TaskSnapshot) -> bool {
    match &snapshot.data {
        TaskData::LocalAgent(_) => snapshot
            .metadata
            .get(LOCAL_AGENT_IDLE_METADATA_KEY)
            .and_then(Value::as_bool)
            .unwrap_or(false),
        TaskData::InProcessTeammate(data) => data.is_idle,
        _ => false,
    }
}

/// A local agent parked at a turn boundary: its query loop finished but
/// `keep_runtime_resumable` deliberately left the snapshot non-terminal so
/// the persistent actor can take a follow-up. Every such agent — not just
/// `Explore` — is a reclamation candidate; without this, a coordinator's
/// research/verification workers stay resident for the life of the process.
fn is_idle_local_agent(snapshot: &TaskSnapshot) -> bool {
    task_is_idle(snapshot) && matches!(&snapshot.data, TaskData::LocalAgent(_))
}

/// A backgrounded agent still owes its result to whoever spawned it until the
/// terminal/idle notification has actually been delivered. Reclaiming it
/// before that drops the report on the floor with nothing left to re-derive
/// it from, so retention must wait even once the TTL is up.
fn notification_is_outstanding(snapshot: &TaskSnapshot) -> bool {
    snapshot.is_backgrounded && !snapshot.notified
}

fn terminal_notification_ready(snapshot: &TaskSnapshot) -> bool {
    let lifecycle_ready = match snapshot.kind {
        TaskKind::LocalAgent => snapshot.status.is_terminal() || task_is_idle(snapshot),
        TaskKind::LocalShell | TaskKind::LocalWorkflow | TaskKind::Monitor => {
            snapshot.status.is_terminal()
        }
        _ => false,
    };
    lifecycle_ready && snapshot.is_backgrounded && !snapshot.notified
}

fn retention_ttl_for_snapshot(
    snapshot: &TaskSnapshot,
    policy: &AgentRetentionPolicy,
) -> Option<Duration> {
    if matches!(snapshot.data, TaskData::InProcessTeammate(_)) {
        None
    } else if snapshot.status.is_terminal() {
        Some(policy.terminal_ttl)
    } else if is_idle_local_agent(snapshot) && !notification_is_outstanding(snapshot) {
        Some(policy.internal_idle_ttl)
    } else {
        None
    }
}

pub fn is_agent_snapshot_idle(snapshot: &TaskSnapshot) -> bool {
    task_is_idle(snapshot)
}

/// Shared task state behind the registry's lock.
#[derive(Debug)]
struct TaskState {
    snapshot: TaskSnapshot,
    cancel: PromptCancel,
    background_request: watch::Sender<bool>,
    runtime: TaskRuntimeState,
}

/// Thread-safe registry that owns every running task.
#[derive(Debug, Clone)]
pub struct TaskRegistry {
    inner: Arc<Mutex<HashMap<TaskId, TaskState>>>,
    removed_ids: Arc<Mutex<HashSet<TaskId>>>,
    removed_order: Arc<Mutex<VecDeque<TaskId>>>,
    active_workflow_runs: Arc<Mutex<HashMap<String, TaskId>>>,
    escalation_registry: EscalationRegistry,
    event_journals: Arc<Mutex<TaskEventJournals>>,
    monitor_notifications: Arc<Mutex<MonitorNotificationState>>,
    notification_revision: watch::Sender<u64>,
    retention_policy: Arc<AgentRetentionPolicy>,
    retention_started: Arc<AtomicBool>,
    retention_notify: Arc<Notify>,
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn remove_task_from_registry_parts(
    inner: &Arc<Mutex<HashMap<TaskId, TaskState>>>,
    removed_ids: &Arc<Mutex<HashSet<TaskId>>>,
    removed_order: &Arc<Mutex<VecDeque<TaskId>>>,
    event_journals: &Arc<Mutex<TaskEventJournals>>,
    id: &TaskId,
) -> Option<TaskSnapshot> {
    remove_task_from_registry_parts_if(
        inner,
        removed_ids,
        removed_order,
        event_journals,
        id,
        |_| true,
    )
}

fn remove_task_from_registry_parts_if(
    inner: &Arc<Mutex<HashMap<TaskId, TaskState>>>,
    removed_ids: &Arc<Mutex<HashSet<TaskId>>>,
    removed_order: &Arc<Mutex<VecDeque<TaskId>>>,
    event_journals: &Arc<Mutex<TaskEventJournals>>,
    id: &TaskId,
    should_remove: impl FnOnce(&TaskState) -> bool,
) -> Option<TaskSnapshot> {
    let mut tombstones = removed_ids
        .lock()
        .expect("task registry tombstones poisoned");
    let mut guard = inner.lock().expect("task registry poisoned");
    if !guard.get(id).is_some_and(should_remove) {
        return None;
    }
    let removed = guard.remove(id).map(|mut task| {
        task.cancel.cancel();
        task.runtime.invalidate();
        task.snapshot
    });
    drop(guard);
    if removed.is_some() {
        tombstones.insert(id.clone());
        let mut order = removed_order
            .lock()
            .expect("task registry tombstone order poisoned");
        order.push_back(id.clone());
        while order.len() > MAX_TASK_TOMBSTONES {
            if let Some(expired) = order.pop_front() {
                tombstones.remove(&expired);
            }
        }
        event_journals
            .lock()
            .expect("task event journals poisoned")
            .tasks
            .remove(id);
    }
    removed
}

#[derive(Clone)]
enum TaskRegistryRuntimeSource {
    Resolver(crate::TaskRegistryResolver),
    #[cfg(test)]
    Fixed(Arc<TaskRegistry>),
}

#[derive(Clone)]
pub struct TaskRegistryRuntimeController {
    source: TaskRegistryRuntimeSource,
}

impl std::fmt::Debug for TaskRegistryRuntimeController {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TaskRegistryRuntimeController")
            .finish_non_exhaustive()
    }
}

impl TaskRegistryRuntimeController {
    /// Build the kernel-free runtime callback bridge. Every callback resolves
    /// the exact session's typed `task-registry` seat; it never owns or creates
    /// registry identity itself.
    pub fn new(resolver: crate::TaskRegistryResolver) -> Self {
        Self {
            source: TaskRegistryRuntimeSource::Resolver(resolver),
        }
    }

    #[cfg(test)]
    fn for_test_registry(registry: TaskRegistry) -> Self {
        Self {
            source: TaskRegistryRuntimeSource::Fixed(Arc::new(registry)),
        }
    }

    fn registry(&self, session_id: &str) -> Result<Arc<TaskRegistry>, String> {
        match &self.source {
            TaskRegistryRuntimeSource::Resolver(resolver) => resolver.resolve(session_id),
            #[cfg(test)]
            TaskRegistryRuntimeSource::Fixed(registry) => Ok(registry.clone()),
        }
    }
}

const SHELL_PROGRESS_MAX_CHARS: usize = 4096;

fn shell_progress_tail(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let start = text
        .char_indices()
        .rev()
        .nth(SHELL_PROGRESS_MAX_CHARS - 1)
        .map(|(index, _)| index)
        .unwrap_or(0);
    Some(text[start..].to_string())
}

#[async_trait::async_trait]
impl TaskRuntimeController for TaskRegistryRuntimeController {
    async fn stop_task(&self, session_id: &str, task_id: &str) -> Result<StopTaskOutcome, String> {
        let registry = self.registry(session_id)?;
        let id = TaskId::new(task_id.to_string());
        match stop_task(&registry, &id) {
            Ok(result) => {
                registry
                    .escalation_registry()
                    .cancel_agent(task_id, "worker was stopped by coordinator");
                Ok(StopTaskOutcome::Stopped {
                    task_id: result.task_id,
                    task_type: result.task_type.to_string(),
                    command: result.command,
                })
            }
            Err(StopTaskError::NotFound(_)) => Ok(StopTaskOutcome::NotFound),
            Err(StopTaskError::NotRunning(_, _)) => {
                let Some(snapshot) = registry.snapshot(&id) else {
                    return Ok(StopTaskOutcome::NotFound);
                };
                let already_stopped_monitor = matches!(
                    &snapshot.data,
                    TaskData::Monitor(data)
                        if snapshot.status == TaskStatus::Killed
                            && data.end_reason == Some(MonitorEndReason::Stopped)
                );
                if already_stopped_monitor {
                    Ok(StopTaskOutcome::Stopped {
                        task_id: snapshot.id.as_str().to_string(),
                        task_type: snapshot.kind.as_str().to_string(),
                        command: snapshot.title,
                    })
                } else {
                    Err(
                        StopTaskError::NotRunning(task_id.to_string(), snapshot.status.as_str())
                            .to_string(),
                    )
                }
            }
        }
    }

    async fn send_message_to_task(
        &self,
        session_id: &str,
        task_id: &str,
        message: String,
    ) -> Result<(), ToolErrorPresentation> {
        let registry = self.registry(session_id).map_err(|error| {
            ToolErrorPresentation::new(
                "task_registry_unavailable",
                "Task runtime is unavailable for this session.",
                error,
            )
        })?;
        send_message_to_local_agent_task(&registry, task_id, message)
    }

    fn background_shell_started(
        &self,
        session_id: &str,
        spec: BackgroundShellTaskSpec,
        cancel: PromptCancel,
    ) {
        let Ok(registry) = self.registry(session_id) else {
            return;
        };
        let id = TaskId::new(spec.shell_id);
        let mut metadata = serde_json::Map::new();
        metadata.insert("shell_tool".into(), Value::String(spec.tool_name));
        if let Some(session_id) = spec.session_id {
            metadata.insert("parent_session_id".into(), Value::String(session_id));
        }
        if let Some(agent_id) = spec.agent_id.as_ref() {
            metadata.insert("agent_id".into(), Value::String(agent_id.clone()));
        }
        let mut snapshot = TaskSnapshot::new_pending(
            id.clone(),
            spec.command.clone(),
            TaskData::LocalShell(LocalShellData {
                command: spec.command,
                exit_code: None,
                interrupted: false,
                display_kind: BashTaskKind::Bash,
                agent_id: spec.agent_id,
            }),
        );
        snapshot.status = TaskStatus::Running;
        snapshot.is_backgrounded = true;
        snapshot.start_time_ms = spec.started_at_ms;
        snapshot.metadata = Value::Object(metadata);
        registry.insert(id.clone(), snapshot, cancel);
        registry.record_live_event(&id, TaskLiveEventKind::Started);
    }

    fn background_shell_finished(
        &self,
        session_id: &str,
        completion: BackgroundShellTaskCompletion,
    ) {
        let Ok(registry) = self.registry(session_id) else {
            return;
        };
        let BackgroundShellTaskCompletion {
            shell_id,
            status,
            completed_at_ms,
            exit_code,
            output,
            stderr,
            stream_order,
            error,
            next_cursor,
            has_more,
            cursor_truncated,
            oldest_cursor,
            observed,
        } = completion;
        let id = TaskId::new(shell_id.clone());
        let (task_status, error) = match status {
            BackgroundShellCompletionStatus::Exited if exit_code == Some(0) => {
                (TaskStatus::Completed, error)
            }
            BackgroundShellCompletionStatus::Exited => (
                TaskStatus::Failed,
                error.or_else(|| {
                    Some(match exit_code {
                        Some(code) => format!("background shell exited with code {code}"),
                        None => "background shell exited without an exit code".to_string(),
                    })
                }),
            ),
            BackgroundShellCompletionStatus::Stopped => (TaskStatus::Killed, error),
            BackgroundShellCompletionStatus::TimedOut => (
                TaskStatus::Failed,
                error.or_else(|| Some("background shell timed out".to_string())),
            ),
            BackgroundShellCompletionStatus::Failed => (
                TaskStatus::Failed,
                error.or_else(|| Some("background shell failed".to_string())),
            ),
        };
        let last_progress = shell_progress_tail(if output.is_empty() { &stderr } else { &output });
        let mut result = serde_json::Map::new();
        result.insert("shellId".into(), Value::String(shell_id));
        result.insert("status".into(), Value::String(status.as_str().to_string()));
        result.insert("completed".into(), Value::Bool(true));
        result.insert("nextCursor".into(), Value::from(next_cursor));
        if let Some(exit_code) = exit_code {
            result.insert("exitCode".into(), Value::from(exit_code));
        }
        if !output.is_empty() {
            result.insert("output".into(), Value::String(output));
        }
        if !stderr.is_empty() {
            result.insert("stderr".into(), Value::String(stderr));
        }
        // Mirrors the `ShellOutput` poll shape so the finished card replays
        // the same arrival order the live polls did.
        if let Some(sketch) = stream_order {
            result.insert(
                rebon_tools_core::shell_stream_order::STREAM_ORDER_KEY.into(),
                Value::String(sketch),
            );
        }
        if let Some(error) = error.as_ref() {
            result.insert("error".into(), Value::String(error.clone()));
        }
        if status == BackgroundShellCompletionStatus::TimedOut {
            result.insert("timedOut".into(), Value::Bool(true));
        }
        if status == BackgroundShellCompletionStatus::Stopped {
            result.insert("stopRequested".into(), Value::Bool(true));
        }
        if has_more {
            result.insert("hasMore".into(), Value::Bool(true));
        }
        if cursor_truncated {
            result.insert("cursorTruncated".into(), Value::Bool(true));
            result.insert("oldestCursor".into(), Value::from(oldest_cursor));
            result.insert("requestedCursor".into(), Value::from(0));
        }
        registry.update(&id, |snapshot| {
            snapshot.status = task_status;
            snapshot.error = error.clone();
            snapshot.result = Some(Value::Object(result));
            snapshot.last_progress = last_progress;
            snapshot.end_time_ms = Some(completed_at_ms);
            snapshot.notified |= observed;
            if let TaskData::LocalShell(data) = &mut snapshot.data {
                data.exit_code = exit_code;
                data.interrupted = matches!(
                    status,
                    BackgroundShellCompletionStatus::Stopped
                        | BackgroundShellCompletionStatus::TimedOut
                );
            }
        });
        if let Some(snapshot) = registry.snapshot(&id) {
            registry.record_live_event(
                &id,
                TaskLiveEventKind::Finished {
                    status: snapshot.status,
                    error: snapshot.error,
                },
            );
        }
    }

    fn background_shell_observed(&self, session_id: &str, shell_id: &str) {
        let Ok(registry) = self.registry(session_id) else {
            return;
        };
        registry.mark_notifications_delivered(&[TaskId::new(shell_id)]);
    }

    fn monitor_started(&self, session_id: &str, spec: MonitorTaskSpec, cancel: PromptCancel) {
        let Ok(registry) = self.registry(session_id) else {
            return;
        };
        let id = TaskId::new(spec.task_id);
        let mut metadata = serde_json::Map::new();
        if let Some(session_id) = spec.session_id {
            metadata.insert("parent_session_id".into(), Value::String(session_id));
        }
        if let Some(agent_id) = spec.agent_id.as_ref() {
            metadata.insert("agent_id".into(), Value::String(agent_id.clone()));
        }
        let source = match spec.source {
            MonitorTaskSource::Command => MonitorSourceKind::Command,
            MonitorTaskSource::WebSocket => MonitorSourceKind::WebSocket,
        };
        let mut snapshot = TaskSnapshot::new_pending(
            id.clone(),
            spec.description.clone(),
            TaskData::Monitor(MonitorData {
                description: spec.description,
                source,
                redacted_target: spec.redacted_target,
                event_count: 0,
                suppressed_count: 0,
                end_reason: None,
            }),
        );
        snapshot.status = TaskStatus::Running;
        snapshot.is_backgrounded = true;
        snapshot.start_time_ms = spec.started_at_ms;
        snapshot.metadata = Value::Object(metadata);
        registry.insert(id.clone(), snapshot, cancel);
        registry.record_live_event(&id, TaskLiveEventKind::Started);
    }

    fn monitor_event(
        &self,
        session_id: &str,
        task_id: &str,
        event: String,
    ) -> MonitorEventDisposition {
        let Ok(registry) = self.registry(session_id) else {
            return MonitorEventDisposition::Ignored;
        };
        match registry.enqueue_monitor_event(&TaskId::new(task_id), event) {
            MonitorEventEnqueueOutcome::Ignored => MonitorEventDisposition::Ignored,
            MonitorEventEnqueueOutcome::Queued { .. } => MonitorEventDisposition::Queued,
            MonitorEventEnqueueOutcome::Suppressed { .. } => MonitorEventDisposition::Suppressed,
            MonitorEventEnqueueOutcome::AutoStop { .. } => MonitorEventDisposition::AutoStop,
        }
    }

    fn monitor_finished(&self, session_id: &str, completion: MonitorTaskCompletion) {
        let Ok(registry) = self.registry(session_id) else {
            return;
        };
        let id = TaskId::new(completion.task_id);
        let (task_status, end_reason) = match completion.status {
            MonitorTaskCompletionStatus::Exited => {
                (TaskStatus::Completed, MonitorEndReason::Exited)
            }
            MonitorTaskCompletionStatus::Closed => {
                (TaskStatus::Completed, MonitorEndReason::Closed)
            }
            MonitorTaskCompletionStatus::Failed => (TaskStatus::Failed, MonitorEndReason::Failed),
            MonitorTaskCompletionStatus::Stopped => (TaskStatus::Killed, MonitorEndReason::Stopped),
            MonitorTaskCompletionStatus::TimedOut => {
                (TaskStatus::Failed, MonitorEndReason::TimedOut)
            }
            MonitorTaskCompletionStatus::AutoStopped => {
                (TaskStatus::Failed, MonitorEndReason::AutoStopped)
            }
        };
        let mut result = serde_json::Map::new();
        result.insert(
            "status".into(),
            Value::String(completion.status.as_str().to_string()),
        );
        if let Some(exit_code) = completion.exit_code {
            result.insert("exitCode".into(), Value::from(exit_code));
        }
        if let Some(stderr) = completion.stderr.as_ref() {
            result.insert("stderr".into(), Value::String(stderr.clone()));
        }
        let fallback_error = match completion.status {
            MonitorTaskCompletionStatus::TimedOut => Some("monitor timed out".to_string()),
            MonitorTaskCompletionStatus::AutoStopped => Some(
                "monitor was automatically stopped because it produced too many events".to_string(),
            ),
            _ => None,
        };
        let error = completion.error.or(fallback_error);
        registry.update(&id, |snapshot| {
            snapshot.status = task_status;
            snapshot.error = error.clone();
            snapshot.result = Some(Value::Object(result));
            snapshot.last_progress = completion.stderr.as_deref().and_then(shell_progress_tail);
            snapshot.end_time_ms = Some(completion.completed_at_ms);
            if let TaskData::Monitor(data) = &mut snapshot.data {
                data.end_reason = Some(end_reason);
            }
        });
        if let Some(snapshot) = registry.snapshot(&id) {
            registry.record_live_event(
                &id,
                TaskLiveEventKind::Finished {
                    status: snapshot.status,
                    error: snapshot.error,
                },
            );
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskNotificationKind {
    Terminal,
    MonitorEvent,
}

#[derive(Debug, Clone)]
struct MonitorQueuedEvent {
    sequence: u64,
    received_at: Instant,
    content: Option<String>,
}

#[derive(Debug, Default)]
struct MonitorTaskNotificationState {
    next_sequence: u64,
    events: VecDeque<MonitorQueuedEvent>,
    recent_event_times: VecDeque<Instant>,
    flood_started_at: Option<Instant>,
}

#[derive(Debug, Default)]
struct MonitorNotificationState {
    tasks: HashMap<TaskId, MonitorTaskNotificationState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorEventEnqueueOutcome {
    Ignored,
    Queued { sequence: u64 },
    Suppressed { sequence: u64 },
    AutoStop { sequence: u64 },
}

/// A model-facing task notification that has not yet been acked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskNotification {
    pub task_id: TaskId,
    pub generation: u64,
    pub kind: TaskNotificationKind,
    pub message: String,
    pub output_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskNotificationClaim {
    pub task_id: TaskId,
    pub generation: u64,
    pub kind: TaskNotificationKind,
}

impl TaskNotification {
    pub fn claim(&self) -> TaskNotificationClaim {
        TaskNotificationClaim {
            task_id: self.task_id.clone(),
            generation: self.generation,
            kind: self.kind,
        }
    }
}

impl Drop for TaskRegistry {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            self.escalation_registry.cancel_all(
                "coordinator session ended before pending question escalations were resolved",
            );
            self.retention_notify.notify_waiters();
        }
    }
}

impl TaskRegistry {
    /// Construct an empty registry with the default bounded live-event
    /// journal capacity.
    pub fn new() -> Self {
        Self::with_event_journal_capacity(DEFAULT_TASK_EVENT_JOURNAL_CAPACITY)
    }

    /// Construct an empty registry whose per-task and merged-session journals
    /// each retain at most `capacity` events. A capacity of zero disables
    /// retention while cursors still advance, allowing callers to detect that
    /// they must resync from snapshots.
    pub fn with_event_journal_capacity(capacity: usize) -> Self {
        let (notification_revision, _) = watch::channel(0);
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            removed_ids: Arc::new(Mutex::new(HashSet::new())),
            removed_order: Arc::new(Mutex::new(VecDeque::new())),
            active_workflow_runs: Arc::new(Mutex::new(HashMap::new())),
            escalation_registry: EscalationRegistry::new(),
            event_journals: Arc::new(Mutex::new(TaskEventJournals::new(capacity))),
            monitor_notifications: Arc::new(Mutex::new(MonitorNotificationState::default())),
            notification_revision,
            retention_policy: Arc::new(AgentRetentionPolicy::default()),
            retention_started: Arc::new(AtomicBool::new(false)),
            retention_notify: Arc::new(Notify::new()),
        }
    }

    #[cfg(test)]
    fn with_retention_policy(policy: AgentRetentionPolicy) -> Self {
        let mut registry = Self::new();
        registry.retention_policy = Arc::new(policy);
        registry
    }

    pub fn escalation_registry(&self) -> EscalationRegistry {
        self.escalation_registry.clone()
    }

    pub fn unnotified_question_escalation_notifications(
        &self,
    ) -> Vec<QuestionEscalationNotification> {
        self.escalation_registry.unnotified_question_notifications()
    }

    pub fn mark_question_escalation_notifications_delivered(&self, ids: &[EscalationId]) {
        self.escalation_registry.mark_notifications_delivered(ids);
    }

    pub fn subscribe_notification_revision(&self) -> watch::Receiver<u64> {
        self.notification_revision.subscribe()
    }

    pub fn notification_revision(&self) -> u64 {
        *self.notification_revision.borrow()
    }

    fn bump_notification_revision(&self) {
        self.notification_revision.send_modify(|revision| {
            *revision = revision.wrapping_add(1);
        });
    }

    fn ensure_retention_supervisor(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self
            .retention_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let inner = Arc::downgrade(&self.inner);
        let removed_ids = Arc::downgrade(&self.removed_ids);
        let removed_order = Arc::downgrade(&self.removed_order);
        let event_journals = Arc::downgrade(&self.event_journals);
        let notify = self.retention_notify.clone();
        let policy = self.retention_policy.clone();
        runtime.spawn(async move {
            loop {
                let Some(inner) = inner.upgrade() else {
                    break;
                };
                let now = Instant::now();
                let (expired, next_deadline) = {
                    let guard = inner.lock().expect("task registry poisoned");
                    let mut expired: HashMap<TaskId, bool> = HashMap::new();
                    let mut next_deadline: Option<Instant> = None;
                    let mut idle_internal_by_owner: HashMap<String, Vec<(Instant, TaskId)>> =
                        HashMap::new();

                    for (id, state) in guard.iter() {
                        let Some(idle_since) = state.runtime.idle_since else {
                            continue;
                        };
                        let Some(ttl) = retention_ttl_for_snapshot(&state.snapshot, &policy) else {
                            continue;
                        };
                        let deadline = idle_since + ttl;
                        if deadline <= now {
                            expired.entry(id.clone()).or_insert(false);
                        } else {
                            next_deadline = Some(
                                next_deadline.map_or(deadline, |current| current.min(deadline)),
                            );
                        }
                        if is_idle_local_agent(&state.snapshot)
                            && !state.snapshot.status.is_terminal()
                            && !notification_is_outstanding(&state.snapshot)
                        {
                            idle_internal_by_owner
                                .entry(state.runtime.owner_session_id.clone().unwrap_or_default())
                                .or_default()
                                .push((idle_since, id.clone()));
                        }
                    }

                    for agents in idle_internal_by_owner.values_mut() {
                        if agents.len() <= policy.max_idle_internal_agents_per_session {
                            continue;
                        }
                        agents.sort_by_key(|(idle_since, _)| *idle_since);
                        let excess = agents.len() - policy.max_idle_internal_agents_per_session;
                        for (_, task_id) in agents.iter().take(excess) {
                            expired.insert(task_id.clone(), true);
                        }
                    }
                    (expired, next_deadline)
                };

                if !expired.is_empty() {
                    let Some(removed_ids) = removed_ids.upgrade() else {
                        break;
                    };
                    let Some(removed_order) = removed_order.upgrade() else {
                        break;
                    };
                    let Some(event_journals) = event_journals.upgrade() else {
                        break;
                    };
                    for (id, remove_for_capacity) in expired {
                        let removal_now = Instant::now();
                        remove_task_from_registry_parts_if(
                            &inner,
                            &removed_ids,
                            &removed_order,
                            &event_journals,
                            &id,
                            |state| {
                                let Some(idle_since) = state.runtime.idle_since else {
                                    return false;
                                };
                                if state.snapshot.status.is_terminal() {
                                    return idle_since + policy.terminal_ttl <= removal_now;
                                }
                                is_idle_local_agent(&state.snapshot)
                                    && !notification_is_outstanding(&state.snapshot)
                                    && (remove_for_capacity
                                        || idle_since + policy.internal_idle_ttl <= removal_now)
                            },
                        );
                    }
                    continue;
                }

                let deadline = next_deadline.unwrap_or_else(|| now + Duration::from_secs(60 * 60));
                drop(inner);
                tokio::select! {
                    _ = notify.notified() => {}
                    _ = tokio::time::sleep_until(deadline) => {}
                }
            }
        });
    }

    pub fn lease_pending_local_agent_messages(
        &self,
        id: &TaskId,
    ) -> Option<LocalAgentMessageLease> {
        let (messages, message_generation) = {
            let mut guard = self.inner.lock().expect("task registry poisoned");
            let state = guard.get_mut(id)?;
            if state.snapshot.status.is_terminal() {
                return None;
            }
            let TaskData::LocalAgent(data) = &mut state.snapshot.data else {
                return None;
            };
            if data.pending_messages.is_empty() {
                return None;
            }
            let messages = std::mem::take(&mut data.pending_messages);
            state.runtime.leased_local_agent_messages = state
                .runtime
                .leased_local_agent_messages
                .saturating_add(messages.len());
            (messages, state.runtime.message_generation)
        };
        Some(LocalAgentMessageLease {
            registry: self.clone(),
            task_id: id.clone(),
            messages,
            message_generation,
            settled: false,
        })
    }

    fn settle_local_agent_message_lease(
        &self,
        id: &TaskId,
        mut messages: Vec<String>,
        restore: bool,
    ) {
        if messages.is_empty() {
            return;
        }
        let mut guard = self.inner.lock().expect("task registry poisoned");
        let Some(state) = guard.get_mut(id) else {
            return;
        };
        state.runtime.leased_local_agent_messages = state
            .runtime
            .leased_local_agent_messages
            .saturating_sub(messages.len());
        if !restore || state.snapshot.status.is_terminal() {
            return;
        }
        let TaskData::LocalAgent(data) = &mut state.snapshot.data else {
            return;
        };
        messages.append(&mut data.pending_messages);
        data.pending_messages = messages;
    }

    pub fn local_agent_message_generation(&self, id: &TaskId) -> Option<u64> {
        self.inner
            .lock()
            .expect("task registry poisoned")
            .get(id)
            .map(|state| state.runtime.message_generation)
    }

    pub fn task_waker(&self, id: &TaskId) -> Option<Arc<Notify>> {
        self.inner
            .lock()
            .expect("task registry poisoned")
            .get(id)
            .map(|task| task.runtime.wake.clone())
    }

    pub fn wake_task(&self, id: &TaskId) -> bool {
        let wake = self.task_waker(id);
        if let Some(wake) = wake {
            wake.notify_one();
            true
        } else {
            false
        }
    }

    pub fn begin_task_turn(&self, id: &TaskId) -> Option<TaskTurnToken> {
        let mut guard = self.inner.lock().expect("task registry poisoned");
        let state = guard.get_mut(id)?;
        if state.snapshot.status.is_terminal() || state.runtime.current_turn_id.is_some() {
            return None;
        }
        if state.snapshot.kind == TaskKind::LocalAgent {
            state.snapshot.status = TaskStatus::Running;
            state.snapshot.end_time_ms = None;
            state.snapshot.notified = false;
            if let Some(metadata) = state.snapshot.metadata.as_object_mut() {
                metadata.insert(LOCAL_AGENT_IDLE_METADATA_KEY.into(), Value::Bool(false));
            }
        }
        let token = state.runtime.begin_turn(id);
        drop(guard);
        self.retention_notify.notify_one();
        Some(token)
    }

    pub fn task_turn_is_active(&self, id: &TaskId) -> bool {
        self.inner
            .lock()
            .expect("task registry poisoned")
            .get(id)
            .is_some_and(|state| state.runtime.current_turn_id.is_some())
    }

    pub fn update_task_turn(
        &self,
        token: &TaskTurnToken,
        update: impl FnOnce(&mut TaskSnapshot),
    ) -> bool {
        let mut guard = self.inner.lock().expect("task registry poisoned");
        let Some(state) = guard.get_mut(&token.task_id) else {
            return false;
        };
        if !state.runtime.matches(token) || state.snapshot.status.is_terminal() {
            return false;
        }
        update(&mut state.snapshot);
        true
    }

    pub fn record_task_turn_event(&self, token: &TaskTurnToken, kind: TaskLiveEventKind) -> bool {
        let guard = self.inner.lock().expect("task registry poisoned");
        let Some(state) = guard.get(&token.task_id) else {
            return false;
        };
        if !state.runtime.matches(token) || state.snapshot.status.is_terminal() {
            return false;
        }
        self.event_journals
            .lock()
            .expect("task event journals poisoned")
            .record(&token.task_id, kind);
        true
    }

    pub fn record_task_turn_terminal_event(
        &self,
        token: &TaskTurnToken,
        kind: TaskLiveEventKind,
    ) -> bool {
        let guard = self.inner.lock().expect("task registry poisoned");
        let Some(state) = guard.get(&token.task_id) else {
            return false;
        };
        if !state.runtime.matches(token) {
            return false;
        }
        self.event_journals
            .lock()
            .expect("task event journals poisoned")
            .record(&token.task_id, kind);
        true
    }

    pub fn finish_task_turn(&self, token: &TaskTurnToken) -> bool {
        let mut guard = self.inner.lock().expect("task registry poisoned");
        let Some(state) = guard.get_mut(&token.task_id) else {
            return false;
        };
        if !state.runtime.matches(token) {
            return false;
        }
        state.runtime.current_turn_id = None;
        state.runtime.idle_since = Some(Instant::now());
        if state.snapshot.kind == TaskKind::LocalAgent {
            state.snapshot.status = TaskStatus::Running;
            state.snapshot.end_time_ms = None;
            if let Some(metadata) = state.snapshot.metadata.as_object_mut() {
                metadata.insert(LOCAL_AGENT_IDLE_METADATA_KEY.into(), Value::Bool(true));
            }
        }
        let notification_ready = terminal_notification_ready(&state.snapshot);
        let wake = state.runtime.wake.clone();
        drop(guard);
        wake.notify_one();
        self.retention_notify.notify_one();
        if notification_ready {
            self.bump_notification_revision();
        }
        true
    }

    pub fn finish_terminal_task_turn(&self, token: &TaskTurnToken) -> bool {
        let mut guard = self.inner.lock().expect("task registry poisoned");
        let Some(state) = guard.get_mut(&token.task_id) else {
            return false;
        };
        if !state.runtime.matches(token) {
            return false;
        }
        state.runtime.current_turn_id = None;
        state.runtime.idle_since = Some(Instant::now());
        let notification_ready = terminal_notification_ready(&state.snapshot);
        let wake = state.runtime.wake.clone();
        drop(guard);
        wake.notify_one();
        self.retention_notify.notify_one();
        if notification_ready {
            self.bump_notification_revision();
        }
        true
    }

    pub fn fail_active_task_turn(&self, id: &TaskId, error: String) -> bool {
        let mut guard = self.inner.lock().expect("task registry poisoned");
        let Some(state) = guard.get_mut(id) else {
            return false;
        };
        if state.runtime.current_turn_id.is_none() || state.snapshot.status.is_terminal() {
            return false;
        }
        state.runtime.current_turn_id = None;
        state.runtime.idle_since = Some(Instant::now());
        state.snapshot.status = TaskStatus::Running;
        state.snapshot.error = Some(error.clone());
        state.snapshot.last_progress = Some(error.clone());
        state.snapshot.result = Some(serde_json::json!({
            "status": "failed",
            "final_text": error,
            "tool_call_count": 0,
            "total_tokens": 0,
        }));
        state.snapshot.end_time_ms = None;
        state.snapshot.notified = false;
        if let Some(metadata) = state.snapshot.metadata.as_object_mut() {
            metadata.insert(LOCAL_AGENT_IDLE_METADATA_KEY.into(), Value::Bool(true));
        }
        let notification_ready = terminal_notification_ready(&state.snapshot);
        let wake = state.runtime.wake.clone();
        drop(guard);
        wake.notify_one();
        self.retention_notify.notify_one();
        if notification_ready {
            self.bump_notification_revision();
        }
        true
    }

    pub fn fail_idle_task_turn(&self, id: &TaskId, error: String) -> bool {
        let mut guard = self.inner.lock().expect("task registry poisoned");
        let Some(state) = guard.get_mut(id) else {
            return false;
        };
        if state.runtime.current_turn_id.is_some() || state.snapshot.status.is_terminal() {
            return false;
        }
        state.runtime.notification_generation =
            state.runtime.notification_generation.saturating_add(1);
        state.runtime.idle_since = Some(Instant::now());
        state.snapshot.status = TaskStatus::Running;
        state.snapshot.error = Some(error.clone());
        state.snapshot.last_progress = Some(error.clone());
        state.snapshot.result = Some(serde_json::json!({
            "status": "failed",
            "final_text": error,
            "tool_call_count": 0,
            "total_tokens": 0,
        }));
        state.snapshot.end_time_ms = None;
        state.snapshot.notified = false;
        if let Some(metadata) = state.snapshot.metadata.as_object_mut() {
            metadata.insert(LOCAL_AGENT_IDLE_METADATA_KEY.into(), Value::Bool(true));
        }
        let notification_ready = terminal_notification_ready(&state.snapshot);
        drop(guard);
        self.retention_notify.notify_one();
        if notification_ready {
            self.bump_notification_revision();
        }
        true
    }

    pub fn close_owner_session(&self, owner_session_id: &str) -> usize {
        let ids = {
            let guard = self.inner.lock().expect("task registry poisoned");
            guard
                .iter()
                .filter_map(|(id, state)| {
                    (state.runtime.owner_session_id.as_deref() == Some(owner_session_id))
                        .then(|| id.clone())
                })
                .collect::<Vec<_>>()
        };
        for id in &ids {
            self.escalation_registry
                .cancel_agent(id.as_str(), "owner session closed");
            self.remove(id);
        }
        if let Err(error) = remove_session_default_team(owner_session_id) {
            tracing::warn!(
                session_id = owner_session_id,
                %error,
                "failed to remove session default team during teardown"
            );
        }
        ids.len()
    }

    fn reserve_local_workflow_run(
        &self,
        run_id: &str,
        task_id: &TaskId,
    ) -> Result<(), LocalWorkflowRegistrationError> {
        let mut active = self
            .active_workflow_runs
            .lock()
            .expect("workflow runs poisoned");
        if let Some(owner_task_id) = active.get(run_id) {
            return Err(LocalWorkflowRegistrationError::RunIdAlreadyRegistered {
                run_id: run_id.into(),
                owner_task_id: owner_task_id.clone(),
            });
        }
        active.insert(run_id.into(), task_id.clone());
        Ok(())
    }
    fn release_local_workflow_run(&self, run_id: &str, task_id: &TaskId) -> bool {
        let mut active = self
            .active_workflow_runs
            .lock()
            .expect("workflow runs poisoned");
        if active.get(run_id) != Some(task_id) {
            return false;
        }
        active.remove(run_id);
        true
    }

    /// Insert a task, keyed by its id.
    ///
    /// Public so that downstream crates can populate
    /// the registry in unit tests. Production code should use the
    /// `spawn_*_task` helpers which call this internally.
    pub fn insert(&self, id: TaskId, snapshot: TaskSnapshot, cancel: PromptCancel) {
        let notification_ready = terminal_notification_ready(&snapshot);
        let removed_ids = self
            .removed_ids
            .lock()
            .expect("task registry tombstones poisoned");
        if removed_ids.contains(&id) {
            return;
        }
        let mut guard = self.inner.lock().expect("task registry poisoned");
        if guard
            .get(&id)
            .is_some_and(|entry| entry.snapshot.status == TaskStatus::Killed)
        {
            return;
        }
        let (background_request, _) = watch::channel(snapshot.is_backgrounded);
        let runtime = TaskRuntimeState::new(&snapshot);
        guard.insert(
            id.clone(),
            TaskState {
                snapshot,
                cancel,
                background_request,
                runtime,
            },
        );
        drop(guard);
        self.ensure_retention_supervisor();
        self.retention_notify.notify_one();
        self.event_journals
            .lock()
            .expect("task event journals poisoned")
            .ensure_task(id);
        if notification_ready {
            self.bump_notification_revision();
        }
    }

    /// Update the status + optional progress blurb of a task.
    ///
    /// Exposed so coordinator spawners can mutate lifecycle fields after
    /// their worker terminates without routing every mutation through a
    /// per-field setter. Runtime helpers use this primitive too. A killed
    /// task cannot transition back to another lifecycle state when an
    /// asynchronous callback arrives late.
    pub fn update<F: FnOnce(&mut TaskSnapshot)>(&self, id: &TaskId, update: F) {
        let mut retention_changed = false;
        let mut notification_changed = false;
        let mut guard = self.inner.lock().expect("task registry poisoned");
        if let Some(entry) = guard.get_mut(id) {
            let was_killed = entry.snapshot.status == TaskStatus::Killed;
            let was_idle = task_is_idle(&entry.snapshot);
            let was_terminal = entry.snapshot.status.is_terminal();
            let was_notification_ready = terminal_notification_ready(&entry.snapshot);
            update(&mut entry.snapshot);
            if was_killed {
                entry.snapshot.status = TaskStatus::Killed;
            }
            let is_idle = task_is_idle(&entry.snapshot);
            let is_terminal = entry.snapshot.status.is_terminal();
            let is_notification_ready = terminal_notification_ready(&entry.snapshot);
            notification_changed = was_notification_ready != is_notification_ready;
            if (!was_idle && is_idle) || (!was_terminal && is_terminal) {
                entry.runtime.idle_since = Some(Instant::now());
                retention_changed = true;
            } else if (was_idle && !is_idle) || (was_terminal && !is_terminal) {
                entry.runtime.idle_since = None;
                retention_changed = true;
            }
        }
        drop(guard);
        if retention_changed {
            self.retention_notify.notify_one();
        }
        if notification_changed {
            self.bump_notification_revision();
        }
    }

    /// Take the current snapshot of a single task.
    pub fn snapshot(&self, id: &TaskId) -> Option<TaskSnapshot> {
        let guard = self.inner.lock().expect("task registry poisoned");
        guard.get(id).map(|t| t.snapshot.clone())
    }

    /// Take a snapshot of every registered task, in insertion order
    /// undefined (callers that need a stable order should sort).
    pub fn snapshots(&self) -> Vec<TaskSnapshot> {
        let guard = self.inner.lock().expect("task registry poisoned");
        guard.values().map(|t| t.snapshot.clone()).collect()
    }

    /// Read retained live events for one task. Events are returned strictly
    /// after `after`; pass `None` for the oldest retained event. Returns
    /// `None` when the task has no journal (unknown or removed task).
    pub fn task_live_events(
        &self,
        id: &TaskId,
        after: Option<TaskEventCursor>,
    ) -> Option<TaskLiveEventBatch> {
        self.event_journals
            .lock()
            .expect("task event journals poisoned")
            .tasks
            .get(id)
            .map(|journal| journal.read(after))
    }

    /// Read the merged registry-session live stream across all tasks. Events
    /// carry their [`TaskLiveEvent::task_id`] and share one monotonic cursor,
    /// so an app can maintain a single poll cursor for the whole session.
    pub fn session_live_events(&self, after: Option<TaskEventCursor>) -> TaskLiveEventBatch {
        self.event_journals
            .lock()
            .expect("task event journals poisoned")
            .session
            .read(after)
    }

    /// Append a live journal event for `id`.
    ///
    /// Public so out-of-crate bridges (e.g. background job task-store
    /// persistence) and their tests can emit the same events production
    /// spawners write. Tombstoned task ids are ignored.
    pub fn record_live_event(&self, id: &TaskId, kind: TaskLiveEventKind) {
        // Drop events for tombstoned ids so a late callback cannot resurrect a
        // journal that `remove` cleared — mirrors the tombstone guard in
        // `insert`. Without it `TaskEventJournals::record`'s `ensure_task`
        // re-creates the entry, leaking it and breaking the documented
        // None-for-removed contract of `task_live_events`. Hold `removed_ids`
        // across the journal write (same lock order as `insert` / `remove`:
        // removed_ids → event_journals) so a concurrent `remove` can't slip in
        // between the check and the record.
        let removed_ids = self
            .removed_ids
            .lock()
            .expect("task registry tombstones poisoned");
        if removed_ids.contains(id) {
            return;
        }
        self.event_journals
            .lock()
            .expect("task event journals poisoned")
            .record(id, kind);
    }

    /// Number of registered tasks.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("task registry poisoned").len()
    }

    /// True when the registry has no tasks.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Trip the cancel handle for a specific task. No-op if the task
    /// is unknown or already terminal.
    pub fn cancel(&self, id: &TaskId) -> bool {
        let mut guard = self.inner.lock().expect("task registry poisoned");
        match guard.get_mut(id) {
            Some(task) => {
                task.runtime.invalidate();
                task.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Clone the cancellation handle for a registered task.
    pub fn cancel_handle(&self, id: &TaskId) -> Option<PromptCancel> {
        let guard = self.inner.lock().expect("task registry poisoned");
        guard.get(id).map(|t| t.cancel.clone())
    }

    /// Remove a task from the registry. Callers typically invoke
    /// this after observing a terminal snapshot via
    /// [`TaskObservation::Finished`].
    pub fn remove(&self, id: &TaskId) -> Option<TaskSnapshot> {
        let removed = remove_task_from_registry_parts(
            &self.inner,
            &self.removed_ids,
            &self.removed_order,
            &self.event_journals,
            id,
        );
        if removed.is_some() {
            let removed_monitor_notifications = self
                .monitor_notifications
                .lock()
                .expect("monitor notifications poisoned")
                .tasks
                .remove(id)
                .is_some();
            self.retention_notify.notify_one();
            if removed_monitor_notifications {
                self.bump_notification_revision();
            }
        }
        removed
    }

    /// Clone a receiver that fires when a registered foreground task is
    /// requested to detach into the background.
    pub fn background_request_receiver(&self, id: &TaskId) -> Option<watch::Receiver<bool>> {
        let guard = self.inner.lock().expect("task registry poisoned");
        guard
            .get(id)
            .map(|task| task.background_request.subscribe())
    }

    /// Mark a task as backgrounded. Flips the backgrounded flag and
    /// tells the task it was asked to detach.
    pub fn set_backgrounded(&self, id: &TaskId) -> bool {
        let mut guard = self.inner.lock().expect("task registry poisoned");
        let Some(entry) = guard.get_mut(id) else {
            return false;
        };
        let was_backgrounded = entry.snapshot.is_backgrounded;
        let was_terminal_ready = terminal_notification_ready(&entry.snapshot);
        entry.snapshot.is_backgrounded = true;
        let is_terminal_ready = terminal_notification_ready(&entry.snapshot);
        let is_monitor = entry.snapshot.kind == TaskKind::Monitor;
        let _ = entry.background_request.send(true);
        drop(guard);

        let monitor_has_pending = is_monitor
            && self
                .monitor_notifications
                .lock()
                .expect("monitor notifications poisoned")
                .tasks
                .get(id)
                .is_some_and(|state| !state.events.is_empty());
        if was_terminal_ready != is_terminal_ready || (!was_backgrounded && monitor_has_pending) {
            self.bump_notification_revision();
        }
        true
    }

    pub fn enqueue_monitor_event(
        &self,
        id: &TaskId,
        content: impl Into<String>,
    ) -> MonitorEventEnqueueOutcome {
        let content = truncate_monitor_event(content.into().trim());
        if content.is_empty() {
            return MonitorEventEnqueueOutcome::Ignored;
        }

        let now = Instant::now();
        let mut guard = self.inner.lock().expect("task registry poisoned");
        let Some(entry) = guard.get_mut(id) else {
            return MonitorEventEnqueueOutcome::Ignored;
        };
        if entry.snapshot.kind != TaskKind::Monitor || entry.snapshot.status.is_terminal() {
            return MonitorEventEnqueueOutcome::Ignored;
        }
        let is_backgrounded = entry.snapshot.is_backgrounded;
        let TaskData::Monitor(data) = &mut entry.snapshot.data else {
            return MonitorEventEnqueueOutcome::Ignored;
        };

        let mut notifications = self
            .monitor_notifications
            .lock()
            .expect("monitor notifications poisoned");
        let state = notifications.tasks.entry(id.clone()).or_default();
        while state
            .recent_event_times
            .front()
            .is_some_and(|timestamp| now.duration_since(*timestamp) > MONITOR_BURST_WINDOW)
        {
            state.recent_event_times.pop_front();
        }

        let in_burst = state.recent_event_times.len() >= MONITOR_BURST_MAX_KEPT;
        if in_burst {
            state.flood_started_at.get_or_insert(now);
        } else {
            state.flood_started_at = None;
        }
        state.recent_event_times.push_back(now);
        state.next_sequence = state.next_sequence.saturating_add(1);
        let sequence = state.next_sequence;
        state.events.push_back(MonitorQueuedEvent {
            sequence,
            received_at: now,
            content: Some(content),
        });

        let mut suppressed = false;
        if in_burst {
            if let Some(event) = state.events.iter_mut().find(|event| {
                event.content.is_some()
                    && now.duration_since(event.received_at) <= MONITOR_BURST_WINDOW
            }) {
                event.content = None;
                suppressed = true;
                data.suppressed_count = data.suppressed_count.saturating_add(1);
            }
        }
        data.event_count = data.event_count.saturating_add(1);
        let auto_stop = state.events.len() >= MONITOR_PENDING_HARD_LIMIT
            || state
                .flood_started_at
                .is_some_and(|started| now.duration_since(started) >= MONITOR_BURST_MAX_DURATION);
        drop(notifications);
        drop(guard);

        if is_backgrounded {
            self.bump_notification_revision();
        }
        if auto_stop {
            MonitorEventEnqueueOutcome::AutoStop { sequence }
        } else if suppressed {
            MonitorEventEnqueueOutcome::Suppressed { sequence }
        } else {
            MonitorEventEnqueueOutcome::Queued { sequence }
        }
    }

    /// Collect terminal task outcomes without consuming them.
    pub fn unnotified_terminal_notifications(&self) -> Vec<TaskNotification> {
        let guard = self.inner.lock().expect("task registry poisoned");
        let mut snapshots: Vec<(TaskSnapshot, u64)> = guard
            .values()
            .filter_map(|state| {
                terminal_notification_ready(&state.snapshot).then(|| {
                    (
                        state.snapshot.clone(),
                        state.runtime.notification_generation,
                    )
                })
            })
            .collect();
        drop(guard);
        snapshots.sort_by_key(|(snapshot, _)| snapshot.start_time_ms);

        snapshots
            .into_iter()
            .map(|(snapshot, generation)| {
                let message = match snapshot.kind {
                    TaskKind::LocalWorkflow => build_local_workflow_notification_xml(&snapshot),
                    TaskKind::LocalShell => build_local_shell_notification_xml(&snapshot),
                    TaskKind::Monitor => {
                        let notifications = self
                            .monitor_notifications
                            .lock()
                            .expect("monitor notifications poisoned");
                        let events = notifications
                            .tasks
                            .get(&snapshot.id)
                            .map(|state| &state.events);
                        build_monitor_terminal_notification_xml(&snapshot, events)
                    }
                    _ => build_local_agent_notification_xml(&snapshot),
                };
                let output_file = notification_output_file(&snapshot).map(str::to_string);
                TaskNotification {
                    task_id: snapshot.id,
                    generation,
                    kind: TaskNotificationKind::Terminal,
                    message,
                    output_file,
                }
            })
            .collect()
    }

    pub fn unnotified_notifications(&self) -> Vec<TaskNotification> {
        let mut notifications = self.unnotified_terminal_notifications();
        notifications.extend(self.unnotified_monitor_notifications());
        notifications
    }

    fn unnotified_monitor_notifications(&self) -> Vec<TaskNotification> {
        let mut snapshots = self
            .inner
            .lock()
            .expect("task registry poisoned")
            .values()
            .filter_map(|state| {
                let snapshot = &state.snapshot;
                (snapshot.kind == TaskKind::Monitor
                    && !snapshot.status.is_terminal()
                    && snapshot.is_backgrounded)
                    .then(|| snapshot.clone())
            })
            .collect::<Vec<_>>();
        snapshots.sort_by_key(|snapshot| snapshot.start_time_ms);

        let notifications = self
            .monitor_notifications
            .lock()
            .expect("monitor notifications poisoned");
        snapshots
            .into_iter()
            .filter_map(|snapshot| {
                let state = notifications.tasks.get(&snapshot.id)?;
                let generation = state.events.back()?.sequence;
                Some(TaskNotification {
                    task_id: snapshot.id.clone(),
                    generation,
                    kind: TaskNotificationKind::MonitorEvent,
                    message: build_monitor_event_notification_xml(&snapshot, &state.events),
                    output_file: None,
                })
            })
            .collect()
    }

    pub fn unnotified_terminal_agent_notifications(&self) -> Vec<TaskNotification> {
        self.unnotified_terminal_notifications()
    }

    pub fn notification_claim_is_current(&self, claim: &TaskNotificationClaim) -> bool {
        match claim.kind {
            TaskNotificationKind::Terminal => {
                let guard = self.inner.lock().expect("task registry poisoned");
                guard.get(&claim.task_id).is_some_and(|state| {
                    terminal_notification_ready(&state.snapshot)
                        && state.runtime.notification_generation == claim.generation
                })
            }
            TaskNotificationKind::MonitorEvent => {
                let monitor_is_ready = self
                    .inner
                    .lock()
                    .expect("task registry poisoned")
                    .get(&claim.task_id)
                    .is_some_and(|state| {
                        state.snapshot.kind == TaskKind::Monitor
                            && !state.snapshot.status.is_terminal()
                            && state.snapshot.is_backgrounded
                    });
                monitor_is_ready
                    && self
                        .monitor_notifications
                        .lock()
                        .expect("monitor notifications poisoned")
                        .tasks
                        .get(&claim.task_id)
                        .is_some_and(|state| {
                            state.events.front().is_some_and(|event| {
                                event.sequence <= claim.generation
                                    && state
                                        .events
                                        .back()
                                        .is_some_and(|last| claim.generation <= last.sequence)
                            })
                        })
            }
        }
    }

    pub fn notification_generation_is_current(&self, id: &TaskId, generation: u64) -> bool {
        self.notification_claim_is_current(&TaskNotificationClaim {
            task_id: id.clone(),
            generation,
            kind: TaskNotificationKind::Terminal,
        })
    }

    pub fn mark_notification_claims_delivered(&self, claims: &[TaskNotificationClaim]) {
        self.set_notification_claims(claims, true);
    }

    pub fn mark_notification_claims_undelivered(&self, claims: &[TaskNotificationClaim]) {
        self.set_notification_claims(claims, false);
    }

    fn set_notification_claims(&self, claims: &[TaskNotificationClaim], delivered: bool) {
        if claims.is_empty() {
            return;
        }

        let terminal_claims = claims
            .iter()
            .filter(|claim| claim.kind == TaskNotificationKind::Terminal)
            .collect::<Vec<_>>();
        let mut changed = false;
        let mut delivered_terminal_monitors = Vec::new();
        if !terminal_claims.is_empty() {
            let mut guard = self.inner.lock().expect("task registry poisoned");
            for claim in terminal_claims {
                if let Some(entry) = guard.get_mut(&claim.task_id) {
                    if entry.runtime.notification_generation == claim.generation {
                        if delivered {
                            if !entry.snapshot.notified {
                                entry.snapshot.notified = true;
                                changed = true;
                            }
                            if entry.snapshot.kind == TaskKind::Monitor {
                                delivered_terminal_monitors.push(claim.task_id.clone());
                            }
                        } else {
                            entry.snapshot.notified = false;
                            changed |= terminal_notification_ready(&entry.snapshot);
                        }
                    }
                }
            }
        }
        if !delivered_terminal_monitors.is_empty() {
            let mut notifications = self
                .monitor_notifications
                .lock()
                .expect("monitor notifications poisoned");
            for task_id in delivered_terminal_monitors {
                notifications.tasks.remove(&task_id);
            }
        }

        let monitor_claims = claims
            .iter()
            .filter(|claim| claim.kind == TaskNotificationKind::MonitorEvent)
            .collect::<Vec<_>>();
        if !monitor_claims.is_empty() {
            let mut notifications = self
                .monitor_notifications
                .lock()
                .expect("monitor notifications poisoned");
            for claim in monitor_claims {
                let Some(state) = notifications.tasks.get_mut(&claim.task_id) else {
                    continue;
                };
                if delivered {
                    let original_len = state.events.len();
                    while state
                        .events
                        .front()
                        .is_some_and(|event| event.sequence <= claim.generation)
                    {
                        state.events.pop_front();
                    }
                    changed |= state.events.len() != original_len;
                } else if state
                    .events
                    .front()
                    .is_some_and(|event| event.sequence <= claim.generation)
                {
                    changed = true;
                }
            }
        }

        if changed {
            self.bump_notification_revision();
        }
    }

    pub fn mark_notification_generations_delivered(&self, claims: &[(TaskId, u64)]) {
        let claims = claims
            .iter()
            .map(|(task_id, generation)| TaskNotificationClaim {
                task_id: task_id.clone(),
                generation: *generation,
                kind: TaskNotificationKind::Terminal,
            })
            .collect::<Vec<_>>();
        self.mark_notification_claims_delivered(&claims);
    }

    pub fn mark_notification_generations_undelivered(&self, claims: &[(TaskId, u64)]) {
        let claims = claims
            .iter()
            .map(|(task_id, generation)| TaskNotificationClaim {
                task_id: task_id.clone(),
                generation: *generation,
                kind: TaskNotificationKind::Terminal,
            })
            .collect::<Vec<_>>();
        self.mark_notification_claims_undelivered(&claims);
    }

    /// Mark already-delivered terminal notifications as surfaced.
    pub fn mark_notifications_delivered(&self, ids: &[TaskId]) {
        if ids.is_empty() {
            return;
        }
        let mut changed = false;
        let mut guard = self.inner.lock().expect("task registry poisoned");
        for id in ids {
            if let Some(entry) = guard.get_mut(id) {
                if !entry.snapshot.notified {
                    entry.snapshot.notified = true;
                    changed = true;
                }
            }
        }
        drop(guard);
        if changed {
            self.bump_notification_revision();
        }
    }

    /// Release delivery acknowledgements for a failed notification turn.
    pub fn mark_notifications_undelivered(&self, ids: &[TaskId]) {
        if ids.is_empty() {
            return;
        }
        let mut changed = false;
        let mut guard = self.inner.lock().expect("task registry poisoned");
        for id in ids {
            if let Some(entry) = guard.get_mut(id) {
                entry.snapshot.notified = false;
                changed |= terminal_notification_ready(&entry.snapshot);
            }
        }
        drop(guard);
        if changed {
            self.bump_notification_revision();
        }
    }

    /// Back-compat helper for callers that truly want take-and-ack semantics.
    pub fn take_unnotified_terminal_notifications(&self) -> Vec<String> {
        let notifications = self.unnotified_terminal_notifications();
        let claims = notifications
            .iter()
            .map(|notification| (notification.task_id.clone(), notification.generation))
            .collect::<Vec<_>>();
        self.mark_notification_generations_delivered(&claims);
        notifications.into_iter().map(|n| n.message).collect()
    }
    pub fn take_unnotified_terminal_agent_notifications(&self) -> Vec<String> {
        self.take_unnotified_terminal_notifications()
    }
}

fn truncate_monitor_text(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars();
    let head = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{head}...(truncated)")
    } else {
        head
    }
}

fn truncate_monitor_event(input: &str) -> String {
    truncate_monitor_text(input, MONITOR_EVENT_MAX_CHARS)
}

fn build_monitor_event_notification_xml(
    snap: &TaskSnapshot,
    events: &VecDeque<MonitorQueuedEvent>,
) -> String {
    let description = match &snap.data {
        TaskData::Monitor(data) => data.description.as_str(),
        _ => snap.title.as_str(),
    };
    let kept = events
        .iter()
        .filter_map(|event| event.content.as_deref())
        .collect::<Vec<_>>();
    let suppressed = events.len().saturating_sub(kept.len());
    let body = if kept.is_empty() {
        "Monitor events were suppressed because the source is too noisy.".to_string()
    } else {
        truncate_monitor_text(&kept.join("\n"), MONITOR_NOTIFICATION_MAX_CHARS)
    };
    let mut message = format!(
        "<task-notification>\n<task-id>{}</task-id>\n<task-type>monitor</task-type>\n<status>event</status>\n<summary>Monitor event: &quot;{}&quot;</summary>\n<event>{}</event>",
        xml_escape(snap.id.as_str()),
        xml_escape(description),
        xml_escape(&body),
    );
    if suppressed > 0 {
        message.push_str(&format!(
            "\n<suppressed>{suppressed} earlier events in this burst omitted. Restart with a more selective source.</suppressed>"
        ));
    }
    message.push_str("\n</task-notification>");
    message
}

fn build_monitor_terminal_notification_xml(
    snap: &TaskSnapshot,
    pending_events: Option<&VecDeque<MonitorQueuedEvent>>,
) -> String {
    let (description, source, target, event_count, suppressed_count, end_reason) = match &snap.data
    {
        TaskData::Monitor(data) => (
            data.description.as_str(),
            data.source.as_str(),
            data.redacted_target.as_str(),
            data.event_count,
            data.suppressed_count,
            data.end_reason.map(MonitorEndReason::as_str),
        ),
        _ => (snap.title.as_str(), "unknown", "redacted", 0, 0, None),
    };
    let status = end_reason.unwrap_or_else(|| snap.status.as_str());
    let summary = match status {
        "exited" | "closed" | "completed" => {
            format!("Monitor \"{description}\" stream ended")
        }
        "stopped" | "killed" => format!("Monitor \"{description}\" was stopped"),
        "timed_out" => format!("Monitor \"{description}\" timed out"),
        "auto_stopped" => format!(
            "Monitor \"{description}\" was automatically stopped because it produced too many events"
        ),
        _ => format!(
            "Monitor \"{description}\" failed: {}",
            snap.error.as_deref().unwrap_or("unknown error")
        ),
    };
    let mut message = format!(
        "<task-notification>\n<task-id>{}</task-id>\n<task-type>monitor</task-type>\n<status>{}</status>\n<summary>{}</summary>\n<description>{}</description>\n<source>{}</source>\n<target>{}</target>\n<event-count>{event_count}</event-count>\n<suppressed-count>{suppressed_count}</suppressed-count>",
        xml_escape(snap.id.as_str()),
        xml_escape(status),
        xml_escape(&summary),
        xml_escape(description),
        xml_escape(source),
        xml_escape(target),
    );
    if let Some(events) = pending_events.filter(|events| !events.is_empty()) {
        let kept = events
            .iter()
            .filter_map(|event| event.content.as_deref())
            .collect::<Vec<_>>();
        let suppressed = events.len().saturating_sub(kept.len());
        let body = if kept.is_empty() {
            "Monitor events were suppressed because the source is too noisy.".to_string()
        } else {
            truncate_monitor_text(&kept.join("\n"), MONITOR_NOTIFICATION_MAX_CHARS)
        };
        message.push_str(&format!("\n<event>{}</event>", xml_escape(&body)));
        if suppressed > 0 {
            message.push_str(&format!(
                "\n<suppressed>{suppressed} earlier events in this burst omitted. Restart with a more selective source.</suppressed>"
            ));
        }
    }
    if let Some(error) = snap.error.as_deref().filter(|error| !error.is_empty()) {
        message.push_str(&format!("\n<error>{}</error>", xml_escape(error)));
    }
    message.push_str("\n</task-notification>");
    message
}

fn notification_output_file(snap: &TaskSnapshot) -> Option<&str> {
    match snap.kind {
        TaskKind::LocalAgent => local_agent_output_file(snap),
        TaskKind::LocalWorkflow => local_workflow_output_path(snap),
        _ => None,
    }
}

fn local_workflow_output_path(snap: &TaskSnapshot) -> Option<&str> {
    snap.result
        .as_ref()
        .and_then(|v| v.get("outputPath").or_else(|| v.get("output_path")))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| match &snap.data {
            TaskData::LocalWorkflow(data) => data.output_path.as_deref().filter(|s| !s.is_empty()),
            _ => None,
        })
}

fn local_workflow_script_path(snap: &TaskSnapshot) -> Option<&str> {
    snap.result
        .as_ref()
        .and_then(|v| v.get("scriptPath").or_else(|| v.get("script_path")))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| match &snap.data {
            TaskData::LocalWorkflow(data) => data.script_path.as_deref().filter(|s| !s.is_empty()),
            _ => None,
        })
}

fn local_agent_output_file(snap: &TaskSnapshot) -> Option<&str> {
    snap.result
        .as_ref()
        .and_then(|v| v.get("output_file"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn build_local_shell_notification_xml(snap: &TaskSnapshot) -> String {
    let command = match &snap.data {
        TaskData::LocalShell(data) => data.command.as_str(),
        _ => snap.title.as_str(),
    };
    let result = snap.result.as_ref();
    let shell_status = result
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| snap.status.as_str());
    let exit_code = result
        .and_then(|value| value.get("exitCode"))
        .and_then(Value::as_i64);
    let summary = match (shell_status, exit_code) {
        ("exited", Some(code)) => format!("Background shell exited with code {code}"),
        ("timed_out", _) => "Background shell timed out".to_string(),
        ("stopped", _) => "Background shell was stopped".to_string(),
        ("failed", _) => format!(
            "Background shell failed: {}",
            snap.error.as_deref().unwrap_or("Unknown error")
        ),
        _ => format!("Background shell finished with status {shell_status}"),
    };
    let mut message = format!(
        "<task-notification>\n<task-id>{}</task-id>\n<task-type>local_shell</task-type>\n<shell-id>{}</shell-id>\n<status>{}</status>\n<summary>{}</summary>\n<command>{}</command>",
        xml_escape(snap.id.as_str()),
        xml_escape(snap.id.as_str()),
        xml_escape(shell_status),
        xml_escape(&summary),
        xml_escape(command),
    );
    if let Some(exit_code) = exit_code {
        message.push_str(&format!("\n<exit-code>{exit_code}</exit-code>"));
    }
    for (json_key, xml_key) in [
        ("output", "output"),
        ("stderr", "stderr"),
        ("error", "error"),
    ] {
        if let Some(text) = result
            .and_then(|value| value.get(json_key))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            message.push_str(&format!("\n<{xml_key}>{}</{xml_key}>", xml_escape(text)));
        }
    }
    if result
        .and_then(|value| value.get("cursorTruncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        message.push_str("\n<cursor-truncated>true</cursor-truncated>");
        if let Some(oldest_cursor) = result
            .and_then(|value| value.get("oldestCursor"))
            .and_then(Value::as_u64)
        {
            message.push_str(&format!("\n<oldest-cursor>{oldest_cursor}</oldest-cursor>"));
        }
    }
    if result
        .and_then(|value| value.get("hasMore"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        message.push_str("\n<has-more>true</has-more>");
        if let Some(next_cursor) = result
            .and_then(|value| value.get("nextCursor"))
            .and_then(Value::as_u64)
        {
            message.push_str(&format!("\n<next-cursor>{next_cursor}</next-cursor>"));
        }
    }
    if let Some(duration_ms) = snap
        .end_time_ms
        .map(|end| end.saturating_sub(snap.start_time_ms))
    {
        message.push_str(&format!("\n<duration-ms>{duration_ms}</duration-ms>"));
    }
    message.push_str("\n</task-notification>");
    message
}

fn build_local_workflow_notification_xml(snap: &TaskSnapshot) -> String {
    let status = snap.status.as_str();
    let (workflow_name, run_id, agent_count, total_tokens, tool_uses, output_path, script_path) =
        match &snap.data {
            TaskData::LocalWorkflow(data) => (
                data.workflow_name.as_str(),
                data.run_id.as_str(),
                data.agent_count,
                data.token_count,
                data.tool_use_count,
                data.output_path.as_deref(),
                data.script_path.as_deref(),
            ),
            _ => (snap.title.as_str(), "", 0, 0, 0, None, None),
        };
    let summary = match snap.status {
        TaskStatus::Completed => format!("Workflow \"{}\" completed", snap.title),
        TaskStatus::Failed => format!(
            "Workflow \"{}\" failed: {}",
            snap.title,
            snap.error.as_deref().unwrap_or("Unknown error")
        ),
        TaskStatus::Killed => format!("Workflow \"{}\" was stopped", snap.title),
        TaskStatus::Pending | TaskStatus::Running => {
            format!("Workflow \"{}\" is {}", snap.title, snap.status.as_str())
        }
    };
    let result_text = snap
        .result
        .as_ref()
        .map(|value| serde_json::to_string(value).unwrap_or_default())
        .filter(|value| !value.is_empty())
        .or(snap.last_progress.clone());
    let duration_ms = snap
        .end_time_ms
        .map(|end| end.saturating_sub(snap.start_time_ms));
    let mut message = format!(
        "<task-notification>\n<task-id>{}</task-id>\n<task-type>local_workflow</task-type>",
        xml_escape(snap.id.as_str())
    );
    if let Some(output_path) = local_workflow_output_path(snap).or(output_path) {
        message.push_str(&format!(
            "\n<output-file>{}</output-file>",
            xml_escape(output_path)
        ));
    }
    message.push_str(&format!(
        "\n<workflow><run_id>{}</run_id><name>{}</name>",
        xml_escape(run_id),
        xml_escape(workflow_name),
    ));
    if let Some(script_path) = local_workflow_script_path(snap).or(script_path) {
        message.push_str(&format!(
            "<script_path>{}</script_path>",
            xml_escape(script_path)
        ));
    }
    message.push_str(&format!(
        "<agent_count>{agent_count}</agent_count></workflow>"
    ));
    message.push_str(&format!(
        "\n<status>{}</status>\n<summary>{}</summary>",
        xml_escape(status),
        xml_escape(&summary)
    ));
    if let Some(result_text) = result_text.as_deref() {
        message.push_str(&format!("\n<result>{}</result>", xml_escape(result_text)));
    }
    if total_tokens > 0 || tool_uses > 0 || duration_ms.is_some() {
        message.push_str("\n<usage>");
        if total_tokens > 0 {
            message.push_str(&format!("<total_tokens>{total_tokens}</total_tokens>"));
        }
        if tool_uses > 0 {
            message.push_str(&format!("<tool_uses>{tool_uses}</tool_uses>"));
        }
        if let Some(duration_ms) = duration_ms {
            message.push_str(&format!("<duration_ms>{duration_ms}</duration_ms>"));
        }
        message.push_str("</usage>");
    }
    message.push_str("\n</task-notification>");
    message
}

fn build_local_agent_notification_xml(snap: &TaskSnapshot) -> String {
    let status = snap
        .result
        .as_ref()
        .and_then(|result| result.get("status"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| snap.status.as_str());
    // A local agent that finished a turn without going terminal kept its
    // runtime: the persistent actor is parked on its mailbox with the
    // transcript intact, so `SendMessage` resumes *this* worker instead of
    // forcing the coordinator to rebuild the context in a fresh one. The
    // outcome word ("completed"/"failed") describes the turn; this flag
    // describes the worker. It rides in its own element rather than the
    // summary, which is the line the user's transcript renders.
    let continuable = !snap.status.is_terminal();
    let summary = match status {
        "completed" => format!("Agent \"{}\" completed", snap.title),
        "failed" => format!(
            "Agent \"{}\" failed: {}",
            snap.title,
            snap.error.as_deref().unwrap_or("Unknown error")
        ),
        "cancelled" | "killed" => format!("Agent \"{}\" was stopped", snap.title),
        other => format!("Agent \"{}\" is {other}", snap.title),
    };

    let output_file = local_agent_output_file(snap);
    let result_text = snap
        .result
        .as_ref()
        .and_then(|v| v.get("final_text"))
        .and_then(Value::as_str)
        .or(snap.last_progress.as_deref())
        .filter(|s| !s.is_empty());
    let total_tokens = snap
        .result
        .as_ref()
        .and_then(|v| v.get("total_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| match &snap.data {
            TaskData::LocalAgent(data) if data.token_count > 0 => Some(data.token_count),
            _ => None,
        });
    let tool_uses = snap
        .result
        .as_ref()
        .and_then(|v| v.get("tool_call_count").or_else(|| v.get("tool_use_count")))
        .and_then(Value::as_u64)
        .or_else(|| match &snap.data {
            TaskData::LocalAgent(data) if data.tool_use_count > 0 => Some(data.tool_use_count),
            _ => None,
        });
    let duration_ms = snap
        .result
        .as_ref()
        .and_then(|v| v.get("duration_ms"))
        .and_then(Value::as_u64)
        .or_else(|| {
            snap.end_time_ms
                .map(|end| end.saturating_sub(snap.start_time_ms))
        });

    let git = snap.result.as_ref().and_then(|v| v.get("git"));

    let mut message = format!(
        "<task-notification>\n<task-id>{}</task-id>",
        xml_escape(snap.id.as_str())
    );
    if let Some(output_file) = output_file {
        message.push_str(&format!(
            "\n<output-file>{}</output-file>",
            xml_escape(output_file)
        ));
    }
    message.push_str(&format!(
        "\n<status>{}</status>\n<continuable>{continuable}</continuable>\n<summary>{}</summary>",
        xml_escape(status),
        xml_escape(&summary)
    ));
    if let Some(result_text) = result_text {
        message.push_str(&format!("\n<result>{}</result>", xml_escape(result_text)));
    }
    if let Some(git) = git {
        message.push_str("\n<git>");
        for (json_key, xml_key) in [
            ("worktree_path", "worktree_path"),
            ("worktree_branch", "branch"),
            ("base_commit", "base"),
            ("head_commit", "head"),
            ("commit_hash", "commit"),
            ("validation_status", "validation_status"),
            ("validation_error", "validation_error"),
            ("status_output", "status_output"),
        ] {
            if let Some(value) = git
                .get(json_key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                message.push_str(&format!("<{xml_key}>{}</{xml_key}>", xml_escape(value)));
            }
        }
        if let Some(dirty) = git.get("dirty_after_commit").and_then(Value::as_bool) {
            message.push_str(&format!("<dirty>{dirty}</dirty>"));
        }
        message.push_str("</git>");
    }
    if total_tokens.is_some() || tool_uses.is_some() || duration_ms.is_some() {
        message.push_str("\n<usage>");
        if let Some(total_tokens) = total_tokens {
            message.push_str(&format!("<total_tokens>{total_tokens}</total_tokens>"));
        }
        if let Some(tool_uses) = tool_uses {
            message.push_str(&format!("<tool_uses>{tool_uses}</tool_uses>"));
        }
        if let Some(duration_ms) = duration_ms {
            message.push_str(&format!("<duration_ms>{duration_ms}</duration_ms>"));
        }
        message.push_str("</usage>");
    }
    message.push_str("\n</task-notification>");
    message
}

/// Error returned from `spawn_*_task` when setup fails before any
/// progress can be made.
#[derive(Debug, Error)]
pub enum TaskSpawnError {
    /// No matching tool was registered on the engine.
    #[error("task spawn failed: unknown tool {0}")]
    UnknownTool(String),
    /// The shell command tool returned an error before the task
    /// could be observed.
    #[error("shell tool execution failed: {0}")]
    ShellExecution(String),
}

/// Marker trait for every task kind the registry tracks. Useful
/// for callers that want to accept an opaque "something I can
/// spawn" parameter — today it only pins the associated
/// [`TaskKind`] enum but future task extensions can extend it.
pub trait Task: Send + Sync {
    /// Static kind tag for this task.
    fn kind(&self) -> TaskKind;
}

/// Spec for a [`LocalShell`](TaskKind::LocalShell) task.
#[derive(Debug, Clone)]
pub struct LocalShellTaskSpec {
    /// Unique task id.
    pub id: TaskId,
    /// Tool name — typically `"Bash"` on unix or `"PowerShell"` on
    /// Windows. The registry passes the input straight through to
    /// `Engine::invoke_tool`, so whichever tool ships under that
    /// name will be called.
    pub tool_name: String,
    /// Raw JSON input forwarded to the shell tool. Usually an
    /// object with at least a `command` field, matching what the
    /// BashTool / PowerShellTool in `rebon-tool` expects.
    pub input: Value,
    /// Human-readable title — typically the command itself.
    pub title: String,
    /// Whether to start in background mode.
    pub start_backgrounded: bool,
}

impl LocalShellTaskSpec {
    /// Minimal constructor.
    pub fn new(id: impl Into<String>, tool_name: impl Into<String>, input: Value) -> Self {
        let tool_name = tool_name.into();
        let title = input
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap_or(&tool_name)
            .to_string();
        Self {
            id: TaskId::new(id),
            tool_name,
            input,
            title,
            start_backgrounded: false,
        }
    }

    /// Start in backgrounded mode.
    pub fn backgrounded(mut self) -> Self {
        self.start_backgrounded = true;
        self
    }
}

impl Task for LocalShellTaskSpec {
    fn kind(&self) -> TaskKind {
        TaskKind::LocalShell
    }
}

/// Spawn a [`LocalShellTaskSpec`] — run a shell command via the
/// engine's registered bash/powershell tool and observe its
/// lifecycle.
pub fn spawn_local_shell_task(
    engine: Arc<Engine>,
    registry: TaskRegistry,
    spec: LocalShellTaskSpec,
) -> Result<mpsc::UnboundedReceiver<TaskObservation>, TaskSpawnError> {
    if engine.find_tool(&spec.tool_name).is_none() {
        return Err(TaskSpawnError::UnknownTool(spec.tool_name));
    }

    let cancel = PromptCancel::new();
    let command_text = spec
        .input
        .get("command")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| spec.title.clone());
    let snapshot = TaskSnapshot {
        id: spec.id.clone(),
        kind: TaskKind::LocalShell,
        status: TaskStatus::Pending,
        title: spec.title.clone(),
        last_progress: None,
        error: None,
        result: None,
        is_backgrounded: spec.start_backgrounded,
        notified: false,
        start_time_ms: rebon_types::wall_clock_ms(),
        end_time_ms: None,
        metadata: empty_metadata(),
        data: TaskData::LocalShell(LocalShellData {
            command: command_text,
            exit_code: None,
            interrupted: false,
            display_kind: BashTaskKind::Bash,
            agent_id: None,
        }),
    };
    registry.insert(spec.id.clone(), snapshot, cancel);

    let (obs_tx, obs_rx) = mpsc::unbounded_channel::<TaskObservation>();
    let task_id = spec.id.clone();
    let registry_clone = registry.clone();
    let tool_name = spec.tool_name.clone();
    let input = spec.input.clone();

    tokio::spawn(async move {
        let mut should_start = false;
        registry_clone.update(&task_id, |snap| {
            if snap.status == TaskStatus::Pending {
                snap.status = TaskStatus::Running;
                should_start = true;
            }
        });
        if !should_start {
            if let Some(snap) = registry_clone.snapshot(&task_id) {
                registry_clone.record_live_event(
                    &task_id,
                    TaskLiveEventKind::Finished {
                        status: snap.status,
                        error: snap.error.clone(),
                    },
                );
                let _ = obs_tx.send(TaskObservation::Finished(snap));
            }
            return;
        }
        registry_clone.record_live_event(&task_id, TaskLiveEventKind::Started);
        let _ = obs_tx.send(TaskObservation::Started);
        // registry's observation channel is enough.
        let input_command = input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let outcome = engine
            .invoke_tool(&tool_name, input, &ToolContext::new())
            .await;

        let (status, error, result) = match outcome {
            Ok(value) => {
                let stdout = value
                    .get("stdout")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let stderr = value
                    .get("stderr")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if !stdout.is_empty() {
                    registry_clone.record_live_event(
                        &task_id,
                        TaskLiveEventKind::TerminalOutput {
                            stream: TaskTerminalStream::Stdout,
                            chunk: stdout.clone(),
                        },
                    );
                    let _ = obs_tx.send(TaskObservation::Progress {
                        channel: "stdout".into(),
                        payload: stdout,
                    });
                }
                if !stderr.is_empty() {
                    registry_clone.record_live_event(
                        &task_id,
                        TaskLiveEventKind::TerminalOutput {
                            stream: TaskTerminalStream::Stderr,
                            chunk: stderr.clone(),
                        },
                    );
                    let _ = obs_tx.send(TaskObservation::Progress {
                        channel: "stderr".into(),
                        payload: stderr,
                    });
                }
                (TaskStatus::Completed, None, Some(value))
            }
            Err(err) => {
                let msg = tool_error_message(&err);
                let stderr = err.to_string();
                registry_clone.record_live_event(
                    &task_id,
                    TaskLiveEventKind::TerminalOutput {
                        stream: TaskTerminalStream::Stderr,
                        chunk: stderr.clone(),
                    },
                );
                let _ = obs_tx.send(TaskObservation::Progress {
                    channel: "stderr".into(),
                    payload: stderr.clone(),
                });
                let result = serde_json::json!({
                    "stdout": "",
                    "stderr": stderr,
                    "interrupted": matches!(err, ToolError::Cancelled { .. }),
                    "exitCode": serde_json::Value::Null,
                    "timedOut": false,
                    "command": input_command,
                    "error": msg.clone(),
                    "toolError": err.to_string(),
                });
                (TaskStatus::Failed, Some(msg), Some(result))
            }
        };

        registry_clone.update(&task_id, |snap| {
            if snap.status.is_terminal() {
                return;
            }
            snap.status = status;
            snap.error = error.clone();
            snap.result = result.clone();
            snap.last_progress = result.as_ref().and_then(|r| {
                r.get("stdout")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
            });
            snap.end_time_ms = Some(rebon_types::wall_clock_ms());
        });
        if let Some(snap) = registry_clone.snapshot(&task_id) {
            registry_clone.record_live_event(
                &task_id,
                TaskLiveEventKind::Finished {
                    status: snap.status,
                    error: snap.error.clone(),
                },
            );
            let _ = obs_tx.send(TaskObservation::Finished(snap));
        }
    });

    Ok(obs_rx)
}

fn tool_error_message(err: &ToolError) -> String {
    match err {
        ToolError::UnknownTool { tool } => format!("unknown tool: {tool}"),
        ToolError::InvalidInput { tool, reason, .. } => {
            format!("invalid input for {tool}: {reason}")
        }
        ToolError::PermissionDenied { tool, reason } => {
            format!("permission denied for {tool}: {reason}")
        }
        ToolError::Cancelled { tool, reason } => format!("{tool} cancelled: {reason}"),
        ToolError::Presented { presentation, .. } => presentation.display_message.clone(),
        ToolError::Execution { tool, source } => format!("{tool} execution failed: {source}"),
    }
}

/// Silence "unused" warning for the ToolId re-export from
/// rebon_tools_core — it's here so downstream call sites can
/// construct task ids keyed by the same namespace as tool ids.
#[doc(hidden)]
pub fn _compile_check_tool_id(_: ToolId) {}

// ---------------------------------------------------------------------------
// Remote agent — state-only registration helpers.
// ---------------------------------------------------------------------------

/// Spec for [`register_remote_agent_task`].
#[derive(Debug, Clone)]
pub struct RemoteAgentTaskSpec {
    /// Local task id. Callers typically mint it with
    /// [`generate_task_id`].
    pub id: TaskId,
    /// Remote task type.
    pub remote_task_type: RemoteTaskType,
    /// Remote session id returned by the backend.
    pub session_id: String,
    /// Original command string.
    pub command: String,
    /// Session title — shown as `description` on the snapshot.
    pub title: String,
    /// `true` when this is an `/ultrareview` teleport.
    pub is_remote_review: bool,
    /// `true` when this is an ultraplan invocation.
    pub is_ultraplan: bool,
    /// `true` for long-running tasks that must not complete on the
    /// first result event.
    pub is_long_running: bool,
}

/// Register a remote-agent task into the registry without actually
/// starting a polling loop.
///
/// Returns the minted task id so callers can pass it to the
/// eventual polling loop when the remote backend is wired.
pub fn register_remote_agent_task(registry: &TaskRegistry, spec: RemoteAgentTaskSpec) -> TaskId {
    let start = rebon_types::wall_clock_ms();
    let snapshot = TaskSnapshot {
        id: spec.id.clone(),
        kind: TaskKind::RemoteAgent,
        status: TaskStatus::Running,
        title: spec.title.clone(),
        last_progress: None,
        error: None,
        result: None,
        is_backgrounded: true,
        notified: false,
        start_time_ms: start,
        end_time_ms: None,
        metadata: empty_metadata(),
        data: TaskData::RemoteAgent(Box::new(RemoteAgentData {
            remote_task_type: spec.remote_task_type,
            session_id: spec.session_id,
            command: spec.command,
            title: spec.title,
            poll_started_at_ms: start,
            is_remote_review: spec.is_remote_review,
            is_ultraplan: spec.is_ultraplan,
            is_long_running: spec.is_long_running,
            ultraplan_phase: None,
            review_progress: None,
        })),
    };
    let id = spec.id.clone();
    registry.insert(spec.id, snapshot, PromptCancel::new());
    id
}

/// Update the review-progress counts on a running remote-agent task.
/// Parses
/// the `<remote-review-progress>` heartbeat.
pub fn update_review_progress(registry: &TaskRegistry, id: &TaskId, progress: ReviewProgress) {
    registry.update(id, |snap| {
        if let TaskData::RemoteAgent(data) = &mut snap.data {
            data.review_progress = Some(progress);
        }
    });
}

/// Update the ultraplan phase on a running remote-agent task.
pub fn update_ultraplan_phase(registry: &TaskRegistry, id: &TaskId, phase: Option<UltraplanPhase>) {
    registry.update(id, |snap| {
        if let TaskData::RemoteAgent(data) = &mut snap.data {
            data.ultraplan_phase = phase;
        }
    });
}

/// Mark a remote-agent task as killed — the local state mutation
/// (the archive
/// call to the remote is a separate concern the caller handles).
pub fn kill_remote_agent_task(registry: &TaskRegistry, id: &TaskId) -> bool {
    let mut killed = false;
    registry.update(id, |snap| {
        if snap.status.is_terminal() {
            return;
        }
        snap.status = TaskStatus::Killed;
        snap.notified = true;
        snap.end_time_ms = Some(rebon_types::wall_clock_ms());
        killed = true;
    });
    killed
}

// ---------------------------------------------------------------------------
// In-process teammate — state-only registration helpers.
// ---------------------------------------------------------------------------

/// Spec for [`register_in_process_teammate_task`].
#[derive(Debug, Clone)]
pub struct InProcessTeammateTaskSpec {
    /// Local task id.
    pub id: TaskId,
    /// Teammate identity.
    pub identity: TeammateIdentity,
    /// User prompt.
    pub prompt: String,
    /// Optional model override.
    pub model: Option<String>,
    /// Optional model profile override.
    pub model_profile: Option<String>,
    /// Initial permission mode.
    pub permission_mode: String,
    /// Stable agent type/role label.
    pub agent_type: Option<String>,
    /// Human-readable teammate responsibility.
    pub description: Option<String>,
}

/// Register an in-process teammate without actually spawning one.
/// The real runtime (process spawn, team
/// context book-keeping) belongs in runtime integration.
pub fn register_in_process_teammate_task(
    registry: &TaskRegistry,
    spec: InProcessTeammateTaskSpec,
) -> TaskId {
    register_in_process_teammate_task_with_cancel(registry, spec, PromptCancel::new())
}

/// Register an in-process teammate using an externally-owned cancel handle.
pub fn register_in_process_teammate_task_with_cancel(
    registry: &TaskRegistry,
    spec: InProcessTeammateTaskSpec,
    cancel: PromptCancel,
) -> TaskId {
    let metadata = serde_json::json!({
        "agent_id": spec.identity.agent_id.clone(),
        "agent_name": spec.identity.agent_name.clone(),
        "display_name": spec.identity.agent_name.clone(),
        "agent_type": spec.agent_type.clone(),
        "description": spec.description.clone(),
        "parent_session_id": spec.identity.parent_session_id.clone(),
        "current_task": compact_teammate_text(&spec.prompt, 512),
    });
    let snapshot = TaskSnapshot {
        id: spec.id.clone(),
        kind: TaskKind::InProcessTeammate,
        status: TaskStatus::Running,
        title: format!("@{}", spec.identity.agent_name),
        last_progress: None,
        error: None,
        result: None,
        is_backgrounded: true,
        notified: false,
        start_time_ms: rebon_types::wall_clock_ms(),
        end_time_ms: None,
        metadata,
        data: TaskData::InProcessTeammate(Box::new(InProcessTeammateData {
            identity: spec.identity,
            prompt: spec.prompt,
            model: spec.model,
            model_profile: spec.model_profile,
            permission_mode: spec.permission_mode,
            awaiting_plan_approval: false,
            is_idle: false,
            shutdown_requested: false,
            pending_user_messages: Vec::new(),
            tool_use_count: 0,
            token_count: 0,
            transcript: Vec::new(),
            streaming_text: None,
        })),
    };
    let id = spec.id.clone();
    registry.insert(spec.id, snapshot, cancel);
    id
}

static TEAMMATE_REQUEST_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_teammate_request_id() -> String {
    format!(
        "teammate-request-{}-{}",
        rebon_types::wall_clock_ms(),
        TEAMMATE_REQUEST_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

fn compact_teammate_text(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut compact = text.chars().take(max_chars).collect::<String>();
    compact.push('…');
    compact
}

pub fn enqueue_teammate_request(
    registry: &TaskRegistry,
    id: &TaskId,
    request: TeammateRequest,
) -> bool {
    let mut ok = false;
    registry.update(id, |snap| {
        if snap.status.is_terminal() {
            return;
        }
        if let TaskData::InProcessTeammate(data) = &mut snap.data {
            if data.pending_user_messages.len() >= MAX_PENDING_AGENT_MESSAGES {
                return;
            }
            data.pending_user_messages.push(request.clone());
            push_bounded_agent_transcript(
                &mut data.transcript,
                LocalAgentTranscriptEntry::User {
                    text: request.message.clone(),
                },
            );
            if let Some(metadata) = snap.metadata.as_object_mut() {
                metadata.insert(
                    "current_task".into(),
                    Value::String(compact_teammate_text(&request.message, 512)),
                );
            }
            ok = true;
        }
    });
    if ok {
        registry.record_live_event(
            id,
            TaskLiveEventKind::UserMessage {
                text: request.message,
            },
        );
        registry.wake_task(id);
    }
    ok
}

/// Inject a user message into a running teammate's pending queue.
pub fn inject_user_message_to_teammate(
    registry: &TaskRegistry,
    id: &TaskId,
    message: String,
) -> bool {
    enqueue_teammate_request(
        registry,
        id,
        TeammateRequest {
            request_id: next_teammate_request_id(),
            message,
        },
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalAgentMessageEnqueueError {
    Closed,
    Full,
    InvalidTarget,
}

fn enqueue_local_agent_message(
    registry: &TaskRegistry,
    id: &TaskId,
    message: String,
) -> Result<(), LocalAgentMessageEnqueueError> {
    let wake = {
        let mut guard = registry.inner.lock().expect("task registry poisoned");
        let state = guard
            .get_mut(id)
            .ok_or(LocalAgentMessageEnqueueError::Closed)?;
        if state.snapshot.status.is_terminal() {
            return Err(LocalAgentMessageEnqueueError::Closed);
        }
        let TaskData::LocalAgent(data) = &mut state.snapshot.data else {
            return Err(LocalAgentMessageEnqueueError::InvalidTarget);
        };
        if data
            .pending_messages
            .len()
            .saturating_add(state.runtime.leased_local_agent_messages)
            >= MAX_PENDING_AGENT_MESSAGES
        {
            return Err(LocalAgentMessageEnqueueError::Full);
        }
        data.pending_messages.push(message.clone());
        push_bounded_agent_transcript(
            &mut data.transcript,
            LocalAgentTranscriptEntry::User {
                text: message.clone(),
            },
        );
        if let Some(metadata) = state.snapshot.metadata.as_object_mut() {
            metadata.insert(LOCAL_AGENT_IDLE_METADATA_KEY.into(), Value::Bool(false));
        }
        state.runtime.idle_since = None;
        state.runtime.message_generation = state.runtime.message_generation.saturating_add(1);
        state.runtime.wake.clone()
    };
    registry.record_live_event(id, TaskLiveEventKind::UserMessage { text: message });
    registry.retention_notify.notify_one();
    wake.notify_one();
    Ok(())
}

/// Inject a user message into a running local-agent task's pending queue.
pub fn inject_user_message_to_local_agent(
    registry: &TaskRegistry,
    id: &TaskId,
    message: String,
) -> bool {
    enqueue_local_agent_message(registry, id, message).is_ok()
}

pub fn send_message_to_local_agent_task(
    registry: &TaskRegistry,
    task_id: &str,
    message: String,
) -> Result<(), ToolErrorPresentation> {
    let requested_id = TaskId::new(task_id.to_string());
    let (id, snapshot) = if let Some(snapshot) = registry.snapshot(&requested_id) {
        (requested_id, snapshot)
    } else {
        let aliases = registry
            .snapshots()
            .into_iter()
            .filter(|snapshot| {
                snapshot.kind == TaskKind::LocalAgent
                    && snapshot
                        .metadata_str("display_name")
                        .or_else(|| snapshot.metadata_str("name"))
                        .is_some_and(|name| name.eq_ignore_ascii_case(task_id))
            })
            .collect::<Vec<_>>();
        let active = aliases
            .iter()
            .filter(|snapshot| !snapshot.status.is_terminal())
            .collect::<Vec<_>>();
        if active.len() > 1 {
            return Err(ToolErrorPresentation::new(
                "agent_ambiguous",
                format!("More than one Agent is named \"{task_id}\"."),
                format!(
                    "Multiple active Agent tasks use the display name \"{task_id}\". Retry SendMessage with the exact agent_id returned by the Agent tool."
                ),
            ));
        }
        if let Some(snapshot) = active.first() {
            (snapshot.id.clone(), (*snapshot).clone())
        } else if let Some(snapshot) = aliases.first() {
            return Err(ToolErrorPresentation::new(
                "agent_closed",
                format!("Agent \"{task_id}\" is no longer available."),
                format!(
                    "Agent \"{task_id}\" is in terminal state \"{}\" and cannot receive new messages. Spawn a fresh worker via the Agent tool with the follow-up instructions.",
                    snapshot.status.as_str()
                ),
            ));
        } else {
            let was_closed = registry
                .removed_ids
                .lock()
                .expect("task registry tombstones poisoned")
                .contains(&requested_id);
            return Err(if was_closed {
                ToolErrorPresentation::new(
                    "agent_closed",
                    format!("Agent \"{task_id}\" is no longer available."),
                    format!(
                        "Agent \"{task_id}\" was closed or expired and cannot receive new messages. Spawn a fresh worker via the Agent tool with the follow-up instructions."
                    ),
                )
            } else {
                ToolErrorPresentation::new(
                    "agent_not_found",
                    format!("Agent \"{task_id}\" was not found."),
                    format!(
                        "Agent \"{task_id}\" has no active task. Verify the agent id or display name returned by the Agent tool; if it completed and expired, spawn a fresh worker with the follow-up instructions."
                    ),
                )
            });
        }
    };
    if snapshot.status.is_terminal() {
        return Err(ToolErrorPresentation::new(
            "agent_closed",
            format!("Agent \"{task_id}\" is no longer available."),
            format!(
                "Agent \"{task_id}\" is in terminal state \"{}\" and cannot receive new messages. Read its result if needed, then spawn a fresh worker via the Agent tool with synthesized follow-up instructions.",
                snapshot.status.as_str()
            ),
        ));
    }
    if snapshot.kind != TaskKind::LocalAgent {
        return Err(ToolErrorPresentation::new(
            "agent_invalid_target",
            format!("Task \"{task_id}\" is not a messageable Agent."),
            format!(
                "Agent \"{task_id}\" is a {} task, not a local Agent worker",
                snapshot.kind.as_str()
            ),
        ));
    }
    match enqueue_local_agent_message(registry, &id, message) {
        Ok(()) => Ok(()),
        Err(LocalAgentMessageEnqueueError::Full) => Err(ToolErrorPresentation::new(
            "agent_queue_full",
            format!("Agent \"{task_id}\" cannot accept another message yet."),
            format!(
                "Agent \"{task_id}\" has {} queued messages, which is the current limit. Wait for it to process queued work before sending another message.",
                MAX_PENDING_AGENT_MESSAGES
            ),
        )),
        Err(
            LocalAgentMessageEnqueueError::Closed
            | LocalAgentMessageEnqueueError::InvalidTarget,
        ) => Err(ToolErrorPresentation::new(
            "agent_closed",
            format!("Agent \"{task_id}\" is no longer available."),
            format!(
                "Agent \"{task_id}\" stopped or was removed while SendMessage was queueing the message. Spawn a fresh worker if the follow-up is still needed."
            ),
        )),
    }
}

/// Drain pending user messages from a running local-agent task.
pub fn take_pending_local_agent_messages(registry: &TaskRegistry, id: &TaskId) -> Vec<String> {
    let mut messages = Vec::new();
    registry.update(id, |snap| {
        if let TaskData::LocalAgent(data) = &mut snap.data {
            messages = std::mem::take(&mut data.pending_messages);
        }
    });
    messages
}

/// Request a graceful shutdown on a teammate.
pub fn request_teammate_shutdown(registry: &TaskRegistry, id: &TaskId) -> bool {
    let mut ok = false;
    registry.update(id, |snap| {
        if snap.status != TaskStatus::Running {
            return;
        }
        if let TaskData::InProcessTeammate(data) = &mut snap.data {
            if !data.shutdown_requested {
                data.shutdown_requested = true;
                ok = true;
            }
        }
    });
    if ok {
        registry.wake_task(id);
    }
    ok
}

/// Mark an in-process teammate as actively running work.
pub fn mark_in_process_teammate_running(registry: &TaskRegistry, id: &TaskId) -> bool {
    let mut updated = false;
    registry.update(id, |snap| {
        if snap.status.is_terminal() {
            return;
        }
        if let TaskData::InProcessTeammate(data) = &mut snap.data {
            snap.status = TaskStatus::Running;
            snap.end_time_ms = None;
            data.is_idle = false;
            updated = true;
        }
    });
    updated
}

pub fn set_in_process_teammate_current_task(
    registry: &TaskRegistry,
    id: &TaskId,
    current_task: &str,
) -> bool {
    let mut updated = false;
    registry.update(id, |snap| {
        if snap.status.is_terminal() || !matches!(&snap.data, TaskData::InProcessTeammate(_)) {
            return;
        }
        if let Some(metadata) = snap.metadata.as_object_mut() {
            metadata.insert(
                "current_task".into(),
                Value::String(compact_teammate_text(current_task, 512)),
            );
            updated = true;
        }
    });
    updated
}

/// Mark an in-process teammate as idle after a turn completes.
///
/// Tool-use and token tallies are owned by
/// [`drive_in_process_teammate_worker_turn`], which accumulates them
/// across turns as events arrive — this only flips the idle flag and
/// records the last progress line.
pub fn mark_in_process_teammate_idle(
    registry: &TaskRegistry,
    id: &TaskId,
    last_progress: Option<String>,
) -> bool {
    let mut updated = false;
    registry.update(id, |snap| {
        if snap.status.is_terminal() {
            return;
        }
        if let TaskData::InProcessTeammate(data) = &mut snap.data {
            snap.status = TaskStatus::Running;
            snap.last_progress = last_progress.clone();
            data.is_idle = true;
            data.streaming_text = None;
            updated = true;
        }
    });
    updated
}

/// Append the prompt that starts a teammate turn to the live
/// transcript, so the attach view shows the same user/assistant/tool
/// interleaving a `LocalAgent` task gets.
pub fn push_in_process_teammate_turn_prompt(registry: &TaskRegistry, id: &TaskId, prompt: &str) {
    registry.update(id, |snap| {
        if snap.status.is_terminal() {
            return;
        }
        if let TaskData::InProcessTeammate(data) = &mut snap.data {
            push_bounded_agent_transcript(
                &mut data.transcript,
                LocalAgentTranscriptEntry::User {
                    text: prompt.to_string(),
                },
            );
        }
    });
}

/// Finish an in-process teammate with a terminal status.
pub fn finish_in_process_teammate(
    registry: &TaskRegistry,
    id: &TaskId,
    status: TaskStatus,
    last_progress: Option<String>,
    error: Option<String>,
) -> bool {
    let mut updated = false;
    registry.update(id, |snap| {
        if snap.status.is_terminal() {
            return;
        }
        if let TaskData::InProcessTeammate(data) = &mut snap.data {
            snap.status = status;
            snap.last_progress = last_progress.clone();
            snap.error = error.clone();
            snap.end_time_ms = Some(rebon_types::wall_clock_ms());
            data.is_idle = true;
            updated = true;
        }
    });
    updated
}

pub fn take_pending_teammate_request(
    registry: &TaskRegistry,
    id: &TaskId,
) -> Option<TeammateRequest> {
    let mut next = None;
    registry.update(id, |snap| {
        if let TaskData::InProcessTeammate(data) = &mut snap.data {
            if !data.pending_user_messages.is_empty() {
                next = Some(data.pending_user_messages.remove(0));
            }
        }
    });
    next
}

/// Pop the next pending user message from an in-process teammate queue.
pub fn take_pending_user_message(registry: &TaskRegistry, id: &TaskId) -> Option<String> {
    take_pending_teammate_request(registry, id).map(|request| request.message)
}

pub fn revive_in_process_teammate_task(
    registry: &TaskRegistry,
    id: &TaskId,
    cancel: PromptCancel,
    request: TeammateRequest,
) -> Option<TeammateIdentity> {
    let mut guard = registry.inner.lock().expect("task registry poisoned");
    let state = guard.get_mut(id)?;
    if state.snapshot.kind != TaskKind::InProcessTeammate || !state.snapshot.status.is_terminal() {
        return None;
    }
    let identity = match &mut state.snapshot.data {
        TaskData::InProcessTeammate(data) => {
            data.shutdown_requested = false;
            data.awaiting_plan_approval = false;
            data.is_idle = false;
            data.streaming_text = None;
            data.pending_user_messages.clear();
            data.pending_user_messages.push(request.clone());
            push_bounded_agent_transcript(
                &mut data.transcript,
                LocalAgentTranscriptEntry::User {
                    text: request.message.clone(),
                },
            );
            data.identity.clone()
        }
        _ => return None,
    };
    state.snapshot.status = TaskStatus::Running;
    state.snapshot.error = None;
    state.snapshot.result = None;
    state.snapshot.last_progress = None;
    state.snapshot.end_time_ms = None;
    state.snapshot.notified = false;
    if let Some(metadata) = state.snapshot.metadata.as_object_mut() {
        metadata.insert(
            "current_task".into(),
            Value::String(compact_teammate_text(&request.message, 512)),
        );
    }
    state.cancel = cancel;
    state.runtime = TaskRuntimeState::new(&state.snapshot);
    drop(guard);
    registry.record_live_event(
        id,
        TaskLiveEventKind::UserMessage {
            text: request.message,
        },
    );
    registry.retention_notify.notify_one();
    Some(identity)
}

/// Update the stored permission mode of an in-process teammate.
pub fn set_in_process_teammate_permission_mode(
    registry: &TaskRegistry,
    id: &TaskId,
    permission_mode: String,
) -> bool {
    let mut updated = false;
    registry.update(id, |snap| {
        if let TaskData::InProcessTeammate(data) = &mut snap.data {
            if data.permission_mode != permission_mode {
                data.permission_mode = permission_mode.clone();
                updated = true;
            }
        }
    });
    updated
}

/// Set the awaiting-plan-approval flag for an in-process teammate.
pub fn set_in_process_teammate_awaiting_plan_approval(
    registry: &TaskRegistry,
    id: &TaskId,
    awaiting: bool,
) -> bool {
    let mut updated = false;
    registry.update(id, |snap| {
        if let TaskData::InProcessTeammate(data) = &mut snap.data {
            if data.awaiting_plan_approval != awaiting {
                data.awaiting_plan_approval = awaiting;
                updated = true;
            }
        }
    });
    updated
}

/// Kill an in-process teammate.
pub fn kill_in_process_teammate(registry: &TaskRegistry, id: &TaskId) -> bool {
    let mut killed = false;
    let _ = registry.cancel(id);
    registry.update(id, |snap| {
        if snap.status.is_terminal() {
            return;
        }
        snap.status = TaskStatus::Killed;
        snap.notified = true;
        snap.end_time_ms = Some(rebon_types::wall_clock_ms());
        if let TaskData::InProcessTeammate(data) = &mut snap.data {
            data.pending_user_messages.clear();
            data.shutdown_requested = true;
            data.is_idle = true;
            // The pump's Completed arm skips terminal snapshots, so
            // clear the in-flight stream here or the attach view
            // keeps rendering a stale streaming overlay.
            data.streaming_text = None;
        }
        killed = true;
    });
    killed
}

// ---------------------------------------------------------------------------
// Dream task — state-only registration helpers.
// ---------------------------------------------------------------------------

/// Spec for [`register_dream_task`].
#[derive(Debug, Clone)]
pub struct DreamTaskSpec {
    /// Local task id.
    pub id: TaskId,
    /// Number of session transcripts being reviewed.
    pub sessions_reviewing: u64,
    /// Consolidation lock mtime captured at spawn time.
    pub prior_mtime: u64,
}

/// Register a dream task.
pub fn register_dream_task(registry: &TaskRegistry, spec: DreamTaskSpec) -> TaskId {
    let snapshot = TaskSnapshot {
        id: spec.id.clone(),
        kind: TaskKind::Dream,
        status: TaskStatus::Running,
        title: "dreaming".into(),
        last_progress: None,
        error: None,
        result: None,
        is_backgrounded: true,
        notified: false,
        start_time_ms: rebon_types::wall_clock_ms(),
        end_time_ms: None,
        metadata: empty_metadata(),
        data: TaskData::Dream(DreamData {
            phase: DreamPhase::Starting,
            sessions_reviewing: spec.sessions_reviewing,
            files_touched: Vec::new(),
            turns: Vec::new(),
            prior_mtime: spec.prior_mtime,
        }),
    };
    let id = spec.id.clone();
    registry.insert(spec.id, snapshot, PromptCancel::new());
    id
}

/// Append a turn to a running dream task, applying the "skip empty"
/// guard and
/// the Starting → Updating phase transition.
pub fn add_dream_turn(
    registry: &TaskRegistry,
    id: &TaskId,
    turn: DreamTurn,
    touched_paths: Vec<String>,
) {
    registry.update(id, |snap| {
        if let TaskData::Dream(data) = &mut snap.data {
            let mut new_touched: Vec<String> = Vec::new();
            for p in touched_paths {
                if !data.files_touched.contains(&p) && !new_touched.contains(&p) {
                    new_touched.push(p);
                }
            }
            if turn.text.is_empty() && turn.tool_use_count == 0 && new_touched.is_empty() {
                return;
            }
            if !new_touched.is_empty() {
                data.phase = DreamPhase::Updating;
                data.files_touched.extend(new_touched);
            }
            if data.turns.len() >= DREAM_MAX_TURNS {
                data.turns.remove(0);
            }
            data.turns.push(turn);
        }
    });
}

/// Mark a dream task as completed.
pub fn complete_dream_task(registry: &TaskRegistry, id: &TaskId) {
    registry.update(id, |snap| {
        if snap.status.is_terminal() {
            return;
        }
        snap.status = TaskStatus::Completed;
        snap.notified = true;
        snap.end_time_ms = Some(rebon_types::wall_clock_ms());
    });
}

/// Mark a dream task as failed.
pub fn fail_dream_task(registry: &TaskRegistry, id: &TaskId, error: Option<String>) {
    registry.update(id, |snap| {
        if snap.status.is_terminal() {
            return;
        }
        snap.status = TaskStatus::Failed;
        snap.notified = true;
        snap.error = error.clone();
        snap.end_time_ms = Some(rebon_types::wall_clock_ms());
    });
}

/// Kill a running dream task.
/// Returns the captured `prior_mtime` so the
/// caller can rewind the consolidation lock.
pub fn kill_dream_task(registry: &TaskRegistry, id: &TaskId) -> Option<u64> {
    let mut prior: Option<u64> = None;
    registry.update(id, |snap| {
        if snap.status.is_terminal() {
            return;
        }
        if let TaskData::Dream(data) = &snap.data {
            prior = Some(data.prior_mtime);
        }
        snap.status = TaskStatus::Killed;
        snap.notified = true;
        snap.end_time_ms = Some(rebon_types::wall_clock_ms());
    });
    prior
}

// ---------------------------------------------------------------------------
// Local workflow — state-only registration helpers.
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum LocalWorkflowRegistrationError {
    #[error("workflow run id {run_id} is already registered to task {owner_task_id}")]
    RunIdAlreadyRegistered {
        run_id: String,
        owner_task_id: TaskId,
    },
}

/// Spec for [`register_local_workflow_task`].
#[derive(Debug, Clone)]
pub struct LocalWorkflowTaskSpec {
    /// Local task id.
    pub id: TaskId,
    /// Stable workflow run id.
    pub run_id: String,
    /// Workflow display name.
    pub workflow_name: String,
    /// Short summary — overrides the name when present.
    pub summary: Option<String>,
    /// Number of sub-agents participating.
    pub agent_count: u64,
    /// Persisted output directory.
    pub output_path: Option<String>,
    /// Persisted script path.
    pub script_path: Option<String>,
    /// Invocation args, if any.
    pub args: Option<Value>,
    pub is_backgrounded: bool,
    /// Session that launched the workflow. Stamped into task metadata so
    /// notification pollers shared across sessions (ACP) scope terminal
    /// notifications to the owning session.
    pub parent_session_id: Option<String>,
    /// `tool_use_id` of the Workflow tool call that launched the run, when
    /// launched from inside a turn. Stamped into task metadata so frontends
    /// can bind the task to the transcript card that spawned it, and reused
    /// as the stable id on synthesized ToolProgress live events.
    pub parent_tool_call_id: Option<String>,
}

/// Register a local-workflow task with a live cancel handle owned by the
/// workflow runtime.
pub fn register_local_workflow_task(
    registry: &TaskRegistry,
    spec: LocalWorkflowTaskSpec,
    cancel: PromptCancel,
) -> Result<TaskId, LocalWorkflowRegistrationError> {
    registry.reserve_local_workflow_run(&spec.run_id, &spec.id)?;
    let title = spec
        .summary
        .clone()
        .unwrap_or_else(|| spec.workflow_name.clone());
    let mut metadata = serde_json::json!({
        "workflow_run_id": spec.run_id,
    });
    if let Some(parent_session_id) = spec
        .parent_session_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        metadata["parent_session_id"] = serde_json::json!(parent_session_id);
    }
    if let Some(parent_tool_call_id) = spec
        .parent_tool_call_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        metadata["parent_tool_call_id"] = serde_json::json!(parent_tool_call_id);
    }
    let snapshot = TaskSnapshot {
        id: spec.id.clone(),
        kind: TaskKind::LocalWorkflow,
        status: TaskStatus::Running,
        title,
        last_progress: Some("starting workflow".into()),
        error: None,
        result: None,
        is_backgrounded: spec.is_backgrounded,
        notified: false,
        start_time_ms: rebon_types::wall_clock_ms(),
        end_time_ms: None,
        metadata,
        data: TaskData::LocalWorkflow(LocalWorkflowData {
            run_id: spec.run_id,
            workflow_name: spec.workflow_name,
            summary: spec.summary,
            agent_count: spec.agent_count,
            progress_entries: Vec::new(),
            token_count: 0,
            tool_use_count: 0,
            output_path: spec.output_path,
            script_path: spec.script_path,
            args: spec.args,
        }),
    };
    let id = spec.id.clone();
    registry.insert(spec.id, snapshot, cancel);
    // Announce the task on the live journal: the background bridge only
    // streams tasks that emit events, and a silent registration kept the
    // workflow invisible to frontends until it turned terminal.
    registry.record_live_event(&id, TaskLiveEventKind::Started);
    Ok(id)
}
pub fn release_local_workflow_run(registry: &TaskRegistry, run_id: &str, task_id: &TaskId) -> bool {
    registry.release_local_workflow_run(run_id, task_id)
}

pub fn push_local_workflow_progress(
    registry: &TaskRegistry,
    id: &TaskId,
    entry: WorkflowProgressEntry,
) -> u64 {
    let mut sequence = 0;
    registry.update(id, |snap| {
        if snap.status != TaskStatus::Running {
            return;
        }
        match &entry {
            WorkflowProgressEntry::Agent {
                index,
                state,
                label,
                tokens,
                tool_calls,
                ..
            } => {
                snap.last_progress = Some(format!("agent {state}: {label}"));
                if let TaskData::LocalWorkflow(data) = &mut snap.data {
                    data.agent_count = data.agent_count.max(*index);
                    data.token_count = data.token_count.saturating_add(*tokens);
                    data.tool_use_count = data.tool_use_count.saturating_add(*tool_calls);
                    sequence = data.progress_entries.len() as u64 + 1;
                    data.progress_entries.push(entry);
                }
            }
            WorkflowProgressEntry::Phase { title, state, .. } => {
                snap.last_progress = Some(format!("phase {state}: {title}"));
                if let TaskData::LocalWorkflow(data) = &mut snap.data {
                    sequence = data.progress_entries.len() as u64 + 1;
                    data.progress_entries.push(entry);
                }
            }
            WorkflowProgressEntry::Log { message } => {
                snap.last_progress = Some(message.clone());
                if let TaskData::LocalWorkflow(data) = &mut snap.data {
                    sequence = data.progress_entries.len() as u64 + 1;
                    data.progress_entries.push(entry);
                }
            }
        }
    });
    if sequence > 0 {
        // Each accepted progress entry rides the live journal so the bridge
        // streams the refreshed snapshot (workflow progress included) to
        // frontends mid-run — the graph updates as agents start and finish
        // instead of only materialising when the run turns terminal.
        let snapshot = registry.snapshot(id);
        let message = snapshot
            .as_ref()
            .and_then(|snap| snap.last_progress.clone())
            .unwrap_or_default();
        // The synthesized progress rides a stable non-empty id — the parent
        // Workflow tool call when the run was launched from a turn, else a
        // task-scoped placeholder. An empty id made frontends treat every
        // event as a fresh anonymous tool row.
        let tool_use_id = snapshot
            .as_ref()
            .and_then(|snap| snap.metadata_str("parent_tool_call_id"))
            .map(str::to_string)
            .unwrap_or_else(|| format!("workflow:{id}"));
        registry.record_live_event(
            id,
            TaskLiveEventKind::ToolProgress {
                tool_use_id,
                name: "Workflow".into(),
                message,
            },
        );
    }
    sequence
}

pub fn complete_local_workflow_task(
    registry: &TaskRegistry,
    id: &TaskId,
    result: Value,
    agent_count: u64,
    notify: bool,
) {
    let mut transitioned = false;
    registry.update(id, |snap| {
        if snap.status != TaskStatus::Running {
            return;
        }
        transitioned = true;
        snap.status = TaskStatus::Completed;
        snap.result = Some(result);
        snap.notified = !notify;
        snap.end_time_ms = Some(rebon_types::wall_clock_ms());
        snap.last_progress = Some("workflow completed".into());
        if let TaskData::LocalWorkflow(data) = &mut snap.data {
            data.agent_count = agent_count;
        }
    });
    // The `Started`/`ToolProgress` stream needs a matching terminal event,
    // or frontends keep the workflow spinner alive until the parent turn
    // checkpoints. (The kill path emits its own `Finished` in `stop_task`.)
    if transitioned {
        registry.record_live_event(
            id,
            TaskLiveEventKind::Finished {
                status: TaskStatus::Completed,
                error: None,
            },
        );
    }
}

pub fn fail_local_workflow_task(registry: &TaskRegistry, id: &TaskId, error: String, notify: bool) {
    let mut transitioned = false;
    registry.update(id, |snap| {
        if snap.status != TaskStatus::Running {
            return;
        }
        transitioned = true;
        snap.status = TaskStatus::Failed;
        snap.error = Some(error.clone());
        snap.notified = !notify;
        snap.end_time_ms = Some(rebon_types::wall_clock_ms());
        snap.last_progress = Some(format!("workflow failed: {error}"));
    });
    if transitioned {
        registry.record_live_event(
            id,
            TaskLiveEventKind::Finished {
                status: TaskStatus::Failed,
                error: Some(error),
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Cross-cutting helpers — stop_task, kill_shell_tasks_for_agent,
// pill_label, generate_task_id.
// ---------------------------------------------------------------------------

/// Error returned by [`stop_task`] when the task cannot be stopped.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum StopTaskError {
    /// No task in the registry matches the given id.
    #[error("no task found with id: {0}")]
    NotFound(String),
    /// The task exists but is already in a terminal state.
    #[error("task {0} is not running (status: {1})")]
    NotRunning(String, &'static str),
}

/// Result of a successful [`stop_task`] call.
#[derive(Debug, Clone)]
pub struct StopTaskResult {
    /// Id of the stopped task.
    pub task_id: String,
    /// String discriminant of the task type (`"local_bash"` etc.).
    pub task_type: &'static str,
    /// `command` for shell tasks, `description` for all other kinds.
    pub command: String,
}

/// Stop a non-terminal task by id. The cancellation signal and killed
/// snapshot transition happen under the registry lock so a concurrent
/// completion cannot win the lifecycle race.
///
/// Returns [`StopTaskError::NotFound`] if the id is unknown and
/// [`StopTaskError::NotRunning`] if the task has already reached a
/// terminal state.
pub fn stop_task(registry: &TaskRegistry, id: &TaskId) -> Result<StopTaskResult, StopTaskError> {
    let mut guard = registry.inner.lock().expect("task registry poisoned");
    let entry = guard
        .get_mut(id)
        .ok_or_else(|| StopTaskError::NotFound(id.as_str().to_owned()))?;
    if entry.snapshot.status.is_terminal() {
        return Err(StopTaskError::NotRunning(
            id.as_str().to_owned(),
            entry.snapshot.status.as_str(),
        ));
    }

    let command = match &entry.snapshot.data {
        TaskData::LocalShell(data) => data.command.clone(),
        _ => entry.snapshot.title.clone(),
    };
    let task_type = entry.snapshot.kind.as_str();
    // Stopping a worker that was *parked* — idle at a turn boundary with its
    // result already delivered — has nothing new to report: its notification
    // was built from this same snapshot and the kill changes only
    // `continuable`. Clearing `notified` there replays the whole report to the
    // coordinator a second time, which is what closing an idle worker used to
    // do. Only a worker stopped with an undelivered outcome still owes one.
    let outcome_already_reported = entry.snapshot.notified && task_is_idle(&entry.snapshot);
    entry.runtime.invalidate();
    entry.runtime.notification_generation = entry.runtime.notification_generation.saturating_add(1);
    entry.runtime.idle_since = Some(Instant::now());
    entry.cancel.cancel();
    entry.snapshot.status = TaskStatus::Killed;
    entry.snapshot.notified = outcome_already_reported
        || !matches!(
            &entry.snapshot.data,
            TaskData::LocalAgent(_) | TaskData::LocalWorkflow(_) | TaskData::Monitor(_)
        );
    entry.snapshot.end_time_ms = Some(rebon_types::wall_clock_ms());
    match &mut entry.snapshot.data {
        TaskData::LocalShell(data) => data.interrupted = true,
        TaskData::Monitor(data) => data.end_reason = Some(MonitorEndReason::Stopped),
        TaskData::InProcessTeammate(data) => {
            data.pending_user_messages.clear();
            data.shutdown_requested = true;
            data.is_idle = true;
        }
        _ => {}
    }

    let notification_ready = terminal_notification_ready(&entry.snapshot);
    let clear_monitor_events = entry.snapshot.kind == TaskKind::Monitor;
    let result = StopTaskResult {
        task_id: id.as_str().to_owned(),
        task_type,
        command,
    };
    drop(guard);
    if clear_monitor_events {
        registry
            .monitor_notifications
            .lock()
            .expect("monitor notifications poisoned")
            .tasks
            .remove(id);
    }
    registry.record_live_event(
        id,
        TaskLiveEventKind::Finished {
            status: TaskStatus::Killed,
            error: None,
        },
    );
    registry.retention_notify.notify_one();
    if notification_ready {
        registry.bump_notification_revision();
    }
    Ok(result)
}

/// Kill every running bash task spawned by the given sub-agent.
///
/// Returns the number of tasks killed. Used by the sub-agent runner's
/// finally block so bash children don't outlive the agent that
/// spawned them.
pub fn kill_shell_tasks_for_agent(registry: &TaskRegistry, agent_id: &str) -> usize {
    let mut killed_ids: Vec<TaskId> = Vec::new();
    for snap in registry.snapshots() {
        if snap.status != TaskStatus::Running {
            continue;
        }
        if let TaskData::LocalShell(data) = &snap.data {
            if data.agent_id.as_deref() == Some(agent_id) {
                killed_ids.push(snap.id.clone());
            }
        }
    }
    let count = killed_ids.len();
    for id in killed_ids {
        registry.cancel(&id);
        registry.update(&id, |snap| {
            snap.status = TaskStatus::Killed;
            snap.notified = true;
            snap.end_time_ms = Some(rebon_types::wall_clock_ms());
        });
    }
    count
}

/// Generate a task id with the per-kind prefix + 8 lowercase
/// alphanumeric chars.
/// The alphabet is the case-insensitive
/// set (digits + lowercase letters, 36 chars, ~2.8T combinations).
pub fn generate_task_id(kind: TaskKind) -> TaskId {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    use std::time::{SystemTime, UNIX_EPOCH};
    // Seed from wall-clock + an incrementing counter so tests (and
    // programs that mint many ids back-to-back) don't collide. We
    // deliberately avoid pulling a crypto RNG dep; this is a
    // presentation id, not a security token.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut seed = wall ^ counter.wrapping_mul(0x9E3779B97F4A7C15);
    let mut out = String::with_capacity(9);
    out.push(kind.id_prefix());
    for _ in 0..8 {
        let idx = (seed as usize) % ALPHABET.len();
        out.push(ALPHABET[idx] as char);
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
    }
    TaskId::new(out)
}

/// Produce the compact footer-pill label for a set of background
/// tasks.
///
/// Called by the footer pill and transcript turn-duration line — the
/// two surfaces share terminology via this single function.
pub fn pill_label(tasks: &[TaskSnapshot]) -> String {
    let n = tasks.len();
    if n == 0 {
        return String::new();
    }
    let first_kind = tasks[0].kind;
    let all_same = tasks.iter().all(|t| t.kind == first_kind);
    if !all_same {
        return format!("{n} background {}", if n == 1 { "task" } else { "tasks" });
    }
    match first_kind {
        TaskKind::LocalShell => {
            let monitors = tasks
                .iter()
                .filter(|t| {
                    matches!(
                        &t.data,
                        TaskData::LocalShell(d) if d.display_kind == BashTaskKind::Monitor
                    )
                })
                .count();
            let shells = n - monitors;
            let mut parts: Vec<String> = Vec::new();
            if shells > 0 {
                parts.push(if shells == 1 {
                    "1 shell".into()
                } else {
                    format!("{shells} shells")
                });
            }
            if monitors > 0 {
                parts.push(if monitors == 1 {
                    "1 monitor".into()
                } else {
                    format!("{monitors} monitors")
                });
            }
            parts.join(", ")
        }
        TaskKind::InProcessTeammate => {
            let mut teams: std::collections::HashSet<String> = std::collections::HashSet::new();
            for t in tasks {
                if let TaskData::InProcessTeammate(d) = &t.data {
                    teams.insert(d.identity.team_name.clone());
                }
            }
            let count = teams.len();
            if count == 1 {
                "1 team".into()
            } else {
                format!("{count} teams")
            }
        }
        TaskKind::LocalAgent => {
            if n == 1 {
                "1 local agent".into()
            } else {
                format!("{n} local agents")
            }
        }
        TaskKind::RemoteAgent => {
            // Single ultraplan gets a dedicated label variant.
            if n == 1 {
                if let TaskData::RemoteAgent(d) = &tasks[0].data {
                    if d.is_ultraplan {
                        return match d.ultraplan_phase {
                            Some(UltraplanPhase::PlanReady) => "\u{25C6} ultraplan ready".into(),
                            Some(UltraplanPhase::NeedsInput) => {
                                "\u{25C7} ultraplan needs your input".into()
                            }
                            None => "\u{25C7} ultraplan".into(),
                        };
                    }
                }
                "\u{25C7} 1 cloud session".into()
            } else {
                format!("\u{25C7} {n} cloud sessions")
            }
        }
        TaskKind::LocalWorkflow => {
            if n == 1 {
                "1 background workflow".into()
            } else {
                format!("{n} background workflows")
            }
        }
        TaskKind::Monitor | TaskKind::MonitorMcp => {
            if n == 1 {
                "1 monitor".into()
            } else {
                format!("{n} monitors")
            }
        }
        TaskKind::Dream => "dreaming".into(),
    }
}

/// True when the pill should show the dimmed " · ↓ to view"
/// call-to-action.
/// Only the two ultraplan attention
/// states surface the CTA.
pub fn pill_needs_cta(tasks: &[TaskSnapshot]) -> bool {
    if tasks.len() != 1 {
        return false;
    }
    if let TaskData::RemoteAgent(d) = &tasks[0].data {
        d.is_ultraplan && d.ultraplan_phase.is_some()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rebon_core::PermissionBroker;
    use rebon_tool::Tool;
    use rebon_tools_core::{
        PermissionBehavior, PermissionDecision, ToolInputSchema, ValidationOutcome,
    };
    use serde_json::json;
    use std::sync::Mutex;

    async fn drain(mut rx: mpsc::UnboundedReceiver<TaskObservation>) -> Vec<TaskObservation> {
        let mut out = Vec::new();
        while let Some(event) = rx.recv().await {
            let terminal = matches!(event, TaskObservation::Finished(_));
            out.push(event);
            if terminal {
                break;
            }
        }
        out
    }

    #[derive(Debug)]
    struct ScriptedShell {
        name: String,
        stdout: String,
        should_fail: bool,
        calls: Mutex<usize>,
    }

    impl ScriptedShell {
        fn new(name: &str, stdout: &str) -> Self {
            Self {
                name: name.into(),
                stdout: stdout.into(),
                should_fail: false,
                calls: Mutex::new(0),
            }
        }
        fn failing(name: &str) -> Self {
            Self {
                name: name.into(),
                stdout: String::new(),
                should_fail: true,
                calls: Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl Tool for ScriptedShell {
        fn id(&self) -> ToolId {
            ToolId::new(self.name.clone())
        }
        fn description(&self) -> &str {
            "scripted shell"
        }
        fn input_schema(&self) -> ToolInputSchema {
            json!({ "type": "object", "additionalProperties": true })
        }
        async fn validate_input(
            &self,
            _input: &Value,
            _context: &ToolContext,
        ) -> Result<ValidationOutcome, ToolError> {
            Ok(ValidationOutcome::valid())
        }
        async fn check_permissions(
            &self,
            _input: &Value,
            _context: &ToolContext,
        ) -> Result<PermissionDecision, ToolError> {
            Ok(PermissionDecision::allow(Value::Null))
        }
        async fn call(&self, _input: Value, _context: &ToolContext) -> Result<Value, ToolError> {
            *self.calls.lock().unwrap() += 1;
            if self.should_fail {
                Err(ToolError::Execution {
                    tool: self.id(),
                    source: anyhow::anyhow!("boom"),
                })
            } else {
                Ok(json!({
                    "stdout": self.stdout,
                    "stderr": "",
                    "exit_code": 0,
                }))
            }
        }
    }

    struct ApproveBroker;

    #[async_trait]
    impl PermissionBroker for ApproveBroker {
        async fn resolve(
            &self,
            tool: &dyn Tool,
            input: Value,
            context: &ToolContext,
            decision: PermissionDecision,
        ) -> Result<Value, ToolError> {
            if matches!(decision.behavior, PermissionBehavior::Deny) {
                return Err(ToolError::PermissionDenied {
                    tool: tool.id(),
                    reason: decision.reason.unwrap_or_else(|| "denied".into()),
                });
            }
            tool.call(input, context).await
        }
    }

    fn build_engine(tool: Arc<dyn Tool>) -> Arc<Engine> {
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(tool);
        Arc::new(engine)
    }

    #[test]
    fn live_event_journals_are_bounded_and_report_stale_cursors() {
        let registry = TaskRegistry::with_event_journal_capacity(2);
        let id = TaskId::new("bounded-events");
        let snapshot = TaskSnapshot::new_pending(
            id.clone(),
            "bounded".into(),
            TaskData::MonitorMcp(MonitorMcpData {
                server_name: "test".into(),
                description: "test".into(),
            }),
        );
        registry.insert(id.clone(), snapshot, PromptCancel::new());
        registry.record_live_event(&id, TaskLiveEventKind::Started);
        registry.record_live_event(
            &id,
            TaskLiveEventKind::TerminalOutput {
                stream: TaskTerminalStream::Stdout,
                chunk: "one".into(),
            },
        );
        registry.record_live_event(
            &id,
            TaskLiveEventKind::TerminalOutput {
                stream: TaskTerminalStream::Stdout,
                chunk: "two".into(),
            },
        );

        let task_batch = registry
            .task_live_events(&id, Some(TaskEventCursor::ZERO))
            .expect("task journal");
        assert!(task_batch.cursor_was_stale);
        assert_eq!(task_batch.events.len(), 2);
        assert_eq!(task_batch.oldest_available_cursor, Some(TaskEventCursor(2)));
        assert_eq!(task_batch.next_cursor, TaskEventCursor(3));
        assert_eq!(task_batch.latest_cursor, TaskEventCursor(3));

        let session_batch = registry.session_live_events(Some(TaskEventCursor::ZERO));
        assert!(session_batch.cursor_was_stale);
        assert_eq!(session_batch.events.len(), 2);
        assert!(session_batch.events.iter().all(|event| event.task_id == id));

        let caught_up = registry
            .task_live_events(&id, Some(task_batch.next_cursor))
            .expect("task journal");
        assert!(!caught_up.cursor_was_stale);
        assert!(caught_up.events.is_empty());
        assert_eq!(caught_up.next_cursor, TaskEventCursor(3));
    }

    #[test]
    fn record_live_event_for_removed_task_does_not_resurrect_journal() {
        let registry = TaskRegistry::new();
        let id = TaskId::new("tombstoned-task");
        let snapshot = TaskSnapshot::new_pending(
            id.clone(),
            "tombstoned".into(),
            TaskData::MonitorMcp(MonitorMcpData {
                server_name: "test".into(),
                description: "test".into(),
            }),
        );
        registry.insert(id.clone(), snapshot, PromptCancel::new());
        registry.record_live_event(&id, TaskLiveEventKind::Started);
        assert!(registry.task_live_events(&id, None).is_some());

        // `remove` tombstones the id and drops its per-task journal.
        assert!(registry.remove(&id).is_some());
        assert!(registry.task_live_events(&id, None).is_none());

        // A late callback recording an event for the removed id must be dropped,
        // not re-create the journal entry.
        registry.record_live_event(
            &id,
            TaskLiveEventKind::TerminalOutput {
                stream: TaskTerminalStream::Stdout,
                chunk: "late".into(),
            },
        );
        assert!(
            registry.task_live_events(&id, None).is_none(),
            "removed task journal must stay gone after a late live event"
        );
        assert!(
            !registry
                .event_journals
                .lock()
                .expect("task event journals poisoned")
                .tasks
                .contains_key(&id),
            "tombstoned id must not leak a journal entry"
        );
    }

    #[tokio::test]
    async fn registry_background_request_emits_only_when_backgrounded_true() {
        let registry = TaskRegistry::new();
        let id = TaskId::new("agent-bg-request");
        let snapshot = TaskSnapshot {
            id: id.clone(),
            kind: TaskKind::LocalAgent,
            status: TaskStatus::Running,
            title: "background me".into(),
            last_progress: None,
            error: None,
            result: None,
            is_backgrounded: false,
            notified: false,
            start_time_ms: 1,
            end_time_ms: None,
            metadata: empty_metadata(),
            data: TaskData::LocalAgent(LocalAgentData {
                prompt: "prompt".into(),
                agent_type: "worker".into(),
                model: Some("mock".into()),
                system: None,
                allowed_tools: None,
                token_count: 0,
                tool_use_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
                pending_messages: Vec::new(),
                retrieved: false,
            }),
        };
        registry.insert(id.clone(), snapshot, PromptCancel::new());

        let mut rx = registry.background_request_receiver(&id).unwrap();
        assert!(!*rx.borrow());
        assert!(!registry.set_backgrounded(&TaskId::new("missing")));
        assert!(!*rx.borrow());

        assert!(registry.set_backgrounded(&id));
        rx.changed().await.unwrap();
        assert!(*rx.borrow());
    }

    #[test]
    fn registry_background_request_receiver_missing_id_is_none() {
        let registry = TaskRegistry::new();
        assert!(registry
            .background_request_receiver(&TaskId::new("missing"))
            .is_none());
    }

    #[test]
    fn registry_drains_unnotified_background_agent_notifications_once() {
        let registry = TaskRegistry::new();
        let id = TaskId::new("agent-1");
        let snapshot = TaskSnapshot {
            id: id.clone(),
            kind: TaskKind::LocalAgent,
            status: TaskStatus::Completed,
            title: "Research <auth>".into(),
            last_progress: Some("done & written".into()),
            error: None,
            result: Some(json!({
                "status": "completed",
                "final_text": "done & written",
                "output_file": "C:\\tmp\\agent-1.report.md",
                "duration_ms": 42,
                "tool_call_count": 3,
                "total_tokens": 123,
            })),
            is_backgrounded: true,
            notified: false,
            start_time_ms: 10,
            end_time_ms: Some(52),
            metadata: empty_metadata(),
            data: TaskData::LocalAgent(LocalAgentData {
                prompt: "research".into(),
                agent_type: "worker".into(),
                model: Some("mock".into()),
                system: None,
                allowed_tools: None,
                token_count: 0,
                tool_use_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
                pending_messages: Vec::new(),
                retrieved: false,
            }),
        };
        registry.insert(id.clone(), snapshot, PromptCancel::new());

        let notifications = registry.take_unnotified_terminal_agent_notifications();
        assert_eq!(notifications.len(), 1);
        let note = &notifications[0];
        assert!(note.contains("<task-id>agent-1</task-id>"));
        assert!(note.contains("<status>completed</status>"));
        assert!(note.contains("Research &lt;auth&gt;"));
        assert!(note.contains("done &amp; written"));
        assert!(note.contains("<total_tokens>123</total_tokens>"));
        assert!(note.contains("<tool_uses>3</tool_uses>"));
        assert!(note.contains("<duration_ms>42</duration_ms>"));
        assert!(registry.snapshot(&id).unwrap().notified);
        assert!(registry
            .take_unnotified_terminal_agent_notifications()
            .is_empty());
    }

    #[test]
    fn runtime_controller_tracks_background_shell_and_notifies_once() {
        let registry = TaskRegistry::new();
        let controller = TaskRegistryRuntimeController::for_test_registry(registry.clone());
        let cancel = PromptCancel::new();
        controller.background_shell_started(
            "session-a",
            BackgroundShellTaskSpec {
                shell_id: "sh_test".into(),
                tool_name: "PowerShell".into(),
                command: "cargo check".into(),
                session_id: Some("session-a".into()),
                agent_id: None,
                started_at_ms: 10,
            },
            cancel.clone(),
        );

        let running = registry.snapshot(&TaskId::new("sh_test")).unwrap();
        assert_eq!(running.status, TaskStatus::Running);
        assert!(running.is_backgrounded);
        assert_eq!(running.metadata["parent_session_id"], "session-a");
        assert_eq!(running.metadata["shell_tool"], "PowerShell");
        assert!(!cancel.is_cancelled());

        controller.background_shell_finished(
            "session-a",
            BackgroundShellTaskCompletion {
                shell_id: "sh_test".into(),
                status: BackgroundShellCompletionStatus::Exited,
                completed_at_ms: 52,
                exit_code: Some(101),
                output: "checking".into(),
                stderr: "type error".into(),
                stream_order: None,
                error: None,
                next_cursor: 2,
                has_more: false,
                cursor_truncated: true,
                oldest_cursor: 4,
                observed: false,
            },
        );

        let finished = registry.snapshot(&TaskId::new("sh_test")).unwrap();
        assert_eq!(finished.status, TaskStatus::Failed);
        assert_eq!(
            finished.error.as_deref(),
            Some("background shell exited with code 101")
        );
        assert_eq!(finished.end_time_ms, Some(52));
        assert_eq!(
            match &finished.data {
                TaskData::LocalShell(data) => data.exit_code,
                _ => None,
            },
            Some(101)
        );
        let notifications = registry.unnotified_terminal_agent_notifications();
        assert_eq!(notifications.len(), 1);
        let message = &notifications[0].message;
        assert!(message.contains("<task-type>local_shell</task-type>"));
        assert!(message.contains("<shell-id>sh_test</shell-id>"));
        assert!(message.contains("<exit-code>101</exit-code>"));
        assert!(message.contains("<output>checking</output>"));
        assert!(message.contains("<stderr>type error</stderr>"));
        assert!(message.contains("<cursor-truncated>true</cursor-truncated>"));
        assert!(message.contains("<oldest-cursor>4</oldest-cursor>"));

        controller.background_shell_observed("session-a", "sh_test");
        assert!(registry
            .unnotified_terminal_agent_notifications()
            .is_empty());
    }

    #[test]
    fn foreground_and_worker_controllers_share_one_runtime_path() {
        let registry = TaskRegistry::new();
        let foreground = TaskRegistryRuntimeController::for_test_registry(registry.clone());
        let worker = TaskRegistryRuntimeController::for_test_registry(registry.clone());
        foreground.background_shell_started(
            "session-shared",
            BackgroundShellTaskSpec {
                shell_id: "sh_shared".into(),
                tool_name: "Bash".into(),
                command: "cargo check".into(),
                session_id: Some("session-shared".into()),
                agent_id: None,
                started_at_ms: 10,
            },
            PromptCancel::new(),
        );
        assert_eq!(
            registry
                .snapshot(&TaskId::new("sh_shared"))
                .expect("foreground registration should be visible to worker")
                .status,
            TaskStatus::Running
        );

        worker.background_shell_finished(
            "session-shared",
            BackgroundShellTaskCompletion {
                shell_id: "sh_shared".into(),
                status: BackgroundShellCompletionStatus::Exited,
                completed_at_ms: 20,
                exit_code: Some(0),
                output: "ok".into(),
                stderr: String::new(),
                stream_order: None,
                error: None,
                next_cursor: 1,
                has_more: false,
                cursor_truncated: false,
                oldest_cursor: 0,
                observed: false,
            },
        );
        assert_eq!(
            registry
                .snapshot(&TaskId::new("sh_shared"))
                .expect("worker completion should be visible to foreground")
                .status,
            TaskStatus::Completed
        );
    }

    #[test]
    fn runtime_controller_maps_shell_completion_statuses() {
        for (shell_status, exit_code, task_status) in [
            (
                BackgroundShellCompletionStatus::Exited,
                Some(0),
                TaskStatus::Completed,
            ),
            (
                BackgroundShellCompletionStatus::Stopped,
                None,
                TaskStatus::Killed,
            ),
            (
                BackgroundShellCompletionStatus::TimedOut,
                None,
                TaskStatus::Failed,
            ),
            (
                BackgroundShellCompletionStatus::Failed,
                None,
                TaskStatus::Failed,
            ),
        ] {
            let registry = TaskRegistry::new();
            let controller = TaskRegistryRuntimeController::for_test_registry(registry.clone());
            let shell_id = format!("sh_{}", shell_status.as_str());
            controller.background_shell_started(
                "session-a",
                BackgroundShellTaskSpec {
                    shell_id: shell_id.clone(),
                    tool_name: "Bash".into(),
                    command: "sleep 30".into(),
                    session_id: Some("session-a".into()),
                    agent_id: None,
                    started_at_ms: 10,
                },
                PromptCancel::new(),
            );
            controller.background_shell_finished(
                "session-a",
                BackgroundShellTaskCompletion {
                    shell_id: shell_id.clone(),
                    status: shell_status,
                    completed_at_ms: 20,
                    exit_code,
                    output: String::new(),
                    stderr: String::new(),
                    stream_order: None,
                    error: None,
                    next_cursor: 0,
                    has_more: false,
                    cursor_truncated: false,
                    oldest_cursor: 0,
                    observed: false,
                },
            );
            assert_eq!(
                registry.snapshot(&TaskId::new(shell_id)).unwrap().status,
                task_status
            );
        }
    }

    #[tokio::test]
    async fn local_shell_task_reports_stdout_on_completion() {
        let tool: Arc<dyn Tool> = Arc::new(ScriptedShell::new("Bash", "hello\nworld"));
        let engine = build_engine(tool);
        let registry = TaskRegistry::new();
        let spec = LocalShellTaskSpec::new("shell-1", "Bash", json!({ "command": "echo hi" }));
        let rx = spawn_local_shell_task(engine, registry.clone(), spec).unwrap();
        let events = drain(rx).await;

        let finished = events
            .iter()
            .find_map(|e| match e {
                TaskObservation::Finished(snap) => Some(snap),
                _ => None,
            })
            .unwrap();
        assert_eq!(finished.status, TaskStatus::Completed);
        assert_eq!(finished.last_progress.as_deref(), Some("hello\nworld"));
        assert!(finished.result.is_some());

        let live = registry
            .task_live_events(&TaskId::new("shell-1"), None)
            .expect("shell live journal");
        assert!(live.events.iter().any(|event| matches!(
            &event.kind,
            TaskLiveEventKind::TerminalOutput {
                stream: TaskTerminalStream::Stdout,
                chunk,
            } if chunk == "hello\nworld"
        )));
        assert!(matches!(
            live.events.last().map(|event| &event.kind),
            Some(TaskLiveEventKind::Finished {
                status: TaskStatus::Completed,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn local_shell_task_surfaces_tool_failure() {
        let tool: Arc<dyn Tool> = Arc::new(ScriptedShell::failing("Bash"));
        let engine = build_engine(tool);
        let registry = TaskRegistry::new();
        let spec = LocalShellTaskSpec::new("shell-fail", "Bash", json!({ "command": "nope" }));
        let rx = spawn_local_shell_task(engine, registry, spec).unwrap();
        let events = drain(rx).await;
        let finished = events
            .iter()
            .find_map(|e| match e {
                TaskObservation::Finished(snap) => Some(snap),
                _ => None,
            })
            .unwrap();
        assert_eq!(finished.status, TaskStatus::Failed);
        assert!(finished.error.is_some());
        let result = finished.result.as_ref().unwrap();
        assert_eq!(result.get("stdout").and_then(|v| v.as_str()), Some(""));
        assert!(result.get("stderr").and_then(|v| v.as_str()).is_some());
        assert_eq!(
            result.get("interrupted").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert!(result.get("exitCode").is_some());
        assert_eq!(
            result.get("timedOut").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(result.get("command").and_then(|v| v.as_str()), Some("nope"));
        assert!(result.get("error").and_then(|v| v.as_str()).is_some());
        assert!(result.get("toolError").and_then(|v| v.as_str()).is_some());
    }

    #[tokio::test]
    async fn local_shell_task_errors_on_unknown_tool() {
        let tool: Arc<dyn Tool> = Arc::new(ScriptedShell::new("Bash", ""));
        let engine = build_engine(tool);
        let registry = TaskRegistry::new();
        let err = spawn_local_shell_task(
            engine,
            registry,
            LocalShellTaskSpec::new("x", "NotAShell", json!({"command": ""})),
        )
        .unwrap_err();
        assert!(matches!(err, TaskSpawnError::UnknownTool(_)));
    }

    fn bash_snapshot(id: &str, status: TaskStatus, end_time_ms: Option<u64>) -> TaskSnapshot {
        TaskSnapshot {
            id: TaskId::new(id),
            kind: TaskKind::LocalShell,
            status,
            title: "title".into(),
            last_progress: None,
            error: None,
            result: None,
            is_backgrounded: false,
            notified: false,
            start_time_ms: 0,
            end_time_ms,
            metadata: empty_metadata(),
            data: TaskData::LocalShell(LocalShellData {
                command: "echo".into(),
                exit_code: None,
                interrupted: false,
                display_kind: BashTaskKind::Bash,
                agent_id: None,
            }),
        }
    }

    #[test]
    fn registry_backgrounded_flag_round_trips() {
        let reg = TaskRegistry::new();
        reg.insert(
            TaskId::new("t"),
            bash_snapshot("t", TaskStatus::Running, None),
            PromptCancel::new(),
        );
        assert!(!reg.snapshot(&TaskId::new("t")).unwrap().is_backgrounded);
        assert!(reg.set_backgrounded(&TaskId::new("t")));
        assert!(reg.snapshot(&TaskId::new("t")).unwrap().is_backgrounded);
        assert!(!reg.set_backgrounded(&TaskId::new("unknown")));
    }

    #[test]
    fn registry_remove_evicts_terminal_snapshot() {
        let reg = TaskRegistry::new();
        reg.insert(
            TaskId::new("t"),
            bash_snapshot("t", TaskStatus::Completed, Some(1000)),
            PromptCancel::new(),
        );
        assert_eq!(reg.len(), 1);
        let removed = reg.remove(&TaskId::new("t")).unwrap();
        assert_eq!(removed.status, TaskStatus::Completed);
        assert!(reg.is_empty());
    }

    #[test]
    fn registry_remove_blocks_late_reinsertion() {
        let reg = TaskRegistry::new();
        let id = TaskId::new("late");
        reg.insert(
            id.clone(),
            bash_snapshot("late", TaskStatus::Running, None),
            PromptCancel::new(),
        );

        assert!(reg.remove(&id).is_some());
        reg.insert(
            id.clone(),
            bash_snapshot("late", TaskStatus::Failed, Some(1000)),
            PromptCancel::new(),
        );

        assert!(reg.snapshot(&id).is_none());
    }

    #[test]
    fn task_status_is_terminal_covers_every_variant() {
        assert!(!TaskStatus::Pending.is_terminal());
        assert!(!TaskStatus::Running.is_terminal());
        assert!(TaskStatus::Completed.is_terminal());
        assert!(TaskStatus::Killed.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
    }

    #[test]
    fn local_shell_spec_derives_title_from_command_field() {
        let spec = LocalShellTaskSpec::new("id", "Bash", json!({ "command": "ls -la" }));
        assert_eq!(spec.title, "ls -la");
    }

    // ── Kind / status vocabulary round-trip ────────────────────────

    #[test]
    fn task_kind_round_trip_all_variants() {
        for kind in [
            TaskKind::LocalShell,
            TaskKind::LocalAgent,
            TaskKind::RemoteAgent,
            TaskKind::InProcessTeammate,
            TaskKind::LocalWorkflow,
            TaskKind::MonitorMcp,
            TaskKind::Dream,
        ] {
            let s = kind.as_str();
            assert_eq!(TaskKind::from_str(s), Some(kind));
        }
        // LocalShell is stored as "local_bash" for compatibility with
        // existing task records, but "local_shell" also parses.
        assert_eq!(TaskKind::from_str("local_bash"), Some(TaskKind::LocalShell));
        assert_eq!(
            TaskKind::from_str("local_shell"),
            Some(TaskKind::LocalShell)
        );
        assert_eq!(TaskKind::from_str("unknown"), None);
    }

    #[test]
    fn task_kind_id_prefix_table() {
        assert_eq!(TaskKind::LocalShell.id_prefix(), 'b');
        assert_eq!(TaskKind::LocalAgent.id_prefix(), 'a');
        assert_eq!(TaskKind::RemoteAgent.id_prefix(), 'r');
        assert_eq!(TaskKind::InProcessTeammate.id_prefix(), 't');
        assert_eq!(TaskKind::LocalWorkflow.id_prefix(), 'w');
        assert_eq!(TaskKind::MonitorMcp.id_prefix(), 'm');
        assert_eq!(TaskKind::Dream.id_prefix(), 'd');
    }

    #[test]
    fn task_status_round_trip_and_killed_alias() {
        for s in ["pending", "running", "completed", "failed", "killed"] {
            let status = TaskStatus::from_str(s).unwrap();
            assert_eq!(status.as_str(), s);
        }
        // Legacy "cancelled" maps to Killed for backward compatibility.
        assert_eq!(TaskStatus::from_str("cancelled"), Some(TaskStatus::Killed));
        assert_eq!(TaskStatus::from_str("nope"), None);
    }

    // ── Remote agent register + update helpers ─────────────────────

    fn remote_spec(id: &str, ultraplan: bool) -> RemoteAgentTaskSpec {
        RemoteAgentTaskSpec {
            id: TaskId::new(id),
            remote_task_type: if ultraplan {
                RemoteTaskType::Ultraplan
            } else {
                RemoteTaskType::RemoteAgent
            },
            session_id: format!("sess-{id}"),
            command: "/ultraplan do the thing".into(),
            title: "Do the thing".into(),
            is_remote_review: false,
            is_ultraplan: ultraplan,
            is_long_running: false,
        }
    }

    #[test]
    fn register_remote_agent_populates_snapshot_and_data() {
        let reg = TaskRegistry::new();
        let id = register_remote_agent_task(&reg, remote_spec("r1", true));
        let snap = reg.snapshot(&id).unwrap();
        assert_eq!(snap.kind, TaskKind::RemoteAgent);
        assert_eq!(snap.status, TaskStatus::Running);
        assert!(snap.is_backgrounded);
        match &snap.data {
            TaskData::RemoteAgent(d) => {
                assert!(d.is_ultraplan);
                assert_eq!(d.session_id, "sess-r1");
                assert!(d.ultraplan_phase.is_none());
            }
            _ => panic!("expected RemoteAgent data"),
        }
    }

    #[test]
    fn update_review_progress_writes_through() {
        let reg = TaskRegistry::new();
        let id = register_remote_agent_task(&reg, remote_spec("r1", false));
        update_review_progress(
            &reg,
            &id,
            ReviewProgress {
                stage: Some(ReviewStage::Verifying),
                bugs_found: 10,
                bugs_verified: 3,
                bugs_refuted: 2,
            },
        );
        let snap = reg.snapshot(&id).unwrap();
        match &snap.data {
            TaskData::RemoteAgent(d) => {
                let p = d.review_progress.unwrap();
                assert_eq!(p.stage, Some(ReviewStage::Verifying));
                assert_eq!(p.bugs_found, 10);
                assert_eq!(p.bugs_verified, 3);
                assert_eq!(p.bugs_refuted, 2);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn update_ultraplan_phase_writes_through() {
        let reg = TaskRegistry::new();
        let id = register_remote_agent_task(&reg, remote_spec("r1", true));
        update_ultraplan_phase(&reg, &id, Some(UltraplanPhase::PlanReady));
        let snap = reg.snapshot(&id).unwrap();
        if let TaskData::RemoteAgent(d) = &snap.data {
            assert_eq!(d.ultraplan_phase, Some(UltraplanPhase::PlanReady));
        } else {
            panic!();
        }
    }

    #[test]
    fn kill_remote_agent_task_flips_to_killed() {
        let reg = TaskRegistry::new();
        let id = register_remote_agent_task(&reg, remote_spec("r1", false));
        assert!(kill_remote_agent_task(&reg, &id));
        let snap = reg.snapshot(&id).unwrap();
        assert_eq!(snap.status, TaskStatus::Killed);
        assert!(snap.notified);
        assert!(snap.end_time_ms.is_some());
        // Second kill is a no-op.
        assert!(!kill_remote_agent_task(&reg, &id));
    }

    // ── Teammate register + message injection + kill ───────────────

    fn teammate_spec(id: &str) -> InProcessTeammateTaskSpec {
        InProcessTeammateTaskSpec {
            id: TaskId::new(id),
            identity: TeammateIdentity {
                agent_id: format!("{id}@team"),
                agent_name: id.into(),
                team_name: "team".into(),
                color: Some("blue".into()),
                plan_mode_required: false,
                parent_session_id: "sess-leader".into(),
            },
            prompt: "investigate x".into(),
            model: None,
            model_profile: None,
            permission_mode: "default".into(),
            agent_type: Some("Explore".into()),
            description: Some("research code".into()),
        }
    }

    #[test]
    fn register_teammate_populates_identity() {
        let reg = TaskRegistry::new();
        let id = register_in_process_teammate_task(&reg, teammate_spec("researcher"));
        let snap = reg.snapshot(&id).unwrap();
        assert_eq!(snap.kind, TaskKind::InProcessTeammate);
        assert_eq!(snap.title, "@researcher");
        match &snap.data {
            TaskData::InProcessTeammate(d) => {
                assert_eq!(d.identity.team_name, "team");
                assert_eq!(d.prompt, "investigate x");
                assert!(!d.shutdown_requested);
                assert!(!d.is_idle);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn inject_user_message_appends_to_queue() {
        let reg = TaskRegistry::new();
        let id = register_in_process_teammate_task(&reg, teammate_spec("researcher"));
        assert!(inject_user_message_to_teammate(&reg, &id, "hello".into()));
        assert!(inject_user_message_to_teammate(&reg, &id, "again".into()));
        let snap = reg.snapshot(&id).unwrap();
        if let TaskData::InProcessTeammate(d) = &snap.data {
            assert_eq!(
                d.pending_user_messages
                    .iter()
                    .map(|request| request.message.as_str())
                    .collect::<Vec<_>>(),
                vec!["hello", "again"]
            );
        } else {
            panic!();
        }
    }

    #[test]
    fn revive_teammate_emits_the_retry_user_message() {
        let reg = TaskRegistry::new();
        let id = register_in_process_teammate_task(&reg, teammate_spec("researcher"));
        assert!(finish_in_process_teammate(
            &reg,
            &id,
            TaskStatus::Failed,
            Some("failed".into()),
            Some("failed".into()),
        ));
        let cursor = reg.session_live_events(None).next_cursor;

        assert!(revive_in_process_teammate_task(
            &reg,
            &id,
            PromptCancel::new(),
            TeammateRequest {
                request_id: "retry-1".into(),
                message: "retry from UI".into(),
            },
        )
        .is_some());

        let batch = reg.session_live_events(Some(cursor));
        assert!(batch.events.iter().any(|event| {
            matches!(
                &event.kind,
                TaskLiveEventKind::UserMessage { text } if text == "retry from UI"
            )
        }));
        assert_eq!(reg.snapshot(&id).unwrap().status, TaskStatus::Running);
    }

    #[test]
    fn inject_user_message_echoes_into_teammate_transcript() {
        let reg = TaskRegistry::new();
        let id = register_in_process_teammate_task(&reg, teammate_spec("researcher"));
        assert!(inject_user_message_to_teammate(&reg, &id, "hello".into()));
        let snap = reg.snapshot(&id).unwrap();
        if let TaskData::InProcessTeammate(d) = &snap.data {
            assert!(matches!(
                d.transcript.as_slice(),
                [LocalAgentTranscriptEntry::User { text }] if text == "hello"
            ));
        } else {
            panic!();
        }
    }

    #[test]
    fn inject_user_message_rejects_terminal_task() {
        let reg = TaskRegistry::new();
        let id = register_in_process_teammate_task(&reg, teammate_spec("researcher"));
        kill_in_process_teammate(&reg, &id);
        assert!(!inject_user_message_to_teammate(&reg, &id, "late".into()));
    }

    #[test]
    fn request_teammate_shutdown_is_idempotent() {
        let reg = TaskRegistry::new();
        let id = register_in_process_teammate_task(&reg, teammate_spec("researcher"));
        assert!(request_teammate_shutdown(&reg, &id));
        // Second call is a no-op once shutdown is already requested.
        assert!(!request_teammate_shutdown(&reg, &id));
    }

    #[test]
    fn kill_in_process_teammate_clears_pending_and_flips_status() {
        let reg = TaskRegistry::new();
        let id = register_in_process_teammate_task(&reg, teammate_spec("researcher"));
        inject_user_message_to_teammate(&reg, &id, "pending".into());
        assert!(kill_in_process_teammate(&reg, &id));
        let snap = reg.snapshot(&id).unwrap();
        assert_eq!(snap.status, TaskStatus::Killed);
        if let TaskData::InProcessTeammate(d) = &snap.data {
            assert!(d.pending_user_messages.is_empty());
            assert!(d.shutdown_requested);
        } else {
            panic!();
        }
    }

    #[test]
    fn teammate_current_task_tracks_the_active_turn() {
        let reg = TaskRegistry::new();
        let id = register_in_process_teammate_task(&reg, teammate_spec("researcher"));

        assert!(set_in_process_teammate_current_task(
            &reg,
            &id,
            "follow-up implementation"
        ));
        assert_eq!(
            reg.snapshot(&id).unwrap().metadata_str("current_task"),
            Some("follow-up implementation")
        );

        stop_task(&reg, &id).unwrap();
        assert!(!set_in_process_teammate_current_task(
            &reg,
            &id,
            "late update"
        ));
    }

    #[test]
    fn killed_in_process_teammate_cannot_be_revived_by_late_updates() {
        let reg = TaskRegistry::new();
        let id = register_in_process_teammate_task(&reg, teammate_spec("researcher"));
        stop_task(&reg, &id).unwrap();

        assert!(!mark_in_process_teammate_running(&reg, &id));
        assert!(!mark_in_process_teammate_idle(
            &reg,
            &id,
            Some("late".into())
        ));
        assert!(!finish_in_process_teammate(
            &reg,
            &id,
            TaskStatus::Completed,
            Some("late".into()),
            None,
        ));
        assert_eq!(reg.snapshot(&id).unwrap().status, TaskStatus::Killed);
    }

    // ── Dream register + add_turn + complete + kill ────────────────

    #[test]
    fn register_dream_starts_in_starting_phase() {
        let reg = TaskRegistry::new();
        let id = register_dream_task(
            &reg,
            DreamTaskSpec {
                id: TaskId::new("d1"),
                sessions_reviewing: 5,
                prior_mtime: 12345,
            },
        );
        let snap = reg.snapshot(&id).unwrap();
        assert_eq!(snap.kind, TaskKind::Dream);
        assert_eq!(snap.title, "dreaming");
        if let TaskData::Dream(d) = &snap.data {
            assert_eq!(d.phase, DreamPhase::Starting);
            assert_eq!(d.sessions_reviewing, 5);
            assert_eq!(d.prior_mtime, 12345);
            assert!(d.turns.is_empty());
            assert!(d.files_touched.is_empty());
        } else {
            panic!();
        }
    }

    #[test]
    fn add_dream_turn_transitions_phase_when_files_touched() {
        let reg = TaskRegistry::new();
        let id = register_dream_task(
            &reg,
            DreamTaskSpec {
                id: TaskId::new("d1"),
                sessions_reviewing: 1,
                prior_mtime: 0,
            },
        );
        add_dream_turn(
            &reg,
            &id,
            DreamTurn {
                text: "reading".into(),
                tool_use_count: 2,
            },
            vec![],
        );
        // Phase still Starting (no file touched).
        if let TaskData::Dream(d) = &reg.snapshot(&id).unwrap().data {
            assert_eq!(d.phase, DreamPhase::Starting);
            assert_eq!(d.turns.len(), 1);
        }
        add_dream_turn(
            &reg,
            &id,
            DreamTurn {
                text: "editing".into(),
                tool_use_count: 3,
            },
            vec!["docs/REBON.md".into(), "docs/REBON.md".into()],
        );
        if let TaskData::Dream(d) = &reg.snapshot(&id).unwrap().data {
            assert_eq!(d.phase, DreamPhase::Updating);
            // Duplicate path de-duped.
            assert_eq!(d.files_touched, vec!["docs/REBON.md"]);
            assert_eq!(d.turns.len(), 2);
        }
    }

    #[test]
    fn add_dream_turn_skips_empty_noop() {
        let reg = TaskRegistry::new();
        let id = register_dream_task(
            &reg,
            DreamTaskSpec {
                id: TaskId::new("d1"),
                sessions_reviewing: 1,
                prior_mtime: 0,
            },
        );
        add_dream_turn(
            &reg,
            &id,
            DreamTurn {
                text: String::new(),
                tool_use_count: 0,
            },
            vec![],
        );
        if let TaskData::Dream(d) = &reg.snapshot(&id).unwrap().data {
            assert!(d.turns.is_empty());
        }
    }

    #[test]
    fn add_dream_turn_caps_at_dream_max_turns() {
        let reg = TaskRegistry::new();
        let id = register_dream_task(
            &reg,
            DreamTaskSpec {
                id: TaskId::new("d1"),
                sessions_reviewing: 1,
                prior_mtime: 0,
            },
        );
        for i in 0..(DREAM_MAX_TURNS + 5) {
            add_dream_turn(
                &reg,
                &id,
                DreamTurn {
                    text: format!("t{i}"),
                    tool_use_count: 0,
                },
                vec![],
            );
        }
        if let TaskData::Dream(d) = &reg.snapshot(&id).unwrap().data {
            assert_eq!(d.turns.len(), DREAM_MAX_TURNS);
            // Oldest turns were evicted; the latest should be t{n-1}.
            assert_eq!(
                d.turns.last().unwrap().text,
                format!("t{}", DREAM_MAX_TURNS + 4)
            );
        }
    }

    #[test]
    fn kill_dream_task_returns_prior_mtime() {
        let reg = TaskRegistry::new();
        let id = register_dream_task(
            &reg,
            DreamTaskSpec {
                id: TaskId::new("d1"),
                sessions_reviewing: 1,
                prior_mtime: 999,
            },
        );
        assert_eq!(kill_dream_task(&reg, &id), Some(999));
        assert_eq!(reg.snapshot(&id).unwrap().status, TaskStatus::Killed);
        // Second kill is a no-op and returns None because the task is
        // no longer running.
        assert_eq!(kill_dream_task(&reg, &id), None);
    }

    #[test]
    fn complete_dream_task_marks_completed() {
        let reg = TaskRegistry::new();
        let id = register_dream_task(
            &reg,
            DreamTaskSpec {
                id: TaskId::new("d1"),
                sessions_reviewing: 1,
                prior_mtime: 0,
            },
        );
        complete_dream_task(&reg, &id);
        let snap = reg.snapshot(&id).unwrap();
        assert_eq!(snap.status, TaskStatus::Completed);
        assert!(snap.notified);
        assert!(snap.end_time_ms.is_some());
    }

    #[test]
    fn killed_dream_cannot_be_revived_by_late_completion() {
        let reg = TaskRegistry::new();
        let id = register_dream_task(
            &reg,
            DreamTaskSpec {
                id: TaskId::new("d1"),
                sessions_reviewing: 1,
                prior_mtime: 0,
            },
        );
        stop_task(&reg, &id).unwrap();

        complete_dream_task(&reg, &id);
        fail_dream_task(&reg, &id, Some("late".into()));

        assert_eq!(reg.snapshot(&id).unwrap().status, TaskStatus::Killed);
    }

    // ── Workflow / monitor mcp ─────────────────────────────────────

    #[test]
    fn register_local_workflow_uses_summary_when_present() {
        let reg = TaskRegistry::new();
        let id = register_local_workflow_task(
            &reg,
            LocalWorkflowTaskSpec {
                id: TaskId::new("w1"),
                run_id: "wf_test".into(),
                workflow_name: "release-train".into(),
                summary: Some("rc-0.0.2".into()),
                agent_count: 4,
                output_path: Some("/tmp/wf".into()),
                script_path: Some("/tmp/wf.js".into()),
                args: Some(serde_json::json!({"release":"0.0.2"})),
                is_backgrounded: true,
                parent_session_id: Some("session-workflow".into()),
                parent_tool_call_id: Some("toolu_workflow_card".into()),
            },
            PromptCancel::new(),
        )
        .unwrap();
        let snap = reg.snapshot(&id).unwrap();
        assert_eq!(snap.kind, TaskKind::LocalWorkflow);
        assert_eq!(snap.title, "rc-0.0.2");
        assert_eq!(
            snap.metadata
                .get("parent_session_id")
                .and_then(|v| v.as_str()),
            Some("session-workflow")
        );
        if let TaskData::LocalWorkflow(d) = &snap.data {
            assert_eq!(d.workflow_name, "release-train");
            assert_eq!(d.run_id, "wf_test");
            assert_eq!(d.agent_count, 4);
            assert_eq!(d.token_count, 0);
        }
    }

    #[test]
    fn register_local_workflow_tracks_progress_and_completion() {
        let reg = TaskRegistry::new();
        let id = register_local_workflow_task(
            &reg,
            LocalWorkflowTaskSpec {
                id: TaskId::new("w2"),
                run_id: "wf_progress".into(),
                workflow_name: "research".into(),
                summary: None,
                agent_count: 0,
                output_path: None,
                script_path: None,
                args: None,
                is_backgrounded: true,
                parent_session_id: None,
                parent_tool_call_id: None,
            },
            PromptCancel::new(),
        )
        .unwrap();
        let sequence = push_local_workflow_progress(
            &reg,
            &id,
            WorkflowProgressEntry::Agent {
                index: 1,
                state: "completed".into(),
                phase_title: Some("Run".into()),
                phase_id: Some("phase-1".into()),
                label: "inspect".into(),
                tokens: 10,
                tool_calls: 2,
                tool_call_details: Vec::new(),
                duration_ms: Some(50),
                error: None,
                agent_id: Some("agent-abc".into()),
            },
        );
        assert_eq!(sequence, 1);
        let snap = reg.snapshot(&id).unwrap();
        assert_eq!(
            snap.last_progress.as_deref(),
            Some("agent completed: inspect")
        );
        if let TaskData::LocalWorkflow(data) = &snap.data {
            assert_eq!(data.agent_count, 1);
            assert_eq!(data.token_count, 10);
            assert_eq!(data.tool_use_count, 2);
            assert_eq!(data.progress_entries.len(), 1);
            assert!(matches!(
                &data.progress_entries[0],
                WorkflowProgressEntry::Agent {
                    agent_id: Some(agent_id),
                    ..
                } if agent_id == "agent-abc"
            ));
        }

        complete_local_workflow_task(&reg, &id, serde_json::json!({"ok": true}), 1, true);
        let snap = reg.snapshot(&id).unwrap();
        assert_eq!(snap.status, TaskStatus::Completed);
        assert!(snap.result.is_some());
        assert!(!snap.notified);
        let notifications = reg.take_unnotified_terminal_agent_notifications();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].contains("<task-type>local_workflow</task-type>"));
        assert!(notifications[0].contains("<run_id>wf_progress</run_id>"));
        assert!(notifications[0].contains("<agent_count>1</agent_count>"));
        assert!(reg.snapshot(&id).unwrap().notified);
    }

    /// The local workflow live stream must close with exactly one terminal
    /// event and carry a stable tool_use_id on synthesized progress — an
    /// empty id (or a stream that never finished) left frontends spinning
    /// until the parent turn checkpointed.
    #[test]
    fn local_workflow_live_events_use_stable_id_and_finish() {
        let reg = TaskRegistry::new();
        let spec = |task: &str, run: &str| LocalWorkflowTaskSpec {
            id: TaskId::new(task),
            run_id: run.into(),
            workflow_name: "research".into(),
            summary: None,
            agent_count: 0,
            output_path: None,
            script_path: None,
            args: None,
            is_backgrounded: true,
            parent_session_id: None,
            parent_tool_call_id: Some("toolu_parent".into()),
        };
        let id = register_local_workflow_task(&reg, spec("w3", "wf_events"), PromptCancel::new())
            .unwrap();
        push_local_workflow_progress(
            &reg,
            &id,
            WorkflowProgressEntry::Log {
                message: "tick".into(),
            },
        );
        complete_local_workflow_task(&reg, &id, serde_json::json!({"ok": true}), 1, false);
        // A late failure after the terminal transition is a no-op and must
        // not emit a second Finished.
        fail_local_workflow_task(&reg, &id, "late".into(), false);

        let events = reg.task_live_events(&id, None).expect("journal").events;
        let progress_ids: Vec<String> = events
            .iter()
            .filter_map(|event| match &event.kind {
                TaskLiveEventKind::ToolProgress { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(progress_ids, vec!["toolu_parent".to_string()]);
        let finished: Vec<TaskStatus> = events
            .iter()
            .filter_map(|event| match &event.kind {
                TaskLiveEventKind::Finished { status, .. } => Some(status.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(finished, vec![TaskStatus::Completed]);

        // Without a parent tool call, the progress id falls back to a
        // task-scoped placeholder rather than an empty string, and a
        // failure closes the stream with the error attached.
        let orphan = register_local_workflow_task(
            &reg,
            LocalWorkflowTaskSpec {
                parent_tool_call_id: None,
                ..spec("w4", "wf_orphan")
            },
            PromptCancel::new(),
        )
        .unwrap();
        push_local_workflow_progress(
            &reg,
            &orphan,
            WorkflowProgressEntry::Log {
                message: "tick".into(),
            },
        );
        fail_local_workflow_task(&reg, &orphan, "boom".into(), false);
        let events = reg.task_live_events(&orphan, None).expect("journal").events;
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            TaskLiveEventKind::ToolProgress { tool_use_id, .. } if tool_use_id == "workflow:w4"
        )));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            TaskLiveEventKind::Finished {
                status: TaskStatus::Failed,
                error: Some(error),
            } if error == "boom"
        )));
    }

    // ── stop_task dispatch ─────────────────────────────────────────

    #[test]
    fn stop_task_not_found_returns_error() {
        let reg = TaskRegistry::new();
        let err = stop_task(&reg, &TaskId::new("ghost")).unwrap_err();
        assert!(matches!(err, StopTaskError::NotFound(_)));
    }

    #[test]
    fn stop_task_not_running_returns_error() {
        let reg = TaskRegistry::new();
        let id = register_in_process_teammate_task(&reg, teammate_spec("bob"));
        kill_in_process_teammate(&reg, &id);
        let err = stop_task(&reg, &id).unwrap_err();
        match err {
            StopTaskError::NotRunning(_, status) => assert_eq!(status, "killed"),
            _ => panic!(),
        }
    }

    #[test]
    fn stop_task_dispatches_to_remote_agent_kill() {
        let reg = TaskRegistry::new();
        let id = register_remote_agent_task(&reg, remote_spec("r1", true));
        let result = stop_task(&reg, &id).unwrap();
        assert_eq!(result.task_type, "remote_agent");
        assert_eq!(reg.snapshot(&id).unwrap().status, TaskStatus::Killed);
    }

    #[test]
    fn stop_task_dispatches_to_dream_kill() {
        let reg = TaskRegistry::new();
        let id = register_dream_task(
            &reg,
            DreamTaskSpec {
                id: TaskId::new("d1"),
                sessions_reviewing: 1,
                prior_mtime: 777,
            },
        );
        let result = stop_task(&reg, &id).unwrap();
        assert_eq!(result.task_type, "dream");
        assert_eq!(reg.snapshot(&id).unwrap().status, TaskStatus::Killed);
    }

    fn local_agent_snapshot(id: &str, status: TaskStatus, backgrounded: bool) -> TaskSnapshot {
        TaskSnapshot {
            id: TaskId::new(id),
            kind: TaskKind::LocalAgent,
            status,
            title: "Stop me".into(),
            last_progress: None,
            error: None,
            result: None,
            is_backgrounded: backgrounded,
            notified: false,
            start_time_ms: 0,
            end_time_ms: None,
            metadata: empty_metadata(),
            data: TaskData::LocalAgent(LocalAgentData {
                prompt: "work".into(),
                agent_type: "worker".into(),
                model: Some("mock".into()),
                system: None,
                allowed_tools: None,
                token_count: 0,
                tool_use_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
                pending_messages: Vec::new(),
                retrieved: false,
            }),
        }
    }

    #[test]
    fn stopped_background_local_agent_still_notifies_coordinator() {
        let reg = TaskRegistry::new();
        let id = TaskId::new("agent-stop");
        reg.insert(
            id.clone(),
            local_agent_snapshot("agent-stop", TaskStatus::Running, true),
            PromptCancel::new(),
        );

        let result = stop_task(&reg, &id).unwrap();
        assert_eq!(result.task_type, "local_agent");
        let snap = reg.snapshot(&id).unwrap();
        assert_eq!(snap.status, TaskStatus::Killed);
        assert!(!snap.notified);
        assert!(matches!(
            reg.task_live_events(&id, None)
                .expect("task events")
                .events
                .last()
                .map(|event| &event.kind),
            Some(TaskLiveEventKind::Finished {
                status: TaskStatus::Killed,
                error: None,
            })
        ));
        let notifications = reg.take_unnotified_terminal_agent_notifications();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].contains("<status>killed</status>"));
        assert!(notifications[0].contains("was stopped"));
    }

    /// Closing a parked worker whose result already reached the coordinator
    /// must not republish it. The kill changes only `continuable`, so a
    /// second notification is the same report a second time — which is what
    /// made "stop the idle worker" look like the thing that delivered it.
    #[test]
    fn stopping_a_reported_parked_worker_does_not_replay_its_result() {
        let reg = TaskRegistry::new();
        let id = TaskId::new("agent-parked-stop");
        let mut snapshot = local_agent_snapshot(id.as_str(), TaskStatus::Running, true);
        snapshot.metadata[LOCAL_AGENT_IDLE_METADATA_KEY] = Value::Bool(true);
        reg.insert(id.clone(), snapshot, PromptCancel::new());

        let delivered = reg.unnotified_terminal_notifications();
        assert_eq!(delivered.len(), 1, "a parked worker reports once");
        reg.mark_notification_generations_delivered(
            &delivered
                .iter()
                .map(|notification| (notification.task_id.clone(), notification.generation))
                .collect::<Vec<_>>(),
        );

        stop_task(&reg, &id).unwrap();

        assert_eq!(reg.snapshot(&id).unwrap().status, TaskStatus::Killed);
        assert!(
            reg.unnotified_terminal_notifications().is_empty(),
            "closing an already-reported worker must not resend its report"
        );
    }

    /// The other half of the same rule: a worker stopped mid-turn never
    /// reported anything, so the kill still owes the coordinator a notice.
    #[test]
    fn stopping_a_working_agent_still_reports_it_was_stopped() {
        let reg = TaskRegistry::new();
        let id = TaskId::new("agent-working-stop");
        reg.insert(
            id.clone(),
            local_agent_snapshot(id.as_str(), TaskStatus::Running, true),
            PromptCancel::new(),
        );

        stop_task(&reg, &id).unwrap();

        let notifications = reg.take_unnotified_terminal_agent_notifications();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].contains("was stopped"));
    }

    #[test]
    fn stop_task_cancels_pending_local_agent_without_resurrection() {
        let reg = TaskRegistry::new();
        let id = TaskId::new("agent-pending-stop");
        let cancel = PromptCancel::new();
        reg.insert(
            id.clone(),
            local_agent_snapshot("agent-pending-stop", TaskStatus::Pending, true),
            cancel.clone(),
        );

        stop_task(&reg, &id).unwrap();

        assert!(cancel.is_cancelled());
        assert_eq!(reg.snapshot(&id).unwrap().status, TaskStatus::Killed);
        reg.update(&id, |snap| snap.status = TaskStatus::Completed);
        reg.insert(
            id.clone(),
            local_agent_snapshot("agent-pending-stop", TaskStatus::Running, true),
            PromptCancel::new(),
        );
        assert_eq!(reg.snapshot(&id).unwrap().status, TaskStatus::Killed);
    }

    #[tokio::test]
    async fn runtime_controller_stop_task_cancels_local_agent() {
        let reg = TaskRegistry::new();
        let id = TaskId::new("agent-runtime-stop");
        let cancel = PromptCancel::new();
        reg.insert(
            id.clone(),
            local_agent_snapshot("agent-runtime-stop", TaskStatus::Running, true),
            cancel.clone(),
        );
        let controller = TaskRegistryRuntimeController::for_test_registry(reg.clone());

        let outcome = controller
            .stop_task("session-a", "agent-runtime-stop")
            .await
            .unwrap();
        match outcome {
            StopTaskOutcome::Stopped {
                task_id,
                task_type,
                command,
            } => {
                assert_eq!(task_id, "agent-runtime-stop");
                assert_eq!(task_type, "local_agent");
                assert_eq!(command, "Stop me");
            }
            other => panic!("expected stopped, got {other:?}"),
        }
        assert!(cancel.is_cancelled());
        assert_eq!(reg.snapshot(&id).unwrap().status, TaskStatus::Killed);
    }

    #[tokio::test]
    async fn runtime_controller_stop_task_is_idempotent_for_monitors() {
        let reg = TaskRegistry::new();
        let cancel = PromptCancel::new();
        let controller = TaskRegistryRuntimeController::for_test_registry(reg.clone());
        controller.monitor_started(
            "session-a",
            MonitorTaskSpec {
                task_id: "monitor-runtime-stop".into(),
                description: "Watch events".into(),
                source: MonitorTaskSource::WebSocket,
                redacted_target: "ws://localhost:9000".into(),
                session_id: Some("session-a".into()),
                agent_id: None,
                started_at_ms: 10,
            },
            cancel.clone(),
        );

        for _ in 0..2 {
            let outcome = controller
                .stop_task("session-a", "monitor-runtime-stop")
                .await
                .unwrap();
            match outcome {
                StopTaskOutcome::Stopped {
                    task_id,
                    task_type,
                    command,
                } => {
                    assert_eq!(task_id, "monitor-runtime-stop");
                    assert_eq!(task_type, "monitor");
                    assert_eq!(command, "Watch events");
                }
                other => panic!("expected stopped, got {other:?}"),
            }
        }

        assert!(cancel.is_cancelled());
        let snapshot = reg.snapshot(&TaskId::new("monitor-runtime-stop")).unwrap();
        assert_eq!(snapshot.status, TaskStatus::Killed);
        assert!(matches!(
            snapshot.data,
            TaskData::Monitor(MonitorData {
                end_reason: Some(MonitorEndReason::Stopped),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn runtime_controller_send_message_to_task_queues_and_poller_drains() {
        let reg = TaskRegistry::new();
        let id = TaskId::new("agent-runtime-message");
        reg.insert(
            id.clone(),
            local_agent_snapshot("agent-runtime-message", TaskStatus::Running, false),
            PromptCancel::new(),
        );
        let controller = TaskRegistryRuntimeController::for_test_registry(reg.clone());

        controller
            .send_message_to_task(
                "session-a",
                "agent-runtime-message",
                "new instruction".into(),
            )
            .await
            .unwrap();
        controller
            .send_message_to_task("session-a", "agent-runtime-message", "follow-up".into())
            .await
            .unwrap();
        assert_eq!(
            match &reg.snapshot(&id).unwrap().data {
                TaskData::LocalAgent(data) => data.pending_messages.clone(),
                _ => unreachable!(),
            },
            vec!["new instruction".to_string(), "follow-up".to_string()]
        );

        let drained = take_pending_local_agent_messages(&reg, &id);
        assert_eq!(
            drained,
            vec!["new instruction".to_string(), "follow-up".to_string()]
        );
        assert!(match &reg.snapshot(&id).unwrap().data {
            TaskData::LocalAgent(data) => data.pending_messages.is_empty(),
            _ => false,
        });
    }

    #[test]
    fn task_snapshot_metadata_helpers_read_string_values() {
        let mut snapshot = local_agent_snapshot("agent-meta", TaskStatus::Running, false);
        snapshot.metadata = json!({
            "ultraplan_id": "ultraplan-123",
            "ultraplan_role": "researcher",
            "non_string": 42,
        });

        assert_eq!(snapshot.metadata_str("ultraplan_id"), Some("ultraplan-123"));
        assert_eq!(snapshot.ultraplan_id(), Some("ultraplan-123"));
        assert_eq!(snapshot.ultraplan_role(), Some("researcher"));
        assert_eq!(snapshot.metadata_str("non_string"), None);
        assert_eq!(snapshot.metadata_str("missing"), None);
    }

    // ── kill_shell_tasks_for_agent ─────────────────────────────────

    fn insert_shell_with_agent(
        reg: &TaskRegistry,
        id: &str,
        agent_id: Option<&str>,
        status: TaskStatus,
    ) {
        let snapshot = TaskSnapshot {
            id: TaskId::new(id),
            kind: TaskKind::LocalShell,
            status,
            title: "cmd".into(),
            last_progress: None,
            error: None,
            result: None,
            is_backgrounded: true,
            notified: false,
            start_time_ms: 0,
            end_time_ms: None,
            metadata: empty_metadata(),
            data: TaskData::LocalShell(LocalShellData {
                command: "echo".into(),
                exit_code: None,
                interrupted: false,
                display_kind: BashTaskKind::Bash,
                agent_id: agent_id.map(str::to_owned),
            }),
        };
        reg.insert(TaskId::new(id), snapshot, PromptCancel::new());
    }

    #[test]
    fn kill_shell_tasks_for_agent_kills_only_matching_running() {
        let reg = TaskRegistry::new();
        insert_shell_with_agent(&reg, "s1", Some("agent-a"), TaskStatus::Running);
        insert_shell_with_agent(&reg, "s2", Some("agent-a"), TaskStatus::Running);
        insert_shell_with_agent(&reg, "s3", Some("agent-b"), TaskStatus::Running);
        insert_shell_with_agent(&reg, "s4", Some("agent-a"), TaskStatus::Completed);
        insert_shell_with_agent(&reg, "s5", None, TaskStatus::Running);

        let killed = kill_shell_tasks_for_agent(&reg, "agent-a");
        assert_eq!(killed, 2);
        assert_eq!(
            reg.snapshot(&TaskId::new("s1")).unwrap().status,
            TaskStatus::Killed
        );
        assert_eq!(
            reg.snapshot(&TaskId::new("s2")).unwrap().status,
            TaskStatus::Killed
        );
        assert_eq!(
            reg.snapshot(&TaskId::new("s3")).unwrap().status,
            TaskStatus::Running
        );
        assert_eq!(
            reg.snapshot(&TaskId::new("s4")).unwrap().status,
            TaskStatus::Completed
        );
        assert_eq!(
            reg.snapshot(&TaskId::new("s5")).unwrap().status,
            TaskStatus::Running
        );
    }

    // ── generate_task_id ───────────────────────────────────────────

    #[test]
    fn generate_task_id_uses_per_kind_prefix() {
        assert_eq!(
            generate_task_id(TaskKind::LocalShell)
                .as_str()
                .chars()
                .next(),
            Some('b')
        );
        assert_eq!(
            generate_task_id(TaskKind::Dream).as_str().chars().next(),
            Some('d')
        );
    }

    #[test]
    fn generate_task_id_has_9_chars_total() {
        let id = generate_task_id(TaskKind::RemoteAgent);
        assert_eq!(id.as_str().len(), 9);
    }

    #[test]
    fn generate_task_id_uniqueness_across_batch() {
        let mut ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for _ in 0..128 {
            ids.insert(generate_task_id(TaskKind::LocalAgent).as_str().to_owned());
        }
        // Even without a crypto RNG, 128 ids from the
        // wall-clock × counter seed should collide at most a handful
        // of times in the worst case — in practice we get a full set.
        assert!(ids.len() >= 120);
    }

    // ── pill_label ─────────────────────────────────────────────────

    #[test]
    fn pill_label_empty_is_empty_string() {
        assert_eq!(pill_label(&[]), "");
    }

    #[test]
    fn local_agent_notification_xml_includes_git_metadata() {
        let snap = TaskSnapshot {
            id: TaskId::new("agent-git"),
            kind: TaskKind::LocalAgent,
            status: TaskStatus::Completed,
            title: "implementation".into(),
            last_progress: Some("done".into()),
            error: None,
            result: Some(json!({
                "final_text": "done",
                "output_file": "C:/tasks/agent-git.report.md",
                "git": {
                    "worktree_path": "C:/repo/.rebon/worktrees/agent-git",
                    "worktree_branch": "rebon/agent-agent-git",
                    "base_commit": "1111111",
                    "head_commit": "2222222",
                    "commit_hash": "2222222",
                    "dirty_after_commit": false,
                    "validation_status": "passed",
                    "status_output": ""
                }
            })),
            is_backgrounded: true,
            notified: false,
            start_time_ms: 0,
            end_time_ms: Some(10),
            metadata: empty_metadata(),
            data: TaskData::LocalAgent(LocalAgentData {
                prompt: "fix".into(),
                agent_type: "worker".into(),
                model: Some("mock".into()),
                system: None,
                allowed_tools: None,
                token_count: 0,
                tool_use_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
                pending_messages: Vec::new(),
                retrieved: false,
            }),
        };

        let xml = build_local_agent_notification_xml(&snap);
        assert!(xml.contains("<git>"));
        assert!(xml.contains("<worktree_path>C:/repo/.rebon/worktrees/agent-git</worktree_path>"));
        assert!(xml.contains("<branch>rebon/agent-agent-git</branch>"));
        assert!(xml.contains("<base>1111111</base>"));
        assert!(xml.contains("<head>2222222</head>"));
        assert!(xml.contains("<commit>2222222</commit>"));
        assert!(xml.contains("<dirty>false</dirty>"));
        assert!(xml.contains("<validation_status>passed</validation_status>"));
    }

    #[test]
    fn pill_label_single_shell() {
        let reg = TaskRegistry::new();
        insert_shell_with_agent(&reg, "s1", None, TaskStatus::Running);
        let label = pill_label(&reg.snapshots());
        assert_eq!(label, "1 shell");
    }

    #[test]
    fn pill_label_multiple_shells() {
        let reg = TaskRegistry::new();
        insert_shell_with_agent(&reg, "s1", None, TaskStatus::Running);
        insert_shell_with_agent(&reg, "s2", None, TaskStatus::Running);
        insert_shell_with_agent(&reg, "s3", None, TaskStatus::Running);
        assert_eq!(pill_label(&reg.snapshots()), "3 shells");
    }

    #[test]
    fn pill_label_mixed_shells_and_monitors() {
        let reg = TaskRegistry::new();
        insert_shell_with_agent(&reg, "s1", None, TaskStatus::Running);
        // Second shell is a monitor.
        let monitor = TaskSnapshot {
            id: TaskId::new("m1"),
            kind: TaskKind::LocalShell,
            status: TaskStatus::Running,
            title: "watch logs".into(),
            last_progress: None,
            error: None,
            result: None,
            is_backgrounded: true,
            notified: false,
            start_time_ms: 0,
            end_time_ms: None,
            metadata: empty_metadata(),
            data: TaskData::LocalShell(LocalShellData {
                command: "tail -f".into(),
                exit_code: None,
                interrupted: false,
                display_kind: BashTaskKind::Monitor,
                agent_id: None,
            }),
        };
        reg.insert(TaskId::new("m1"), monitor, PromptCancel::new());
        let label = pill_label(&reg.snapshots());
        assert!(label.contains("1 shell"));
        assert!(label.contains("1 monitor"));
    }

    #[test]
    fn pill_label_single_local_agent() {
        let reg = TaskRegistry::new();
        let id = TaskId::new("a1");
        let snap = TaskSnapshot {
            id: id.clone(),
            kind: TaskKind::LocalAgent,
            status: TaskStatus::Running,
            title: "agent task".into(),
            last_progress: None,
            error: None,
            result: None,
            is_backgrounded: true,
            notified: false,
            start_time_ms: 0,
            end_time_ms: None,
            metadata: empty_metadata(),
            data: TaskData::LocalAgent(LocalAgentData {
                prompt: "hi".into(),
                agent_type: "general-purpose".into(),
                model: None,
                system: None,
                allowed_tools: None,
                token_count: 0,
                tool_use_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
                pending_messages: Vec::new(),
                retrieved: false,
            }),
        };
        reg.insert(id, snap, PromptCancel::new());
        assert_eq!(pill_label(&reg.snapshots()), "1 local agent");
    }

    #[test]
    fn pill_label_single_ultraplan_plan_ready() {
        let reg = TaskRegistry::new();
        let id = register_remote_agent_task(&reg, remote_spec("r1", true));
        update_ultraplan_phase(&reg, &id, Some(UltraplanPhase::PlanReady));
        let label = pill_label(&reg.snapshots());
        assert!(label.contains("ultraplan ready"));
    }

    #[test]
    fn pill_label_single_ultraplan_needs_input() {
        let reg = TaskRegistry::new();
        let id = register_remote_agent_task(&reg, remote_spec("r1", true));
        update_ultraplan_phase(&reg, &id, Some(UltraplanPhase::NeedsInput));
        let label = pill_label(&reg.snapshots());
        assert!(label.contains("needs your input"));
    }

    #[test]
    fn pill_label_teammates_counts_teams_not_agents() {
        let reg = TaskRegistry::new();
        let mut spec_a = teammate_spec("alice");
        spec_a.identity.team_name = "red".into();
        let mut spec_b = teammate_spec("bob");
        spec_b.identity.team_name = "red".into();
        let mut spec_c = teammate_spec("carol");
        spec_c.identity.team_name = "blue".into();
        register_in_process_teammate_task(&reg, spec_a);
        register_in_process_teammate_task(&reg, spec_b);
        register_in_process_teammate_task(&reg, spec_c);
        assert_eq!(pill_label(&reg.snapshots()), "2 teams");
    }

    #[test]
    fn pill_label_dream_always_dreaming() {
        let reg = TaskRegistry::new();
        register_dream_task(
            &reg,
            DreamTaskSpec {
                id: TaskId::new("d1"),
                sessions_reviewing: 1,
                prior_mtime: 0,
            },
        );
        assert_eq!(pill_label(&reg.snapshots()), "dreaming");
    }

    #[test]
    fn pill_label_mixed_kinds_falls_through_to_generic() {
        let reg = TaskRegistry::new();
        insert_shell_with_agent(&reg, "s1", None, TaskStatus::Running);
        register_remote_agent_task(&reg, remote_spec("r1", false));
        let label = pill_label(&reg.snapshots());
        assert_eq!(label, "2 background tasks");
    }

    #[test]
    fn pill_needs_cta_only_true_for_ultraplan_with_phase() {
        let reg = TaskRegistry::new();
        let id = register_remote_agent_task(&reg, remote_spec("r1", true));
        assert!(!pill_needs_cta(&reg.snapshots()));
        update_ultraplan_phase(&reg, &id, Some(UltraplanPhase::PlanReady));
        assert!(pill_needs_cta(&reg.snapshots()));
    }

    #[test]
    fn pill_needs_cta_false_when_multiple_tasks() {
        let reg = TaskRegistry::new();
        let id = register_remote_agent_task(&reg, remote_spec("r1", true));
        update_ultraplan_phase(&reg, &id, Some(UltraplanPhase::PlanReady));
        insert_shell_with_agent(&reg, "s1", None, TaskStatus::Running);
        assert!(!pill_needs_cta(&reg.snapshots()));
    }

    #[tokio::test]
    async fn task_waker_delivers_pending_message_without_polling() {
        let reg = TaskRegistry::new();
        let id = register_in_process_teammate_task(&reg, teammate_spec("wake-agent"));
        let wake = reg.task_waker(&id).expect("runtime wake handle");

        assert!(inject_user_message_to_teammate(
            &reg,
            &id,
            "continue".into()
        ));
        tokio::time::timeout(Duration::from_millis(50), wake.notified())
            .await
            .expect("queued message should store a wake permit");
    }

    #[test]
    fn stale_turn_token_cannot_update_after_runtime_invalidation() {
        let reg = TaskRegistry::new();
        let id = register_in_process_teammate_task(&reg, teammate_spec("epoch-agent"));
        let turn = reg.begin_task_turn(&id).expect("first turn");
        assert!(reg.update_task_turn(&turn, |snapshot| {
            snapshot.last_progress = Some("current".into());
        }));

        assert!(reg.cancel(&id));
        assert!(!reg.update_task_turn(&turn, |snapshot| {
            snapshot.last_progress = Some("late".into());
        }));
        assert!(!reg.record_task_turn_terminal_event(
            &turn,
            TaskLiveEventKind::Finished {
                status: TaskStatus::Completed,
                error: None,
            },
        ));
        assert_eq!(
            reg.snapshot(&id)
                .and_then(|snapshot| snapshot.last_progress),
            Some("current".into())
        );
        assert!(reg
            .task_live_events(&id, None)
            .expect("task event journal")
            .events
            .is_empty());
    }

    #[test]
    fn stale_notification_ack_cannot_consume_next_agent_turn() {
        let reg = TaskRegistry::new();
        let id = TaskId::new("notification-generation-agent");
        let mut snapshot = local_agent_snapshot(id.as_str(), TaskStatus::Running, true);
        snapshot.metadata[LOCAL_AGENT_IDLE_METADATA_KEY] = Value::Bool(true);
        reg.insert(id.clone(), snapshot, PromptCancel::new());

        let first = reg
            .unnotified_terminal_notifications()
            .into_iter()
            .next()
            .expect("first idle notification");
        let turn = reg.begin_task_turn(&id).expect("next actor turn");
        assert!(reg.finish_task_turn(&turn));
        let second = reg
            .unnotified_terminal_notifications()
            .into_iter()
            .next()
            .expect("second idle notification");
        assert_ne!(first.generation, second.generation);

        reg.mark_notification_generations_delivered(&[(first.task_id, first.generation)]);
        assert!(!reg.snapshot(&id).expect("task").notified);
        assert_eq!(
            reg.unnotified_terminal_notifications()[0].generation,
            second.generation
        );

        reg.mark_notification_generations_delivered(&[(second.task_id, second.generation)]);
        assert!(reg.snapshot(&id).expect("task").notified);
    }

    #[test]
    fn agent_notification_reports_whether_the_worker_is_still_continuable() {
        let reg = TaskRegistry::new();
        let turn_result = json!({ "status": "completed", "final_text": "turn done" });

        let idle_id = TaskId::new("idle-agent");
        let mut idle = local_agent_snapshot(idle_id.as_str(), TaskStatus::Running, true);
        idle.metadata[LOCAL_AGENT_IDLE_METADATA_KEY] = Value::Bool(true);
        idle.result = Some(turn_result.clone());
        reg.insert(idle_id.clone(), idle, PromptCancel::new());

        let closed_id = TaskId::new("closed-agent");
        let mut closed = local_agent_snapshot(closed_id.as_str(), TaskStatus::Completed, true);
        closed.result = Some(turn_result);
        reg.insert(closed_id.clone(), closed, PromptCancel::new());

        let notifications = reg.unnotified_terminal_notifications();
        let note = |id: &TaskId| {
            notifications
                .iter()
                .find(|notification| notification.task_id == *id)
                .map(|notification| notification.message.clone())
                .expect("notification")
        };

        let idle_note = note(&idle_id);
        assert!(
            idle_note.contains("<status>completed</status>"),
            "{idle_note}"
        );
        assert!(
            idle_note.contains("<continuable>true</continuable>"),
            "{idle_note}"
        );
        // The summary is the line the user's transcript renders — the
        // resumability signal belongs in its own element, not there.
        assert!(
            idle_note.contains("<summary>Agent &quot;Stop me&quot; completed</summary>"),
            "{idle_note}"
        );

        let closed_note = note(&closed_id);
        assert!(
            closed_note.contains("<continuable>false</continuable>"),
            "{closed_note}"
        );
    }

    #[test]
    fn local_agent_message_lease_rolls_back_without_losing_or_duplicating_messages() {
        let reg = TaskRegistry::new();
        let id = TaskId::new("leased-agent");
        reg.insert(
            id.clone(),
            local_agent_snapshot(id.as_str(), TaskStatus::Running, true),
            PromptCancel::new(),
        );
        assert!(inject_user_message_to_local_agent(
            &reg,
            &id,
            "first".into()
        ));

        let lease = reg
            .lease_pending_local_agent_messages(&id)
            .expect("message lease");
        assert_eq!(lease.messages(), &["first"]);
        for index in 1..MAX_PENDING_AGENT_MESSAGES {
            assert!(inject_user_message_to_local_agent(
                &reg,
                &id,
                format!("message-{index}")
            ));
        }
        assert!(!inject_user_message_to_local_agent(
            &reg,
            &id,
            "overflow".into()
        ));

        drop(lease);
        let snapshot = reg.snapshot(&id).expect("task");
        let TaskData::LocalAgent(data) = snapshot.data else {
            panic!("expected local agent");
        };
        assert_eq!(data.pending_messages.len(), MAX_PENDING_AGENT_MESSAGES);
        assert_eq!(
            data.pending_messages.first().map(String::as_str),
            Some("first")
        );
        assert_eq!(
            data.transcript
                .iter()
                .filter(|entry| matches!(entry, LocalAgentTranscriptEntry::User { text } if text == "first"))
                .count(),
            1
        );
    }

    #[test]
    fn committed_local_agent_message_lease_releases_queue_capacity() {
        let reg = TaskRegistry::new();
        let id = TaskId::new("committed-lease-agent");
        reg.insert(
            id.clone(),
            local_agent_snapshot(id.as_str(), TaskStatus::Running, true),
            PromptCancel::new(),
        );
        assert!(inject_user_message_to_local_agent(
            &reg,
            &id,
            "first".into()
        ));
        let lease = reg
            .lease_pending_local_agent_messages(&id)
            .expect("message lease");
        lease.commit();

        for index in 0..MAX_PENDING_AGENT_MESSAGES {
            assert!(inject_user_message_to_local_agent(
                &reg,
                &id,
                format!("next-{index}")
            ));
        }
    }

    #[test]
    fn pending_messages_and_transcripts_are_bounded() {
        let reg = TaskRegistry::new();
        let id = TaskId::new("bounded-agent");
        reg.insert(
            id.clone(),
            local_agent_snapshot("bounded-agent", TaskStatus::Running, true),
            PromptCancel::new(),
        );
        for index in 0..MAX_PENDING_AGENT_MESSAGES {
            assert!(inject_user_message_to_local_agent(
                &reg,
                &id,
                format!("message-{index}")
            ));
        }
        assert!(!inject_user_message_to_local_agent(
            &reg,
            &id,
            "overflow".into()
        ));
        let error = send_message_to_local_agent_task(&reg, id.as_str(), "overflow".into())
            .expect_err("bounded queue should return a typed error");
        assert_eq!(error.code, "agent_queue_full");
        assert!(!error.display_message.contains("64"));

        let mut transcript = Vec::new();
        for index in 0..(MAX_AGENT_TRANSCRIPT_ENTRIES + 100) {
            push_bounded_agent_transcript(
                &mut transcript,
                LocalAgentTranscriptEntry::Assistant {
                    text: format!("{index}-{}", "x".repeat(10_000)),
                },
            );
        }
        assert!(transcript.len() <= MAX_AGENT_TRANSCRIPT_ENTRIES);
        assert!(
            transcript.iter().map(transcript_entry_size).sum::<usize>()
                <= MAX_AGENT_TRANSCRIPT_BYTES
        );
    }

    #[test]
    fn thinking_snapshots_update_in_place_until_another_entry_starts() {
        let mut transcript = vec![LocalAgentTranscriptEntry::User {
            text: "inspect".into(),
        }];

        upsert_bounded_agent_thinking(&mut transcript, "first".into());
        upsert_bounded_agent_thinking(&mut transcript, "first complete".into());
        push_bounded_agent_transcript(
            &mut transcript,
            LocalAgentTranscriptEntry::Assistant {
                text: "answer".into(),
            },
        );
        upsert_bounded_agent_thinking(&mut transcript, "next".into());

        assert!(matches!(
            transcript.as_slice(),
            [
                LocalAgentTranscriptEntry::User { text: user },
                LocalAgentTranscriptEntry::Thinking { text: first },
                LocalAgentTranscriptEntry::Assistant { text: answer },
                LocalAgentTranscriptEntry::Thinking { text: next },
            ] if user == "inspect"
                && first == "first complete"
                && answer == "answer"
                && next == "next"
        ));
    }

    #[test]
    fn oversized_tool_output_keeps_surrounding_assistant_messages() {
        let mut transcript = Vec::new();
        push_bounded_agent_transcript(
            &mut transcript,
            LocalAgentTranscriptEntry::Assistant {
                text: "before tool".into(),
            },
        );
        push_bounded_agent_transcript(
            &mut transcript,
            LocalAgentTranscriptEntry::ToolStart {
                tool_use_id: "tool-1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "python inspect.py"}),
                activity: "running python inspect.py".into(),
            },
        );
        push_bounded_agent_transcript(
            &mut transcript,
            LocalAgentTranscriptEntry::ToolFinish {
                tool_use_id: "tool-1".into(),
                name: "Bash".into(),
                ok: true,
                summary: "Bash ok".into(),
                outcome: Ok(serde_json::json!({
                    "stdout": "x".repeat(MAX_AGENT_TOOL_FIELD_CHARS * 4)
                })),
            },
        );
        push_bounded_agent_transcript(
            &mut transcript,
            LocalAgentTranscriptEntry::Assistant {
                text: "after tool".into(),
            },
        );

        assert!(matches!(
            transcript.first(),
            Some(LocalAgentTranscriptEntry::Assistant { text }) if text == "before tool"
        ));
        assert!(matches!(
            transcript.last(),
            Some(LocalAgentTranscriptEntry::Assistant { text }) if text == "after tool"
        ));
        assert_eq!(
            transcript
                .iter()
                .filter(|entry| matches!(entry, LocalAgentTranscriptEntry::ToolStart { tool_use_id, .. } if tool_use_id == "tool-1"))
                .count(),
            1
        );
        assert_eq!(
            transcript
                .iter()
                .filter(|entry| matches!(entry, LocalAgentTranscriptEntry::ToolFinish { tool_use_id, .. } if tool_use_id == "tool-1"))
                .count(),
            1
        );
        assert!(
            transcript.iter().map(transcript_entry_size).sum::<usize>()
                <= MAX_AGENT_TRANSCRIPT_BYTES
        );
    }

    #[test]
    fn transcript_eviction_never_splits_tool_groups() {
        let mut transcript = Vec::new();
        push_bounded_agent_transcript(
            &mut transcript,
            LocalAgentTranscriptEntry::Assistant {
                text: "preserved opening".into(),
            },
        );
        for index in 0..300 {
            let tool_use_id = format!("tool-{index}");
            push_bounded_agent_transcript(
                &mut transcript,
                LocalAgentTranscriptEntry::ToolStart {
                    tool_use_id: tool_use_id.clone(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": format!("command-{index}")}),
                    activity: format!("running command-{index}"),
                },
            );
            push_bounded_agent_transcript(
                &mut transcript,
                LocalAgentTranscriptEntry::ToolFinish {
                    tool_use_id,
                    name: "Bash".into(),
                    ok: true,
                    summary: "Bash ok".into(),
                    outcome: Ok(serde_json::json!({"stdout": "ok"})),
                },
            );
        }

        let starts = transcript
            .iter()
            .filter_map(|entry| match entry {
                LocalAgentTranscriptEntry::ToolStart { tool_use_id, .. } => Some(tool_use_id),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let finishes = transcript
            .iter()
            .filter_map(|entry| match entry {
                LocalAgentTranscriptEntry::ToolFinish { tool_use_id, .. } => Some(tool_use_id),
                _ => None,
            })
            .collect::<HashSet<_>>();
        assert_eq!(starts, finishes);
        assert!(transcript.iter().any(|entry| {
            matches!(entry, LocalAgentTranscriptEntry::Assistant { text } if text == "preserved opening")
        }));
        assert!(
            transcript.iter().map(transcript_entry_size).sum::<usize>()
                <= MAX_AGENT_TRANSCRIPT_BYTES
        );
    }

    #[test]
    fn single_oversized_assistant_message_is_not_dropped() {
        let text = "x".repeat(MAX_AGENT_TRANSCRIPT_BYTES + 1);
        let mut transcript = Vec::new();
        push_bounded_agent_transcript(
            &mut transcript,
            LocalAgentTranscriptEntry::Assistant { text: text.clone() },
        );

        assert!(matches!(
            transcript.as_slice(),
            [LocalAgentTranscriptEntry::Assistant { text: retained }] if retained == &text
        ));
    }

    #[test]
    fn local_agent_message_target_accepts_unique_display_name() {
        let reg = TaskRegistry::new();
        let id = TaskId::new("agent-generated-id");
        let mut snapshot = local_agent_snapshot("agent-generated-id", TaskStatus::Running, true);
        snapshot.metadata["display_name"] = Value::String("responses-memory-explorer".into());
        reg.insert(id.clone(), snapshot, PromptCancel::new());

        send_message_to_local_agent_task(
            &reg,
            "responses-memory-explorer",
            "verify old fields".into(),
        )
        .unwrap();

        let snapshot = reg.snapshot(&id).unwrap();
        let TaskData::LocalAgent(data) = snapshot.data else {
            panic!("expected local agent");
        };
        assert_eq!(data.pending_messages, vec!["verify old fields"]);
    }

    #[test]
    fn local_agent_message_errors_distinguish_missing_from_closed() {
        let reg = TaskRegistry::new();
        let missing = send_message_to_local_agent_task(&reg, "never-existed", "hello".into())
            .expect_err("missing agent should be rejected");
        assert_eq!(missing.code, "agent_not_found");

        let id = TaskId::new("expired-agent");
        reg.insert(
            id.clone(),
            local_agent_snapshot("expired-agent", TaskStatus::Running, true),
            PromptCancel::new(),
        );
        assert!(reg.remove(&id).is_some());
        let closed = send_message_to_local_agent_task(&reg, id.as_str(), "hello".into())
            .expect_err("tombstoned agent should be reported as closed");
        assert_eq!(closed.code, "agent_closed");
        assert!(closed.model_message.contains("fresh worker"));
        assert!(!closed.display_message.contains("fresh worker"));
    }

    #[test]
    fn closing_owner_session_cancels_and_removes_only_owned_agents() {
        let reg = TaskRegistry::new();
        let owned = register_in_process_teammate_task(&reg, teammate_spec("owned"));
        let mut other_spec = teammate_spec("other");
        other_spec.identity.parent_session_id = "sess-other".into();
        let other = register_in_process_teammate_task(&reg, other_spec);

        assert_eq!(reg.close_owner_session("sess-leader"), 1);
        assert!(reg.snapshot(&owned).is_none());
        assert!(reg.snapshot(&other).is_some());
    }

    /// A worker parked at a turn boundary whose result has NOT been handed to
    /// its spawner yet. Retention must leave it alone: reaping it here drops
    /// the only copy of its outcome.
    fn idle_local_agent_snapshot(id: &str, agent_type: &str) -> TaskSnapshot {
        let mut snapshot = local_agent_snapshot(id, TaskStatus::Running, true);
        snapshot.metadata[LOCAL_AGENT_IDLE_METADATA_KEY] = Value::Bool(true);
        let TaskData::LocalAgent(data) = &mut snapshot.data else {
            unreachable!("local agent helper must contain local agent data");
        };
        data.agent_type = agent_type.to_string();
        snapshot
    }

    /// The steady state a parked worker reaches within a poll or two of going
    /// idle: its notification has been delivered, so it is now reclaimable.
    fn reported_idle_local_agent_snapshot(id: &str, agent_type: &str) -> TaskSnapshot {
        let mut snapshot = idle_local_agent_snapshot(id, agent_type);
        snapshot.notified = true;
        snapshot
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_named_teammate_is_retained_until_owner_session_closes() {
        let reg = TaskRegistry::with_retention_policy(AgentRetentionPolicy {
            internal_idle_ttl: Duration::from_secs(10),
            terminal_ttl: Duration::from_secs(10),
            max_idle_internal_agents_per_session: 16,
        });
        let id = register_in_process_teammate_task(&reg, teammate_spec("retained-teammate"));
        assert!(finish_in_process_teammate(
            &reg,
            &id,
            TaskStatus::Failed,
            Some("runtime failed".into()),
            Some("runtime failed".into()),
        ));

        tokio::time::advance(Duration::from_secs(11)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(reg.snapshot(&id).is_some());
        assert_eq!(reg.close_owner_session("sess-leader"), 1);
        assert!(reg.snapshot(&id).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn idle_explore_is_reaped_after_retention_ttl() {
        let reg = TaskRegistry::with_retention_policy(AgentRetentionPolicy {
            internal_idle_ttl: Duration::from_secs(10),
            terminal_ttl: Duration::from_secs(10),
            max_idle_internal_agents_per_session: 16,
        });
        let id = TaskId::new("ttl-explore");
        reg.insert(
            id.clone(),
            reported_idle_local_agent_snapshot(id.as_str(), " explore "),
            PromptCancel::new(),
        );
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(11)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(reg.snapshot(&id).is_none());
        let closed = send_message_to_local_agent_task(&reg, id.as_str(), "hello".into())
            .expect_err("removed Explore agent should be tombstoned");
        assert_eq!(closed.code, "agent_closed");
    }

    #[tokio::test(start_paused = true)]
    async fn excess_idle_explores_reap_the_oldest_agent() {
        let reg = TaskRegistry::with_retention_policy(AgentRetentionPolicy {
            internal_idle_ttl: Duration::from_secs(60 * 60),
            terminal_ttl: Duration::from_secs(10),
            max_idle_internal_agents_per_session: 1,
        });
        let oldest = TaskId::new("oldest-explore");
        reg.insert(
            oldest.clone(),
            reported_idle_local_agent_snapshot(oldest.as_str(), "Explore"),
            PromptCancel::new(),
        );
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        let newest = TaskId::new("newest-explore");
        reg.insert(
            newest.clone(),
            reported_idle_local_agent_snapshot(newest.as_str(), "Explore"),
            PromptCancel::new(),
        );
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(reg.snapshot(&oldest).is_none());
        assert!(reg.snapshot(&newest).is_some());
    }

    /// A coordinator's research/verification workers park exactly like an
    /// Explore agent does, and used to be the one parked shape retention had
    /// no TTL for — so they accumulated for the life of the process. A
    /// teammate is still exempt: it is owned by its session, not by a turn.
    #[tokio::test(start_paused = true)]
    async fn reported_parked_worker_is_reaped_but_teammate_is_not() {
        let reg = TaskRegistry::with_retention_policy(AgentRetentionPolicy {
            internal_idle_ttl: Duration::from_secs(10),
            terminal_ttl: Duration::from_secs(10),
            max_idle_internal_agents_per_session: 16,
        });
        let worker_id = TaskId::new("ttl-worker");
        reg.insert(
            worker_id.clone(),
            reported_idle_local_agent_snapshot(worker_id.as_str(), "verification"),
            PromptCancel::new(),
        );
        let teammate_id = register_in_process_teammate_task(&reg, teammate_spec("ttl-teammate"));
        assert!(mark_in_process_teammate_idle(&reg, &teammate_id, None));
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(11)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(reg.snapshot(&worker_id).is_none());
        assert!(reg.snapshot(&teammate_id).is_some());
    }

    /// The result of a parked worker exists in exactly one place until its
    /// notification is delivered. Retention must not win that race — neither
    /// on the TTL nor on the per-session capacity cap, which a wide fan-out
    /// trips in the same instant every worker parks.
    #[tokio::test(start_paused = true)]
    async fn parked_worker_with_undelivered_result_is_never_reaped() {
        let reg = TaskRegistry::with_retention_policy(AgentRetentionPolicy {
            internal_idle_ttl: Duration::from_secs(10),
            terminal_ttl: Duration::from_secs(10),
            max_idle_internal_agents_per_session: 1,
        });
        let stale = TaskId::new("undelivered-worker");
        reg.insert(
            stale.clone(),
            idle_local_agent_snapshot(stale.as_str(), "research"),
            PromptCancel::new(),
        );
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        let newest = TaskId::new("newest-worker");
        reg.insert(
            newest.clone(),
            reported_idle_local_agent_snapshot(newest.as_str(), "research"),
            PromptCancel::new(),
        );
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(11)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(reg.snapshot(&stale).is_some());
        assert!(
            reg.unnotified_terminal_notifications()
                .iter()
                .any(|notification| notification.task_id == stale),
            "the retained worker must still be able to report its result"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn explore_with_queued_follow_up_is_not_reaped() {
        let reg = TaskRegistry::with_retention_policy(AgentRetentionPolicy {
            internal_idle_ttl: Duration::from_secs(10),
            terminal_ttl: Duration::from_secs(10),
            max_idle_internal_agents_per_session: 16,
        });
        let id = TaskId::new("resumed-explore");
        reg.insert(
            id.clone(),
            reported_idle_local_agent_snapshot(id.as_str(), "Explore"),
            PromptCancel::new(),
        );
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;

        send_message_to_local_agent_task(&reg, id.as_str(), "continue".into())
            .expect("idle Explore should accept a follow-up");
        let snapshot = reg.snapshot(&id).expect("Explore remains registered");
        assert!(!is_agent_snapshot_idle(&snapshot));
        assert!(reg
            .inner
            .lock()
            .expect("task registry poisoned")
            .get(&id)
            .is_some_and(|state| state.runtime.idle_since.is_none()));

        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(reg.snapshot(&id).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn stopped_local_agent_is_reaped_after_terminal_ttl() {
        let reg = TaskRegistry::with_retention_policy(AgentRetentionPolicy {
            internal_idle_ttl: Duration::from_secs(10),
            terminal_ttl: Duration::from_secs(10),
            max_idle_internal_agents_per_session: 16,
        });
        let id = TaskId::new("stopped-ttl-agent");
        reg.insert(
            id.clone(),
            local_agent_snapshot(id.as_str(), TaskStatus::Running, true),
            PromptCancel::new(),
        );
        stop_task(&reg, &id).expect("stop local agent");
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(11)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(reg.snapshot(&id).is_none());
    }

    fn monitor_snapshot(id: &str, backgrounded: bool) -> TaskSnapshot {
        TaskSnapshot {
            id: TaskId::new(id),
            kind: TaskKind::Monitor,
            status: TaskStatus::Running,
            title: "watch build output".into(),
            last_progress: None,
            error: None,
            result: None,
            is_backgrounded: backgrounded,
            notified: false,
            start_time_ms: 1,
            end_time_ms: None,
            metadata: json!({ "parent_session_id": "session-monitor" }),
            data: TaskData::Monitor(MonitorData {
                description: "watch build output".into(),
                source: MonitorSourceKind::Command,
                redacted_target: "command".into(),
                event_count: 0,
                suppressed_count: 0,
                end_reason: None,
            }),
        }
    }

    #[test]
    fn notification_revision_only_advances_when_actionable_monitor_state_changes() {
        let registry = TaskRegistry::new();
        let id = TaskId::new("monitor-revision");
        let initial_revision = registry.notification_revision();
        registry.insert(
            id.clone(),
            monitor_snapshot(id.as_str(), true),
            PromptCancel::new(),
        );
        assert_eq!(registry.notification_revision(), initial_revision);

        registry.update(&id, |snapshot| {
            snapshot.last_progress = Some("still running".into());
        });
        assert_eq!(registry.notification_revision(), initial_revision);

        assert!(matches!(
            registry.enqueue_monitor_event(&id, "build complete"),
            MonitorEventEnqueueOutcome::Queued { sequence: 1 }
        ));
        let event_revision = registry.notification_revision();
        assert!(event_revision > initial_revision);
        let event = registry
            .unnotified_notifications()
            .into_iter()
            .find(|notification| notification.task_id == id)
            .expect("monitor event notification");
        assert_eq!(event.kind, TaskNotificationKind::MonitorEvent);

        registry.mark_notification_claims_delivered(&[event.claim()]);
        let ack_revision = registry.notification_revision();
        assert!(ack_revision > event_revision);
        assert!(registry.unnotified_notifications().is_empty());

        registry.update(&id, |snapshot| {
            snapshot.status = TaskStatus::Completed;
            snapshot.end_time_ms = Some(2);
            let TaskData::Monitor(data) = &mut snapshot.data else {
                unreachable!();
            };
            data.end_reason = Some(MonitorEndReason::Exited);
        });
        assert!(registry.notification_revision() > ack_revision);
        let terminal = registry.unnotified_terminal_notifications();
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0].kind, TaskNotificationKind::Terminal);
        assert!(terminal[0].message.contains("<status>exited</status>"));
    }

    #[test]
    fn terminal_monitor_notification_preserves_unacked_events() {
        let registry = TaskRegistry::new();
        let id = TaskId::new("monitor-terminal-event");
        registry.insert(
            id.clone(),
            monitor_snapshot(id.as_str(), true),
            PromptCancel::new(),
        );
        registry.enqueue_monitor_event(&id, "final <actionable> event");
        registry.update(&id, |snapshot| {
            snapshot.status = TaskStatus::Completed;
            snapshot.end_time_ms = Some(2);
            let TaskData::Monitor(data) = &mut snapshot.data else {
                unreachable!();
            };
            data.end_reason = Some(MonitorEndReason::Exited);
        });

        let notifications = registry.unnotified_notifications();
        assert_eq!(notifications.len(), 1);
        let terminal = &notifications[0];
        assert_eq!(terminal.kind, TaskNotificationKind::Terminal);
        assert!(terminal.message.contains("<status>exited</status>"));
        assert!(terminal.message.contains("final &lt;actionable&gt; event"));

        registry.mark_notification_claims_delivered(&[terminal.claim()]);
        assert!(registry.unnotified_notifications().is_empty());
        assert!(!registry
            .monitor_notifications
            .lock()
            .expect("monitor notifications poisoned")
            .tasks
            .contains_key(&id));
    }

    #[test]
    fn monitor_events_wait_until_backgrounded_and_are_xml_escaped_and_batched() {
        let registry = TaskRegistry::new();
        let id = TaskId::new("monitor-batch");
        registry.insert(
            id.clone(),
            monitor_snapshot(id.as_str(), false),
            PromptCancel::new(),
        );
        let initial_revision = registry.notification_revision();
        registry.enqueue_monitor_event(&id, "first <event>");
        registry.enqueue_monitor_event(&id, "second & event");
        assert_eq!(registry.notification_revision(), initial_revision);
        assert!(registry.unnotified_notifications().is_empty());

        assert!(registry.set_backgrounded(&id));
        assert!(registry.notification_revision() > initial_revision);
        let notifications = registry.unnotified_notifications();
        assert_eq!(notifications.len(), 1);
        let message = &notifications[0].message;
        assert!(message.contains("first &lt;event&gt;"), "{message}");
        assert!(message.contains("second &amp; event"), "{message}");
        assert_eq!(notifications[0].generation, 2);
    }

    #[tokio::test]
    async fn dropping_last_task_registry_cancels_pending_escalations() {
        let reg = TaskRegistry::new();
        let client = reg
            .escalation_registry()
            .worker_client("agent-drop", Some("drop test".into()))
            .with_answer_timeout(std::time::Duration::from_secs(60));
        let handle =
            tokio::spawn(async move { client.escalate("still there?".into(), None, None).await });
        tokio::task::yield_now().await;
        assert_eq!(reg.unnotified_question_escalation_notifications().len(), 1);

        drop(reg);

        let err = handle.await.unwrap().unwrap_err();
        assert!(err.contains("cancelled before it was answered"));
        assert!(err.contains("esc-agent-drop-1"));
    }
}
