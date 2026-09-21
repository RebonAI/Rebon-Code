//! The sub-agent domain: the contract between whoever asks for a sub-agent
//! and whoever runs one.
//!
//! The `Agent` tool itself lives above this module. What lives here is what
//! the tool is not the only reader of:
//!
//! 1. The [`SubAgentSpawner`] trait, so a caller can reach a real runner
//!    without the tool's owner having to know the runner. The host (or a
//!    test harness) injects an implementation into [`ToolContext`] before
//!    invoking the tool.
//! 2. [`SubAgentSpec`] and [`SubAgentResult`] — the request and the reply —
//!    with the option enums that shape them, built and read by the run loop
//!    and the front end alike.
//! 3. [`SubAgentProgressSender`], the channel a running child reports
//!    activity through.
//! 4. The process-wide switches the host sets from configuration: whether
//!    sub-agents are offered at all ([`sub_agents_enabled`]) and which
//!    [`AgentRegistry`] the tool describes ([`agent_registry_selection`]).

use async_trait::async_trait;
use rebon_tools_core::ToolProgressUpdate;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::agent_registry::AgentRegistry;

/// Sub-agent state that [`crate::ToolContext`] carries in its extension bag.
///
/// Storage only: every field is still read and written through the
/// unchanged `ToolContext` accessors (`sub_agent_spawner()`,
/// `with_parallel_agent_write_batch()`, …). Kept out of the context's
/// direct field list so the per-feature state stays self-contained.
#[derive(Clone, Default)]
pub struct SubAgentContext {
    pub spawner: Option<Arc<dyn SubAgentSpawner>>,
    /// True when the current model response contains multiple Agent calls
    /// that may mutate files.
    pub parallel_write_batch: bool,
    pub frozen_parent: Option<FrozenParentContextCapsule>,
}
use crate::{ExecutionPolicy, SharedCoordinatorMode, SharedToolFilter, ToolContext, ToolFilter};

/// Registered name of the agent tool.
pub const AGENT_TOOL_NAME: &str = "Agent";
pub const AGENT_EXTERNAL_PATH_AUTHORIZATION_KIND: &str = "agent_external_path_authorization";

/// Agent type used when the caller omits `subagent_type`.
pub const DEFAULT_SUB_AGENT_TYPE: &str = "general-purpose";
pub const WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT: &str = r#"SHARED WORKTREE SAFETY:
- This may be a shared working tree with uncommitted changes from the user or other agents. Treat pre-existing and concurrent changes as owned work, not as errors to clean up.
- Stay focused on the assigned task. Do not investigate, fix, format, stage, or commit unrelated changes. Scope Git status and diff inspection to task-relevant paths whenever possible.
- Use Read/Edit/Write/MultiEdit for file changes; do not bypass their conflict protection with shell commands or scripts. If a file reports modified-since-read, reread it and preserve the latest contents before retrying.
- If a file you must edit already contains other changes, reread its latest contents and make the smallest compatible edit. If you cannot preserve the existing work safely, report the conflict instead of cleaning the worktree.
- Never use git reset, checkout or switch, restore, clean, stash, revert, force push, or branch deletion to remove observed differences or recover from a suspected bad edit. Only perform such an operation when the user explicitly requested it and the current command receives explicit permission approval."#;

pub fn append_workflow_agent_shared_worktree_prompt(system: Option<String>) -> String {
    let system = system.unwrap_or_default();
    if system.contains(WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT) {
        return system;
    }
    if system.trim().is_empty() {
        WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT.to_string()
    } else {
        format!(
            "{}\n\n{}",
            system.trim_end(),
            WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT
        )
    }
}

/// Process-wide switch gating whether the `Agent` tool advertises
/// itself to the model. The Settings dialog ("Sub-agents" row) flips
/// this via [`set_sub_agents_enabled`] and the host persists the
/// value to `~/.rebon/config.json`.
///
/// `true` by default so sub-agent delegation is available out of the
/// box — users opt out via the settings toggle, not opt in. Tool-list
/// projection already filters on [`crate::Tool::is_enabled`], so
/// flipping this to `false` removes the Agent tool from both the API
/// tool list and
/// the system-prompt "Delegating to sub-agents" section on the next
/// turn without needing a restart.
static SUB_AGENTS_ENABLED: AtomicBool = AtomicBool::new(true);

/// Read the current sub-agent enablement flag.
///
/// Exposed so the engine and the ACP config-option list can keep
/// their views consistent with the persisted setting without having
/// to duplicate the storage.
pub fn sub_agents_enabled() -> bool {
    SUB_AGENTS_ENABLED.load(Ordering::Relaxed)
}

