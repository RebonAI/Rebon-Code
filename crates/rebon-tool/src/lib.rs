//! Tool trait + per-tool implementations.
//!
//! This crate defines the async `Tool` trait that individual tool
//! implementations plug into, plus the concrete tools themselves. The
//! contract carries the runtime needs of a tool: input schemas, validation,
//! permission gating, and streaming progress.

use async_trait::async_trait;
use rebon_shell_policy::{
    is_auto_mode_sensitive_tool_input_with_exempt_deletion_root, is_sensitive_beyond_deletions,
    is_sensitive_content_deletion_input, is_workflow_sensitive_git_command,
};
use rebon_tools_core::{
    FileStateCache, PermissionBehavior, PermissionDecision, PermissionRequest, ToolError,
    ToolErrorPresentation, ToolId, ToolInputSchema, ToolProgressUpdate, ToolResult,
    ValidationOutcome,
};
use rebon_types::UltraplanRunState;
use serde_json::Value;
use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::{mpsc, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

/// The process-wide lock every test that swaps an environment variable
/// takes. Public under `test-support` so a feature plugin's tests can join
/// the same discipline — the `Agent` plugin holds it while it reads the
/// worker report directory, which follows the config home.
#[cfg(any(test, feature = "test-support"))]
pub fn env_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

pub(crate) async fn lock_file_for_read(path: &Path) -> OwnedRwLockReadGuard<()> {
    file_access_lock(path).read_owned().await
}

pub async fn lock_file_for_write(path: &Path) -> OwnedRwLockWriteGuard<()> {
    file_access_lock(path).write_owned().await
}

fn file_access_lock(path: &Path) -> Arc<RwLock<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<RwLock<()>>>>> = OnceLock::new();

    let keys = file_access_keys(path);
    let registry = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = keys
        .iter()
        .find_map(|key| locks.get(key).and_then(Weak::upgrade))
    {
        for key in keys {
            locks.insert(key, Arc::downgrade(&lock));
        }
        return lock;
    }

    let lock = Arc::new(RwLock::new(()));
    for key in keys {
        locks.insert(key, Arc::downgrade(&lock));
    }
    lock
}

fn file_access_keys(path: &Path) -> Vec<String> {
    let mut keys = vec![format!("path:{}", normalized_file_access_key(path))];
    if let Some(identity) = existing_file_identity(path) {
        keys.push(identity);
    }
    keys
}