/// Set the process-wide sub-agent enablement flag.
///
/// Called at startup by the host wiring with the value read from
/// `~/.rebon/config.json`, and at runtime by the TUI settings dialog
/// when the user toggles the "Sub-agents" row.
pub fn set_sub_agents_enabled(enabled: bool) {
    SUB_AGENTS_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Which agent definitions the `Agent` tool describes, and whether the
/// worktree-isolated ones are among them.
///
/// The tool's description is the merged agent list rendered into prose, and
/// `Tool::description(&self) -> &str` has nowhere to take a `ToolContext`
/// from — the engine projects the tool list without one. So the registry
/// reaches the tool the same way [`sub_agents_enabled`] does: a process-wide
/// cell the host fills once at startup, right where it loads
/// `~/.rebon/agents/` + `<cwd>/.rebon/agents/`. The tool's owner reads
/// it when it builds the tool for the seat.
#[derive(Clone)]
pub struct AgentRegistrySelection {
    pub registry: Arc<AgentRegistry>,
    /// Whether this host runs coordinator workers in their own Git
    /// worktrees. Off, the worktree-isolated built-in agents are left out of
    /// the description and resolve back to `general-purpose`.
    pub coordinator_use_worktree: bool,
    /// Bumped on every [`set_agent_registry_selection`], so a reader that
    /// caches what it built from this selection can tell when to rebuild.
    pub generation: u64,
}

static AGENT_REGISTRY_SELECTION: RwLock<Option<AgentRegistrySelection>> = RwLock::new(None);
static AGENT_REGISTRY_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Point the `Agent` tool at `registry`.
///
/// Called by the front end's TUI wiring and its ACP server once each, with
/// merged registry they just loaded from disk. Unset, the tool falls back to
/// the compiled-in built-ins — which is what a bare `Engine` and every test
/// that never calls this see.
pub fn set_agent_registry_selection(registry: Arc<AgentRegistry>, coordinator_use_worktree: bool) {
    let generation = AGENT_REGISTRY_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    let mut slot = AGENT_REGISTRY_SELECTION
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(AgentRegistrySelection {
        registry,
        coordinator_use_worktree,
        generation,
    });
}

/// The current selection, or the built-ins-only default.
///
/// The default's `generation` is `0`, which no [`set_agent_registry_selection`]
/// ever produces, so a cache keyed on the generation still refreshes the
/// first time a host sets one.
pub fn agent_registry_selection() -> AgentRegistrySelection {
    if let Some(selection) = AGENT_REGISTRY_SELECTION
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return selection;
    }
    AgentRegistrySelection {
        registry: Arc::new(AgentRegistry::builtins_only()),
        coordinator_use_worktree: false,
        generation: 0,
    }
}

/// Deterministic task role used by coordinator runtime policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgentTaskKind {
    Research,
    Implementation,
    Verification,
    Other,
}