/// Canonical where it exists, else the canonical deepest existing ancestor with
/// the missing tail appended. Unlike `rebon_tools_core::canonicalize_scope_path`
/// this normalizes `..` away *before* walking (a lock key for `a/b/../c` must
/// match the one for `a/c`), and keeps the `\\?\` prefix `canonicalize` yields
/// on Windows — `normalize_path_key` folds the key afterwards.
fn canonicalized_file_access_path(path: &Path) -> PathBuf {
    let normalized = rebon_tools_core::lexically_normalize_path(path);
    let mut cursor = normalized.as_path();
    let mut suffix = Vec::new();

    loop {
        if let Ok(mut canonical) = std::fs::canonicalize(cursor) {
            for component in suffix.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
        let Some(component) = cursor.file_name() else {
            break;
        };
        suffix.push(component.to_os_string());
        let Some(parent) = cursor.parent() else {
            break;
        };
        cursor = parent;
    }

    normalized
}

fn normalized_file_access_key(path: &Path) -> String {
    rebon_tools_core::file_state::normalize_path_key(&canonicalized_file_access_path(path))
}

#[cfg(unix)]
fn existing_file_identity(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path).ok()?;
    Some(format!("file:unix:{}:{}", metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn existing_file_identity(path: &Path) -> Option<String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let file = std::fs::File::open(path).ok()?;
    let mut info = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    let succeeded = unsafe { GetFileInformationByHandle(file.as_raw_handle() as isize, &mut info) };
    if succeeded == 0 {
        return None;
    }
    let file_index = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
    Some(format!(
        "file:windows:{}:{}",
        info.dwVolumeSerialNumber, file_index
    ))
}

#[cfg(not(any(unix, windows)))]
fn existing_file_identity(_path: &Path) -> Option<String> {
    None
}

#[derive(Debug, Default)]
struct FileMutationBatch;

fn file_mutation_registry() -> &'static Mutex<HashMap<String, Weak<FileMutationBatch>>> {
    static MUTATIONS: OnceLock<Mutex<HashMap<String, Weak<FileMutationBatch>>>> = OnceLock::new();
    MUTATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn ensure_file_not_mutated_in_current_batch(
    path: &Path,
    context: &ToolContext,
    tool: ToolId,
    error_code: i64,
) -> ToolResult<()> {
    let Some(batch) = context.file_mutation_batch.as_ref() else {
        return Ok(());
    };
    let registry = file_mutation_registry();
    let mut mutations = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    mutations.retain(|_, owner| owner.strong_count() > 0);
    let already_mutated = file_access_keys(path).iter().any(|key| {
        mutations
            .get(key)
            .and_then(Weak::upgrade)
            .is_some_and(|owner| Arc::ptr_eq(&owner, batch))
    });
    if already_mutated {
        return Err(ToolError::InvalidInput {
            tool,
            reason: format!(
                "File {} was already modified by another file tool in this parallel tool batch. Read its latest contents in a later tool batch before retrying so the earlier change is preserved.",
                path.display()
            ),
            error_code: Some(error_code),
        });
    }
    Ok(())
}

pub fn record_file_mutation_in_current_batch(path: &Path, context: &ToolContext) {
    let Some(batch) = context.file_mutation_batch.as_ref() else {
        return;
    };
    let registry = file_mutation_registry();
    let mut mutations = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    mutations.retain(|_, owner| owner.strong_count() > 0);
    for key in file_access_keys(path) {
        mutations.insert(key, Arc::downgrade(batch));
    }
}

pub mod agent;
pub mod agent_registry;
pub mod bash;
pub mod builtin_agents;
pub mod command_sandbox;
pub mod core_tools;
pub mod cron;
pub mod edit;
pub mod escalation;
pub mod execution_surface;
pub mod external_agent;
pub mod filter;
pub mod glob;
pub mod grep;
pub mod invoke_deferred_tool;
pub mod mcp;
pub mod monitor;
pub mod multi_edit;
pub mod output_truncation;
#[cfg(windows)]
pub(crate) mod path_lookup;
pub mod path_scope;
pub mod plan_mode;
pub mod powershell;
pub mod queue;
pub mod read;
pub mod shell_management;
pub mod shell_preference;
pub mod shell_process;
pub mod sleep;
pub mod str_replace_editor;
pub mod structured_output;
pub mod tasks;
pub mod team_files;
pub mod team_mailbox;
pub mod team_manager;
#[cfg(test)]
mod test_env;
pub mod todo_write;
pub mod tool_search;
pub mod ultraplan;
pub mod validation;
pub mod web;
pub mod workflow;
pub mod worktree;
pub mod write;

// Per-feature `ToolContext` state. Each struct lives with the feature that
// owns it; `ToolContext` keeps them in its `Extensions` bag instead of one
// direct field per feature.
pub use agent::SubAgentContext;
pub use command_sandbox::{
    BinShell, CommandSandbox, DoctorLevel, DoctorLine, PreparedCommand, RefusingSandbox,
    SandboxDoctor, SessionSandboxService, SessionSandboxSource, SESSION_SANDBOX_SERVICE,
};
pub use cron::CronContext;
pub use escalation::EscalationContext;
pub use mcp::McpContext;
pub use monitor::MonitorContext;
pub use plan_mode::PlanModeContext;
pub use queue::QueueContext;
pub use structured_output::StructuredOutputContext;
pub use tasks::TaskContext;
pub use team_manager::TeamContext;
pub use tool_search::ToolSearchContext;
pub use ultraplan::UltraplanRunContext;
pub use web::WebContext;
pub use workflow::WorkflowContext;

pub use agent::{
    agent_input_may_write, agent_registry_selection, append_workflow_agent_shared_worktree_prompt,
    ensure_agent_id, set_agent_registry_selection, set_sub_agents_enabled, sub_agents_enabled,
    AgentRegistrySelection, CacheStrategy, ContextRequest, ContextShareMode, FileContextMode,
    FrozenParentContextCapsule, SubAgentGitMetadata, SubAgentProgressSender, SubAgentResult,
    SubAgentRuntimeHandle, SubAgentSpawner, SubAgentSpawnerRequest, SubAgentSpawnerService,
    SubAgentSpawnerSource, SubAgentSpec, SubAgentTaskKind, ToolResultMode,
    UnavailableSubAgentSpawner, AGENT_EXTERNAL_PATH_AUTHORIZATION_KIND, AGENT_TOOL_NAME,
    DEFAULT_SUB_AGENT_MAX_ITERATIONS, DEFAULT_SUB_AGENT_TYPE, SUB_AGENTS_UNAVAILABLE,
    SUB_AGENT_SPAWNER_SERVICE, WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT,
};
pub use external_agent::{
    compose_external_first_prompt, external_route_for_spec, parse_external_model_spec,
    ExternalRoute, ExternalSubAgentRunner, ExternalTaskEvent, ExternalTaskOutcome,
    ExternalTaskRequest, ExternalTaskStatus,
};

pub use agent_registry::{
    AgentGroups, AgentRegistry, AgentRuntime, AgentSource, AgentSource as AgentRegistrySource,
    ResolvedAgentDef, SettingSource, SettingSource as AgentRegistrySettingSource,
};
pub use bash::BashTool;
pub use builtin_agents::{
    all_builtin_agents, format_agent_line, format_all_agent_lines, resolve_builtin_agent,
    BuiltInAgentDef, EXPLORE_AGENT_MIN_QUERIES,
};
// PermissionBroker + DenyAskPermissionBroker are defined later in this file.
pub use core_tools::{
    builtin_tool_facts, core_tool_set, facts_for_name, file_target_field_for_name,
    render_tool_kind, render_tool_kind_for_name, tool_kind_for_name, tool_names_of_kind, ToolFacts,
    CORE_TOOL_COUNT,
};
pub use cron::tasks::SessionCronStore;
pub use edit::{EditTool, FILE_EDIT_TOOL_NAME};
pub use escalation::{
    format_question_escalation_xml, EscalationAnswer, EscalationId, EscalationRegistry,
    EscalationResolver, EscalationSource, QuestionEscalation, QuestionEscalationNotification,
    WorkerEscalationClient,
};
pub use execution_surface::{
    escalation_preauthorized, execution_surface, is_unattended, set_escalation_preauthorized,
    set_execution_surface, ExecutionSurface, EXECUTION_SURFACE_ENV_VAR,
};
pub use filter::{SharedToolFilter, ToolFilter};
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use invoke_deferred_tool::{
    InvokeDeferredTool, InvokeDeferredToolInput, INVOKE_DEFERRED_TOOL_NAME,
};
pub use mcp::{
    build_mcp_tool_name, default_mcp_input_schema, join_closed_servers, normalize_name_for_mcp,
    McpClient, McpClientError, McpShutdownReport, McpToolCall, McpToolDefinition, McpToolResult,
};
pub use monitor::{MonitorRegistry, MONITOR_TOOL_NAME};
pub use multi_edit::{MultiEditTool, FILE_MULTI_EDIT_TOOL_NAME};
/// Notebook tool identity used by lower-layer exposure and permission contracts.
pub const NOTEBOOK_EDIT_TOOL_NAME: &str = "NotebookEdit";
pub use plan_mode::{ENTER_PLAN_MODE_TOOL_NAME, EXIT_PLAN_MODE_TOOL_NAME, PLAN_LEDGER_TOOL_NAME};
pub use powershell::{PowerShellEdition, PowerShellTool, POWERSHELL_TOOL_NAME};
pub use read::ReadTool;
pub use rebon_types::{
    CapabilityContext, ExecutionPolicy, PolicyMode, ShellPolicy, UltraplanContext,
};
pub use shell_management::{
    ShellOutputTool, ShellStopTool, SHELL_OUTPUT_TOOL_NAME, SHELL_STOP_TOOL_NAME,
};
pub use shell_preference::{
    bash_tool_enabled, powershell_tool_enabled, set_shell_tool_preference, shell_tool_preference,
    ShellToolPreference, SHELL_TOOL_ENV_VAR,
};
pub use shell_process::ShellProcessRegistry;
pub use sleep::SleepTool;
pub use str_replace_editor::{
    str_replace_editor_input_schema, StrReplaceEditorTool, STR_REPLACE_EDITOR_DESCRIPTION,
    STR_REPLACE_EDITOR_TOOL_NAME,
};
pub use structured_output::{
    validate_structured_output, StructuredOutputChannel, STRUCTURED_OUTPUT_TOOL_NAME,
};
pub use team_files::{
    add_hidden_pane_id, append_team_member, bind_session_team, clear_current_team_name,
    clear_session_team_bindings, current_team_name, default_team_name, ensure_session_default_team,
    format_agent_id, is_session_default_team_name, list_team_files, read_team_file,
    remove_hidden_pane_id, remove_member_by_agent_id, remove_member_by_pane_id,
    remove_session_default_team, sanitize_agent_name, sanitize_team_name, set_current_team_name,
    set_team_member_active, set_team_member_mode, team_file_path, team_name_for_session,
    write_team_file, TeamFile, TeamMember,
};
pub use team_mailbox::{
    drain_unread_mailbox, drain_unread_mailbox_matching, mailbox_notification, read_mailbox,
    write_mailbox_message, TeamMailboxMessage,
};
pub use team_manager::{
    TeamManager, TeamManagerRequest, TeamManagerService, TeamManagerSource, TeammateHandoff,
    TeammateRosterEntry, TeammateSpawnResult, TeammateSpawnSpec, TEAM_MANAGER_SERVICE,
};
pub use tool_search::{
    is_tool_search_enabled, ToolSearchIndex, ToolSearchTool, TOOL_SEARCH_TOOL_NAME,
};
pub use web::{WebSearchDelegate, WEB_FETCH_TOOL_NAME, WEB_SEARCH_TOOL_NAME};
pub use workflow::{
    WorkflowLaunchSpec, WorkflowLaunchStatus, WorkflowLauncher, WorkflowLauncherRequest,
    WorkflowLauncherService, WorkflowLauncherSource, WorkflowNesting, WorkflowPermissionPreview,
    WorkflowPermissionReviewCall, WorkflowPermissionReviewPhase, WorkflowTaskRuntimeHandle,
    RUN_WORKFLOW_ALIAS, WORKFLOW_LAUNCHER_SERVICE, WORKFLOW_STRUCTURED_OUTPUT_SCHEMA_KEY,
    WORKFLOW_TOOL_NAME,
};
pub use write::{WriteTool, FILE_WRITE_TOOL_NAME};

/// Identity of the teammate that issued a tool call.
///
/// Read back through [`ToolContext::team_identity`], and through the
/// accessors that fall back to it: `agent_id`, `current_team_name`, and
/// `permission_mode`, which outranks the context's permission-mode provider.
/// A tool that cannot run from inside a team tells a teammate's call from a
/// top-level session's by its presence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamIdentityContext {
    pub agent_id: String,
    pub agent_name: String,
    pub team_name: String,
    pub permission_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopTaskOutcome {
    Stopped {
        task_id: String,
        task_type: String,
        command: String,
    },
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundShellTaskSpec {
    pub shell_id: String,
    pub tool_name: String,
    pub command: String,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub started_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundShellCompletionStatus {
    Exited,
    TimedOut,
    Stopped,
    Failed,
}

impl BackgroundShellCompletionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::TimedOut => "timed_out",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundShellTaskCompletion {
    pub shell_id: String,
    pub status: BackgroundShellCompletionStatus,
    pub completed_at_ms: u64,
    pub exit_code: Option<i32>,
    pub output: String,
    pub stderr: String,
    /// `shell_stream_order` sketch describing how `output` and `stderr`
    /// interleaved, so the completed card can be rendered in arrival
    /// order instead of stdout-then-stderr. `None` when the two did not
    /// genuinely interleave (the sketch would say nothing new).
    pub stream_order: Option<String>,
    pub error: Option<String>,
    pub next_cursor: u64,
    pub has_more: bool,
    pub cursor_truncated: bool,
    pub oldest_cursor: u64,
    pub observed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorTaskSource {
    Command,
    WebSocket,
}

impl MonitorTaskSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::WebSocket => "websocket",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorTaskSpec {
    pub task_id: String,
    pub description: String,
    pub source: MonitorTaskSource,
    pub redacted_target: String,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub started_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorTaskCompletionStatus {
    Exited,
    Closed,
    Failed,
    Stopped,
    TimedOut,
    AutoStopped,
}

impl MonitorTaskCompletionStatus {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorTaskCompletion {
    pub task_id: String,
    pub status: MonitorTaskCompletionStatus,
    pub completed_at_ms: u64,
    pub exit_code: Option<i32>,
    pub stderr: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorEventDisposition {
    Ignored,
    Queued,
    Suppressed,
    AutoStop,
}

/// Read access to the Agent Queue that owns the calling session.
///
/// The queue store has exactly one writer — the app that hosts the queue UI.
/// It keeps the whole store in memory and persists by overwriting the file
/// wholesale, so a second process writing `queues.json` would be invisible to
/// the app and erased by its next save (which runs about once a second while
/// any row is active). Every mutation therefore has to travel to that single
/// writer as a request; this trait deliberately exposes reads only, and the
/// write side will arrive as submitted intents rather than direct stores.
#[async_trait]
pub trait QueueController: Send + Sync {
    /// The queue owned by `session_id`, as a JSON document: rows with their
    /// status, dependencies, worktree, review boundary and gates.
    ///
    /// Takes the session explicitly because a session id is minted after the
    /// runtime that would hold this controller is built.
    ///
    /// `Ok(None)` means that session is not a queue coordinator.
    async fn queue_plan(&self, session_id: &str) -> Result<Option<serde_json::Value>, String>;

    /// Record a review outcome for one row.
    ///
    /// A request, not a store: the single writer applies it. `generation` is
    /// the row's execution generation as read from the plan — a verdict for a
    /// round that has already been superseded is rejected rather than applied
    /// to whatever is running now.
    async fn submit_verdict(
        &self,
        verdict: QueueVerdict<'_>,
    ) -> Result<QueueVerdictOutcome, String>;

    /// Ask the queue to start one row.
    ///
    /// A request, and one the writer is free to refuse: dependencies, the
    /// concurrency limit and worktree conflicts are the queue's to enforce, not
    /// the coordinator's to assert. A refusal names the reason.
    ///
    /// `worktree` is the coordinator's proposal for where the row should run —
    /// `auto`, `inherit` or `isolate` — and the writer refuses one it cannot
    /// honour rather than quietly substituting another.
    async fn dispatch_row(
        &self,
        session_id: &str,
        row_id: &str,
        worktree: &str,
    ) -> Result<QueueVerdictOutcome, String>;

    /// Suspend a row nobody can currently judge.
    ///
    /// Frees its concurrency slot without ever satisfying a dependency: rows
    /// downstream keep waiting, unrelated rows carry on.
    async fn block_row(
        &self,
        session_id: &str,
        row_id: &str,
        reason: &str,
    ) -> Result<QueueVerdictOutcome, String>;
}

/// A review outcome addressed to one execution of one row.
#[derive(Debug, Clone, Copy)]
pub struct QueueVerdict<'a> {
    pub session_id: &'a str,
    pub row_id: &'a str,
    pub generation: u64,
    pub pass: bool,
    pub reason: &'a str,
}

/// What the queue's writer did with a submitted verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueVerdictOutcome {
    /// Applied to the row.
    Applied { status: String },
    /// Refused, with the reason the coordinator needs in order to react —
    /// a stale generation, an unknown row, a row not awaiting review.
    Rejected { reason: String },
    /// Submitted, but the writer did not answer in time. The verdict may still
    /// land; it is never silently dropped, so this is reported rather than
    /// presented as success.
    Unconfirmed,
}

#[async_trait]
pub trait TaskRuntimeController: Send + Sync {
    /// Stop a task in the exact owning session. Implementations must not
    /// substitute another session or create a fallback registry.
    async fn stop_task(&self, session_id: &str, task_id: &str) -> Result<StopTaskOutcome, String>;

    async fn send_message_to_task(
        &self,
        session_id: &str,
        task_id: &str,
        message: String,
    ) -> Result<(), ToolErrorPresentation>;

    fn background_shell_started(
        &self,
        _session_id: &str,
        _spec: BackgroundShellTaskSpec,
        _cancel: rebon_types::PromptCancel,
    ) {
    }

    fn background_shell_finished(
        &self,
        _session_id: &str,
        _completion: BackgroundShellTaskCompletion,
    ) {
    }

    fn background_shell_observed(&self, _session_id: &str, _shell_id: &str) {}

    fn monitor_started(
        &self,
        _session_id: &str,
        _spec: MonitorTaskSpec,
        _cancel: rebon_types::PromptCancel,
    ) {
    }

    fn monitor_event(
        &self,
        _session_id: &str,
        _task_id: &str,
        _event: String,
    ) -> MonitorEventDisposition {
        MonitorEventDisposition::Ignored
    }

    fn monitor_finished(&self, _session_id: &str, _completion: MonitorTaskCompletion) {}
}

#[derive(Clone, Default)]
pub struct SharedCoordinatorMode {
    inner: Arc<AtomicBool>,
}

impl SharedCoordinatorMode {
    pub fn new(enabled: bool) -> Self {
        Self {
            inner: Arc::new(AtomicBool::new(enabled)),
        }
    }

    pub fn get(&self) -> bool {
        self.inner.load(Ordering::Relaxed)
    }

    pub fn set(&self, enabled: bool) {
        self.inner.store(enabled, Ordering::Relaxed);
    }
}

use rebon_agent_core::file_history::FileHistoryTracker;

pub type UltraplanRunSyncer = Arc<dyn Fn(&Arc<Mutex<UltraplanRunState>>) -> bool + Send + Sync>;
pub type UltraplanRunPersister = Arc<dyn Fn(&Arc<Mutex<UltraplanRunState>>) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UltraplanRepositoryError {
    Missing,
    StaleRevision { expected: u64, actual: u64 },
    RunIdMismatch { expected: String, actual: String },
    Storage(String),
}

impl std::fmt::Display for UltraplanRepositoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "active ultraplan run state is missing"),
            Self::StaleRevision { expected, actual } => write!(
                f,
                "stale ultraplan revision: expected {expected}, current revision is {actual}"
            ),
            Self::RunIdMismatch { expected, actual } => write!(
                f,
                "ultraplan run id mismatch: expected `{expected}`, got `{actual}`"
            ),
            Self::Storage(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for UltraplanRepositoryError {}

pub trait UltraplanRunRepository: Send + Sync {
    fn load_current(&self) -> Result<UltraplanRunState, UltraplanRepositoryError>;

    fn load_run(&self, run_id: &str)
        -> Result<Option<UltraplanRunState>, UltraplanRepositoryError>;

    fn compare_and_swap(
        &self,
        expected_revision: u64,
        state: &UltraplanRunState,
    ) -> Result<(), UltraplanRepositoryError>;

    fn create_run(&self, state: &UltraplanRunState) -> Result<(), UltraplanRepositoryError>;

    fn switch_current(&self, run_id: &str) -> Result<(), UltraplanRepositoryError> {
        Err(UltraplanRepositoryError::Storage(format!(
            "ultraplan repository cannot switch to run `{run_id}`"
        )))
    }
}

/// Compatibility adapter for plugin-contributed tools. New Engine consumption
/// resolves this provider alongside local and MCP providers through the kernel
/// `tool-registry` seat; this trait remains the plugin registration surface.
pub trait PluginToolProvider: Send + Sync {
    /// Build the proxy tool for `name`, if a plugin registered it.
    fn tool(&self, name: &str) -> Option<Arc<dyn Tool>>;

    /// Registered plugin tool names (diagnostics / future exposure).
    fn tool_names(&self) -> Vec<String>;
}

/// Consumer face of the kernel `tool-registry` seat.
///
/// Engine code performs exactly one resolution through this interface. Local,
/// MCP, and plugin tools are providers behind the seat, so adding another tool
/// transport does not add another Engine fallback branch.
pub trait ToolResolver: Send + Sync {
    /// Resolve the highest-precedence provider for `name`.
    fn resolve(&self, name: &str, filter: Option<&ToolFilter>)
        -> ToolResult<Option<Arc<dyn Tool>>>;

    /// Enumerate the effective, precedence-deduplicated catalog.
    fn tools(&self, filter: Option<&ToolFilter>) -> ToolResult<Vec<Arc<dyn Tool>>>;
}

/// Arbitrated web-provider routing for the WebSearch/WebFetch tools (the
/// kernel's `ctx.web` seat).
///
/// Deliberately NOT [`WebSearchDelegate`]: the delegate is a best-effort
/// preempt whose failures fall back to the builtin path, while a seat route
/// is an arbitration outcome — a configured provider that is missing or
/// broken must fail LOUDLY, never silently degrade to another engine.
#[async_trait]
pub trait WebProviderRouter: Send + Sync {
    /// Route one search. `Ok(None)` = no plugin route (run the builtin
    /// path); `Ok(Some(v))` = a plugin provider handled it and `v` is the
    /// finished tool output; `Err` = loud arbitration or provider failure.
    async fn search(&self, input: &Value) -> ToolResult<Option<Value>>;

    /// Route one fetch; same contract as [`Self::search`].
    async fn fetch(&self, input: &Value) -> ToolResult<Option<Value>>;
}

/// Type-keyed bag of per-feature [`ToolContext`] state.
///
/// One entry per feature context struct ([`crate::team_manager::TeamContext`],
/// [`crate::mcp::McpContext`], …). It keeps `ToolContext`'s own field list to
/// core runtime concerns — the path/permission/file state every primitive
/// tool needs — so a session-scoped feature can move behind a plugin
/// boundary without adding another field to the shared struct.
#[derive(Clone, Default)]
pub struct Extensions(HashMap<TypeId, Arc<dyn Any + Send + Sync>>);

impl Extensions {
    /// Store `value`, replacing any previous value of the same type.
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) {
        self.0.insert(TypeId::of::<T>(), Arc::new(value));
    }

    /// Borrow the stored value of type `T`, if one was inserted.
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.0
            .get(&TypeId::of::<T>())
            .and_then(|value| value.downcast_ref::<T>())
    }

    /// Absorb another bag, replacing same-typed values with `other`'s.
    ///
    /// The whole point of a bag rather than a field is that its owner never
    /// learns the types in it, so a host builds one and hands it over intact
    /// rather than the runtime offering a setter per feature.
    pub fn extend(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    /// How many feature contexts are attached.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Extensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extensions")
            .field("len", &self.len())
            .finish()
    }
}

/// Coordinator-session state carried in the [`Extensions`] bag.
#[derive(Clone, Default)]
pub struct CoordinatorContext {
    /// Explicit session-scoped coordinator mode. Runtime policy must not read
    /// REBON_COORDINATOR_MODE after startup; this flag is propagated through
    /// executor/tool contexts instead.
    pub mode: bool,
    /// Absolute worker report paths this coordinator turn may read. Empty means
    /// coordinator Read calls are denied rather than falling back to broad FS access.
    pub report_paths: Vec<PathBuf>,
}

/// Auto-mode classifier state carried in the [`Extensions`] bag.
#[derive(Clone, Default)]
pub struct AutoModeContext {
    pub classifier_transcript: Option<Arc<str>>,
}

#[derive(Clone, Default)]
pub struct ToolContext {
    tool_use_id: Option<String>,
    session_id: Option<String>,
    progress: Option<ToolProgressSender>,
    permission_broker: Option<Arc<dyn PermissionBroker>>,
    permission_mode_provider: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    agent_id: Option<String>,
    cwd: Option<String>,
    /// True only when the runtime itself created a dedicated Git worktree
    /// for this agent (or the agent inherited one unchanged from its
    /// parent). Never derived from `cwd`: the path is caller-controlled
    /// input, so shared-worktree Git protections keyed off a path pattern
    /// could be lifted by pointing a sub-agent at a lookalike directory.
    is_isolated_worktree: bool,
    additional_working_directories: Vec<String>,
    path_scope_roots: Vec<PathBuf>,
    write_scope_roots: Option<Vec<PathBuf>>,
    auto_approved_write_roots: Vec<PathBuf>,
    command_sandbox: Option<Arc<dyn CommandSandbox>>,
    plugin_tools: Option<Arc<dyn PluginToolProvider>>,
    tool_resolver: Option<Arc<dyn ToolResolver>>,
    tool_filter: Option<ToolFilter>,
    /// Per-session cache of files the model has observed via Read,
    /// used by Edit / Write to enforce "must read before edit" and to
    /// detect external modification. Handle is cheap to clone (Arc
    /// internally); all derived contexts in a query share the same
    /// underlying map.
    file_state_cache: Option<FileStateCache>,
    /// Shared by every tool call emitted in one model response. A successful
    /// mutation records its path against this token so a later-scheduled
    /// same-path mutation from the same parallel batch cannot mistake the
    /// refreshed shared cache for a sequential user-approved edit.
    file_mutation_batch: Option<Arc<FileMutationBatch>>,
    file_history_tracker: Option<Arc<dyn FileHistoryTracker>>,
    shell_process_registry: Option<Arc<ShellProcessRegistry>>,
    /// Optional request-scoped execution policy for the current prompt turn.
    execution_policy: Option<ExecutionPolicy>,
    /// Request-scoped permission bypass for `/permissions retry` replays.
    /// This is not persisted policy state: it lives only on the single
    /// `ToolContext` used to re-dispatch a previously denied invocation after
    /// the user explicitly requested retry.
    denial_replay_reason: Option<String>,
    permission_prompts_unavailable: bool,
    /// Per-feature state that used to sit here as its own field. Read and
    /// written only through the accessors below, which kept their signatures
    /// when their storage moved into the `Extensions` bag.
    extensions: Extensions,
}

impl ToolContext {
    /// Number of direct fields on [`ToolContext`], including the
    /// [`Extensions`] bag itself.
    ///
    /// The budget is 25. Adding a direct field must bump this constant and
    /// the destructuring in `tool_context_direct_field_budget`; the point of
    /// both is that the next field is a deliberate choice, not a reflex.
    pub const DIRECT_FIELDS: usize = 24;
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("tool_use_id", &self.tool_use_id)
            .field("session_id", &self.session_id)
            .field(
                "has_auto_mode_classifier_transcript",
                &self.auto_mode_classifier_transcript().is_some(),
            )
            .field("has_progress", &self.progress.is_some())
            .field("has_sub_agent_spawner", &self.sub_agent_spawner().is_some())
            .field("has_mcp_client", &self.mcp_client().is_some())
            .field(
                "has_web_search_delegate",
                &self.web_search_delegate().is_some(),
            )
            .field("has_workflow_launcher", &self.workflow_launcher().is_some())
            .field("has_permission_broker", &self.permission_broker.is_some())
            .field("has_team_manager", &self.team_manager().is_some())
            .field(
                "has_task_runtime_controller",
                &self.task_runtime_controller().is_some(),
            )
            .field("has_queue_controller", &self.queue_controller().is_some())
            .field("team_identity", &self.team_identity())
            .field(
                "has_permission_mode_provider",
                &self.permission_mode_provider.is_some(),
            )
            .field("agent_id", &self.agent_id)
            .field("cwd", &self.cwd)
            .field("is_isolated_worktree", &self.is_isolated_worktree)
            .field(
                "additional_working_directories_len",
                &self.additional_working_directories.len(),
            )
            .field("path_scope_roots_len", &self.path_scope_roots.len())
            .field(
                "write_scope_roots_len",
                &self.write_scope_roots.as_ref().map(Vec::len),
            )
            .field(
                "auto_approved_write_roots_len",
                &self.auto_approved_write_roots.len(),
            )
            .field("has_command_sandbox", &self.command_sandbox.is_some())
            .field(
                "has_mcp_tool_definitions",
                &self.mcp_tool_definitions().is_some(),
            )
            .field("has_tool_resolver", &self.tool_resolver.is_some())
            .field("has_tool_filter", &self.tool_filter.is_some())
            .field("has_tool_search_index", &self.tool_search_index().is_some())
            .field(
                "has_discovered_deferred_tools",
                &self.discovered_deferred_tools().is_some(),
            )
            .field("has_file_state_cache", &self.file_state_cache.is_some())
            .field(
                "has_file_mutation_batch",
                &self.file_mutation_batch.is_some(),
            )
            .field(
                "parallel_agent_write_batch",
                &self.parallel_agent_write_batch(),
            )
            .field(
                "has_file_history_tracker",
                &self.file_history_tracker.is_some(),
            )
            .field(
                "has_worker_escalation_client",
                &self.worker_escalation_client().is_some(),
            )
            .field(
                "has_escalation_resolver",
                &self.escalation_resolver().is_some(),
            )
            .field(
                "has_session_cron_store",
                &self.session_cron_store().is_some(),
            )
            .field(
                "has_shell_process_registry",
                &self.shell_process_registry.is_some(),
            )
            .field("has_monitor_registry", &self.monitor_registry().is_some())
            .field(
                "has_ultraplan_run_repository",
                &self.ultraplan_run_repository().is_some(),
            )
            .field(
                "has_ultraplan_run_handle",
                &self.ultraplan_run_handle().is_some(),
            )
            .field(
                "has_ultraplan_run_syncer",
                &self.ultraplan_run().is_some_and(|run| run.syncer.is_some()),
            )
            .field(
                "has_ultraplan_run_persister",
                &self
                    .ultraplan_run()
                    .is_some_and(|run| run.persister.is_some()),
            )
            .field("has_execution_policy", &self.execution_policy.is_some())
            .field("exit_plan_mode_approved", &self.exit_plan_mode_approved())
            .field(
                "has_denial_replay_reason",
                &self.denial_replay_reason.is_some(),
            )
            .field("coordinator_mode", &self.coordinator_mode())
            .field("workflow_nesting_depth", &self.workflow_nesting_depth())
            .field(
                "permission_prompts_unavailable",
                &self.permission_prompts_unavailable,
            )
            .field(
                "has_structured_output_channel",
                &self.structured_output_channel().is_some(),
            )
            .field(
                "coordinator_report_paths_len",
                &self.coordinator_report_paths().len(),
            )
            .field(
                "has_frozen_parent_context",
                &self.frozen_parent_context().is_some(),
            )
            .field(
                "capability_hash",
                &self
                    .capability_context()
                    .map(|context| context.capability_hash.as_str()),
            )
            .field("extensions", &self.extensions)
            .finish()
    }
}

impl ToolContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn for_tool_use(tool_use_id: impl Into<String>) -> Self {
        Self {
            tool_use_id: Some(tool_use_id.into()),
            ..Self::default()
        }
    }

    /// Clone the context and overwrite the `tool_use_id`. Used by
    /// the engine's dispatch loop to mint a per-call context that
    /// still carries the parent's injected extensions (permission
    /// broker, sub-agent spawner, MCP client, plugin feature state).
    pub fn with_tool_use_id(&self, tool_use_id: impl Into<String>) -> Self {
        Self {
            tool_use_id: Some(tool_use_id.into()),
            ..self.clone()
        }
    }

    pub fn with_progress(
        &self,
        tool_use_id: impl Into<String>,
    ) -> (Self, mpsc::UnboundedReceiver<ToolProgressUpdate>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tool_use_id: Some(tool_use_id.into()),
                progress: Some(ToolProgressSender::new(tx)),
                ..self.clone()
            },
            rx,
        )
    }

    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Mark this context as running inside a dedicated agent worktree.
    ///
    /// Only the code that actually creates the worktree — or propagates
    /// an inherited one whose `cwd` was not rewritten — may pass `true`.
    /// A `cwd` supplied by a model or a caller must never set this: the
    /// flag lifts the shared-worktree Git approval gate.
    pub fn with_isolated_worktree(mut self, isolated: bool) -> Self {
        self.is_isolated_worktree = isolated;
        self
    }

    /// Whether the runtime placed this agent in its own Git worktree.
    pub fn is_isolated_worktree(&self) -> bool {
        self.is_isolated_worktree
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Attach a per-feature context struct, replacing any previous value of
    /// the same type. Prefer the named `with_*` builders below; this is the
    /// escape hatch for a feature whose state has no builder yet.
    pub fn with_extension<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.extensions.insert(value);
        self
    }

    /// Attach a whole bag of per-feature context structs at once, replacing
    /// any same-typed values already here.
    ///
    /// The plugin path: a host collects its features' state once per session
    /// and hands the bag to every context built from it, so a feature whose
    /// code lives outside this crate needs no builder here.
    pub fn with_extensions(mut self, extensions: Extensions) -> Self {
        self.extensions.extend(extensions);
        self
    }

    /// Borrow an attached per-feature context struct.
    pub fn extension<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.extensions.get::<T>()
    }

    /// How many feature contexts are attached (diagnostics and tests).
    pub fn extension_count(&self) -> usize {
        self.extensions.len()
    }

    /// Read-modify-write one feature context, creating it on first write.
    fn update_extension<T>(&mut self, update: impl FnOnce(&mut T))
    where
        T: Clone + Default + Send + Sync + 'static,
    {
        let mut value = self.extension::<T>().cloned().unwrap_or_default();
        update(&mut value);
        self.extensions.insert(value);
    }

    pub fn with_auto_mode_classifier_transcript(mut self, transcript: impl Into<String>) -> Self {
        let transcript: Arc<str> = Arc::from(transcript.into());
        self.update_extension::<AutoModeContext>(|auto| {
            auto.classifier_transcript = Some(transcript);
        });
        self
    }

    pub fn auto_mode_classifier_transcript(&self) -> Option<&str> {
        self.extension::<AutoModeContext>()?
            .classifier_transcript
            .as_deref()
    }

    pub fn with_additional_working_directories<I, S>(mut self, dirs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.additional_working_directories = dirs.into_iter().map(Into::into).collect();
        self
    }

    pub fn additional_working_directories(&self) -> &[String] {
        &self.additional_working_directories
    }

    pub fn with_path_scope_roots<I, P>(mut self, roots: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.path_scope_roots = roots.into_iter().map(Into::into).collect();
        self
    }

    pub fn path_scope_roots(&self) -> &[PathBuf] {
        &self.path_scope_roots
    }

    pub fn with_write_scope_roots<I, P>(mut self, roots: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.write_scope_roots = Some(roots.into_iter().map(Into::into).collect());
        self
    }

    pub fn write_scope_roots(&self) -> Option<&[PathBuf]> {
        self.write_scope_roots.as_deref()
    }

    pub fn with_auto_approved_write_roots<I, P>(mut self, roots: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.auto_approved_write_roots = roots.into_iter().map(Into::into).collect();
        self
    }

    pub fn auto_approved_write_roots(&self) -> &[PathBuf] {
        &self.auto_approved_write_roots
    }

    /// Attach a [`SubAgentSpawner`] — used by `AgentTool` to
    /// spawn a sub-agent query when invoked.
    pub fn with_sub_agent_spawner(mut self, spawner: Arc<dyn SubAgentSpawner>) -> Self {
        self.update_extension::<SubAgentContext>(|agent| agent.spawner = Some(spawner));
        self
    }

    /// Attach an [`McpClient`] — used by `McpTool` to dispatch
    /// tool calls to an MCP server.
    pub fn with_mcp_client(mut self, client: Arc<dyn McpClient>) -> Self {
        self.update_extension::<McpContext>(|mcp| mcp.client = Some(client));
        self
    }

    /// Attach a [`WebSearchDelegate`] — used by `WebSearchTool` to run
    /// provider-native web search through the active model route.
    pub fn with_web_search_delegate(mut self, delegate: Arc<dyn WebSearchDelegate>) -> Self {
        self.update_extension::<WebContext>(|web| web.search_delegate = Some(delegate));
        self
    }

    /// Attach a [`WorkflowLauncher`] — used by `WorkflowTool` to register and
    /// run local workflow tasks.
    pub fn with_workflow_launcher(mut self, launcher: Arc<dyn WorkflowLauncher>) -> Self {
        self.update_extension::<WorkflowContext>(|workflow| workflow.launcher = Some(launcher));
        self
    }

    /// Override the engine's default [`PermissionBroker`] for this
    /// call. Used by the ACP executor to wire the reverse-RPC
    /// permission publisher into the tool dispatch path.
    pub fn with_permission_broker(mut self, broker: Arc<dyn PermissionBroker>) -> Self {
        self.permission_broker = Some(broker);
        self
    }

    /// Attach a [`TeamManager`] - used by `AgentTool` when
    /// `team_name`/`name` indicate teammate spawning.
    pub fn with_team_manager(mut self, manager: Arc<dyn TeamManager>) -> Self {
        self.update_extension::<TeamContext>(|team| team.manager = Some(manager));
        self
    }

    /// Attach a [`TaskRuntimeController`] so task tools can control
    /// coordinator-owned runtime tasks without depending on coordinator.
    pub fn with_task_runtime_controller(
        mut self,
        controller: Arc<dyn TaskRuntimeController>,
    ) -> Self {
        self.update_extension::<TaskContext>(|task| task.runtime_controller = Some(controller));
        self
    }

    /// Attach a [`QueueController`] so a queue coordinator session can read the
    /// outline it is steering without depending on the app that owns it.
    pub fn with_queue_controller(mut self, controller: Arc<dyn QueueController>) -> Self {
        self.update_extension::<QueueContext>(|queue| queue.controller = Some(controller));
        self
    }

    pub fn with_team_identity(mut self, identity: TeamIdentityContext) -> Self {
        if self.agent_id.is_none() {
            self.agent_id = Some(identity.agent_id.clone());
        }
        self.update_extension::<TeamContext>(|team| team.identity = Some(identity));
        self
    }

    pub fn with_permission_mode_provider<F>(mut self, provider: F) -> Self
    where
        F: Fn() -> Option<String> + Send + Sync + 'static,
    {
        self.permission_mode_provider = Some(Arc::new(provider));
        self
    }

    pub fn with_permission_mode(self, mode: impl Into<String>) -> Self {
        let mode = mode.into();
        self.with_permission_mode_provider(move || Some(mode.clone()))
    }

    pub fn permission_mode(&self) -> Option<String> {
        self.team_identity()
            .and_then(|identity| identity.permission_mode.clone())
            .or_else(|| {
                self.permission_mode_provider
                    .as_ref()
                    .and_then(|provider| provider())
            })
    }

    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    pub fn agent_id(&self) -> Option<&str> {
        self.agent_id.as_deref()
    }

    /// Attach a [`CommandSandbox`] — used by `BashTool` /
    /// `PowerShellTool` to decide whether a command must be confined,
    /// and to build the process that confines it.
    pub fn with_command_sandbox(mut self, sandbox: Arc<dyn CommandSandbox>) -> Self {
        self.command_sandbox = Some(sandbox);
        self
    }

    /// Attach a resolved task list ID so all task tools within a
    /// query turn use the same value.
    pub fn with_task_list_id(mut self, id: impl Into<String>) -> Self {
        let id = id.into();
        self.update_extension::<TaskContext>(|task| task.list_id = Some(id));
        self
    }

    /// Return the resolved task list ID, falling back to
    /// [`crate::tasks::current_task_list_id`] when the context
    /// was not populated by the engine.
    pub fn task_list_id(&self) -> String {
        self.extension::<TaskContext>()
            .and_then(|task| task.list_id.clone())
            .unwrap_or_else(crate::tasks::current_task_list_id)
    }

    pub fn tool_use_id(&self) -> Option<&str> {
        self.tool_use_id.as_deref()
    }

    pub fn emit_progress(&self, update: ToolProgressUpdate) -> bool {
        self.progress
            .as_ref()
            .is_some_and(|progress| progress.send(update))
    }

    /// Borrow the injected sub-agent spawner, if any.
    pub fn sub_agent_spawner(&self) -> Option<&Arc<dyn SubAgentSpawner>> {
        self.extension::<SubAgentContext>()?.spawner.as_ref()
    }

    /// Borrow the injected MCP client, if any.
    pub fn mcp_client(&self) -> Option<&Arc<dyn McpClient>> {
        self.extension::<McpContext>()?.client.as_ref()
    }

    pub fn with_mcp_tool_definitions(
        mut self,
        definitions: Arc<Vec<(String, McpToolDefinition)>>,
    ) -> Self {
        self.update_extension::<McpContext>(|mcp| mcp.tool_definitions = Some(definitions));
        self
    }

    pub fn mcp_tool_definitions(&self) -> Option<&[(String, McpToolDefinition)]> {
        self.extension::<McpContext>()?
            .tool_definitions
            .as_deref()
            .map(Vec::as_slice)
    }

    pub fn with_plugin_tools(mut self, provider: Arc<dyn PluginToolProvider>) -> Self {
        self.plugin_tools = Some(provider);
        self
    }

    pub fn plugin_tools(&self) -> Option<&Arc<dyn PluginToolProvider>> {
        self.plugin_tools.as_ref()
    }

    pub fn with_tool_resolver(mut self, resolver: Arc<dyn ToolResolver>) -> Self {
        self.tool_resolver = Some(resolver);
        self
    }

    pub fn tool_resolver(&self) -> Option<&Arc<dyn ToolResolver>> {
        self.tool_resolver.as_ref()
    }

    /// Attach a [`WebProviderRouter`] — the WebSearch/WebFetch tools consult
    /// it before their builtin paths (kernel `ctx.web` seat).
    pub fn with_web_provider_router(mut self, router: Arc<dyn WebProviderRouter>) -> Self {
        self.update_extension::<WebContext>(|web| web.provider_router = Some(router));
        self
    }

    pub fn web_provider_router(&self) -> Option<&Arc<dyn WebProviderRouter>> {
        self.extension::<WebContext>()?.provider_router.as_ref()
    }

    pub fn with_tool_filter(mut self, filter: Option<ToolFilter>) -> Self {
        self.tool_filter = filter;
        self
    }

    pub fn tool_filter(&self) -> Option<&ToolFilter> {
        self.tool_filter.as_ref()
    }

    /// Borrow the injected web search delegate, if any.
    pub fn web_search_delegate(&self) -> Option<&Arc<dyn WebSearchDelegate>> {
        self.extension::<WebContext>()?.search_delegate.as_ref()
    }

    /// Borrow the injected workflow launcher, if any.
    pub fn workflow_launcher(&self) -> Option<&Arc<dyn WorkflowLauncher>> {
        self.extension::<WorkflowContext>()?.launcher.as_ref()
    }

    /// Borrow the injected permission broker override, if any.
    pub fn permission_broker(&self) -> Option<&Arc<dyn PermissionBroker>> {
        self.permission_broker.as_ref()
    }

    /// Borrow the injected team manager, if any.
    pub fn team_manager(&self) -> Option<&Arc<dyn TeamManager>> {
        self.extension::<TeamContext>()?.manager.as_ref()
    }

    /// Borrow the injected task runtime controller, if any.
    pub fn task_runtime_controller(&self) -> Option<&Arc<dyn TaskRuntimeController>> {
        self.extension::<TaskContext>()?.runtime_controller.as_ref()
    }

    /// Borrow the injected queue controller, if any.
    pub fn queue_controller(&self) -> Option<&Arc<dyn QueueController>> {
        self.extension::<QueueContext>()?.controller.as_ref()
    }

    pub fn team_identity(&self) -> Option<&TeamIdentityContext> {
        self.extension::<TeamContext>()?.identity.as_ref()
    }

    /// Resolve the current team without allowing one session's process-wide
    /// compatibility environment variable to leak into another session.
    pub fn current_team_name(&self) -> Option<String> {
        self.team_identity()
            .map(|identity| identity.team_name.clone())
            .or_else(|| {
                let session_id = self.session_id.as_deref()?;
                self.team_manager()
                    .and_then(|manager| manager.team_name_for_session(session_id))
                    .or_else(|| {
                        crate::team_files::team_name_for_session(session_id)
                            .ok()
                            .flatten()
                    })
            })
            .or_else(|| {
                self.session_id
                    .is_none()
                    .then(crate::team_files::current_team_name)
                    .flatten()
                    .filter(|team_name| {
                        crate::team_files::read_team_file(team_name)
                            .ok()
                            .flatten()
                            .is_some()
                    })
            })
    }

    /// Borrow the injected command sandbox, if any.
    ///
    /// `None` is the ordinary case and means "no opinion": the tool spawns
    /// the argv it built, exactly as it would on a machine where the sandbox
    /// feature was never written.
    pub fn command_sandbox(&self) -> Option<&Arc<dyn CommandSandbox>> {
        self.command_sandbox.as_ref()
    }

    /// Attach a [`ToolSearchIndex`] — used by `ToolSearchTool` to
    /// search deferred tools by keyword.
    pub fn with_tool_search_index(mut self, index: Arc<tool_search::ToolSearchIndex>) -> Self {
        self.update_extension::<ToolSearchContext>(|search| {
            search.index = Some(index);
            if search.discovered.is_none() {
                search.discovered = Some(Arc::new(Mutex::new(HashSet::new())));
            }
        });
        self
    }

    pub fn without_tool_search_index(mut self) -> Self {
        self.update_extension::<ToolSearchContext>(|search| {
            search.index = None;
            search.discovered = None;
        });
        self
    }

    /// Borrow the injected tool search index, if any.
    pub fn tool_search_index(&self) -> Option<&Arc<tool_search::ToolSearchIndex>> {
        self.extension::<ToolSearchContext>()?.index.as_ref()
    }

    /// Record a deferred tool name returned by ToolSearch for this context.
    pub fn record_discovered_deferred_tool(&self, name: &str) {
        if let Some(discovered) = self.discovered_deferred_tools() {
            discovered.lock().unwrap().insert(name.to_string());
        }
    }

    /// Whether a deferred tool has actually been returned by ToolSearch.
    pub fn is_deferred_tool_discovered(&self, name: &str) -> bool {
        self.discovered_deferred_tools()
            .is_some_and(|discovered| discovered.lock().unwrap().contains(name))
    }

    /// Snapshot the discovered deferred tool names for diagnostics/tests.
    pub fn discovered_deferred_tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .discovered_deferred_tools()
            .map(|discovered| discovered.lock().unwrap().iter().cloned().collect())
            .unwrap_or_default();
        names.sort_unstable();
        names
    }

    /// Attach an existing discovered deferred tool set, used to share discovery
    /// state across cloned contexts in a running query/session.
    pub fn with_discovered_deferred_tools(
        mut self,
        discovered: Arc<Mutex<HashSet<String>>>,
    ) -> Self {
        self.update_extension::<ToolSearchContext>(|search| {
            search.discovered = Some(discovered);
        });
        self
    }

    /// Return the shared discovered deferred tool set, if any.
    pub fn discovered_deferred_tools(&self) -> Option<&Arc<Mutex<HashSet<String>>>> {
        self.extension::<ToolSearchContext>()?.discovered.as_ref()
    }

    /// Attach a shared [`FileStateCache`] so Read can register files
    /// and Edit / Write can enforce "must read first".
    pub fn with_file_state_cache(mut self, cache: FileStateCache) -> Self {
        self.file_state_cache = Some(cache);
        self
    }

    /// Borrow the shared file-state cache, if any. Tools that don't
    /// need it (Grep, Bash, Agent, …) can ignore the absence — this
    /// is `None` in unit tests that construct `ToolContext::new()`
    /// directly.
    pub fn file_state_cache(&self) -> Option<&FileStateCache> {
        self.file_state_cache.as_ref()
    }

    /// Start a fresh logical tool-dispatch batch. All clones keep the same
    /// token; the next model response gets a new token.
    pub fn with_fresh_file_mutation_batch(mut self) -> Self {
        self.file_mutation_batch = Some(Arc::new(FileMutationBatch));
        self
    }

    pub fn with_parallel_agent_write_batch(mut self, parallel: bool) -> Self {
        self.update_extension::<SubAgentContext>(|agent| agent.parallel_write_batch = parallel);
        self
    }

    pub fn parallel_agent_write_batch(&self) -> bool {
        self.extension::<SubAgentContext>()
            .is_some_and(|agent| agent.parallel_write_batch)
    }

    pub fn with_file_history_tracker(mut self, tracker: Arc<dyn FileHistoryTracker>) -> Self {
        self.file_history_tracker = Some(tracker);
        self
    }

    pub fn file_history_tracker(&self) -> Option<&Arc<dyn FileHistoryTracker>> {
        self.file_history_tracker.as_ref()
    }

    pub fn with_worker_escalation_client(mut self, client: WorkerEscalationClient) -> Self {
        self.update_extension::<EscalationContext>(|escalation| {
            escalation.worker_client = Some(client);
        });
        self
    }

    pub fn worker_escalation_client(&self) -> Option<&WorkerEscalationClient> {
        self.extension::<EscalationContext>()?
            .worker_client
            .as_ref()
    }

    pub fn with_escalation_resolver(mut self, resolver: EscalationResolver) -> Self {
        self.update_extension::<EscalationContext>(|escalation| {
            escalation.resolver = Some(resolver);
        });
        self
    }

    pub fn escalation_resolver(&self) -> Option<&EscalationResolver> {
        self.extension::<EscalationContext>()?.resolver.as_ref()
    }

    pub fn with_session_cron_store(mut self, store: Arc<SessionCronStore>) -> Self {
        self.update_extension::<CronContext>(|cron| cron.session_store = Some(store));
        self
    }

    pub fn session_cron_store(&self) -> Option<&Arc<SessionCronStore>> {
        self.extension::<CronContext>()?.session_store.as_ref()
    }

    pub fn with_shell_process_registry(mut self, registry: Arc<ShellProcessRegistry>) -> Self {
        self.shell_process_registry = Some(registry);
        self
    }

    pub fn with_shell_process_registry_if_absent(
        mut self,
        registry: Arc<ShellProcessRegistry>,
    ) -> Self {
        if self.shell_process_registry.is_none() {
            self.shell_process_registry = Some(registry);
        }
        self
    }

    pub fn shell_process_registry(&self) -> Option<&Arc<ShellProcessRegistry>> {
        self.shell_process_registry.as_ref()
    }

    pub fn with_monitor_registry(mut self, registry: Arc<MonitorRegistry>) -> Self {
        self.update_extension::<MonitorContext>(|monitor| monitor.registry = Some(registry));
        self
    }

    pub fn with_monitor_registry_if_absent(mut self, registry: Arc<MonitorRegistry>) -> Self {
        self.update_extension::<MonitorContext>(|monitor| {
            if monitor.registry.is_none() {
                monitor.registry = Some(registry);
            }
        });
        self
    }

    pub fn monitor_registry(&self) -> Option<&Arc<MonitorRegistry>> {
        self.extension::<MonitorContext>()?.registry.as_ref()
    }

    /// Borrow the ultraplan run slice of the extension bag.
    fn ultraplan_run(&self) -> Option<&UltraplanRunContext> {
        self.extension::<UltraplanRunContext>()
    }

    pub fn with_ultraplan_run_repository(
        mut self,
        repository: Arc<dyn UltraplanRunRepository>,
    ) -> Self {
        self.update_extension::<UltraplanRunContext>(|run| run.repository = Some(repository));
        self
    }

    pub fn ultraplan_run_repository(&self) -> Option<&Arc<dyn UltraplanRunRepository>> {
        self.ultraplan_run()?.repository.as_ref()
    }

    pub fn load_ultraplan_run_state(
        &self,
    ) -> Result<Option<UltraplanRunState>, UltraplanRepositoryError> {
        if let Some(repository) = self.ultraplan_run_repository() {
            return repository.load_current().map(Some);
        }
        let Some(handle) = self.ultraplan_run_handle() else {
            return Ok(None);
        };
        let has_syncer = self.ultraplan_run().is_some_and(|run| run.syncer.is_some());
        if has_syncer && !self.sync_ultraplan_run_handle() {
            return Ok(None);
        }
        Ok(Some(
            handle.lock().expect("ultraplan run state poisoned").clone(),
        ))
    }

    pub fn load_ultraplan_run_by_id(
        &self,
        run_id: &str,
    ) -> Result<Option<UltraplanRunState>, UltraplanRepositoryError> {
        if let Some(repository) = self.ultraplan_run_repository() {
            return repository.load_run(run_id);
        }
        Ok(self.ultraplan_run_handle().and_then(|handle| {
            let state = handle.lock().expect("ultraplan run state poisoned");
            (state.run_id == run_id).then(|| state.clone())
        }))
    }

    pub fn compare_and_swap_ultraplan_run(
        &self,
        expected_revision: u64,
        state: &UltraplanRunState,
    ) -> Result<(), UltraplanRepositoryError> {
        if let Some(repository) = self.ultraplan_run_repository() {
            return repository.compare_and_swap(expected_revision, state);
        }
        let Some(handle) = self.ultraplan_run_handle() else {
            return Err(UltraplanRepositoryError::Missing);
        };
        {
            let mut current = handle.lock().expect("ultraplan run state poisoned");
            if current.run_id != state.run_id {
                return Err(UltraplanRepositoryError::RunIdMismatch {
                    expected: current.run_id.clone(),
                    actual: state.run_id.clone(),
                });
            }
            if current.state_revision != expected_revision {
                return Err(UltraplanRepositoryError::StaleRevision {
                    expected: expected_revision,
                    actual: current.state_revision,
                });
            }
            *current = state.clone();
        }
        self.persist_ultraplan_run_handle();
        Ok(())
    }

    pub fn create_ultraplan_run(
        &self,
        state: &UltraplanRunState,
    ) -> Result<(), UltraplanRepositoryError> {
        self.ultraplan_run_repository()
            .ok_or(UltraplanRepositoryError::Missing)?
            .create_run(state)
    }

    pub fn switch_current_ultraplan_run(
        &self,
        run_id: &str,
    ) -> Result<(), UltraplanRepositoryError> {
        self.ultraplan_run_repository()
            .ok_or(UltraplanRepositoryError::Missing)?
            .switch_current(run_id)
    }

    pub fn with_ultraplan_run_handle(mut self, handle: Arc<Mutex<UltraplanRunState>>) -> Self {
        self.update_extension::<UltraplanRunContext>(|run| run.handle = Some(handle));
        self
    }

    pub fn with_ultraplan_run_syncer(mut self, syncer: UltraplanRunSyncer) -> Self {
        self.update_extension::<UltraplanRunContext>(|run| run.syncer = Some(syncer));
        self
    }

    pub fn with_ultraplan_run_persister(mut self, persister: UltraplanRunPersister) -> Self {
        self.update_extension::<UltraplanRunContext>(|run| run.persister = Some(persister));
        self
    }

    pub fn sync_ultraplan_run_handle(&self) -> bool {
        let Some(run) = self.ultraplan_run() else {
            return false;
        };
        if let Some(repository) = &run.repository {
            let Ok(state) = repository.load_current() else {
                return false;
            };
            if let Some(handle) = &run.handle {
                *handle.lock().expect("ultraplan run state poisoned") = state;
            }
            return true;
        }
        match (&run.handle, &run.syncer) {
            (Some(handle), Some(syncer)) => syncer(handle),
            _ => false,
        }
    }

    pub fn persist_ultraplan_run_handle(&self) {
        let Some(run) = self.ultraplan_run() else {
            return;
        };
        if let (Some(handle), Some(persister)) = (&run.handle, &run.persister) {
            persister(handle);
        }
    }

    pub fn ultraplan_run_handle(&self) -> Option<&Arc<Mutex<UltraplanRunState>>> {
        self.ultraplan_run()?.handle.as_ref()
    }

    pub fn with_execution_policy(mut self, policy: ExecutionPolicy) -> Self {
        self.execution_policy = Some(policy);
        self
    }

    pub fn with_optional_execution_policy(mut self, policy: Option<ExecutionPolicy>) -> Self {
        self.execution_policy = policy;
        self
    }

    pub fn without_execution_policy(mut self) -> Self {
        self.execution_policy = None;
        self
    }

    pub fn execution_policy(&self) -> Option<&ExecutionPolicy> {
        self.execution_policy.as_ref()
    }

    pub fn with_exit_plan_mode_approval(&self) -> Self {
        let mut context = self.clone();
        context.update_extension::<PlanModeContext>(|plan| plan.exit_approved = true);
        context
    }

    pub fn exit_plan_mode_approved(&self) -> bool {
        self.extension::<PlanModeContext>()
            .is_some_and(|plan| plan.exit_approved)
    }

    pub fn with_structured_output_channel(mut self, channel: Arc<StructuredOutputChannel>) -> Self {
        self.update_extension::<StructuredOutputContext>(|output| output.channel = Some(channel));
        self
    }

    pub fn structured_output_channel(&self) -> Option<&Arc<StructuredOutputChannel>> {
        self.extension::<StructuredOutputContext>()?
            .channel
            .as_ref()
    }

    pub fn with_denial_replay_reason(mut self, reason: impl Into<String>) -> Self {
        self.denial_replay_reason = Some(reason.into());
        self
    }

    pub fn denial_replay_reason(&self) -> Option<&str> {
        self.denial_replay_reason.as_deref()
    }

    pub fn with_coordinator_mode(mut self, enabled: bool) -> Self {
        self.update_extension::<CoordinatorContext>(|coordinator| coordinator.mode = enabled);
        self
    }

    pub fn coordinator_mode(&self) -> bool {
        self.extension::<CoordinatorContext>()
            .is_some_and(|coordinator| coordinator.mode)
    }

    pub fn with_workflow_nesting_depth(mut self, depth: usize) -> Self {
        self.update_extension::<WorkflowContext>(|workflow| workflow.nesting_depth = depth);
        self
    }

    pub fn workflow_nesting_depth(&self) -> usize {
        self.extension::<WorkflowContext>()
            .map_or(0, |workflow| workflow.nesting_depth)
    }

    pub fn with_permission_prompts_unavailable(mut self, unavailable: bool) -> Self {
        self.permission_prompts_unavailable = unavailable;
        self
    }

    pub fn permission_prompts_unavailable(&self) -> bool {
        self.permission_prompts_unavailable
    }

    pub fn with_coordinator_report_paths<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let paths: Vec<PathBuf> = paths.into_iter().map(Into::into).collect();
        self.update_extension::<CoordinatorContext>(|coordinator| {
            coordinator.report_paths = paths;
        });
        self
    }

    pub fn extend_coordinator_report_paths<I, P>(&mut self, paths: I)
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let paths: Vec<PathBuf> = paths.into_iter().map(Into::into).collect();
        self.update_extension::<CoordinatorContext>(|coordinator| {
            for path in paths {
                if !coordinator.report_paths.contains(&path) {
                    coordinator.report_paths.push(path);
                }
            }
        });
    }

    pub fn coordinator_report_paths(&self) -> &[PathBuf] {
        const NONE: &[PathBuf] = &[];
        self.extension::<CoordinatorContext>()
            .map_or(NONE, |coordinator| coordinator.report_paths.as_slice())
    }

    pub fn with_frozen_parent_context(mut self, capsule: FrozenParentContextCapsule) -> Self {
        self.update_extension::<SubAgentContext>(|agent| agent.frozen_parent = Some(capsule));
        self
    }

    pub fn frozen_parent_context(&self) -> Option<&FrozenParentContextCapsule> {
        self.extension::<SubAgentContext>()?.frozen_parent.as_ref()
    }

    pub fn with_capability_context(mut self, context: CapabilityContext) -> Self {
        self.update_extension::<UltraplanRunContext>(|run| run.capability = Some(context));
        self
    }

    pub fn capability_context(&self) -> Option<&CapabilityContext> {
        self.ultraplan_run()?.capability.as_ref()
    }

    pub fn ultraplan_context(&self) -> Option<&UltraplanContext> {
        self.execution_policy
            .as_ref()
            .and_then(|p| p.ultraplan.as_ref())
    }

    pub fn effective_ultraplan_context(&self) -> Option<UltraplanContext> {
        let mut context = self.ultraplan_context()?.clone();
        if let Ok(Some(state)) = self.load_ultraplan_run_state() {
            if state.profile == context.profile {
                context = context.with_run_head(&state.head());
                context.manifest = state.manifest.clone();
            }
        }
        Some(context)
    }
}