impl SubAgentTaskKind {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "implementation" | "implement" | "impl" | "coding" | "code" => {
                Some(Self::Implementation)
            }
            "research" | "explore" | "exploration" => Some(Self::Research),
            "verification" | "verify" | "review" | "qa" => Some(Self::Verification),
            "other" | "none" | "default" => Some(Self::Other),
            _ => None,
        }
    }

    pub fn from_metadata(metadata: &Value) -> Self {
        for key in [
            "coordinator_task_kind",
            "coordinator_role",
            "task_kind",
            "taskKind",
            "role",
        ] {
            if let Some(raw) = metadata.get(key).and_then(|v| v.as_str()) {
                if let Some(kind) = Self::parse(raw) {
                    return kind;
                }
            }
        }
        Self::Other
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Research => "research",
            Self::Implementation => "implementation",
            Self::Verification => "verification",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextShareMode {
    None,
    PlanOnly,
    Compact,
    CompactWithRecentTurns,
    TranscriptSlice,
}

impl ContextShareMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "none" | "off" | "false" => Some(Self::None),
            "plan_only" | "planonly" | "plan" => Some(Self::PlanOnly),
            "compact" => Some(Self::Compact),
            "compact_with_recent_turns" | "compactwithrecentturns" | "recent" => {
                Some(Self::CompactWithRecentTurns)
            }
            "transcript_slice" | "transcriptslice" | "transcript" => Some(Self::TranscriptSlice),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PlanOnly => "plan_only",
            Self::Compact => "compact",
            Self::CompactWithRecentTurns => "compact_with_recent_turns",
            Self::TranscriptSlice => "transcript_slice",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultMode {
    None,
    Facts,
    Summary,
}

impl ToolResultMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "none" | "off" => Some(Self::None),
            "facts" | "facts_only" | "factsonly" => Some(Self::Facts),
            "summary" | "summaries" => Some(Self::Summary),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Facts => "facts",
            Self::Summary => "summary",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileContextMode {
    None,
    References,
    Contents,
}

impl FileContextMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "none" | "off" => Some(Self::None),
            "references" | "refs" | "reference" => Some(Self::References),
            "contents" | "content" | "full" | "full_content" => Some(Self::Contents),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::References => "references",
            Self::Contents => "contents",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRequest {
    pub mode: ContextShareMode,
    pub include_recent_turns: Option<usize>,
    pub include_tool_results: ToolResultMode,
    pub include_files: FileContextMode,
    pub instructions: Option<String>,
}

impl ContextRequest {
    pub fn none() -> Self {
        Self {
            mode: ContextShareMode::None,
            include_recent_turns: None,
            include_tool_results: ToolResultMode::Facts,
            include_files: FileContextMode::References,
            instructions: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStrategy {
    Auto,
    Fresh,
    StableContextCapsule,
    ProviderNative,
    ProviderNativeContinuation,
    NoCache,
}

impl CacheStrategy {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "auto" => Some(Self::Auto),
            "fresh" => Some(Self::Fresh),
            "stable_context_capsule" | "stablecontextcapsule" | "stable" => {
                Some(Self::StableContextCapsule)
            }
            "provider_native" | "providernative" => Some(Self::ProviderNative),
            "provider_native_continuation" | "providernativecontinuation" => {
                Some(Self::ProviderNativeContinuation)
            }
            "no_cache" | "nocache" | "none" | "off" => Some(Self::NoCache),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fresh => "fresh",
            Self::StableContextCapsule => "stable_context_capsule",
            Self::ProviderNative => "provider_native",
            Self::ProviderNativeContinuation => "provider_native_continuation",
            Self::NoCache => "no_cache",
        }
    }
}

/// Git metadata captured by the sub-agent runtime for isolated workers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubAgentGitMetadata {
    pub worktree_path: Option<String>,
    pub worktree_branch: Option<String>,
    pub base_commit: Option<String>,
    pub head_commit: Option<String>,
    pub commit_hash: Option<String>,
    pub dirty_after_commit: Option<bool>,
    pub validation_status: Option<String>,
    pub validation_error: Option<String>,
    pub status_output: Option<String>,
    pub source_worktree: Option<String>,
    pub source_branch: Option<String>,
    pub git_root: Option<String>,
    pub integration_status: Option<String>,
    pub merge_commit: Option<String>,
    pub integration_error: Option<String>,
    pub worktree_preserved: Option<bool>,
}

impl SubAgentGitMetadata {
    pub fn to_json(&self) -> Value {
        json!({
            "worktree_path": self.worktree_path,
            "worktree_branch": self.worktree_branch,
            "branch": self.worktree_branch,
            "base_commit": self.base_commit,
            "base": self.base_commit,
            "head_commit": self.head_commit,
            "head": self.head_commit,
            "commit_hash": self.commit_hash,
            "commit": self.commit_hash,
            "dirty_after_commit": self.dirty_after_commit,
            "dirty": self.dirty_after_commit,
            "validation_status": self.validation_status,
            "validation_error": self.validation_error,
            "status_output": self.status_output,
            "source_worktree": self.source_worktree,
            "source_branch": self.source_branch,
            "git_root": self.git_root,
            "integration_status": self.integration_status,
            "merge_commit": self.merge_commit,
            "integration_error": self.integration_error,
            "worktree_preserved": self.worktree_preserved,
        })
    }

    pub fn from_worktree_info(info: &crate::worktree::AgentWorktreeInfo) -> Self {
        Self {
            worktree_path: Some(info.worktree_path.to_string_lossy().to_string()),
            worktree_branch: Some(info.worktree_branch.clone()),
            base_commit: Some(info.head_commit.clone()),
            source_worktree: Some(info.source_worktree.to_string_lossy().to_string()),
            source_branch: info.source_branch.clone(),
            git_root: Some(info.git_root.to_string_lossy().to_string()),
            validation_status: Some("pending".to_string()),
            ..Default::default()
        }
    }

    pub fn apply_integration_result(
        &mut self,
        result: &crate::worktree::WorktreeIntegrationResult,
    ) {
        self.integration_status = Some(result.status.as_str().to_string());
        self.commit_hash = result.commit_hash.clone();
        self.head_commit = result.commit_hash.clone();
        self.merge_commit = result.merge_commit.clone();
        self.integration_error = result.error.clone();
        self.worktree_preserved = Some(result.preserves_worktree());
    }
}

#[derive(Debug, Clone)]
pub struct FrozenParentContextCapsule {
    pub text: Arc<str>,
    pub hash: String,
    pub token_estimate: u32,
}

/// Input to a sub-agent spawn.
///
/// Matches the options field,
/// trimmed to the keys the current Rust runtime honours. Adding
/// fields here is zero-risk: the spawner owns the interpretation.
#[derive(Clone)]
pub struct SubAgentSpec {
    /// Prompt the sub-agent sees as its initial user message.
    pub prompt: String,
    /// Optional concrete model override. `None` lets the spawner pick the
    /// default (e.g. same model as the parent).
    pub model: Option<String>,
    /// Optional model profile override. `None` lets the spawner pick the
    /// agent/provider default profile.
    pub model_profile: Option<String>,
    /// Optional provider override. `None` lets the spawner inherit the active provider.
    pub provider: Option<String>,
    /// Optional context sharing request for context-capsule handoff.
    pub context: Option<ContextRequest>,
    pub frozen_parent_context: Option<FrozenParentContextCapsule>,
    /// Optional cache/session strategy hint for the worker.
    pub cache_strategy: Option<CacheStrategy>,
    /// Optional system prompt override.
    pub system: Option<String>,
    /// Tool visibility filter. `None` inherits the parent's full
    /// tool list; `Some(filter)` restricts the sub-agent via allow
    /// lists, deny lists, or both.
    pub tool_filter: Option<ToolFilter>,
    /// Iteration cap for the sub-agent's agentic loop.
    pub max_iterations: usize,
    /// Free-form metadata the caller wants surfaced to the spawner
    /// implementation (e.g. `agent_type`, `agent_id`).
    pub metadata: Value,
    /// Whether to run in the background and return immediately.
    pub run_in_background: bool,
    /// Whether this worker belongs to a runtime with no interactive permission consumer.
    pub permission_prompts_unavailable: bool,
    pub workflow_nesting_depth: usize,
    /// Working directory override for the sub-agent.
    pub cwd: Option<String>,
    /// True only when the runtime created a dedicated Git worktree for this
    /// worker, or the worker inherits its parent's runtime-created worktree
    /// with `cwd` untouched. The spawner copies it onto the child
    /// `ToolContext`, where it lifts the shared-worktree Git approval gate —
    /// so it must never be set from a caller-supplied `cwd` or metadata.
    /// [`parse_agent_tool_input`] always leaves it `false`.
    pub runtime_isolated_worktree: bool,
    /// Explicit filesystem roots this sub-agent may access. Empty means
    /// the spawner derives a single root from `cwd`.
    pub allowed_roots: Vec<PathBuf>,
    /// Immutable run-scoped capability snapshot inherited from the parent.
    /// Ultraplan workers must receive this explicitly instead of inferring
    /// permissions or session identity from global process state.
    pub capability_context: Option<crate::CapabilityContext>,
    /// RunState authority used to persist preflight retry/circuit state.
    pub ultraplan_run_repository: Option<Arc<dyn crate::UltraplanRunRepository>>,
    /// Resolved task list ID inherited from the parent turn.
    pub task_list_id: Option<String>,
    /// Optional request-scoped execution policy inherited from the
    /// parent tool call. Ordinary sub-agents leave this unset.
    pub execution_policy: Option<ExecutionPolicy>,
    /// Deterministic role marker used by coordinator runtime policy.
    pub task_kind: Option<SubAgentTaskKind>,
    /// Parent turn broker used only for sensitive sub-agent permission prompts.
    pub permission_broker: Option<Arc<dyn crate::PermissionBroker>>,
}

/// Default agentic iteration cap for sub-agents. Raised above the
/// main engine's 200 so workers have ample room to finish real
/// multi-step tasks (browse → grep → edit → verify → report) before
/// the loop trips — in practice coordinator workers hit 10 often and
/// even 200 occasionally, so 600 leaves comfortable headroom.
/// Callers can still override via [`SubAgentSpec::max_iterations`] /
/// the `max_iterations` JSON field.
pub const DEFAULT_SUB_AGENT_MAX_ITERATIONS: usize = 600;

impl SubAgentSpec {
    /// Minimal constructor.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            model: None,
            model_profile: None,
            provider: None,
            context: None,
            frozen_parent_context: None,
            cache_strategy: None,
            system: None,
            tool_filter: None,
            max_iterations: DEFAULT_SUB_AGENT_MAX_ITERATIONS,
            metadata: Value::Null,
            run_in_background: false,
            permission_prompts_unavailable: false,
            workflow_nesting_depth: 0,
            cwd: None,
            runtime_isolated_worktree: false,
            allowed_roots: Vec::new(),
            capability_context: None,
            ultraplan_run_repository: None,
            task_list_id: None,
            execution_policy: None,
            task_kind: None,
            permission_broker: None,
        }
    }
}