pub const PERMISSION_AUTHORIZED_PATHS_METADATA_KEY: &str = "authorized_path_roots";
const PERMISSION_METADATA_KIND_KEY: &str = "kind";

pub fn context_with_permission_authorized_paths(
    tool_name: &str,
    context: &ToolContext,
    request: &PermissionRequest,
) -> Option<ToolContext> {
    if tool_name != AGENT_TOOL_NAME {
        return None;
    }
    let metadata = request.metadata.as_ref()?;
    if metadata
        .get(PERMISSION_METADATA_KIND_KEY)
        .and_then(Value::as_str)
        != Some(AGENT_EXTERNAL_PATH_AUTHORIZATION_KIND)
    {
        return None;
    }
    let paths = metadata
        .get(PERMISSION_AUTHORIZED_PATHS_METADATA_KEY)?
        .as_array()?;
    let mut directories = context.additional_working_directories().to_vec();
    let initial_len = directories.len();
    for path in paths {
        let Some(path) = path.as_str().map(str::trim).filter(|path| !path.is_empty()) else {
            continue;
        };
        if !directories.iter().any(|existing| existing == path) {
            directories.push(path.to_string());
        }
    }
    (directories.len() > initial_len).then(|| {
        context
            .clone()
            .with_additional_working_directories(directories)
    })
}

/// Cloneable adapter around the runtime's progress channel.
#[derive(Clone)]
pub struct ToolProgressSender {
    tx: mpsc::UnboundedSender<ToolProgressUpdate>,
}

impl std::fmt::Debug for ToolProgressSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolProgressSender").finish_non_exhaustive()
    }
}

impl ToolProgressSender {
    pub fn new(tx: mpsc::UnboundedSender<ToolProgressUpdate>) -> Self {
        Self { tx }
    }

    pub fn send(&self, update: ToolProgressUpdate) -> bool {
        self.tx.send(update).is_ok()
    }
}

/// An async, dynamically-dispatched tool.
///
/// The runtime-facing surface every tool exposes:
///
/// - stable name + aliases
/// - JSON input schema
/// - validation
/// - tool-specific permission gating
/// - execution with optional progress streaming
///
/// Presentational concerns — how a tool call is written into the transcript —
/// live outside this trait for now.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Stable identifier used by the model and harness to route calls.
    fn id(&self) -> ToolId;

    /// Alternate names the tool answers to, in addition to [`Self::id`].
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    /// Human-readable description surfaced to local UI/tests.
    fn description(&self) -> &str;

    /// Description sent in provider-visible tool metadata. Defaults to the
    /// full local description; tools with large operational guidance can
    /// override this when equivalent guidance is already carried elsewhere in
    /// the system prompt/runtime context.
    fn model_description(&self) -> &str {
        self.description()
    }

    /// JSON schema for the tool's input object.
    fn input_schema(&self) -> ToolInputSchema;

    /// Whether the tool is currently enabled.
    fn is_enabled(&self) -> bool {
        true
    }

    /// Whether this tool should be deferred (not sent to the model
    /// upfront). Deferred tools are discoverable via the
    /// [`ToolSearchTool`] and their names are listed in the system
    /// prompt. The model must call ToolSearchTool to load their
    /// schemas before it can invoke them. MCP tools override this
    /// to `true` by default.
    fn should_defer(&self) -> bool {
        false
    }

    /// What this tool is to policy (see [`rebon_tools_core::ToolKind`]).
    /// Permission modes, plan-mode deny lists and path rules ask this
    /// instead of matching names, so a tool that edits files under any
    /// name is treated as one. Defaults to `Other`: nothing special.
    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::Other
    }

    /// The input field naming the file this tool acts on, when its kind is a
    /// file kind (`file_path` for the editors, `notebook_path` for
    /// NotebookEdit). Path rules such as `Edit(src/**)` read the target from
    /// here.
    fn file_target_field(&self) -> Option<&'static str> {
        None
    }

    /// Short capability phrase (3–10 words) used by ToolSearchTool
    /// for keyword matching. Prefer terms not already in the tool
    /// name. Example: `"jupyter ipython python notebook"` for
    /// NotebookEdit.
    fn search_hint(&self) -> Option<&str> {
        None
    }

    /// Whether parallel calls are safe.
    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    /// Whether the tool is read-only.
    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }

    /// Whether the tool is destructive / irreversible.
    fn is_destructive(&self, _input: &Value) -> bool {
        false
    }

    /// Fast path used by the runtime to decide whether permission handling is needed.
    fn needs_permission(&self, _input: &Value) -> bool {
        false
    }

    /// Tool-local validation of the input JSON.
    async fn validate_input(
        &self,
        _input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        Ok(ValidationOutcome::valid())
    }

    /// Tool-local permission shaping: this tool's own decision on the call,
    /// before the broker has its say.
    async fn check_permissions(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        Ok(PermissionDecision::allow(input.clone()))
    }

    /// Execute the tool with the given JSON input, returning a JSON result.
    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value>;

    /// This tool's own projection of a successful result into the content the
    /// model sees.
    ///
    /// `None` — the default — leaves the caller's generic projection in
    /// charge. Override it when the result needs a shape only the tool knows
    /// how to build (an image block beside its summary line, say); the
    /// judgment then sits next to the payload it judges instead of becoming
    /// another name branch in the engine's formatter.
    ///
    /// Only called for a result the tool returned successfully, so an
    /// implementation may assume its own output shape and fall back to `None`
    /// for anything it does not recognise.
    fn project_result_for_model(&self, _value: &Value) -> Option<rebon_api::ToolResultContent> {
        None
    }
}