/// Read back, or mint, the stable id a spawned agent is known by.
///
/// Here rather than with the spawner that mints most of them: the workflow
/// runtime stamps ids the same way before it ever reaches a spawner, and the
/// two live in different plugins. One function is what keeps a workflow agent
/// and a directly spawned one from being known by two different ids.
pub fn ensure_agent_id(metadata: &mut Value) -> String {
    if !metadata.is_object() {
        *metadata = serde_json::json!({});
    }
    if let Some(existing) = metadata
        .get("agent_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return existing.to_string();
    }
    let agent_id = format!(
        "agent-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            & 0xFFFFFFFFFFFF
    );
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert("agent_id".into(), Value::String(agent_id.clone()));
    }
    agent_id
}

impl std::fmt::Debug for SubAgentSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubAgentSpec")
            .field("prompt", &self.prompt)
            .field("model", &self.model)
            .field("model_profile", &self.model_profile)
            .field("provider", &self.provider)
            .field("context", &self.context)
            .field(
                "has_frozen_parent_context",
                &self.frozen_parent_context.is_some(),
            )
            .field("cache_strategy", &self.cache_strategy)
            .field("has_system", &self.system.is_some())
            .field("has_tool_filter", &self.tool_filter.is_some())
            .field("max_iterations", &self.max_iterations)
            .field("metadata", &self.metadata)
            .field("run_in_background", &self.run_in_background)
            .field(
                "permission_prompts_unavailable",
                &self.permission_prompts_unavailable,
            )
            .field("workflow_nesting_depth", &self.workflow_nesting_depth)
            .field("cwd", &self.cwd)
            .field("runtime_isolated_worktree", &self.runtime_isolated_worktree)
            .field("allowed_roots", &self.allowed_roots)
            .field(
                "capability_hash",
                &self
                    .capability_context
                    .as_ref()
                    .map(|context| context.capability_hash.as_str()),
            )
            .field(
                "has_ultraplan_run_repository",
                &self.ultraplan_run_repository.is_some(),
            )
            .field("task_list_id", &self.task_list_id)
            .field("has_execution_policy", &self.execution_policy.is_some())
            .field("task_kind", &self.task_kind)
            .field("has_permission_broker", &self.permission_broker.is_some())
            .finish()
    }
}