/// Runtime adapter that mediates between a tool's
/// [`PermissionDecision`] and the actual execution.
///
/// The harness holds one `Arc<dyn PermissionBroker>` on the engine;
/// individual call sites can override it per-call via
/// [`ToolContext::with_permission_broker`] (typically the ACP reverse
/// RPC broker, which prompts the ACP client for approval).
///
/// This trait lives in `rebon-tool` rather than in the run loop, so
/// [`ToolContext`] can carry an override without a circular dep.
#[async_trait]
pub trait PermissionBroker: Send + Sync {
    /// Resolve the tool's permission decision and, on approval,
    /// invoke the tool.
    async fn resolve(
        &self,
        tool: &dyn Tool,
        input: Value,
        context: &ToolContext,
        decision: PermissionDecision,
    ) -> ToolResult<Value>;

    /// Downcast helper so the executor can forward routed permission
    /// queries to the concrete broker implementation.
    fn as_any(&self) -> &dyn std::any::Any {
        // Default: return a non-matchable reference. Concrete brokers
        // that need downcasting (e.g. ChannelPermissionBroker) override
        // this to return `self`.
        &()
    }
}

/// Default [`PermissionBroker`] used when the caller has not wired
/// any reverse-RPC approval path. `Allow` dispatches the tool,
/// `Deny` errors out, and `Ask` is surfaced as a
/// [`ToolError::PermissionDenied`] explaining that no approval
/// transport is available.
#[derive(Debug, Default)]
pub struct DenyAskPermissionBroker;

#[async_trait]
impl PermissionBroker for DenyAskPermissionBroker {
    async fn resolve(
        &self,
        tool: &dyn Tool,
        input: Value,
        context: &ToolContext,
        decision: PermissionDecision,
    ) -> ToolResult<Value> {
        match decision.behavior {
            PermissionBehavior::Allow => {
                let effective_input = decision.updated_input.unwrap_or(input);
                tool.call(effective_input, context).await
            }
            PermissionBehavior::Deny => Err(ToolError::PermissionDenied {
                tool: tool.id(),
                reason: decision
                    .reason
                    .unwrap_or_else(|| "tool permission denied".into()),
            }),
            PermissionBehavior::Ask => Err(ToolError::PermissionDenied {
                tool: tool.id(),
                reason: decision
                    .request
                    .map(|request| format!("permission required: {}", request.title))
                    .or(decision.reason)
                    .unwrap_or_else(|| "tool permission request is not yet wired".into()),
            }),
        }
    }
}

/// [`PermissionBroker`] that auto-approves `Ask` decisions.
///
/// Used for sub-agent workers that are already restricted by a
/// [`ToolFilter`] — the filter controls *which* tools the worker
/// can see, and this broker ensures the worker can actually *use*
/// them without a human-in-the-loop approval path (which
/// sub-agents don't have).
///
/// `Deny` decisions are still rejected.
#[derive(Debug, Default)]
pub struct AutoApprovePermissionBroker;

#[async_trait]
impl PermissionBroker for AutoApprovePermissionBroker {
    async fn resolve(
        &self,
        tool: &dyn Tool,
        input: Value,
        context: &ToolContext,
        decision: PermissionDecision,
    ) -> ToolResult<Value> {
        match decision.behavior {
            PermissionBehavior::Allow | PermissionBehavior::Ask => {
                let effective_input = decision.updated_input.unwrap_or(input);
                tool.call(effective_input, context).await
            }
            PermissionBehavior::Deny => Err(ToolError::PermissionDenied {
                tool: tool.id(),
                reason: decision
                    .reason
                    .unwrap_or_else(|| "tool permission denied".into()),
            }),
        }
    }
}

#[derive(Clone)]
pub struct AutoApproveExceptSensitivePermissionBroker {
    delegate: Arc<dyn PermissionBroker>,
    /// Checked at resolve time for sensitive inputs. When it returns
    /// true the broker denies outright instead of delegating: the
    /// delegate ultimately blocks on an interactive user prompt with
    /// no timeout, and a backgrounded worker has nobody watching that
    /// prompt — delegation would hang the agent indefinitely.
    background_probe: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Deletions whose targets all statically resolve inside this root
    /// (the session scratchpad) are NOT treated as sensitive, so
    /// workers can clean up their own temp artifacts without an
    /// interactive prompt they may never get.
    exempt_deletion_root: Option<PathBuf>,
    /// When true, a *file deletion* from a backgrounded worker
    /// delegates to the interactive broker instead of being denied:
    /// the permission request floats to the frontend (the TUI drains
    /// the permission channel every tick; the app follows background
    /// permissions), so the user gets asked "may I delete X?" and the
    /// worker blocks until they answer. The spawner sets this only
    /// when the parent runtime actually has an interactive prompt
    /// surface; every other sensitive class keeps the hard deny.
    background_deletion_ask: bool,
}

impl AutoApproveExceptSensitivePermissionBroker {
    pub fn new(delegate: Arc<dyn PermissionBroker>) -> Self {
        Self {
            delegate,
            background_probe: None,
            exempt_deletion_root: None,
            background_deletion_ask: false,
        }
    }

    pub fn with_background_probe(
        delegate: Arc<dyn PermissionBroker>,
        background_probe: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Self {
            delegate,
            background_probe: Some(background_probe),
            exempt_deletion_root: None,
            background_deletion_ask: false,
        }
    }

    /// Exempt deletions confined to `root` (the session scratchpad)
    /// from the sensitive-command gate. `None` clears the exemption.
    pub fn with_exempt_deletion_root(mut self, root: Option<PathBuf>) -> Self {
        self.exempt_deletion_root = root;
        self
    }

    /// Let file deletions from a backgrounded worker float to the
    /// frontend as an interactive permission request instead of being
    /// denied. Pass `true` only when the parent runtime has an
    /// interactive prompt surface — the worker blocks on the answer.
    pub fn with_background_deletion_ask(mut self, enabled: bool) -> Self {
        self.background_deletion_ask = enabled;
        self
    }

    pub fn delegate(&self) -> &Arc<dyn PermissionBroker> {
        &self.delegate
    }
}