/// Structured result returned from a sub-agent run.
#[derive(Debug, Clone)]
pub struct SubAgentResult {
    /// Final assistant text.
    pub final_text: String,
    /// Terminal status as a wire-stable string: `"completed"`,
    /// `"cancelled"`, `"failed"`, `"async_launched"`.
    pub status: String,
    /// Number of tool calls the sub-agent made.
    pub tool_call_count: usize,
    /// Stop reason as a debug-printed string (the spawner chooses
    /// the exact format).
    pub stop_reason: Option<String>,
    /// Error message if the run failed or was cancelled.
    pub error: Option<String>,
    /// Absolute path to the worker's structured report file, if the
    /// spawner was running in coordinator mode and assigned one.
    pub output_file: Option<String>,
    /// Duration of the worker run in milliseconds.
    pub duration_ms: Option<u64>,
    /// Unique identifier for this agent run, used for tracking
    /// background agents.
    pub agent_id: Option<String>,
    /// Agent type string (e.g. `"Explore"`, `"general-purpose"`).
    pub agent_type: Option<String>,
    /// Provider selected for the sub-agent run.
    pub provider: Option<String>,
    /// Model selected for the sub-agent run.
    pub model: Option<String>,
    /// Runtime-derived child tool-call history, used by renderers to summarize
    /// completed sub-agent activity without relying on agent-provided counts.
    pub sub_agent_tool_calls: Option<Vec<Value>>,
    /// Number of Read/FileReadTool calls the sub-agent completed.
    /// Legacy fallback for old spawners that do not provide tool-call history.
    pub read_file_count: Option<usize>,
    /// Total tokens consumed across all iterations.
    pub total_tokens: Option<u64>,
    /// Output tokens summed across all iterations. Unlike
    /// `total_tokens` (which includes the billed input context and
    /// thus scales with context size), this measures only generated
    /// text — the basis for workflow budget charging.
    pub output_tokens: Option<u64>,
    /// Full usage breakdown as JSON (input_tokens, output_tokens,
    /// Anthropic cache_* fields, and DeepSeek prompt_cache_* fields).
    pub usage: Option<Value>,
    /// Coordinator/worker lifecycle diagnostics for workflow hard contracts.
    pub diagnostics: Option<Value>,
    /// Git/worktree runtime metadata for coordinator implementation workers.
    pub git: Option<SubAgentGitMetadata>,
}

/// Trait every sub-agent backend implements.
///
/// The main consumer is `rebon_plugin_agents::runtime::spawner::EngineSubAgentSpawner`,
/// which wires the trait up to a real `Engine` + `ModelClient`. The
/// test `InMemorySubAgentSpawner` below plays a scripted response.
#[async_trait]
pub trait SubAgentSpawner: Send + Sync {
    /// Validate and, where safe, normalize a worker specification before any
    /// task registration or model request. Implementations should return a
    /// structured diagnostic string on failure.
    fn preflight(&self, _spec: &mut SubAgentSpec) -> Result<(), String> {
        Ok(())
    }

    /// The canonical id of the declared external ACP agent that
    /// `prefix` names, when this spawner can delegate whole tasks to
    /// it. `None` (the default) means external delegation is not
    /// available — dispatch keeps refusing external-runtime agent
    /// types rather than silently degrading them into local workers.
    fn resolve_external_agent(&self, _prefix: &str) -> Option<String> {
        None
    }

    fn manages_worktree_lifecycle(&self) -> bool {
        false
    }

    /// Run the sub-agent with `spec` and return the final result.
    ///
    /// Implementations own the whole lifecycle: they may persist
    /// transcripts, forward progress to an ACP publisher, or keep
    /// the sub-agent registered in a task registry — none of that
    /// leaks into the tool layer.
    async fn spawn(&self, spec: SubAgentSpec) -> Result<SubAgentResult, String>;

    /// Run the sub-agent with `spec` and report live child activity through the
    /// parent Agent tool's progress channel.
    async fn spawn_with_progress(
        &self,
        spec: SubAgentSpec,
        _progress: Option<SubAgentProgressSender>,
    ) -> Result<SubAgentResult, String> {
        self.spawn(spec).await
    }

    /// Spawn the sub-agent in the background and return immediately
    /// with an agent ID. The worker continues running in a detached
    /// task.
    ///
    /// Returns the generated `agent_id` on success.
    async fn spawn_background(&self, spec: SubAgentSpec) -> Result<String, String> {
        // Default: fall back to synchronous spawn and return the
        // agent_id from the result (for spawners that don't support
        // true background execution).
        let result = self.spawn(spec).await?;
        Ok(result
            .agent_id
            .unwrap_or_else(|| format!("agent-{:x}", rand_id())))
    }

    /// Detach the caller from the first turn while preserving the spec's
    /// foreground/background permission and presentation semantics.
    async fn spawn_detached(&self, spec: SubAgentSpec) -> Result<String, String> {
        self.spawn_background(spec).await
    }
}

/// What a session gets when nothing is on the `sub-agent-spawner` seat.
///
/// `plugins.agents.enabled = false` takes the `Agent` tool off the tool seat
/// in the same breath, so the model cannot ask for a sub-agent. What is left
/// are the front end's own spawn paths — the ultraplan review, resuming a
/// background agent — and they get a refusal naming the switch instead of a
/// panic or a spawn that quietly never happens.
pub struct UnavailableSubAgentSpawner;

/// Why every method of [`UnavailableSubAgentSpawner`] refuses. One string, so
/// a user who turned the plugin off reads the same sentence wherever they hit
/// it.
pub const SUB_AGENTS_UNAVAILABLE: &str =
    "sub-agents are unavailable: the `agents` plugin is off (plugins.agents.enabled)";

#[async_trait]
impl SubAgentSpawner for UnavailableSubAgentSpawner {
    fn preflight(&self, _spec: &mut SubAgentSpec) -> Result<(), String> {
        Err(SUB_AGENTS_UNAVAILABLE.to_string())
    }

    async fn spawn(&self, _spec: SubAgentSpec) -> Result<SubAgentResult, String> {
        Err(SUB_AGENTS_UNAVAILABLE.to_string())
    }
}

/// Stable typed name of the seat a front end resolves a session's sub-agent
/// spawner off.
pub const SUB_AGENT_SPAWNER_SERVICE: &str = "sub-agent-spawner";

/// Typed definition for the kernel's `sub-agent-spawner` seat.
pub struct SubAgentSpawnerService;

impl rebon_kernel::Service for SubAgentSpawnerService {
    type Interface = dyn SubAgentSpawnerSource;
    const NAME: &'static str = SUB_AGENT_SPAWNER_SERVICE;
}

/// The provider behind the seat: whatever can build a spawner for a session.
///
/// One provider, filled by the crate that owns the `Agent` tool. With it off
/// nothing is on the seat, the front end attaches no spawner, and the `Agent`
/// tool is off the tool seat in the same breath — the model cannot delegate,
/// and nothing is standing by to run a delegation if it did.
pub trait SubAgentSpawnerSource: Send + Sync {
    /// Build the spawner this session's `Agent` calls go through.
    ///
    /// Called once per session, on the path that builds the executor. `Err` is
    /// a wiring fault (a handle of the wrong shape), not "no sub-agents here"
    /// — that case is the seat being empty.
    fn for_session(
        &self,
        request: SubAgentSpawnerRequest,
    ) -> Result<Arc<dyn SubAgentSpawner>, String>;
}