impl std::fmt::Debug for AutoApproveExceptSensitivePermissionBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoApproveExceptSensitivePermissionBroker")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl PermissionBroker for AutoApproveExceptSensitivePermissionBroker {
    async fn resolve(
        &self,
        tool: &dyn Tool,
        input: Value,
        context: &ToolContext,
        decision: PermissionDecision,
    ) -> ToolResult<Value> {
        let check_input = decision.updated_input.as_ref().unwrap_or(&input);
        // Agents the runtime placed in their own worktree are exempt: the
        // tree is not shared, so there is no other-agent work a Git command
        // could destroy. This reads the explicit creation-time flag, never
        // the caller-supplied `cwd` — a spoofed path must not lift the gate.
        if decision.behavior != PermissionBehavior::Deny
            && WorkflowNesting::of(context).is_within_workflow()
            && !context.is_isolated_worktree()
            && is_workflow_sensitive_git_command(tool.id().as_str(), check_input)
        {
            if context.permission_prompts_unavailable()
                || self.background_probe.as_ref().is_some_and(|probe| probe())
            {
                return Err(ToolError::PermissionDenied {
                    tool: tool.id(),
                    reason: "this shared-worktree Git command requires explicit user approval, \
                             which a background workflow agent cannot request. The command was \
                             NOT run. Do not retry it or attempt another cleanup command."
                        .into(),
                });
            }
            let command = check_input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("<unknown command>");
            let forced_decision = PermissionDecision::ask(
                PermissionRequest::new(
                    "Approve shared-worktree Git operation",
                    format!(
                        "A workflow agent wants to run a Git command that can overwrite, hide, \
                         or move shared working-tree changes:\n\n{command}\n\nOnly approve if this exact operation was explicitly requested and will not discard user or other-agent work. (Non-destructive alternatives like `git switch`, `git checkout -b`, and plain `git pull` do not require approval.)"
                    ),
                )
                // No `allow_always`: the prompt promises approval covers
                // "this exact operation", but a persisted rule would let
                // the rules broker wave through every later command of the
                // same class. One-shot approval is the only honest option.
                .with_options(["allow_once", "reject_once"])
                .with_metadata(serde_json::json!({
                    "kind": "workflow_sensitive_git",
                    "workflowNestingDepth": context.workflow_nesting_depth(),
                })),
                Some(check_input.clone()),
            );
            return self
                .delegate
                .resolve(tool, input, context, forced_decision)
                .await;
        }

        if decision.behavior == PermissionBehavior::Ask {
            let check_input = decision.updated_input.as_ref().unwrap_or(&input);
            let grill_exit_plan_mode = tool.id().as_str() == "ExitPlanMode"
                && context.ultraplan_context().is_some_and(|ultraplan| {
                    ultraplan.profile == rebon_types::UltraplanProfile::Grill
                });
            if grill_exit_plan_mode
                || tool.id().as_str() == AGENT_TOOL_NAME
                || is_auto_mode_sensitive_tool_input_with_exempt_deletion_root(
                    tool.id().as_str(),
                    check_input,
                    self.exempt_deletion_root.as_deref(),
                )
            {
                if context.permission_prompts_unavailable()
                    || self.background_probe.as_ref().is_some_and(|probe| probe())
                {
                    // A backgrounded worker's *file deletion* may still
                    // ask: the request floats to the frontend as a
                    // permission modal and the worker blocks on the
                    // answer. Gated on the spawner-set flag so runtimes
                    // with no prompt surface keep denying, and scoped
                    // to deletions *only* — a compound command that
                    // also carries another sensitive class (git stash,
                    // script execution, …) keeps the hard deny, so a
                    // deletion cannot chaperone something the user
                    // would have to spot in the prompt text.
                    let deletion_ask_floats = self.background_deletion_ask
                        && is_sensitive_content_deletion_input(tool.id().as_str(), check_input)
                        && !is_sensitive_beyond_deletions(tool.id().as_str(), check_input);
                    if !deletion_ask_floats {
                        return Err(ToolError::PermissionDenied {
                            tool: tool.id(),
                            reason: "this command requires interactive user approval, which a \
                                 background agent cannot request. The command was NOT run. \
                                 Do not retry it; continue with the rest of the task without it."
                                .into(),
                        });
                    }
                }
                return self.delegate.resolve(tool, input, context, decision).await;
            }
        }
        AutoApprovePermissionBroker
            .resolve(tool, input, context, decision)
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// No-op placeholder tool used by tests and the initial scaffold.
pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn id(&self) -> ToolId {
        ToolId::new("Echo")
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["EchoTool"]
    }

    fn description(&self) -> &str {
        "Placeholder tool that echoes its JSON input back as output."
    }

    fn input_schema(&self) -> ToolInputSchema {
        serde_json::json!({
            "type": "object",
            "additionalProperties": true
        })
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        if let Some(tool_use_id) = context.tool_use_id() {
            context.emit_progress(
                ToolProgressUpdate::new("echo")
                    .with_message(format!("echo:{tool_use_id}"))
                    .with_payload(input.clone()),
            );
        }
        Ok(input)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    use rebon_tools_core::file_state::{file_mtime_ms, FileState};
    use rebon_tools_core::PermissionRequest;

    use super::*;

    struct BlockingHistoryTracker {
        entered: Arc<tokio::sync::Notify>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl FileHistoryTracker for BlockingHistoryTracker {
        fn track_before_write(&self, _file_path: &Path) -> anyhow::Result<()> {
            self.entered.notify_one();
            self.release
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv()
                .expect("test release signal");
            Ok(())
        }
    }

    struct ProbeHistoryTracker {
        entered: Arc<AtomicBool>,
    }

    impl FileHistoryTracker for ProbeHistoryTracker {
        fn track_before_write(&self, _file_path: &Path) -> anyhow::Result<()> {
            self.entered.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    struct NotifyingHistoryTracker {
        entered: Arc<tokio::sync::Notify>,
    }

    impl FileHistoryTracker for NotifyingHistoryTracker {
        fn track_before_write(&self, _file_path: &Path) -> anyhow::Result<()> {
            self.entered.notify_one();
            Ok(())
        }
    }

    fn observed_context(
        file: &Path,
        content: &str,
        cache: FileStateCache,
        tracker: Arc<dyn FileHistoryTracker>,
    ) -> ToolContext {
        cache.set(
            file,
            FileState {
                content: content.to_string(),
                timestamp_ms: file_mtime_ms(file).unwrap_or(0),
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        ToolContext::new()
            .with_file_state_cache(cache)
            .with_file_history_tracker(tracker)
    }

    #[derive(Clone, Copy)]
    enum FileMutationToolKind {
        Edit,
        Write,
        MultiEdit,
    }

    impl FileMutationToolKind {
        const ALL: [Self; 3] = [Self::Edit, Self::Write, Self::MultiEdit];

        fn name(self) -> &'static str {
            match self {
                Self::Edit => "edit",
                Self::Write => "write",
                Self::MultiEdit => "multi-edit",
            }
        }

        fn tool(self) -> Arc<dyn Tool> {
            match self {
                Self::Edit => Arc::new(EditTool),
                Self::Write => Arc::new(WriteTool),
                Self::MultiEdit => Arc::new(MultiEditTool),
            }
        }

        fn first_input(self, file: &Path) -> Value {
            match self {
                Self::Edit => serde_json::json!({
                    "file_path": file,
                    "old_string": "alpha",
                    "new_string": "ALPHA"
                }),
                Self::Write => {
                    serde_json::json!({ "file_path": file, "content": "ALPHA beta" })
                }
                Self::MultiEdit => serde_json::json!({
                    "file_path": file,
                    "edits": [{ "old_string": "alpha", "new_string": "ALPHA" }]
                }),
            }
        }

        fn second_input(self, file: &Path) -> Value {
            match self {
                Self::Edit => serde_json::json!({
                    "file_path": file,
                    "old_string": "beta",
                    "new_string": "BETA"
                }),
                Self::Write => {
                    serde_json::json!({ "file_path": file, "content": "ALPHA BETA" })
                }
                Self::MultiEdit => serde_json::json!({
                    "file_path": file,
                    "edits": [{ "old_string": "beta", "new_string": "BETA" }]
                }),
            }
        }
    }

    /// Wait until `expected` parties hold or await the path lock.
    ///
    /// Every file-mutation tool snapshots the file-state cache and only
    /// *then* awaits `lock_file_for_write`, so "queued on the lock" is
    /// the observable form of "has already taken its snapshot" — which
    /// is the ordering these tests need to establish before they release
    /// the first writer. Releasing it earlier lets the first writer
    /// refresh the shared cache first, and the second writer then
    /// correctly sees nothing stale.
    ///
    /// `file_access_lock` hands every caller its own `Arc` while the
    /// registry keeps only a `Weak`, so the strong count is exactly the
    /// number of parties holding or waiting for the lock, including the
    /// handle passed in here.
    ///
    /// Waiting on the condition rather than on a fixed number of
    /// `yield_now` calls is what makes this deterministic: a cold or
    /// loaded machine takes longer to get there, it does not take a
    /// different path.
    async fn wait_for_path_lock_parties(lock: &Arc<RwLock<()>>, expected: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while Arc::strong_count(lock) < expected {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {expected} parties on the path lock, saw {}",
                Arc::strong_count(lock)
            );
            tokio::task::yield_now().await;
        }
    }

    async fn assert_same_path_calls_serialize(
        first_tool: Arc<dyn Tool>,
        second_tool: Arc<dyn Tool>,
        file: PathBuf,
        initial: &str,
        first_input: Value,
        second_input: Value,
        expected_after_first: &str,
        shared_cache: bool,
    ) {
        std::fs::write(&file, initial).unwrap();
        // Held for the whole window so the lock registry cannot evict the
        // entry and both writers upgrade the same `Arc`.
        let path_lock = file_access_lock(&file);
        let shared = FileStateCache::new();
        let first_cache = if shared_cache {
            shared.clone()
        } else {
            FileStateCache::new()
        };
        let second_cache = if shared_cache {
            shared
        } else {
            FileStateCache::new()
        };
        let first_entered = Arc::new(tokio::sync::Notify::new());
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_context = observed_context(
            &file,
            initial,
            first_cache,
            Arc::new(BlockingHistoryTracker {
                entered: first_entered.clone(),
                release: Mutex::new(release_rx),
            }),
        );
        let second_entered = Arc::new(AtomicBool::new(false));
        let second_context = observed_context(
            &file,
            initial,
            second_cache,
            Arc::new(ProbeHistoryTracker {
                entered: second_entered.clone(),
            }),
        );

        let first = tokio::spawn(async move { first_tool.call(first_input, &first_context).await });
        first_entered.notified().await;

        let second_started = Arc::new(tokio::sync::Notify::new());
        let second_started_task = second_started.clone();
        let second = tokio::spawn(async move {
            second_started_task.notify_one();
            second_tool.call(second_input, &second_context).await
        });
        second_started.notified().await;
        // This helper's handle, the first writer's guard, and the second
        // writer waiting behind it.
        wait_for_path_lock_parties(&path_lock, 3).await;
        assert!(
            !second_entered.load(Ordering::SeqCst),
            "the second writer reached file history while the first still held the path lock"
        );

        release_tx.send(()).unwrap();
        first.await.unwrap().expect("first write should succeed");
        let second_error = second.await.unwrap().unwrap_err();
        assert!(matches!(
            second_error,
            ToolError::InvalidInput {
                error_code: Some(edit::FILE_MODIFIED_SINCE_READ_CODE),
                ..
            }
        ));
        assert_eq!(std::fs::read_to_string(file).unwrap(), expected_after_first);
    }

    #[test]
    fn file_access_lock_key_collapses_lexical_path_variants() {
        let root = tempfile::tempdir().unwrap();
        let direct = root.path().join("target.txt");
        let variant = root.path().join("nested").join("..").join("target.txt");

        assert_eq!(
            normalized_file_access_key(&direct),
            normalized_file_access_key(&variant)
        );
    }

    #[test]
    fn file_access_lock_is_shared_by_hard_link_aliases() {
        let root = tempfile::tempdir().unwrap();
        let direct = root.path().join("target.txt");
        let alias = root.path().join("alias.txt");
        std::fs::write(&direct, "content").unwrap();
        std::fs::hard_link(&direct, &alias).unwrap();

        let direct_lock = file_access_lock(&direct);
        let alias_lock = file_access_lock(&alias);

        assert!(Arc::ptr_eq(&direct_lock, &alias_lock));
    }

    #[cfg(unix)]
    #[test]
    fn file_access_lock_key_resolves_symlinked_parent_for_new_file() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let real_parent = root.path().join("real");
        let alias_parent = root.path().join("alias");
        std::fs::create_dir(&real_parent).unwrap();
        symlink(&real_parent, &alias_parent).unwrap();

        assert_eq!(
            normalized_file_access_key(&real_parent.join("new.txt")),
            normalized_file_access_key(&alias_parent.join("new.txt"))
        );
    }

    #[test]
    fn tool_context_extends_coordinator_report_paths_without_duplicates() {
        let mut context = ToolContext::new().with_coordinator_report_paths(["first.report.md"]);

        context.extend_coordinator_report_paths(["first.report.md", "second.report.md"]);

        assert_eq!(
            context.coordinator_report_paths(),
            &[
                PathBuf::from("first.report.md"),
                PathBuf::from("second.report.md")
            ]
        );
    }

    #[test]
    fn ultraplan_run_handle_requires_syncer_to_be_available() {
        let handle = Arc::new(Mutex::new(UltraplanRunState::new(
            "run".into(),
            "session".into(),
            "task".into(),
            None,
            1,
        )));
        let context = ToolContext::new().with_ultraplan_run_handle(handle.clone());

        assert!(!context.sync_ultraplan_run_handle());

        let expected = handle.clone();
        let context = context.with_ultraplan_run_syncer(Arc::new(move |candidate| {
            Arc::ptr_eq(candidate, &expected)
        }));
        assert!(context.sync_ultraplan_run_handle());
    }

    async fn assert_file_mutation_tools_serialize_same_path(shared_cache: bool) {
        let dir = tempfile::tempdir().unwrap();

        for first_kind in FileMutationToolKind::ALL {
            for second_kind in FileMutationToolKind::ALL {
                let file = dir.path().join(format!(
                    "{}-then-{}.txt",
                    first_kind.name(),
                    second_kind.name()
                ));
                assert_same_path_calls_serialize(
                    first_kind.tool(),
                    second_kind.tool(),
                    file.clone(),
                    "alpha beta",
                    first_kind.first_input(&file),
                    second_kind.second_input(&file),
                    "ALPHA beta",
                    shared_cache,
                )
                .await;
            }
        }
    }

    async fn assert_same_batch_rejects_later_scheduled_mutation(
        first_tool: Arc<dyn Tool>,
        second_tool: Arc<dyn Tool>,
        file: PathBuf,
        initial: &str,
        first_input: Value,
        second_input: Value,
        expected_after_first: &str,
        expected_after_next_batch: &str,
    ) {
        std::fs::write(&file, initial).unwrap();
        let base_context = observed_context(
            &file,
            initial,
            FileStateCache::new(),
            Arc::new(ProbeHistoryTracker {
                entered: Arc::new(AtomicBool::new(false)),
            }),
        );
        let batch_context = base_context.clone().with_fresh_file_mutation_batch();

        first_tool
            .call(first_input, &batch_context)
            .await
            .expect("first batch mutation");
        let error = second_tool
            .call(second_input.clone(), &batch_context)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ToolError::InvalidInput {
                error_code: Some(edit::FILE_MODIFIED_SINCE_READ_CODE),
                ..
            }
        ));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            expected_after_first
        );

        second_tool
            .call(second_input, &base_context.with_fresh_file_mutation_batch())
            .await
            .expect("a later model-response batch may build on the refreshed cache");
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            expected_after_next_batch
        );
    }

    #[tokio::test]
    async fn same_dispatch_batch_rejects_later_scheduled_same_path_mutations() {
        let dir = tempfile::tempdir().unwrap();

        for first_kind in FileMutationToolKind::ALL {
            for second_kind in FileMutationToolKind::ALL {
                let file = dir.path().join(format!(
                    "batch-{}-then-{}.txt",
                    first_kind.name(),
                    second_kind.name()
                ));
                assert_same_batch_rejects_later_scheduled_mutation(
                    first_kind.tool(),
                    second_kind.tool(),
                    file.clone(),
                    "alpha beta",
                    first_kind.first_input(&file),
                    second_kind.second_input(&file),
                    "ALPHA beta",
                    "ALPHA BETA",
                )
                .await;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn file_mutation_tools_serialize_same_path_across_independent_contexts() {
        assert_file_mutation_tools_serialize_same_path(false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn file_mutation_tools_reject_parallel_stale_writes_with_shared_cache() {
        assert_file_mutation_tools_serialize_same_path(true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn parallel_new_file_writes_do_not_silently_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("new.txt");
        let path_lock = file_access_lock(&file);
        let shared_cache = FileStateCache::new();
        let first_entered = Arc::new(tokio::sync::Notify::new());
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_context = ToolContext::new()
            .with_file_state_cache(shared_cache.clone())
            .with_file_history_tracker(Arc::new(BlockingHistoryTracker {
                entered: first_entered.clone(),
                release: Mutex::new(release_rx),
            }));
        let second_entered = Arc::new(AtomicBool::new(false));
        let second_context = ToolContext::new()
            .with_file_state_cache(shared_cache)
            .with_file_history_tracker(Arc::new(ProbeHistoryTracker {
                entered: second_entered.clone(),
            }));

        let first_path = file.clone();
        let first = tokio::spawn(async move {
            WriteTool
                .call(
                    serde_json::json!({ "file_path": first_path, "content": "first" }),
                    &first_context,
                )
                .await
        });
        first_entered.notified().await;

        let second_path = file.clone();
        let second_started = Arc::new(tokio::sync::Notify::new());
        let second_started_task = second_started.clone();
        let second = tokio::spawn(async move {
            second_started_task.notify_one();
            WriteTool
                .call(
                    serde_json::json!({ "file_path": second_path, "content": "second" }),
                    &second_context,
                )
                .await
        });
        second_started.notified().await;
        wait_for_path_lock_parties(&path_lock, 3).await;
        assert!(!second_entered.load(Ordering::SeqCst));

        release_tx.send(()).unwrap();
        first.await.unwrap().expect("first creator");
        let error = second.await.unwrap().unwrap_err();
        assert!(matches!(
            error,
            ToolError::InvalidInput {
                error_code: Some(edit::MUST_READ_BEFORE_EDIT_CODE),
                ..
            }
        ));
        assert_eq!(std::fs::read_to_string(file).unwrap(), "first");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn different_file_paths_remain_parallel() {
        let dir = tempfile::tempdir().unwrap();
        let first_file = dir.path().join("first.txt");
        let second_file = dir.path().join("second.txt");
        std::fs::write(&first_file, "first-before").unwrap();
        std::fs::write(&second_file, "second-before").unwrap();

        let first_entered = Arc::new(tokio::sync::Notify::new());
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_context = observed_context(
            &first_file,
            "first-before",
            FileStateCache::new(),
            Arc::new(BlockingHistoryTracker {
                entered: first_entered.clone(),
                release: Mutex::new(release_rx),
            }),
        );
        let second_entered = Arc::new(tokio::sync::Notify::new());
        let second_context = observed_context(
            &second_file,
            "second-before",
            FileStateCache::new(),
            Arc::new(NotifyingHistoryTracker {
                entered: second_entered.clone(),
            }),
        );

        let first_path = first_file.clone();
        let first = tokio::spawn(async move {
            WriteTool
                .call(
                    serde_json::json!({ "file_path": first_path, "content": "first-after" }),
                    &first_context,
                )
                .await
        });
        first_entered.notified().await;

        let second_path = second_file.clone();
        let second = tokio::spawn(async move {
            WriteTool
                .call(
                    serde_json::json!({ "file_path": second_path, "content": "second-after" }),
                    &second_context,
                )
                .await
        });
        let ran_in_parallel =
            tokio::time::timeout(std::time::Duration::from_secs(1), second_entered.notified())
                .await
                .is_ok();

        release_tx.send(()).unwrap();
        first.await.unwrap().expect("first path write");
        second.await.unwrap().expect("second path write");
        assert!(
            ran_in_parallel,
            "a writer for a different path should not wait for the first path lock"
        );
        assert_eq!(std::fs::read_to_string(first_file).unwrap(), "first-after");
        assert_eq!(
            std::fs::read_to_string(second_file).unwrap(),
            "second-after"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn read_waits_for_same_path_write_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("read-after-write.txt");
        std::fs::write(&file, "before").unwrap();

        let writer_entered = Arc::new(tokio::sync::Notify::new());
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer_context = observed_context(
            &file,
            "before",
            FileStateCache::new(),
            Arc::new(BlockingHistoryTracker {
                entered: writer_entered.clone(),
                release: Mutex::new(release_rx),
            }),
        );
        let writer_path = file.clone();
        let writer = tokio::spawn(async move {
            WriteTool
                .call(
                    serde_json::json!({ "file_path": writer_path, "content": "after" }),
                    &writer_context,
                )
                .await
        });
        writer_entered.notified().await;

        let reader_path = file.clone();
        let mut reader = tokio::spawn(async move {
            ReadTool
                .call(
                    serde_json::json!({ "file_path": reader_path }),
                    &ToolContext::new().with_file_state_cache(FileStateCache::new()),
                )
                .await
        });
        let completed_while_locked = tokio::select! {
            result = &mut reader => Some(result),
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => None,
        };
        assert!(
            completed_while_locked.is_none(),
            "Read should wait while the same-path write transaction is in progress"
        );

        release_tx.send(()).unwrap();
        writer.await.unwrap().expect("writer");
        let read = reader.await.unwrap().expect("reader");
        assert!(read["file"]["content"]
            .as_str()
            .expect("text content")
            .contains("after"));
    }

    #[tokio::test]
    async fn echo_tool_returns_input_unchanged() {
        let tool = EchoTool;
        let input = serde_json::json!({ "hello": "world" });
        let out = tool.call(input.clone(), &ToolContext::new()).await.unwrap();
        assert_eq!(out, input);
        assert_eq!(tool.id().as_str(), "Echo");
    }

    #[tokio::test]
    async fn default_validation_allows_input() {
        let tool = EchoTool;
        let input = serde_json::json!({ "hello": "world" });
        let out = tool
            .validate_input(&input, &ToolContext::new())
            .await
            .unwrap();
        assert_eq!(out, ValidationOutcome::valid());
    }

    #[tokio::test]
    async fn default_permission_check_allows_and_preserves_input() {
        let tool = EchoTool;
        let input = serde_json::json!({ "path": "src/lib.rs" });
        let out = tool
            .check_permissions(&input, &ToolContext::new())
            .await
            .unwrap();
        assert_eq!(out, PermissionDecision::allow(input));
    }

    #[tokio::test]
    async fn tool_context_progress_channel_receives_updates() {
        let tool = EchoTool;
        let input = serde_json::json!({ "hello": "world" });
        let (context, mut rx) = ToolContext::new().with_progress("tool-use-1");

        let out = tool.call(input.clone(), &context).await.unwrap();
        let update = rx.recv().await.expect("progress update should be sent");

        assert_eq!(out, input);
        assert_eq!(update.kind, "echo");
        assert_eq!(update.message.as_deref(), Some("echo:tool-use-1"));
        assert_eq!(
            update.payload,
            Some(serde_json::json!({ "hello": "world" }))
        );
    }

    #[tokio::test]
    async fn auto_approve_permission_broker_runs_ask_decision() {
        let tool = EchoTool;
        let input = serde_json::json!({ "hello": "world" });
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Review workflow demo", "Overview\n- Name: demo"),
            Some(input.clone()),
        );

        let output = AutoApprovePermissionBroker
            .resolve(&tool, input.clone(), &ToolContext::new(), decision)
            .await
            .expect("auto approved result");

        assert_eq!(output, input);
    }

    #[tokio::test]
    async fn auto_approve_permission_broker_does_not_authorize_external_agent_paths() {
        struct ContextTool;

        #[async_trait]
        impl Tool for ContextTool {
            fn id(&self) -> ToolId {
                ToolId::new(AGENT_TOOL_NAME)
            }

            fn description(&self) -> &str {
                "context tool"
            }

            fn input_schema(&self) -> ToolInputSchema {
                serde_json::json!({"type":"object"})
            }

            async fn call(&self, _input: Value, context: &ToolContext) -> ToolResult<Value> {
                Ok(serde_json::json!(context.additional_working_directories()))
            }
        }

        let decision = PermissionDecision::ask(
            PermissionRequest::new("Authorize path", "Authorize F:/other").with_metadata(
                serde_json::json!({
                    "kind": AGENT_EXTERNAL_PATH_AUTHORIZATION_KIND,
                    "authorized_path_roots": ["F:/other"]
                }),
            ),
            Some(serde_json::json!({})),
        );
        let context = ToolContext::new().with_additional_working_directories(["F:/existing"]);

        let output = AutoApprovePermissionBroker
            .resolve(&ContextTool, serde_json::json!({}), &context, decision)
            .await
            .expect("auto approved result");

        assert_eq!(output, serde_json::json!(["F:/existing"]));
        assert_eq!(context.additional_working_directories(), ["F:/existing"]);
    }

    struct ShellEchoTool {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for ShellEchoTool {
        fn id(&self) -> ToolId {
            ToolId::new("Bash")
        }

        fn description(&self) -> &str {
            "shell echo tool"
        }

        fn input_schema(&self) -> ToolInputSchema {
            serde_json::json!({"type":"object"})
        }

        async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(input)
        }
    }

    struct PowerShellEchoTool {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for PowerShellEchoTool {
        fn id(&self) -> ToolId {
            ToolId::new("PowerShell")
        }

        fn description(&self) -> &str {
            "PowerShell echo tool"
        }

        fn input_schema(&self) -> ToolInputSchema {
            serde_json::json!({"type":"object"})
        }

        async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(input)
        }
    }

    struct CountingDenyBroker {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl PermissionBroker for CountingDenyBroker {
        async fn resolve(
            &self,
            tool: &dyn Tool,
            _input: Value,
            _context: &ToolContext,
            _decision: PermissionDecision,
        ) -> ToolResult<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(ToolError::PermissionDenied {
                tool: tool.id(),
                reason: "delegated".into(),
            })
        }
    }

    struct WorkflowApprovalDenyBroker {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl PermissionBroker for WorkflowApprovalDenyBroker {
        async fn resolve(
            &self,
            tool: &dyn Tool,
            _input: Value,
            _context: &ToolContext,
            decision: PermissionDecision,
        ) -> ToolResult<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(decision.behavior, PermissionBehavior::Ask);
            let request = decision.request.expect("workflow approval request");
            assert_eq!(request.title, "Approve shared-worktree Git operation");
            // `allow_always` must stay out: the prompt scopes approval to
            // this exact operation, and a persisted rule would not.
            assert_eq!(request.options, ["allow_once", "reject_once"]);
            assert_eq!(
                request
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("kind"))
                    .and_then(Value::as_str),
                Some("workflow_sensitive_git")
            );
            Err(ToolError::PermissionDenied {
                tool: tool.id(),
                reason: "delegated".into(),
            })
        }
    }

    #[tokio::test]
    async fn auto_approve_except_sensitive_delegates_git_stash_pop() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({ "command": "git stash pop" });
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Run shell command", "Bash wants to run: git stash pop"),
            Some(input.clone()),
        );
        let broker =
            AutoApproveExceptSensitivePermissionBroker::new(Arc::new(CountingDenyBroker {
                calls: delegate_calls.clone(),
            }));

        let result = broker
            .resolve(&tool, input, &ToolContext::new(), decision)
            .await;

        assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn auto_approve_except_sensitive_delegates_git_checkout() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({ "command": "git checkout -- src/lib.rs" });
        let decision = PermissionDecision::ask(
            PermissionRequest::new(
                "Run shell command",
                "Bash wants to run: git checkout -- src/lib.rs",
            ),
            Some(input.clone()),
        );
        let broker =
            AutoApproveExceptSensitivePermissionBroker::new(Arc::new(CountingDenyBroker {
                calls: delegate_calls.clone(),
            }));

        let result = broker
            .resolve(&tool, input, &ToolContext::new(), decision)
            .await;

        assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn workflow_sensitive_git_overrides_preapproved_shell_decision() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({ "command": "git reset --hard HEAD~1" });
        let broker =
            AutoApproveExceptSensitivePermissionBroker::new(Arc::new(WorkflowApprovalDenyBroker {
                calls: delegate_calls.clone(),
            }));
        let context = ToolContext::new().with_workflow_nesting_depth(1);

        let result = broker
            .resolve(
                &tool,
                input.clone(),
                &context,
                PermissionDecision::allow(input),
            )
            .await;

        assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn workflow_sensitive_git_is_exempt_inside_isolated_worktree() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({ "command": "git reset --hard HEAD~1" });
        let broker =
            AutoApproveExceptSensitivePermissionBroker::new(Arc::new(WorkflowApprovalDenyBroker {
                calls: delegate_calls.clone(),
            }));
        let context = ToolContext::new()
            .with_workflow_nesting_depth(1)
            .with_cwd(r"F:\repo\.rebon\worktrees\agent-x")
            .with_isolated_worktree(true);

        let result = broker
            .resolve(
                &tool,
                input.clone(),
                &context,
                PermissionDecision::allow(input),
            )
            .await;

        assert!(result.is_ok(), "isolated worktree must skip the git gate");
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 0);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    }

    /// A parent agent controls the `cwd` it hands its children, so a
    /// directory merely *shaped* like a runtime worktree must not buy the
    /// exemption — only the runtime's own creation flag does.
    #[tokio::test]
    async fn workflow_sensitive_git_gate_ignores_spoofed_worktree_cwd() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({ "command": "git reset --hard HEAD~1" });
        let broker =
            AutoApproveExceptSensitivePermissionBroker::new(Arc::new(WorkflowApprovalDenyBroker {
                calls: delegate_calls.clone(),
            }));
        let context = ToolContext::new()
            .with_workflow_nesting_depth(1)
            .with_cwd(r"F:\repo\.rebon\worktrees\agent-x");

        let result = broker
            .resolve(
                &tool,
                input.clone(),
                &context,
                PermissionDecision::allow(input),
            )
            .await;

        assert!(
            matches!(result, Err(ToolError::PermissionDenied { .. })),
            "a worktree-shaped cwd without runtime isolation must stay gated"
        );
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn workflow_sensitive_git_is_denied_when_permission_prompts_are_unavailable() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({ "command": "git.exe reset --hard HEAD~1" });
        let broker =
            AutoApproveExceptSensitivePermissionBroker::new(Arc::new(WorkflowApprovalDenyBroker {
                calls: delegate_calls.clone(),
            }));
        let context = ToolContext::new()
            .with_workflow_nesting_depth(1)
            .with_permission_prompts_unavailable(true);

        let result = broker
            .resolve(
                &tool,
                input.clone(),
                &context,
                PermissionDecision::allow(input),
            )
            .await;

        let Err(ToolError::PermissionDenied { reason, .. }) = result else {
            panic!("non-interactive workflow Git risk must be denied");
        };
        assert!(reason.contains("background workflow agent"));
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 0);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn workflow_sensitive_git_does_not_change_depth_zero_permissions() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({ "command": "git reset --hard HEAD~1" });
        let broker =
            AutoApproveExceptSensitivePermissionBroker::new(Arc::new(CountingDenyBroker {
                calls: delegate_calls.clone(),
            }));

        let output = broker
            .resolve(
                &tool,
                input.clone(),
                &ToolContext::new(),
                PermissionDecision::allow(input.clone()),
            )
            .await
            .expect("ordinary preapproved command should retain existing behavior");

        assert_eq!(output, input);
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 0);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn auto_approve_except_sensitive_delegates_file_deletion() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({ "command": "rm old.log" });
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Run shell command", "Bash wants to run: rm old.log"),
            Some(input.clone()),
        );
        let broker =
            AutoApproveExceptSensitivePermissionBroker::new(Arc::new(CountingDenyBroker {
                calls: delegate_calls.clone(),
            }));

        let result = broker
            .resolve(&tool, input, &ToolContext::new(), decision)
            .await;

        assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn auto_approve_except_sensitive_delegates_embedded_script_without_classifier() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({
            "command": "python - <<'PY'\nprint(open('package.json').read())\nPY"
        });
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Run shell command", "Bash wants to inspect package.json"),
            Some(input.clone()),
        );
        let broker =
            AutoApproveExceptSensitivePermissionBroker::new(Arc::new(CountingDenyBroker {
                calls: delegate_calls.clone(),
            }));

        let result = broker
            .resolve(&tool, input, &ToolContext::new(), decision)
            .await;

        assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn auto_approve_except_sensitive_denies_embedded_script_when_backgrounded() {
        for command in [
            "PYTHON=python\n$PYTHON - <<'PY'\nfrom pathlib import Path\nPath('x').write_text('data')\nPY",
            ":\np\\ython - <<'PY'\nfrom pathlib import Path\nPath('x').write_text('data')\nPY",
            "# << decoy\nPYTHON=python\n$PYTHON - <<'PY'\nfrom pathlib import Path\nPath('x').write_text('data')\nPY",
            "bash <<'SH'\ntouch x\nSH",
            "cat <<'EOF' > $OUT\ndata\nEOF",
        ] {
            let tool_calls = Arc::new(AtomicUsize::new(0));
            let delegate_calls = Arc::new(AtomicUsize::new(0));
            let tool = ShellEchoTool {
                calls: tool_calls.clone(),
            };
            let input = serde_json::json!({ "command": command });
            let decision = PermissionDecision::ask(
                PermissionRequest::new("Run shell command", "Bash wants to inspect package.json"),
                Some(input.clone()),
            );
            let broker = AutoApproveExceptSensitivePermissionBroker::with_background_probe(
                Arc::new(CountingDenyBroker {
                    calls: delegate_calls.clone(),
                }),
                Arc::new(|| true),
            );

            let result = broker
                .resolve(&tool, input, &ToolContext::new(), decision)
                .await;

            let Err(ToolError::PermissionDenied { reason, .. }) = result else {
                panic!("backgrounded embedded script must be denied");
            };
            assert!(reason.contains("background agent"));
            assert_eq!(delegate_calls.load(Ordering::SeqCst), 0);
            assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn auto_approve_except_sensitive_denies_sensitive_when_backgrounded() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({ "command": "rm /tmp/scratch.sh" });
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Run shell command", "Bash wants to run: rm /tmp/scratch.sh"),
            Some(input.clone()),
        );
        let broker = AutoApproveExceptSensitivePermissionBroker::with_background_probe(
            Arc::new(CountingDenyBroker {
                calls: delegate_calls.clone(),
            }),
            Arc::new(|| true),
        );

        let result = broker
            .resolve(&tool, input, &ToolContext::new(), decision)
            .await;

        let Err(ToolError::PermissionDenied { reason, .. }) = result else {
            panic!("backgrounded sensitive command must be denied");
        };
        assert!(reason.contains("background agent"));
        // The interactive delegate must never be reached — it blocks
        // on a user prompt nobody is watching.
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 0);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    }

    fn test_scratchpad_root() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Users\u\AppData\Local\Temp\rebon\proj\sess\scratchpad")
        } else {
            PathBuf::from("/tmp/rebon/proj/sess/scratchpad")
        }
    }

    #[tokio::test]
    async fn auto_approve_except_sensitive_exempts_scratchpad_deletion_when_backgrounded() {
        let root = test_scratchpad_root();
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({
            "command": format!("rm -rf {}/pkg-bundle", root.display())
        });
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Run shell command", "Bash wants to clean the scratchpad"),
            Some(input.clone()),
        );
        let broker = AutoApproveExceptSensitivePermissionBroker::with_background_probe(
            Arc::new(CountingDenyBroker {
                calls: delegate_calls.clone(),
            }),
            Arc::new(|| true),
        )
        .with_exempt_deletion_root(Some(root));

        broker
            .resolve(&tool, input, &ToolContext::new(), decision)
            .await
            .expect("scratchpad-confined deletion should auto-approve even in background");

        assert_eq!(delegate_calls.load(Ordering::SeqCst), 0);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn auto_approve_except_sensitive_exempts_guarded_scratchpad_cleanup() {
        let root = test_scratchpad_root();
        let destination = root.join("verifycopy");
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = PowerShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({
            "command": format!(
                "$dest = '{}'; if (Test-Path $dest) {{ Remove-Item -Recurse -Force $dest }}; Copy-Item project $dest",
                destination.display()
            )
        });
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Run PowerShell command", "prepare verification copy"),
            Some(input.clone()),
        );
        let broker = AutoApproveExceptSensitivePermissionBroker::with_background_probe(
            Arc::new(CountingDenyBroker {
                calls: delegate_calls.clone(),
            }),
            Arc::new(|| true),
        )
        .with_exempt_deletion_root(Some(root));

        broker
            .resolve(&tool, input, &ToolContext::new(), decision)
            .await
            .expect("scratchpad cleanup and recreation should stay prompt-free");

        assert_eq!(delegate_calls.load(Ordering::SeqCst), 0);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn auto_approve_except_sensitive_exempt_root_keeps_other_deletions_sensitive() {
        let root = test_scratchpad_root();
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({ "command": "rm -rf ./src" });
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Run shell command", "Bash wants to run: rm -rf ./src"),
            Some(input.clone()),
        );
        let broker = AutoApproveExceptSensitivePermissionBroker::with_background_probe(
            Arc::new(CountingDenyBroker {
                calls: delegate_calls.clone(),
            }),
            Arc::new(|| true),
        )
        .with_exempt_deletion_root(Some(root));

        let result = broker
            .resolve(&tool, input, &ToolContext::new(), decision)
            .await;

        assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 0);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn backgrounded_deletion_floats_to_interactive_prompt_when_enabled() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({ "command": "rm -rf ./stray-export-dir" });
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Run shell command", "Bash wants to delete a stray dir"),
            Some(input.clone()),
        );
        let broker = AutoApproveExceptSensitivePermissionBroker::with_background_probe(
            Arc::new(CountingDenyBroker {
                calls: delegate_calls.clone(),
            }),
            Arc::new(|| true),
        )
        .with_background_deletion_ask(true);

        let result = broker
            .resolve(&tool, input, &ToolContext::new(), decision)
            .await;

        // The interactive delegate WAS reached — the ask floated to
        // the frontend instead of being auto-denied. (The counting
        // delegate answers deny, hence the error result.)
        assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn backgrounded_non_deletion_sensitive_stays_denied_despite_deletion_ask() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({ "command": "git stash drop" });
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Run shell command", "Bash wants to run: git stash drop"),
            Some(input.clone()),
        );
        let broker = AutoApproveExceptSensitivePermissionBroker::with_background_probe(
            Arc::new(CountingDenyBroker {
                calls: delegate_calls.clone(),
            }),
            Arc::new(|| true),
        )
        .with_background_deletion_ask(true);

        let result = broker
            .resolve(&tool, input, &ToolContext::new(), decision)
            .await;

        let Err(ToolError::PermissionDenied { reason, .. }) = result else {
            panic!("backgrounded non-deletion sensitive command must stay denied");
        };
        assert!(reason.contains("background agent"));
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 0);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_compound_command_cannot_ride_its_deletion_into_the_float() {
        // `rm -rf x && git stash drop` contains a deletion, but also a
        // class a background worker must hard-deny. The deletion must
        // not chaperone the stash drop into an interactive prompt.
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({ "command": "rm -rf ./stray-export-dir && git stash drop" });
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Run shell command", "Bash wants to clean up"),
            Some(input.clone()),
        );
        let broker = AutoApproveExceptSensitivePermissionBroker::with_background_probe(
            Arc::new(CountingDenyBroker {
                calls: delegate_calls.clone(),
            }),
            Arc::new(|| true),
        )
        .with_background_deletion_ask(true);

        let result = broker
            .resolve(&tool, input, &ToolContext::new(), decision)
            .await;

        let Err(ToolError::PermissionDenied { reason, .. }) = result else {
            panic!("a deletion mixed with another sensitive class must stay denied");
        };
        assert!(reason.contains("background agent"));
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 0);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn auto_approve_except_sensitive_delegates_when_probe_is_foreground() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({ "command": "rm old.log" });
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Run shell command", "Bash wants to run: rm old.log"),
            Some(input.clone()),
        );
        let broker = AutoApproveExceptSensitivePermissionBroker::with_background_probe(
            Arc::new(CountingDenyBroker {
                calls: delegate_calls.clone(),
            }),
            Arc::new(|| false),
        );

        let result = broker
            .resolve(&tool, input, &ToolContext::new(), decision)
            .await;

        assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn auto_approve_except_sensitive_backgrounded_still_allows_non_sensitive() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({ "command": "cargo test --workspace" });
        let decision = PermissionDecision::ask(
            PermissionRequest::new(
                "Run shell command",
                "Bash wants to run: cargo test --workspace",
            ),
            Some(input.clone()),
        );
        let broker = AutoApproveExceptSensitivePermissionBroker::with_background_probe(
            Arc::new(CountingDenyBroker {
                calls: delegate_calls.clone(),
            }),
            Arc::new(|| true),
        );

        let output = broker
            .resolve(&tool, input.clone(), &ToolContext::new(), decision)
            .await
            .expect("non-sensitive command should auto-approve even in background");

        assert_eq!(output, input);
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 0);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn auto_approve_except_sensitive_allows_non_stash_commands() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let delegate_calls = Arc::new(AtomicUsize::new(0));
        let tool = ShellEchoTool {
            calls: tool_calls.clone(),
        };
        let input = serde_json::json!({ "command": "git status" });
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Run shell command", "Bash wants to run: git status"),
            Some(input.clone()),
        );
        let broker =
            AutoApproveExceptSensitivePermissionBroker::new(Arc::new(CountingDenyBroker {
                calls: delegate_calls.clone(),
            }));

        let output = broker
            .resolve(&tool, input.clone(), &ToolContext::new(), decision)
            .await
            .expect("non-stash command should auto-approve");

        assert_eq!(output, input);
        assert_eq!(delegate_calls.load(Ordering::SeqCst), 0);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    }

    struct CountingAskTool {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for CountingAskTool {
        fn id(&self) -> ToolId {
            ToolId::new("CountingAsk")
        }

        fn description(&self) -> &str {
            "counting ask tool"
        }

        fn input_schema(&self) -> ToolInputSchema {
            serde_json::json!({"type":"object"})
        }

        async fn check_permissions(
            &self,
            _input: &Value,
            _context: &ToolContext,
        ) -> ToolResult<PermissionDecision> {
            Ok(PermissionDecision::ask(
                PermissionRequest::new("Review", "Overview\n- Name: demo"),
                None,
            ))
        }

        async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(input)
        }
    }

    #[tokio::test]
    async fn deny_ask_permission_broker_rejects_without_calling_tool() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tool = CountingAskTool {
            calls: calls.clone(),
        };
        let input = serde_json::json!({ "hello": "world" });
        let decision = tool
            .check_permissions(&input, &ToolContext::new())
            .await
            .expect("permission decision");

        let result = DenyAskPermissionBroker
            .resolve(&tool, input, &ToolContext::new(), decision)
            .await;

        assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn echo_tool_defaults_match_placeholder_expectations() {
        let tool = EchoTool;
        assert!(tool.is_enabled());
        assert!(!tool.is_concurrency_safe(&serde_json::json!({})));
        assert!(tool.is_read_only(&serde_json::json!({})));
        assert!(!tool.is_destructive(&serde_json::json!({})));
        assert!(!tool.needs_permission(&serde_json::json!({})));
        assert_eq!(tool.aliases(), &["EchoTool"]);
    }

    #[test]
    fn tool_context_helpers_preserve_execution_policy() {
        let mut policy = ExecutionPolicy::default();
        policy.eager_promotions.insert("task-1".into());
        let context = ToolContext::new().with_execution_policy(policy.clone());

        let with_id = context.with_tool_use_id("tool-use-1");
        assert_eq!(with_id.execution_policy(), Some(&policy));

        let (with_progress, _rx) = context.with_progress("tool-use-2");
        assert_eq!(with_progress.execution_policy(), Some(&policy));
    }

    #[test]
    fn tool_context_helpers_preserve_escalation_handles() {
        let registry = EscalationRegistry::new();
        let context = ToolContext::new()
            .with_worker_escalation_client(registry.worker_client("agent-1", Some("desc".into())))
            .with_escalation_resolver(registry.resolver());

        let with_id = context.with_tool_use_id("tool-use-1");
        assert!(with_id.worker_escalation_client().is_some());
        assert!(with_id.escalation_resolver().is_some());

        let (with_progress, _rx) = context.with_progress("tool-use-2");
        assert!(with_progress.worker_escalation_client().is_some());
        assert!(with_progress.escalation_resolver().is_some());
    }

    struct StubWebProvider;

    #[async_trait]
    impl WebSearchDelegate for StubWebProvider {
        async fn web_search(&self, _input: Value) -> ToolResult<Value> {
            Ok(Value::Null)
        }
    }

    #[async_trait]
    impl WebProviderRouter for StubWebProvider {
        async fn search(&self, _input: &Value) -> ToolResult<Option<Value>> {
            Ok(None)
        }

        async fn fetch(&self, _input: &Value) -> ToolResult<Option<Value>> {
            Ok(None)
        }
    }

    struct StubQueueController;

    #[async_trait]
    impl QueueController for StubQueueController {
        async fn queue_plan(&self, _session_id: &str) -> Result<Option<Value>, String> {
            Ok(None)
        }

        async fn submit_verdict(
            &self,
            _verdict: QueueVerdict<'_>,
        ) -> Result<QueueVerdictOutcome, String> {
            Err("stub".to_string())
        }

        async fn dispatch_row(
            &self,
            _session_id: &str,
            _row_id: &str,
            _worktree: &str,
        ) -> Result<QueueVerdictOutcome, String> {
            Err("stub".to_string())
        }

        async fn block_row(
            &self,
            _session_id: &str,
            _row_id: &str,
            _reason: &str,
        ) -> Result<QueueVerdictOutcome, String> {
            Err("stub".to_string())
        }
    }

    /// The cap on direct fields on [`ToolContext`].
    ///
    /// The destructuring is exhaustive on purpose. A new direct field stops
    /// compiling here until someone bumps `DIRECT_FIELDS` and names the field
    /// in `NAMES` — feature state belongs in the `Extensions` bag instead,
    /// and only the core runtime fields earn a direct slot.
    #[test]
    fn tool_context_direct_field_budget() {
        const NAMES: [&str; ToolContext::DIRECT_FIELDS] = [
            "tool_use_id",
            "session_id",
            "progress",
            "permission_broker",
            "permission_mode_provider",
            "agent_id",
            "cwd",
            "is_isolated_worktree",
            "additional_working_directories",
            "path_scope_roots",
            "write_scope_roots",
            "auto_approved_write_roots",
            "command_sandbox",
            "plugin_tools",
            "tool_resolver",
            "tool_filter",
            "file_state_cache",
            "file_mutation_batch",
            "file_history_tracker",
            "shell_process_registry",
            "execution_policy",
            "denial_replay_reason",
            "permission_prompts_unavailable",
            "extensions",
        ];

        let ToolContext {
            tool_use_id: _,
            session_id: _,
            progress: _,
            permission_broker: _,
            permission_mode_provider: _,
            agent_id: _,
            cwd: _,
            is_isolated_worktree: _,
            additional_working_directories: _,
            path_scope_roots: _,
            write_scope_roots: _,
            auto_approved_write_roots: _,
            command_sandbox: _,
            plugin_tools: _,
            tool_resolver: _,
            tool_filter: _,
            file_state_cache: _,
            file_mutation_batch: _,
            file_history_tracker: _,
            shell_process_registry: _,
            execution_policy: _,
            denial_replay_reason: _,
            permission_prompts_unavailable: _,
            extensions: _,
        } = ToolContext::new();

        assert_eq!(NAMES.len(), ToolContext::DIRECT_FIELDS);
        assert!(
            ToolContext::DIRECT_FIELDS <= 25,
            "ToolContext direct fields must stay at or below 25; move feature state into Extensions"
        );
        assert_eq!(ToolContext::new().extension_count(), 0);
    }

    #[test]
    fn auto_mode_extension_round_trips() {
        assert!(ToolContext::new()
            .auto_mode_classifier_transcript()
            .is_none());

        let context = ToolContext::new().with_auto_mode_classifier_transcript("recent turns");
        assert_eq!(
            context.auto_mode_classifier_transcript(),
            Some("recent turns")
        );
        assert_eq!(
            context
                .with_tool_use_id("tool-use-1")
                .auto_mode_classifier_transcript(),
            Some("recent turns")
        );
    }

    #[test]
    fn sub_agent_extension_round_trips() {
        let empty = ToolContext::new();
        assert!(empty.sub_agent_spawner().is_none());
        assert!(empty.frozen_parent_context().is_none());
        assert!(!empty.parallel_agent_write_batch());

        let context = empty
            .with_parallel_agent_write_batch(true)
            .with_frozen_parent_context(FrozenParentContextCapsule {
                text: Arc::from("parent transcript"),
                hash: "hash".to_string(),
                token_estimate: 7,
            });

        assert!(context.parallel_agent_write_batch());
        assert_eq!(
            context
                .frozen_parent_context()
                .map(|capsule| capsule.token_estimate),
            Some(7)
        );
        assert!(context
            .with_tool_use_id("tool-use-1")
            .parallel_agent_write_batch());
    }

    #[test]
    fn mcp_extension_round_trips() {
        let empty = ToolContext::new();
        assert!(empty.mcp_client().is_none());
        assert!(empty.mcp_tool_definitions().is_none());

        let context = empty.with_mcp_tool_definitions(Arc::new(Vec::new()));
        assert_eq!(
            context.mcp_tool_definitions().map(|defs| defs.len()),
            Some(0)
        );
        assert!(context
            .with_tool_use_id("tool-use-1")
            .mcp_tool_definitions()
            .is_some());
    }

    #[test]
    fn web_extension_round_trips() {
        let empty = ToolContext::new();
        assert!(empty.web_search_delegate().is_none());
        assert!(empty.web_provider_router().is_none());

        let context = empty
            .with_web_search_delegate(Arc::new(StubWebProvider))
            .with_web_provider_router(Arc::new(StubWebProvider));

        assert!(context.web_search_delegate().is_some());
        assert!(context.web_provider_router().is_some());
        assert!(context
            .with_tool_use_id("tool-use-1")
            .web_provider_router()
            .is_some());
    }

    #[test]
    fn workflow_extension_round_trips() {
        let empty = ToolContext::new();
        assert!(empty.workflow_launcher().is_none());
        assert_eq!(empty.workflow_nesting_depth(), 0);

        let context = empty.with_workflow_nesting_depth(3);
        assert_eq!(context.workflow_nesting_depth(), 3);
        assert_eq!(
            context
                .with_tool_use_id("tool-use-1")
                .workflow_nesting_depth(),
            3
        );
    }

    #[test]
    fn team_extension_round_trips() {
        let empty = ToolContext::new();
        assert!(empty.team_manager().is_none());
        assert!(empty.team_identity().is_none());

        let context = empty.with_team_identity(TeamIdentityContext {
            agent_id: "agent-1".to_string(),
            agent_name: "Agent".to_string(),
            team_name: "team-1".to_string(),
            permission_mode: Some("auto".to_string()),
        });

        assert_eq!(
            context
                .team_identity()
                .map(|identity| identity.team_name.as_str()),
            Some("team-1")
        );
        assert_eq!(context.permission_mode().as_deref(), Some("auto"));
        assert_eq!(context.agent_id(), Some("agent-1"));
    }

    #[test]
    fn task_extension_round_trips() {
        let empty = ToolContext::new();
        assert!(empty.extension::<TaskContext>().is_none());
        assert!(empty.task_runtime_controller().is_none());

        let context = empty.with_task_list_id("list-1");
        assert_eq!(context.task_list_id(), "list-1");
        assert_eq!(
            context.with_tool_use_id("tool-use-1").task_list_id(),
            "list-1"
        );
    }

    #[test]
    fn queue_extension_round_trips() {
        assert!(ToolContext::new().queue_controller().is_none());

        let context = ToolContext::new().with_queue_controller(Arc::new(StubQueueController));
        assert!(context.queue_controller().is_some());
        assert!(context
            .with_tool_use_id("tool-use-1")
            .queue_controller()
            .is_some());
    }

    #[test]
    fn tool_search_extension_round_trips() {
        let empty = ToolContext::new();
        assert!(empty.tool_search_index().is_none());
        assert!(empty.discovered_deferred_tools().is_none());

        let context =
            empty.with_tool_search_index(Arc::new(tool_search::ToolSearchIndex::build(&[])));
        assert!(context.tool_search_index().is_some());
        assert!(context.discovered_deferred_tools().is_some());

        context.record_discovered_deferred_tool("Monitor");
        assert!(context.is_deferred_tool_discovered("Monitor"));
        assert_eq!(
            context.discovered_deferred_tool_names(),
            vec!["Monitor".to_string()]
        );

        let cleared = context.without_tool_search_index();
        assert!(cleared.tool_search_index().is_none());
        assert!(cleared.discovered_deferred_tools().is_none());
    }

    #[test]
    fn escalation_extension_defaults_to_absent() {
        let empty = ToolContext::new();
        assert!(empty.worker_escalation_client().is_none());
        assert!(empty.escalation_resolver().is_none());

        let registry = EscalationRegistry::new();
        let context = empty.with_escalation_resolver(registry.resolver());
        assert!(context.escalation_resolver().is_some());
        assert!(context.worker_escalation_client().is_none());
    }

    #[test]
    fn cron_extension_round_trips() {
        assert!(ToolContext::new().session_cron_store().is_none());

        let context = ToolContext::new().with_session_cron_store(SessionCronStore::new());
        assert!(context.session_cron_store().is_some());
        assert!(context
            .with_tool_use_id("tool-use-1")
            .session_cron_store()
            .is_some());
    }

    #[test]
    fn monitor_extension_round_trips() {
        assert!(ToolContext::new().monitor_registry().is_none());

        let first = Arc::new(MonitorRegistry::new());
        let context = ToolContext::new().with_monitor_registry(first.clone());
        assert!(context.monitor_registry().is_some());

        let context = context.with_monitor_registry_if_absent(Arc::new(MonitorRegistry::new()));
        assert!(Arc::ptr_eq(context.monitor_registry().unwrap(), &first));
    }

    #[test]
    fn ultraplan_extension_round_trips() {
        let empty = ToolContext::new();
        assert!(empty.ultraplan_run_repository().is_none());
        assert!(empty.ultraplan_run_handle().is_none());
        assert!(empty.capability_context().is_none());

        let handle = Arc::new(Mutex::new(UltraplanRunState::new(
            "run".into(),
            "session".into(),
            "task".into(),
            None,
            1,
        )));
        let context = empty
            .with_ultraplan_run_handle(handle.clone())
            .with_capability_context(CapabilityContext {
                run_id: "run".to_string(),
                ..CapabilityContext::default()
            });

        assert!(Arc::ptr_eq(
            context.ultraplan_run_handle().unwrap(),
            &handle
        ));
        assert_eq!(
            context
                .capability_context()
                .map(|capability| capability.run_id.as_str()),
            Some("run")
        );
        assert!(matches!(context.load_ultraplan_run_state(), Ok(Some(_))));
    }

    #[test]
    fn plan_mode_extension_round_trips() {
        let context = ToolContext::new();
        assert!(!context.exit_plan_mode_approved());

        let approved = context.with_exit_plan_mode_approval();
        assert!(approved.exit_plan_mode_approved());
        assert!(approved
            .with_tool_use_id("tool-use-1")
            .exit_plan_mode_approved());
        assert!(!context.exit_plan_mode_approved());
    }

    #[test]
    fn structured_output_extension_round_trips() {
        assert!(ToolContext::new().structured_output_channel().is_none());

        let context = ToolContext::new()
            .with_structured_output_channel(Arc::new(StructuredOutputChannel::new(None)));
        assert!(context.structured_output_channel().is_some());
        assert!(context
            .with_tool_use_id("tool-use-1")
            .structured_output_channel()
            .is_some());
    }

    #[test]
    fn coordinator_extension_round_trips() {
        let empty = ToolContext::new();
        assert!(!empty.coordinator_mode());
        assert!(empty.coordinator_report_paths().is_empty());

        let context = empty
            .with_coordinator_mode(true)
            .with_coordinator_report_paths(["first.report.md"]);

        assert!(context.coordinator_mode());
        assert_eq!(context.coordinator_report_paths().len(), 1);
        assert!(context.with_tool_use_id("tool-use-1").coordinator_mode());
    }
}