/// What only the front end knows about a session's sub-agents.
///
/// Every field is something the spawner is *configured* with rather than
/// something it can look up: which engine and model client a worker runs on,
/// which models it may pick, and which tools it inherits.
pub struct SubAgentSpawnerRequest {
    /// The session's engine, as a `Weak` — see [`SubAgentRuntimeHandle`].
    pub engine: SubAgentRuntimeHandle,
    /// The session's task runtime. `None` on a surface with no task registry;
    /// a spawn then fails before the worker starts rather than registering
    /// nowhere.
    pub task_runtime: Option<SubAgentRuntimeHandle>,
    /// The client a locally routed worker sends its turns to.
    pub client: Arc<dyn rebon_api::ModelClient>,
    /// The model a spec that names none inherits — the parent's.
    pub default_model: String,
    /// `agents.json` model selections, by agent type / category / alias.
    pub model_config: rebon_types::SubAgentModelConfig,
    /// The active provider's profile map, for `modelProfile` names.
    pub model_profiles: rebon_types::ModelProfileMap,
    /// Which provider, model and effort each spawn resolves to.
    pub model_router: Arc<dyn rebon_agent_core::model_router::AgentModelRouter>,
    /// The base tool filter every sub-agent inherits, as a live cell so a
    /// mid-session `/ceo` toggle reaches workers spawned afterwards. A caller
    /// holding a plain [`ToolFilter`] wraps it in a fresh cell nobody swaps.
    pub base_filter: Option<SharedToolFilter>,
    /// Whether this session is a coordinator, read live for the same reason.
    pub coordinator_mode: Option<SharedCoordinatorMode>,
    /// Whether a coordinator's implementation workers get their own worktree.
    pub coordinator_use_worktree: bool,
    /// Where a worker's `Write` / `Edit` snapshots the pre-write file, so
    /// `/rewind` covers what a sub-agent changed.
    pub file_history_tracker: Option<Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>>,
    /// Runs the whole task on a declared ACP agent when a spec's model reads
    /// `<agentId>:<model>`. `None` keeps every such spec on the local path.
    pub external_runner: Option<Arc<dyn crate::external_agent::ExternalSubAgentRunner>>,
    /// The session's policy-event subscribers, so a sub-agent's tool calls
    /// reach the same guards the session's own do — see
    /// [`SubAgentRuntimeHandle`] for why it crosses unnamed. `None` means a
    /// worker asks nobody, which is what every worker did before this
    /// handle existed.
    pub policy: Option<SubAgentRuntimeHandle>,
}

/// A piece of the session's runtime, carried across a seat without being named.
///
/// The engine and the task registry both sit *above* this crate — naming
/// either here would be a dependency cycle, and giving the seat a type
/// parameter would stop it being a `dyn` service. So the front end, which
/// already holds them, puts them in, and the one implementation takes them
/// back out with [`get`](Self::get).
///
/// The exchange is checked, not assumed: a handle of the wrong shape comes
/// back as an `Err` from [`SubAgentSpawnerSource::for_session`], at the one
/// call site that builds a session, rather than as a panic mid-run.
#[derive(Clone)]
pub struct SubAgentRuntimeHandle(Arc<dyn std::any::Any + Send + Sync>);

impl SubAgentRuntimeHandle {
    pub fn new<T: Send + Sync + 'static>(value: T) -> Self {
        Self(Arc::new(value))
    }

    /// The handle, if it is a `T`. `None` means the front end and the provider
    /// disagree about what this piece of the runtime is.
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.0.downcast_ref::<T>()
    }
}

impl std::fmt::Debug for SubAgentRuntimeHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubAgentRuntimeHandle")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct SubAgentProgressSender {
    context: ToolContext,
}

impl SubAgentProgressSender {
    pub fn new(context: &ToolContext) -> Self {
        Self {
            context: context.clone(),
        }
    }

    pub fn emit_activity(&self, message: impl Into<String>) -> bool {
        let message = message.into();
        let lines = activity_lines_from_message(&message);
        if lines.is_empty() {
            return false;
        }
        self.context.emit_progress(
            ToolProgressUpdate::new("sub_agent_activity")
                .with_message(lines.join("\n"))
                .with_payload(json!({ "activity_lines": lines })),
        )
    }

    pub fn emit_activity_lines<I, S>(&self, lines: I) -> bool
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let lines = lines
            .into_iter()
            .flat_map(|line| activity_lines_from_message(&line.into()))
            .collect::<Vec<_>>();
        if lines.is_empty() {
            return false;
        }
        self.context.emit_progress(
            ToolProgressUpdate::new("sub_agent_activity")
                .with_message(lines.join("\n"))
                .with_payload(json!({ "activity_lines": lines })),
        )
    }
}

fn activity_lines_from_message(message: &str) -> Vec<String> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let path = trimmed
        .strip_prefix("reading ")
        .or_else(|| trimmed.strip_prefix("editing "))
        .or_else(|| trimmed.strip_prefix("writing "));
    if let Some(path) = path {
        return vec![path.trim().to_string()]
            .into_iter()
            .filter(|line| !line.is_empty())
            .collect();
    }

    vec![trimmed.to_string()]
}

/// Generate a simple pseudorandom hex ID based on the current time.
fn rand_id() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

/// Whether an `Agent` tool input is asking for work that may mutate files.
///
/// Read by the run loop (to spot a parallel write batch) as well as by
/// the tool itself, so it lives with the domain rather than with the tool.
/// Conservative on purpose: only an input that positively says "research"
/// or "verification", or that hands the child a read-only tool list, counts
/// as read-only.
pub fn agent_input_may_write(input: &Value) -> bool {
    for key in ["task_kind", "coordinator_task_kind"] {
        if let Some(raw) = input.get(key).and_then(Value::as_str) {
            return !matches!(
                SubAgentTaskKind::parse(raw),
                Some(SubAgentTaskKind::Research | SubAgentTaskKind::Verification)
            );
        }
    }

    if input
        .get("subagent_type")
        .and_then(Value::as_str)
        .is_some_and(|agent_type| {
            matches!(
                agent_type.trim().to_ascii_lowercase().as_str(),
                "explore" | "plan" | "verification" | "claude-advisor" | "rebon-code-guide"
            )
        })
    {
        return false;
    }

    if let Some(tools) = input.get("allowed_tools").and_then(Value::as_array) {
        return tools.iter().filter_map(Value::as_str).any(|tool| {
            !matches!(
                tool.to_ascii_lowercase().as_str(),
                "read"
                    | "filereadtool"
                    | "glob"
                    | "globtool"
                    | "grep"
                    | "greptool"
                    | "webfetch"
                    | "websearch"
            )
        });
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The prompt is injected into the *spawned* agent, so it must read as
    /// instructions to that agent. Caller-side orchestration rules
    /// (parallel writer isolation, named teammate limits) belong in the
    /// tool description, which left with the tool.
    #[test]
    fn shared_worktree_prompt_targets_the_agent_not_the_caller() {
        assert!(WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT.contains("do not bypass"));
        assert!(WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT.contains("modified-since-read"));
        assert!(!WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT.contains("Parallel implementation agents"));
        assert!(!WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT.contains("Named teammates"));
    }

    #[test]
    fn appending_the_shared_worktree_prompt_is_idempotent() {
        let once = append_workflow_agent_shared_worktree_prompt(Some("BASE".into()));
        assert!(once.starts_with("BASE\n\n"));
        assert_eq!(
            append_workflow_agent_shared_worktree_prompt(Some(once.clone())),
            once,
            "a system prompt that already carries it is left alone"
        );
        assert_eq!(
            append_workflow_agent_shared_worktree_prompt(None),
            WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT
        );
    }

    /// The run loop reads this to decide whether a response's Agent calls
    /// are a parallel write batch, so it lives with the domain rather than
    /// moving with the tool.
    #[test]
    fn agent_input_write_classification_is_conservative() {
        assert!(!agent_input_may_write(&json!({ "task_kind": "research" })));
        assert!(!agent_input_may_write(
            &json!({ "coordinator_task_kind": "verification" })
        ));
        assert!(agent_input_may_write(
            &json!({ "task_kind": "implementation" })
        ));
        assert!(!agent_input_may_write(
            &json!({ "subagent_type": "Explore" })
        ));
        assert!(!agent_input_may_write(
            &json!({ "allowed_tools": ["Read", "Grep"] })
        ));
        assert!(agent_input_may_write(
            &json!({ "allowed_tools": ["Read", "Edit"] })
        ));
        // Nothing said either way: assume it may write.
        assert!(agent_input_may_write(&json!({ "prompt": "do the thing" })));
    }

    /// The default is what a bare `Engine` and every test that never wires a
    /// host see; a host that sets one bumps the generation so a cached
    /// reader rebuilds.
    #[test]
    fn the_registry_selection_defaults_to_the_builtins() {
        let default = agent_registry_selection();
        assert_eq!(default.generation, 0);
        assert!(!default.coordinator_use_worktree);
        assert!(default.registry.resolve("general-purpose").is_some());

        set_agent_registry_selection(Arc::new(AgentRegistry::builtins_only()), true);
        let set = agent_registry_selection();
        assert!(set.generation > 0, "setting one bumps the generation");
        assert!(set.coordinator_use_worktree);

        let again = agent_registry_selection();
        assert_eq!(
            again.generation, set.generation,
            "reading does not bump it — a cache keyed on it must not thrash"
        );
    }
}
