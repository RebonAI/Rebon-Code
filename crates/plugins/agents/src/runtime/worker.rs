//! Worker spawning + lifecycle.
//!
//! Every method here is a thin facade over
//! [`rebon_core::run_query`]. The goal is to expose a
//! worker-centric API — `spawn_worker(spec) → handle` — without
//! forcing every caller to thread through the engine, the client,
//! the context, and the cancel handle separately.

use std::sync::Arc;

use rebon_api::{PruneLevelHandle, ReasoningEffort, SessionHandle, StopReason, Usage};
use rebon_core::query::{
    filtered_tools_from_engine, run_query, tools_from_engine, QueryEvent, QueryParams,
};
use rebon_core::turn_hook::TurnHook;
use rebon_core::Engine;
use rebon_tool::{ExecutionPolicy, ToolContext, ToolFilter};
use rebon_types::{
    CapabilityContext, CapabilityDiagnostic, CapabilityDiagnosticClass, NetworkCapability,
    PromptCancel,
};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

/// Input bundle passed to [`spawn_worker`].
///
/// Prompt + tool subset + system prompt + iteration cap + model. The
/// options that never reach the worker — agent identifiers, the display
/// prompt, first-user-message hooks — are handled by
/// [`crate::runtime::spawner::EngineSubAgentSpawner`] before worker launch.
#[derive(Clone)]
pub struct WorkerSpec {
    /// Messages the worker will see as its initial conversation.
    pub messages: Vec<rebon_api::Message>,
    /// Target model identifier (forwarded to the [`ModelClient`]).
    pub model: String,
    /// Optional system prompt.
    pub system: Option<String>,
    /// Tool visibility filter applied on top of the engine's
    /// registered tools. `None` means "inherit every tool"; a
    /// [`ToolFilter`] narrows the worker's visible tools via
    /// allow lists, deny lists, or both.
    ///
    /// Callers that only need an allow list can use
    /// [`ToolFilter::allow_only`]; callers that want to combine
    /// multiple restrictions can [`ToolFilter::intersect`] them.
    pub tool_filter: Option<ToolFilter>,
    /// Iteration cap for the worker's agentic loop.
    pub max_iterations: usize,
    /// `max_tokens` forwarded to [`CreateMessageRequest::max_tokens`].
    pub max_tokens: u32,
    /// Base tool context injected into the worker query.
    pub tool_context: ToolContext,
    /// Optional query-local turn hook used to enforce worker delivery contracts.
    pub turn_hook: Option<Arc<dyn TurnHook>>,
    /// The policy-event subscribers this worker's turn asks, already
    /// tagged with the agent it runs as. Default-empty asks nobody.
    pub policy: rebon_core::policy_seat::PolicySources,
    /// Optional per-iteration attachment poller forwarded to the
    /// worker's [`QueryParams`]. Used by `run_teammate_loop` to wire
    /// a `rebon_plugin_tasks::TeammateMailboxPoller` so the teammate sees
    /// mid-turn inter-agent traffic. `None` means no
    /// attachment pass (the default for coordinator-spawned one-shot
    /// Agent-tool workers that don't belong to a team).
    pub attachment_poller: Option<rebon_core::query::AttachmentPollerBinding>,
    /// Optional OpenAI Responses reasoning effort for this worker.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Optional shared context-prune handle used by the worker query.
    pub prune_level: Option<PruneLevelHandle>,
    /// Optional request-scoped execution policy for this worker query.
    pub execution_policy: Option<ExecutionPolicy>,
    /// Immutable run/session/tool capability snapshot supplied by the parent.
    pub capability_context: Option<CapabilityContext>,
    /// Canonical hash of the resolved tool definitions visible to this worker.
    pub tools_hash: Option<String>,
    /// Canonical hash of the resolved tool input schemas visible to this worker.
    pub schema_hash: Option<String>,
    /// Rough token estimate for the resolved tool/schema request shape.
    pub tools_token_estimate: Option<u32>,
    /// Optional request-shape diagnostics and cache-key hints for this worker query.
    pub cache_trace_context: Option<rebon_api::CacheTraceContext>,
}

impl std::fmt::Debug for WorkerSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerSpec")
            .field("messages", &self.messages.len())
            .field("model", &self.model)
            .field("system", &self.system.is_some())
            .field("tool_filter", &self.tool_filter.is_some())
            .field("max_iterations", &self.max_iterations)
            .field("max_tokens", &self.max_tokens)
            .field("has_turn_hook", &self.turn_hook.is_some())
            .field("policy", &self.policy)
            .field("has_attachment_poller", &self.attachment_poller.is_some())
            .field("reasoning_effort", &self.reasoning_effort)
            .field("has_prune_level", &self.prune_level.is_some())
            .field("has_execution_policy", &self.execution_policy.is_some())
            .field(
                "capability_hash",
                &self
                    .capability_context
                    .as_ref()
                    .map(|context| context.capability_hash.as_str()),
            )
            .field("tools_hash", &self.tools_hash)
            .field("schema_hash", &self.schema_hash)
            .field("tools_token_estimate", &self.tools_token_estimate)
            .field(
                "has_cache_trace_context",
                &self.cache_trace_context.is_some(),
            )
            .finish()
    }
}

pub(crate) fn filter_for_execution_policy(policy: Option<&ExecutionPolicy>) -> Option<ToolFilter> {
    let ultraplan = policy.and_then(|policy| policy.ultraplan.as_ref())?;
    Some(
        ToolFilter::allow_only(ultraplan.allowed_tools.clone())
            .with_deny(ultraplan.denied_tools.clone()),
    )
}

pub(crate) fn effective_worker_tool_filter(
    tool_filter: Option<&ToolFilter>,
    execution_policy: Option<&ExecutionPolicy>,
) -> Option<ToolFilter> {
    match (tool_filter, filter_for_execution_policy(execution_policy)) {
        (Some(worker), Some(policy)) => Some(worker.intersect(&policy)),
        (Some(worker), None) => Some(worker.clone()),
        (None, Some(policy)) => Some(policy),
        (None, None) => None,
    }
}

/// Default per-turn output cap for spawned workers. 8192 is the largest
/// value universally safe across worker providers (Claude / OpenAI /
/// DeepSeek all allow >= 8192 output tokens); the previous 4096 truncated
/// large report/file Writes mid-tool-call (report validation then failed
/// the task) and starved reasoning-profile workers whose hidden reasoning
/// counts against the cap.
pub(crate) const DEFAULT_WORKER_MAX_TOKENS: u32 = 8192;

/// Worker output cap for providers whose hidden reasoning is billed
/// against `max_tokens` (OpenAI Responses dialect, plugin providers
/// declaring `reasoningText`). On those providers 8192 lets a
/// medium/high-effort reasoning pass starve the visible tool-call
/// JSON, which surfaces as "Model output was truncated while emitting
/// a tool call". The 32K per-turn budget the main session
/// already sends to the same providers (`resolve_thinking_from_effort`).
pub(crate) const REASONING_WORKER_MAX_TOKENS: u32 = 32_000;

/// Provider-aware worker output budget: the flat default unless the
/// resolved client says reasoning shares the output budget.
pub(crate) fn worker_max_tokens_for_client(client: &dyn rebon_api::ModelClient) -> u32 {
    if client.output_budget_includes_reasoning() {
        REASONING_WORKER_MAX_TOKENS
    } else {
        DEFAULT_WORKER_MAX_TOKENS
    }
}

impl WorkerSpec {
    /// Minimal constructor — prompt + model, everything else default.
    pub fn new(prompt: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            messages: vec![rebon_api::Message::user_text(prompt)],
            model: model.into(),
            system: None,
            tool_filter: None,
            // The shared sub-agent iteration default, so sub-agents
            // spawned through `WorkerSpec::new` without an explicit
            // override can complete realistic multi-step tasks.
            max_iterations: rebon_tool::DEFAULT_SUB_AGENT_MAX_ITERATIONS,
            max_tokens: DEFAULT_WORKER_MAX_TOKENS,
            tool_context: ToolContext::new(),
            turn_hook: None,
            policy: rebon_core::policy_seat::PolicySources::default(),
            attachment_poller: None,
            reasoning_effort: None,
            prune_level: None,
            execution_policy: None,
            capability_context: None,
            tools_hash: None,
            schema_hash: None,
            tools_token_estimate: None,
            cache_trace_context: None,
        }
    }

    /// Attach a per-iteration attachment poller (builder-style).
    pub fn with_attachment_poller(
        mut self,
        poller: Arc<dyn rebon_core::query::AttachmentPoller>,
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
    ) -> Self {
        self.attachment_poller = Some(rebon_core::query::AttachmentPollerBinding::new(
            poller, session_id, turn_id,
        ));
        self
    }

    /// Attach a system prompt (builder-style).
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Attach a pre-built [`ToolFilter`] (builder-style).
    pub fn with_tool_filter(mut self, filter: ToolFilter) -> Self {
        self.tool_filter = Some(filter);
        self
    }

    /// Restrict the visible tool set to the listed names
    /// (builder-style). Shortcut for `with_tool_filter(ToolFilter::allow_only(tools))`.
    pub fn with_allowed_tools(mut self, tools: Vec<String>) -> Self {
        self.tool_filter = Some(ToolFilter::allow_only(tools));
        self
    }

    /// Override iteration cap.
    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    /// Override `max_tokens`.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_tool_context(mut self, tool_context: ToolContext) -> Self {
        self.tool_context = tool_context;
        self
    }

    /// Attach a query-local terminal policy hook.
    pub fn with_turn_hook(mut self, hook: Arc<dyn TurnHook>) -> Self {
        self.turn_hook = Some(hook);
        self
    }

    pub fn with_prune_level(mut self, prune_level: PruneLevelHandle) -> Self {
        self.prune_level = Some(prune_level);
        self
    }

    pub fn with_resolved_tool_trace(
        mut self,
        tools_hash: String,
        schema_hash: String,
        tools_token_estimate: u32,
    ) -> Self {
        self.tools_hash = Some(tools_hash);
        self.schema_hash = Some(schema_hash);
        self.tools_token_estimate = Some(tools_token_estimate);
        self
    }
}

fn estimate_tools_tokens(tools: &[rebon_api::Tool]) -> Result<u32, WorkerSpawnError> {
    let serialized = serde_json::to_string(tools)
        .map_err(|err| WorkerSpawnError::ToolSchemaTrace(err.to_string()))?;
    Ok(estimate_text_tokens(&serialized))
}

fn estimate_text_tokens(value: &str) -> u32 {
    let chars = value.chars().count();
    chars.div_ceil(4).min(u32::MAX as usize) as u32
}

/// Lifecycle status of a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerStatus {
    /// The worker task has been spawned but has not yet reached a
    /// terminal state.
    Running,
    /// The worker terminated cleanly with a stop reason.
    Completed,
    /// The worker was cancelled mid-run via the shared
    /// [`PromptCancel`] handle.
    Cancelled,
    /// The worker failed during its query loop (model error or
    /// tool error that propagated out).
    Failed,
}

impl WorkerStatus {
    /// True when the worker has reached a terminal state.
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// Structured final result of a worker run.
///
/// Carries the final text, stop reason,
/// cumulative usage, and a per-tool-call summary the caller can
/// surface in UI or fold into a structured result JSON.
#[derive(Debug, Clone)]
pub struct WorkerResult {
    /// Terminal status (always terminal on a [`WorkerHandle::wait`]
    /// return).
    pub status: WorkerStatus,
    /// Final assistant message text. Empty if the worker failed
    /// before producing any assistant output.
    pub final_text: String,
    /// Stop reason reported by the model. `None` if the worker
    /// failed or was cancelled before any `message_delta` arrived.
    pub stop_reason: Option<StopReason>,
    /// Usage totals accumulated across every iteration.
    pub total_usage: Usage,
    /// Output tokens summed across every iteration. `total_usage`
    /// merges per-field last-write-wins, so its `output_tokens` only
    /// reflects the final iteration — this counter is the true
    /// cumulative amount (the basis for workflow budget charging).
    pub cumulative_output_tokens: u64,
    /// Summary of every tool call the worker made.
    pub tool_calls: Vec<WorkerToolCall>,
    /// Whether the query loop performed a context reset before this terminal result.
    pub context_reset_occurred: bool,
    /// Final error message if [`WorkerStatus::Failed`].
    pub error: Option<String>,
}

/// One tool call recorded during a worker run.
#[derive(Debug, Clone)]
pub struct WorkerToolCall {
    /// Tool use id assigned by the model.
    pub tool_use_id: String,
    /// Registered tool name.
    pub name: String,
    /// Input the model sent.
    pub input: Value,
    /// Outcome — either the tool's JSON output or an error string
    /// that was returned to the model as `is_error: true`.
    pub outcome: Result<Value, String>,
}

/// Public re-emission of [`QueryEvent`].
///
/// Kept as a separate enum so consumers of this crate do not have
/// to import `rebon-core::query::QueryEvent` just to pattern
/// match on worker progress.
#[derive(Debug)]
pub enum WorkerEvent {
    /// Assistant-text snapshot while the model is streaming a
    /// response. `text` is the complete text accumulated since the current
    /// message start; it is **not** a delta. Consumers that need event-level
    /// deltas should consume `TaskRegistry::task_live_events` /
    /// `session_live_events`, whose
    /// `TaskLiveEventKind::AssistantTextDelta` carries both `delta` and the
    /// snapshot.
    AssistantText {
        /// Complete assistant text accumulated for the in-flight message.
        text: String,
    },
    /// Thinking snapshot while the model is streaming reasoning.
    /// `text` is the complete thinking text accumulated since the current
    /// message start, not a delta.
    Thinking { text: String },
    /// The worker is about to call a tool.
    ToolStart {
        /// Tool use id.
        tool_use_id: String,
        /// Registered tool name.
        name: String,
        /// Input the model sent.
        input: Value,
    },
    /// A tool emitted a progress update while executing.
    ToolProgress {
        /// Tool use id.
        tool_use_id: String,
        /// Registered tool name.
        name: String,
        /// Progress message (e.g. stdout/stderr line).
        message: Option<String>,
    },
    /// A tool finished executing.
    ToolFinish {
        /// Tool use id.
        tool_use_id: String,
        /// Registered tool name.
        name: String,
        /// Outcome — either the tool's JSON output or an error
        /// string surfaced back to the model as `is_error: true`.
        outcome: Result<Value, String>,
    },
    /// One agentic iteration finished (model produced an assistant
    /// message; may or may not be the terminal one).
    IterationComplete {
        /// Iteration number (0-indexed).
        iteration: usize,
        /// Final text for this iteration.
        text: String,
        /// Running usage total after this iteration.
        total_usage: Usage,
    },
    /// A worker tool needs parent-session permission approval.
    PermissionQuery(rebon_core::permission::OutboundPermissionQuery),
    /// The worker reached a terminal state.
    Completed(WorkerResult),
}

/// Summary snapshot of a worker's progress so far.
#[derive(Debug, Clone, Default)]
pub struct WorkerSummary {
    /// Total tool calls observed (successful + failed).
    pub tool_call_count: usize,
    /// Last assistant-message text observed.
    pub last_text: String,
    /// Running usage total.
    pub total_usage: Usage,
}

/// Error returned from [`spawn_worker`] when setup fails before
/// any iteration has a chance to run.
#[derive(Debug, Error)]
pub enum WorkerSpawnError {
    /// The caller asked for specific tools but none of them were
    /// registered on the engine.
    #[error("worker spec lists tools but none are registered on the engine: {0:?}")]
    NoMatchingTools(Vec<String>),
    /// The resolved tool list could not be serialized for cache diagnostics.
    #[error("failed to serialize worker tool schema for cache trace: {0}")]
    ToolSchemaTrace(String),
    /// Ultraplan worker capabilities failed before a model request was sent.
    #[error("{0}")]
    CapabilityPreflight(CapabilityDiagnostic),
}

/// Check the worker's toolkit against what its capability grant permits.
///
/// Every gate here answers the same question from a different angle: the tool
/// list the worker is about to get must be a subset of what the grant allows,
/// and the write, network and sub-agent powers it implies must each be
/// separately granted.
fn preflight_worker_tools(
    capability: &CapabilityContext,
    ultraplan: Option<&rebon_types::UltraplanContext>,
    tools: &[rebon_api::Tool],
    role: &str,
) -> Result<(), CapabilityDiagnostic> {
    let tool_names = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let permitted_tools = capability
        .tool_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let unavailable = tool_names
        .difference(&permitted_tools)
        .copied()
        .collect::<Vec<_>>();
    if !unavailable.is_empty() {
        return Err(capability_diagnostic(
            Some(capability),
            ultraplan,
            role,
            CapabilityDiagnosticClass::ToolUnavailable,
            format!(
                "worker requested tools outside the inherited capability set: {}",
                unavailable.join(", ")
            ),
            None,
            Some("tool_ids"),
            false,
        ));
    }
    if tool_names
        .iter()
        .any(|name| matches!(*name, "Read" | "Glob" | "Grep"))
        && !capability.read_allowed
    {
        return Err(capability_diagnostic(
            Some(capability),
            ultraplan,
            role,
            CapabilityDiagnosticClass::ReadDenied,
            "worker requires read access but the parent capability denies it",
            None,
            Some("read"),
            false,
        ));
    }
    if tool_names.iter().any(|name| {
        rebon_tools_core::tool_kind_for_name(name) == rebon_tools_core::ToolKind::FileEdit
    }) && !capability.write_allowed
    {
        return Err(capability_diagnostic(
            Some(capability),
            ultraplan,
            role,
            CapabilityDiagnosticClass::ToolUnavailable,
            "worker requires write access but the parent capability denies it",
            None,
            Some("write"),
            false,
        ));
    }
    if tool_names
        .iter()
        .any(|name| matches!(*name, "Bash" | "PowerShell"))
        && !capability.shell_allowed
    {
        return Err(capability_diagnostic(
            Some(capability),
            ultraplan,
            role,
            CapabilityDiagnosticClass::ToolUnavailable,
            "worker requires shell access but the parent capability denies it",
            None,
            Some("shell"),
            false,
        ));
    }
    for name in tool_names
        .iter()
        .filter(|name| matches!(**name, "WebSearch" | "WebFetch" | "Mcp"))
    {
        let allowed = match &capability.network {
            NetworkCapability::Denied => false,
            NetworkCapability::ToolScoped(tool_ids) => tool_ids.iter().any(|tool| tool == *name),
        };
        if !allowed {
            return Err(capability_diagnostic(
                Some(capability),
                ultraplan,
                role,
                CapabilityDiagnosticClass::ToolUnavailable,
                format!("network capability for `{name}` is not available"),
                None,
                Some("network"),
                false,
            ));
        }
    }
    Ok(())
}

pub(crate) fn preflight_worker_capabilities(
    capability: Option<&CapabilityContext>,
    execution_policy: Option<&ExecutionPolicy>,
    session_id: Option<&str>,
    cwd: Option<&str>,
    allowed_roots: &[std::path::PathBuf],
    tools: &[rebon_api::Tool],
    role: &str,
) -> Result<(), CapabilityDiagnostic> {
    let ultraplan = execution_policy.and_then(|policy| policy.ultraplan.as_ref());
    let capability_required = ultraplan.is_some_and(|context| context.ledger_revision > 0);
    if !capability_required && capability.is_none() {
        return Ok(());
    }
    let Some(capability) = capability else {
        return Err(capability_diagnostic(
            None,
            ultraplan,
            role,
            CapabilityDiagnosticClass::CapabilityDrift,
            "ultraplan worker is missing its immutable CapabilityContext",
            None,
            Some("capability_context"),
            true,
        ));
    };
    if !capability.hash_is_valid() {
        return Err(capability_diagnostic(
            Some(capability),
            ultraplan,
            role,
            CapabilityDiagnosticClass::CapabilityDrift,
            "worker capability hash does not match its immutable payload",
            None,
            Some("capability_hash"),
            true,
        ));
    }
    if let Some(ultraplan) = ultraplan {
        if capability.run_id != ultraplan.run_id {
            return Err(capability_diagnostic(
                Some(capability),
                Some(ultraplan),
                role,
                CapabilityDiagnosticClass::SessionScopeMismatch,
                format!(
                    "worker run `{}` does not match active run `{}`",
                    capability.run_id, ultraplan.run_id
                ),
                None,
                Some("run_id"),
                true,
            ));
        }
        if capability.ledger_revision != ultraplan.ledger_revision
            || capability.requirements_hash != ultraplan.requirements_hash
        {
            return Err(capability_diagnostic(
                Some(capability),
                Some(ultraplan),
                role,
                CapabilityDiagnosticClass::StaleRevision,
                format!(
                    "worker capability references ledger revision {} but the policy references {}",
                    capability.ledger_revision, ultraplan.ledger_revision
                ),
                None,
                Some("ledger_revision"),
                true,
            ));
        }
    }
    match session_id {
        None => {
            return Err(capability_diagnostic(
                Some(capability),
                ultraplan,
                role,
                CapabilityDiagnosticClass::SessionScopeMismatch,
                "worker is missing the parent session id",
                None,
                Some("session_id"),
                true,
            ));
        }
        Some(session_id) if session_id != capability.session_id => {
            return Err(capability_diagnostic(
                Some(capability),
                ultraplan,
                role,
                CapabilityDiagnosticClass::SessionScopeMismatch,
                format!(
                    "worker session `{session_id}` does not match capability session `{}`",
                    capability.session_id
                ),
                None,
                Some("session_id"),
                true,
            ));
        }
        Some(_) => {}
    }
    if !capability.sub_agent_available {
        return Err(capability_diagnostic(
            Some(capability),
            ultraplan,
            role,
            CapabilityDiagnosticClass::ToolUnavailable,
            "sub-agent spawning is unavailable for this run",
            None,
            Some("sub_agent"),
            false,
        ));
    }

    preflight_worker_tools(capability, ultraplan, tools, role)?;

    let parent_roots = canonical_capability_roots(capability, ultraplan, role)?;
    let child_roots = if allowed_roots.is_empty() {
        cwd.map(std::path::PathBuf::from).into_iter().collect()
    } else {
        allowed_roots.to_vec()
    };
    if child_roots.is_empty() {
        return Err(capability_diagnostic(
            Some(capability),
            ultraplan,
            role,
            CapabilityDiagnosticClass::MissingRoot,
            "worker has no cwd or allowed root to preflight",
            None,
            Some("allowed_roots"),
            false,
        ));
    }
    for root in child_roots {
        let canonical = std::fs::canonicalize(&root).map_err(|_| {
            capability_diagnostic(
                Some(capability),
                ultraplan,
                role,
                CapabilityDiagnosticClass::MissingRoot,
                format!("worker root `{}` does not exist", root.display()),
                Some(root.to_string_lossy().as_ref()),
                Some("allowed_roots"),
                false,
            )
        })?;
        if !parent_roots
            .iter()
            .any(|parent| canonical.starts_with(parent))
        {
            return Err(capability_diagnostic(
                Some(capability),
                ultraplan,
                role,
                CapabilityDiagnosticClass::SessionScopeMismatch,
                format!(
                    "worker root `{}` is outside inherited roots",
                    canonical.display()
                ),
                Some(canonical.to_string_lossy().as_ref()),
                Some("allowed_roots"),
                true,
            ));
        }
        std::fs::read_dir(&canonical).map_err(|err| {
            capability_diagnostic(
                Some(capability),
                ultraplan,
                role,
                CapabilityDiagnosticClass::ReadDenied,
                format!("worker cannot read root `{}`: {err}", canonical.display()),
                Some(canonical.to_string_lossy().as_ref()),
                Some("read"),
                false,
            )
        })?;
    }
    if let Some(cwd) = cwd {
        let cwd = std::fs::canonicalize(cwd).map_err(|_| {
            capability_diagnostic(
                Some(capability),
                ultraplan,
                role,
                CapabilityDiagnosticClass::MissingRoot,
                format!("worker cwd `{cwd}` does not exist"),
                Some(cwd),
                Some("cwd"),
                false,
            )
        })?;
        if !parent_roots.iter().any(|parent| cwd.starts_with(parent)) {
            return Err(capability_diagnostic(
                Some(capability),
                ultraplan,
                role,
                CapabilityDiagnosticClass::SessionScopeMismatch,
                format!("worker cwd `{}` is outside inherited roots", cwd.display()),
                Some(cwd.to_string_lossy().as_ref()),
                Some("cwd"),
                true,
            ));
        }
    }
    Ok(())
}

fn canonical_capability_roots(
    capability: &CapabilityContext,
    ultraplan: Option<&rebon_types::UltraplanContext>,
    role: &str,
) -> Result<Vec<std::path::PathBuf>, CapabilityDiagnostic> {
    let mut roots = Vec::new();
    for root in &capability.allowed_roots {
        let canonical = std::fs::canonicalize(root).map_err(|_| {
            capability_diagnostic(
                Some(capability),
                ultraplan,
                role,
                CapabilityDiagnosticClass::MissingRoot,
                format!("inherited root `{root}` does not exist"),
                Some(root),
                Some("allowed_roots"),
                false,
            )
        })?;
        std::fs::read_dir(&canonical).map_err(|err| {
            capability_diagnostic(
                Some(capability),
                ultraplan,
                role,
                CapabilityDiagnosticClass::ReadDenied,
                format!(
                    "inherited root `{}` is unreadable: {err}",
                    canonical.display()
                ),
                Some(canonical.to_string_lossy().as_ref()),
                Some("read"),
                false,
            )
        })?;
        roots.push(canonical);
    }
    if roots.is_empty() {
        return Err(capability_diagnostic(
            Some(capability),
            ultraplan,
            role,
            CapabilityDiagnosticClass::MissingRoot,
            "CapabilityContext contains no allowed roots",
            None,
            Some("allowed_roots"),
            false,
        ));
    }
    Ok(roots)
}

fn capability_diagnostic(
    capability_context: Option<&CapabilityContext>,
    ultraplan: Option<&rebon_types::UltraplanContext>,
    role: &str,
    class: CapabilityDiagnosticClass,
    message: impl Into<String>,
    root: Option<&str>,
    capability: Option<&str>,
    retryable: bool,
) -> CapabilityDiagnostic {
    CapabilityDiagnostic {
        class,
        message: message.into(),
        run_id: capability_context
            .map(|context| context.run_id.clone())
            .or_else(|| ultraplan.map(|context| context.run_id.clone()))
            .unwrap_or_default(),
        ledger_revision: capability_context
            .map(|context| context.ledger_revision)
            .or_else(|| ultraplan.map(|context| context.ledger_revision))
            .unwrap_or_default(),
        capability_hash: capability_context
            .map(|context| context.capability_hash.clone())
            .unwrap_or_default(),
        role: role.to_string(),
        root: root.map(str::to_string),
        capability: capability.map(str::to_string),
        retryable,
        fallback_to_parent: true,
    }
}

/// Handle to a running worker.
///
/// The handle exposes an unbounded receiver of [`WorkerEvent`]s for
/// callers that want to observe progress, plus a convenience
/// [`Self::wait`] that drains the stream and returns the final
/// [`WorkerResult`]. Dropping the handle without draining is safe
/// — the underlying tokio task continues to run (preserves
/// "fire and forget" semantics) until it finishes naturally or the
/// shared [`PromptCancel`] is tripped.
pub struct WorkerHandle {
    cancel: PromptCancel,
    events: mpsc::UnboundedReceiver<WorkerEvent>,
    summary: WorkerSummary,
}

impl std::fmt::Debug for WorkerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerHandle")
            .field("cancelled", &self.cancel.is_cancelled())
            .field("tool_call_count", &self.summary.tool_call_count)
            .finish()
    }
}

impl WorkerHandle {
    /// Trip the cancel flag so the worker exits its next await
    /// point.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Borrow the shared cancel handle — useful when the caller
    /// wants to wire it into its own cancellation graph.
    pub fn cancel_handle(&self) -> PromptCancel {
        self.cancel.clone()
    }

    /// Pull the next event without blocking forever.
    pub async fn next_event(&mut self) -> Option<WorkerEvent> {
        self.events.recv().await
    }

    /// Snapshot of the running tally so far. Updated every time
    /// the caller calls [`Self::next_event`] and receives a
    /// non-terminal event.
    pub fn summary(&self) -> WorkerSummary {
        self.summary.clone()
    }

    /// Drain every event until the worker terminates, then return
    /// the final [`WorkerResult`].
    ///
    /// The returned result carries the accumulated tool-call log
    /// and usage totals even if the caller never pulled individual
    /// events — useful for "fire a worker, await the result" call
    /// sites (e.g. `AgentTool` driving a sub-agent).
    pub async fn wait(mut self) -> WorkerResult {
        let mut result = WorkerResult {
            status: WorkerStatus::Running,
            final_text: String::new(),
            stop_reason: None,
            total_usage: Usage::default(),
            cumulative_output_tokens: 0,
            tool_calls: Vec::new(),
            context_reset_occurred: false,
            error: None,
        };
        let mut tool_inputs = std::collections::HashMap::new();
        while let Some(event) = self.events.recv().await {
            match event {
                WorkerEvent::AssistantText { text } => {
                    self.summary.last_text = text.clone();
                    result.final_text = text;
                }
                WorkerEvent::Thinking { .. } => {}
                WorkerEvent::ToolStart {
                    tool_use_id, input, ..
                } => {
                    tool_inputs.insert(tool_use_id, input);
                }
                WorkerEvent::ToolProgress { .. } => {}
                WorkerEvent::PermissionQuery(_) => {}
                WorkerEvent::ToolFinish {
                    tool_use_id,
                    name,
                    outcome,
                } => {
                    let input = tool_inputs.remove(&tool_use_id).unwrap_or(Value::Null);
                    result.tool_calls.push(WorkerToolCall {
                        tool_use_id,
                        name,
                        input,
                        outcome,
                    });
                    self.summary.tool_call_count = result.tool_calls.len();
                }
                WorkerEvent::IterationComplete { text, .. } => {
                    self.summary.last_text = text.clone();
                    result.final_text = text;
                }
                WorkerEvent::Completed(final_result) => {
                    return final_result;
                }
            }
        }
        // Channel closed without a `Completed` event — mark as
        // failed with a generic error so the caller knows something
        // went wrong mid-stream.
        result.status = WorkerStatus::Failed;
        result.error = Some("worker event stream closed unexpectedly".into());
        result
    }
}

/// Drain the query stream into worker events.
///
/// Runs on its own task for the worker's whole life: it accumulates the
/// running text, thinking and tool calls, forwards each event to the
/// caller, and sends one `Completed` at the end whatever way the stream
/// ended.
async fn drive_worker_events(
    mut query_rx: mpsc::UnboundedReceiver<QueryEvent>,
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
) {
    let mut running_text = String::new();
    let mut running_thinking = String::new();
    let mut last_non_empty_text: Option<String> = None;
    let mut tool_calls: Vec<WorkerToolCall> = Vec::new();
    let mut stop_reason: Option<StopReason> = None;
    let mut total_usage = Usage::default();
    let mut cumulative_output_tokens: u64 = 0;
    let mut failed_with: Option<String> = None;
    let mut iteration_limit_reached: Option<usize> = None;
    let mut context_reset_occurred = false;

    while let Some(event) = query_rx.recv().await {
        match event {
            QueryEvent::Stream(event) => match event {
                rebon_api::StreamEvent::MessageStart { .. } => {
                    running_text.clear();
                    running_thinking.clear();
                }
                rebon_api::StreamEvent::ContentBlockDelta {
                    delta: rebon_api::events::ContentBlockDelta::TextDelta { text },
                    ..
                } => {
                    running_text.push_str(&text);
                    if !running_text.trim().is_empty() {
                        last_non_empty_text = Some(running_text.clone());
                    }
                    let _ = event_tx.send(WorkerEvent::AssistantText {
                        text: running_text.clone(),
                    });
                }
                rebon_api::StreamEvent::ContentBlockDelta {
                    delta: rebon_api::events::ContentBlockDelta::ThinkingDelta { thinking },
                    ..
                } => {
                    running_thinking.push_str(&thinking);
                    let _ = event_tx.send(WorkerEvent::Thinking {
                        text: running_thinking.clone(),
                    });
                }
                _ => {}
            },
            QueryEvent::IterationComplete { iteration, message } => {
                running_text = message.text();
                if !running_text.trim().is_empty() {
                    last_non_empty_text = Some(running_text.clone());
                }
                stop_reason = message.stop_reason.clone();
                total_usage.merge(&message.usage);
                // Every iteration (including the final one) emits
                // IterationComplete before Done, so summing here
                // covers the whole run without double-counting.
                cumulative_output_tokens =
                    cumulative_output_tokens.saturating_add(u64::from(message.usage.output_tokens));
                let _ = event_tx.send(WorkerEvent::IterationComplete {
                    iteration,
                    text: running_text.clone(),
                    total_usage: total_usage.clone(),
                });
            }
            QueryEvent::ToolDispatchStart {
                tool_use_id,
                name,
                input,
            } => {
                let _ = event_tx.send(WorkerEvent::ToolStart {
                    tool_use_id: tool_use_id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
                tool_calls.push(WorkerToolCall {
                    tool_use_id,
                    name,
                    input,
                    outcome: Ok(Value::Null),
                });
            }
            // Display-only annotation for the interactive front-ends;
            // a worker's transcript is reported, not rendered.
            QueryEvent::ToolAutoModeAllowed { .. } => {}
            QueryEvent::ToolDispatchProgress {
                tool_use_id,
                name,
                progress,
            } => {
                let _ = event_tx.send(WorkerEvent::ToolProgress {
                    tool_use_id,
                    name,
                    message: progress.message,
                });
            }
            QueryEvent::ToolDispatchResult {
                tool_use_id,
                name,
                outcome,
                error_presentation,
            } => {
                let display_outcome = outcome.map_err(|error| {
                    error_presentation
                        .as_ref()
                        .map(|presentation| presentation.display_message.clone())
                        .unwrap_or(error)
                });
                if let Some(entry) = tool_calls
                    .iter_mut()
                    .rev()
                    .find(|t| t.tool_use_id == tool_use_id)
                {
                    entry.outcome = display_outcome.clone();
                }
                let _ = event_tx.send(WorkerEvent::ToolFinish {
                    tool_use_id,
                    name,
                    outcome: display_outcome,
                });
            }
            QueryEvent::IterationLimitReached { iterations } => {
                iteration_limit_reached = Some(iterations);
                tracing::warn!(iterations, "rebon-coordinator worker hit iteration limit");
            }
            QueryEvent::AttachmentInjected { iteration, .. } => {
                // Teammate workers don't currently mirror the
                // per-iteration attachment stream into
                // WorkerEvent — they operate on a scripted
                // prompt/response transcript and the injected
                // messages are already folded into
                // `params.messages` by `TurnControlPlugin`. Trace-log
                // the event so operators can still see it in
                // debug output.
                tracing::debug!(
                    iteration,
                    "rebon-coordinator worker observed attachment injection"
                );
            }
            QueryEvent::ContextReset { .. } => {
                context_reset_occurred = true;
                tracing::debug!("rebon-coordinator worker observed context reset");
            }
            QueryEvent::CompactingStarted { .. } | QueryEvent::CompactingFinished { .. } => {
                tracing::debug!("rebon-coordinator worker observed compaction event");
            }
            QueryEvent::PermissionQuery(query) => {
                let _ = event_tx.send(WorkerEvent::PermissionQuery(query));
            }
            QueryEvent::Cancelled => {
                let final_text = if running_text.trim().is_empty() {
                    last_non_empty_text.clone().unwrap_or_default()
                } else {
                    running_text.clone()
                };
                let final_result = WorkerResult {
                    status: WorkerStatus::Cancelled,
                    final_text,
                    stop_reason: stop_reason.clone(),
                    total_usage,
                    cumulative_output_tokens,
                    tool_calls: tool_calls.clone(),
                    context_reset_occurred,
                    error: None,
                };
                let _ = event_tx.send(WorkerEvent::Completed(final_result));
                return;
            }
            QueryEvent::Error(msg) => {
                failed_with = Some(msg);
                break;
            }
            QueryEvent::Done {
                final_message,
                stop_reason: stop,
                total_usage: usage,
            } => {
                running_text = final_message.text();
                if !running_text.trim().is_empty() {
                    last_non_empty_text = Some(running_text.clone());
                }
                total_usage.merge(&usage);
                let final_text = if running_text.trim().is_empty() {
                    last_non_empty_text.clone().unwrap_or_default()
                } else {
                    running_text.clone()
                };
                let limit_error = iteration_limit_reached.map(|iterations| {
                format!("worker reached iteration limit after {iterations} iterations before completing")
            });
                let final_result = WorkerResult {
                    status: if limit_error.is_some() {
                        WorkerStatus::Failed
                    } else {
                        WorkerStatus::Completed
                    },
                    final_text,
                    stop_reason: Some(stop),
                    total_usage,
                    cumulative_output_tokens,
                    tool_calls: tool_calls.clone(),
                    context_reset_occurred,
                    error: limit_error,
                };
                let _ = event_tx.send(WorkerEvent::Completed(final_result));
                return;
            }
        }
    }

    // Stream ended without a `Done`/`Cancelled` event — mark as
    // failed and surface the last captured error if any.
    let final_text = if running_text.trim().is_empty() {
        last_non_empty_text.unwrap_or_default()
    } else {
        running_text
    };
    let final_result = WorkerResult {
        status: WorkerStatus::Failed,
        final_text,
        stop_reason,
        total_usage,
        cumulative_output_tokens,
        tool_calls,
        context_reset_occurred,
        error: failed_with
            .or_else(|| Some("worker query stream ended without terminal event".into())),
    };
    let _ = event_tx.send(WorkerEvent::Completed(final_result));
}

/// Spawn a worker agent.
///
/// Builds a [`QueryParams`] from the [`WorkerSpec`], filters the
/// engine's tool registry down to `spec.allowed_tools` if set, and
/// forwards to [`run_query`]. Every [`QueryEvent`] is translated
/// into a [`WorkerEvent`] and pushed onto the handle's unbounded
/// receiver.
///
/// The shared [`PromptCancel`] lets the caller abort the worker at
/// any time — it is the same cancel primitive used by
/// [`rebon_agent_core::PromptExecutor`], so ACP `session/cancel`
/// propagates straight through.
pub fn spawn_worker(
    engine: Arc<Engine>,
    session: Arc<SessionHandle>,
    spec: WorkerSpec,
    cancel: PromptCancel,
) -> Result<WorkerHandle, WorkerSpawnError> {
    let effective_tool_filter =
        effective_worker_tool_filter(spec.tool_filter.as_ref(), spec.execution_policy.as_ref());
    let tools = match effective_tool_filter.as_ref() {
        Some(filter) => {
            let filtered = filtered_tools_from_engine(&engine, filter);
            if filtered.is_empty() && !filter.is_unrestricted() {
                return Err(WorkerSpawnError::NoMatchingTools(
                    filter.allow_list().unwrap_or_default(),
                ));
            }
            filtered
        }
        None => tools_from_engine(&engine),
    };
    preflight_worker_capabilities(
        spec.capability_context.as_ref(),
        spec.execution_policy.as_ref(),
        spec.tool_context.session_id(),
        spec.tool_context.cwd(),
        spec.tool_context.path_scope_roots(),
        &tools,
        "worker",
    )
    .map_err(WorkerSpawnError::CapabilityPreflight)?;
    let tools_hash = spec
        .tools_hash
        .clone()
        .unwrap_or_else(|| rebon_api::tools_hash(&tools));
    let schema_hash = spec
        .schema_hash
        .clone()
        .unwrap_or_else(|| rebon_api::schema_hash(&tools));
    let tools_token_estimate = match spec.tools_token_estimate {
        Some(tokens) => tokens,
        None => estimate_tools_tokens(&tools)?,
    };
    let mut cache_trace_context = spec.cache_trace_context.clone();
    if let Some(trace) = cache_trace_context.as_mut() {
        let base_tokens_before_capsule = trace
            .tokens_before_capsule
            .map(|base| base.saturating_add(tools_token_estimate))
            .unwrap_or(tools_token_estimate);
        let base_tokens_before_task = trace
            .tokens_before_task
            .map(|base| base.saturating_add(tools_token_estimate))
            .unwrap_or(tools_token_estimate);
        trace.tools_hash = Some(tools_hash.clone());
        trace.schema_hash = Some(schema_hash.clone());
        trace.tokens_before_capsule = Some(base_tokens_before_capsule);
        trace.tokens_before_task = Some(base_tokens_before_task);
        trace.cross_run_cache_eligible = Some(base_tokens_before_capsule >= 1024);
        trace.same_dispatch_cache_eligible = Some(base_tokens_before_task >= 1024);
    }
    // Workers deliberately do NOT inherit the parent's file-state
    // cache. Sharing one cache across concurrently-running agents
    // defeats the Edit/Write stale-read check: agent A's post-write
    // cache refresh makes agent B's freshness check pass silently, so
    // B edits on top of content it never read. A fresh cache (created
    // by the executor when this is None) forces every worker to Read
    // current disk state before it edits. The main session's own
    // cross-turn cache sharing is unaffected.
    let inherited_file_state_cache = None;

    let params = QueryParams {
        model: spec.model.clone(),
        system: spec.system.clone(),
        capability_mode: rebon_types::AgentCapabilityMode::Normal,
        anchored_minimal_promotion: None,
        runtime_context_message: None,
        transient_context_message: None,
        messages: spec.messages.clone(),
        tools,
        max_tokens: spec.max_tokens,
        max_iterations: spec.max_iterations,
        attachment_poller: spec.attachment_poller.clone(),
        turn_hooks: spec.turn_hook.clone().map_or_else(
            rebon_core::turn_hook::TurnHooks::default,
            |hook| {
                rebon_core::turn_hook::TurnHooks::default().with_hook(
                    "coordinator/worker-delivery-contracts",
                    rebon_core::turn_hook::Order::LAST,
                    hook,
                )
            },
        ),
        next_tool_choice: None,
        extensions: rebon_tool::Extensions::default(),
        thinking: None,
        reasoning_effort: spec.reasoning_effort,
        // Sub-agents stay in standard mode; pro mode is a main-loop
        // provider setting.
        reasoning_mode: None,
        web_search: None,
        context_management: None,
        prune_level: spec.prune_level.clone(),
        compact_provider: None,
        compact_fallback_provider: None,
        compact_custom_instructions: None,
        compact_summary_options: rebon_api::CompactSummaryOptions::default(),
        file_state_cache: inherited_file_state_cache,
        execution_policy: spec.execution_policy.clone(),
        invariant_execution_policy: spec.execution_policy.clone(),
        base_tool_filter: spec.tool_filter.clone(),
        effective_tool_filter,
        post_context_reset_system: spec.system.clone(),
        post_context_reset_runtime_context_message: None,
        post_context_reset_transient_context_message: None,
        policy: spec.policy.clone(),
        cache_trace_context,
        mcp_tool_definitions: Vec::new(),
    };

    let mut tool_context = spec.tool_context.clone();
    if let Some(policy) = spec.execution_policy.clone() {
        tool_context = tool_context.with_execution_policy(policy);
    }
    if let Some(capability) = spec.capability_context.clone() {
        tool_context = tool_context.with_capability_context(capability);
    }

    let query_rx = run_query(engine, session, params, tool_context, cancel.clone());

    let (event_tx, event_rx) = mpsc::unbounded_channel::<WorkerEvent>();
    tokio::spawn(async move {
        drive_worker_events(query_rx, event_tx).await;
    });

    Ok(WorkerHandle {
        cancel,
        events: event_rx,
        summary: WorkerSummary::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rebon_api::{
        events::{ContentBlockDelta, ContentBlockStart, MessageDeltaFields},
        MockModelClient, ModelClient, StreamEvent,
    };
    use rebon_tool::Tool;
    use rebon_tools_core::{
        PermissionDecision, ToolError, ToolId, ToolInputSchema, ValidationOutcome,
    };
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct RecordingTool {
        name: String,
        calls: Mutex<Vec<Value>>,
        response: Value,
    }

    impl RecordingTool {
        fn new(name: &str, response: Value) -> Self {
            Self {
                name: name.into(),
                calls: Mutex::new(Vec::new()),
                response,
            }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl Tool for RecordingTool {
        fn id(&self) -> ToolId {
            ToolId::new(self.name.clone())
        }
        fn description(&self) -> &str {
            "recording tool"
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
        async fn call(&self, input: Value, _context: &ToolContext) -> Result<Value, ToolError> {
            self.calls.lock().unwrap().push(input);
            Ok(self.response.clone())
        }
    }

    fn build_engine(tool: Arc<dyn Tool>) -> Arc<Engine> {
        struct ApproveBroker;
        #[async_trait]
        impl rebon_core::PermissionBroker for ApproveBroker {
            async fn resolve(
                &self,
                tool: &dyn Tool,
                input: Value,
                context: &ToolContext,
                _decision: PermissionDecision,
            ) -> Result<Value, ToolError> {
                tool.call(input, context).await
            }
        }
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(tool);
        Arc::new(engine)
    }

    fn text_turn(id: &str, text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message_id: id.into(),
                model: "mock".into(),
                usage: Usage {
                    input_tokens: 4,
                    ..Default::default()
                },
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text {
                    text: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::TextDelta { text: text.into() },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Usage {
                        output_tokens: 3,
                        ..Default::default()
                    },
                },
            },
            StreamEvent::MessageStop,
        ]
    }

    fn split_text_turn(id: &str, first: &str, second: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message_id: id.into(),
                model: "mock".into(),
                usage: Usage::default(),
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text {
                    text: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::TextDelta { text: first.into() },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::TextDelta {
                    text: second.into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Usage::default(),
                },
            },
            StreamEvent::MessageStop,
        ]
    }

    fn split_thinking_turn(id: &str, first: &str, second: &str, answer: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message_id: id.into(),
                model: "mock".into(),
                usage: Usage::default(),
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Thinking {
                    thinking: String::new(),
                    data: None,
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ThinkingDelta {
                    thinking: first.into(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ThinkingDelta {
                    thinking: second.into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlockStart::Text {
                    text: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 1,
                delta: ContentBlockDelta::TextDelta {
                    text: answer.into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 1 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Usage::default(),
                },
            },
            StreamEvent::MessageStop,
        ]
    }

    fn tool_turn(id: &str, tool: &str, tool_id: &str, json_input: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message_id: id.into(),
                model: "mock".into(),
                usage: Usage::default(),
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::ToolUse {
                    id: tool_id.into(),
                    name: tool.into(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::InputJsonDelta {
                    partial_json: json_input.into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields {
                    stop_reason: Some(StopReason::ToolUse),
                    usage: Usage::default(),
                },
            },
            StreamEvent::MessageStop,
        ]
    }

    fn text_and_tool_turn(
        id: &str,
        text: &str,
        tool: &str,
        tool_id: &str,
        json_input: &str,
    ) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message_id: id.into(),
                model: "mock".into(),
                usage: Usage::default(),
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text {
                    text: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::TextDelta { text: text.into() },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlockStart::ToolUse {
                    id: tool_id.into(),
                    name: tool.into(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 1,
                delta: ContentBlockDelta::InputJsonDelta {
                    partial_json: json_input.into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 1 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields {
                    stop_reason: Some(StopReason::ToolUse),
                    usage: Usage::default(),
                },
            },
            StreamEvent::MessageStop,
        ]
    }

    #[tokio::test]
    async fn worker_runs_single_text_turn_and_returns_completed() {
        let tool: Arc<dyn Tool> = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
        let engine = build_engine(tool);
        let client = MockModelClient::new();
        client.push_turn(text_turn("msg_1", "hi from worker"));
        let client: Arc<dyn ModelClient> = Arc::new(client);

        let spec = WorkerSpec::new("run a task", "mock");
        let handle = spawn_worker(
            engine,
            SessionHandle::new(client),
            spec,
            PromptCancel::new(),
        )
        .unwrap();
        let result = handle.wait().await;

        assert_eq!(result.status, WorkerStatus::Completed);
        assert_eq!(result.final_text, "hi from worker");
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(result.tool_calls.len(), 0);
    }

    #[tokio::test]
    async fn worker_assistant_text_events_are_accumulated_snapshots() {
        let tool: Arc<dyn Tool> = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
        let engine = build_engine(tool);
        let client = MockModelClient::new();
        client.push_turn(split_text_turn("msg_1", "hello", " world"));
        let client: Arc<dyn ModelClient> = Arc::new(client);

        let spec = WorkerSpec::new("run a task", "mock");
        let mut handle = spawn_worker(
            engine,
            SessionHandle::new(client),
            spec,
            PromptCancel::new(),
        )
        .unwrap();
        let mut snapshots = Vec::new();
        while let Some(event) = handle.next_event().await {
            match event {
                WorkerEvent::AssistantText { text } => snapshots.push(text),
                WorkerEvent::Completed(_) => break,
                _ => {}
            }
        }

        assert_eq!(snapshots, vec!["hello", "hello world"]);
    }

    #[tokio::test]
    async fn worker_thinking_events_are_accumulated_snapshots() {
        let tool: Arc<dyn Tool> = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
        let engine = build_engine(tool);
        let client = MockModelClient::new();
        client.push_turn(split_thinking_turn("msg_1", "inspect", " events", "done"));
        let client: Arc<dyn ModelClient> = Arc::new(client);

        let spec = WorkerSpec::new("run a task", "mock");
        let mut handle = spawn_worker(
            engine,
            SessionHandle::new(client),
            spec,
            PromptCancel::new(),
        )
        .unwrap();
        let mut snapshots = Vec::new();
        while let Some(event) = handle.next_event().await {
            match event {
                WorkerEvent::Thinking { text } => snapshots.push(text),
                WorkerEvent::Completed(_) => break,
                _ => {}
            }
        }

        assert_eq!(snapshots, ["inspect", "inspect events"]);
    }

    #[tokio::test]
    async fn worker_runs_tool_use_round_trip_and_records_calls() {
        let tool = Arc::new(RecordingTool::new("Read", json!({"body": "file body"})));
        let tool_ref: Arc<dyn Tool> = tool.clone();
        let engine = build_engine(tool_ref);

        let client = MockModelClient::new();
        client.push_turn(tool_turn(
            "msg_tool",
            "Read",
            "toolu_1",
            "{\"path\":\"a.rs\"}",
        ));
        client.push_turn(text_turn("msg_final", "done reading"));
        let client: Arc<dyn ModelClient> = Arc::new(client);

        let spec = WorkerSpec::new("read a.rs", "mock");
        let handle = spawn_worker(
            engine,
            SessionHandle::new(client),
            spec,
            PromptCancel::new(),
        )
        .unwrap();
        let result = handle.wait().await;

        assert_eq!(result.status, WorkerStatus::Completed);
        assert_eq!(result.final_text, "done reading");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "Read");
        assert_eq!(result.tool_calls[0].input, json!({"path": "a.rs"}));
        assert_eq!(tool.call_count(), 1);
    }

    #[tokio::test]
    async fn worker_returns_last_text_when_limit_hits_tool_use_only_turn() {
        let tool = Arc::new(RecordingTool::new("Read", json!({"body": "file body"})));
        let tool_ref: Arc<dyn Tool> = tool.clone();
        let engine = build_engine(tool_ref);

        let client = MockModelClient::new();
        client.push_turn(text_and_tool_turn(
            "msg_intro_tool",
            "I'll inspect the relevant files.",
            "Read",
            "toolu_1",
            "{\"path\":\"a.rs\"}",
        ));
        client.push_turn(tool_turn(
            "msg_limit_tool",
            "Read",
            "toolu_2",
            "{\"path\":\"b.rs\"}",
        ));
        let client: Arc<dyn ModelClient> = Arc::new(client);

        let spec = WorkerSpec::new("read files", "mock").with_max_iterations(2);
        let handle = spawn_worker(
            engine,
            SessionHandle::new(client),
            spec,
            PromptCancel::new(),
        )
        .unwrap();
        let result = handle.wait().await;

        assert_eq!(result.status, WorkerStatus::Failed);
        assert_eq!(result.final_text, "I'll inspect the relevant files.");
        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("iteration limit")));
        assert_eq!(tool.call_count(), 2);
    }

    #[tokio::test]
    async fn worker_respects_allowed_tools_filter() {
        let read_tool: Arc<dyn Tool> = Arc::new(RecordingTool::new("Read", json!({})));
        let write_tool: Arc<dyn Tool> = Arc::new(RecordingTool::new("Write", json!({})));
        let mut engine = build_engine(read_tool);
        // Register a second tool so we can verify the filter takes effect.
        Arc::get_mut(&mut engine)
            .expect("engine Arc is unique in this test")
            .register_tool(write_tool);

        let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let spec = WorkerSpec::new("any prompt", "mock").with_allowed_tools(vec!["Read".into()]);

        // The worker task builds its tool list before it starts, so inspect
        // the same projection `spawn_worker` uses instead of racing the task.
        let filter =
            effective_worker_tool_filter(spec.tool_filter.as_ref(), spec.execution_policy.as_ref())
                .expect("an allow list produces a filter");
        let filtered: Vec<String> = filtered_tools_from_engine(&engine, &filter)
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert_eq!(filtered, vec!["Read".to_string()]);

        let handle = spawn_worker(
            engine,
            SessionHandle::new(client),
            spec,
            PromptCancel::new(),
        )
        .unwrap();
        drop(handle);
    }

    #[tokio::test]
    async fn worker_cancel_propagates_into_cancelled_status() {
        let tool: Arc<dyn Tool> = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
        let engine = build_engine(tool);
        let client = MockModelClient::new();
        for _ in 0..5 {
            client.push_turn(tool_turn("loop", "Read", "toolu_1", "{\"path\":\"a.rs\"}"));
        }
        let client: Arc<dyn ModelClient> = Arc::new(client);

        let cancel = PromptCancel::new();
        cancel.cancel();
        let spec = WorkerSpec::new("spin forever", "mock");
        let handle = spawn_worker(engine, SessionHandle::new(client), spec, cancel).unwrap();
        let result = handle.wait().await;
        assert_eq!(result.status, WorkerStatus::Cancelled);
    }

    #[tokio::test]
    async fn worker_surfaces_model_error_as_failed() {
        let tool: Arc<dyn Tool> = Arc::new(RecordingTool::new("Read", json!({})));
        let engine = build_engine(tool);
        let client = MockModelClient::new();
        client.push_error(rebon_api::ModelError::Permanent("quota exceeded".into()));
        let client: Arc<dyn ModelClient> = Arc::new(client);

        let spec = WorkerSpec::new("do something", "mock");
        let handle = spawn_worker(
            engine,
            SessionHandle::new(client),
            spec,
            PromptCancel::new(),
        )
        .unwrap();
        let result = handle.wait().await;
        assert_eq!(result.status, WorkerStatus::Failed);
        assert!(result.error.is_some());
    }

    #[test]
    fn filtered_tools_all_when_filter_is_none() {
        let tool: Arc<dyn Tool> = Arc::new(RecordingTool::new("Read", json!({})));
        let engine = build_engine(tool);
        let tools = tools_from_engine(&engine);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "Read");
    }

    #[test]
    fn filtered_tools_empty_when_allow_list_is_empty() {
        let tool: Arc<dyn Tool> = Arc::new(RecordingTool::new("Read", json!({})));
        let engine = build_engine(tool);
        let tools =
            filtered_tools_from_engine(&engine, &ToolFilter::allow_only(Vec::<String>::new()));
        assert!(tools.is_empty());
    }

    #[test]
    fn worker_spawn_errors_when_filter_matches_nothing() {
        let tool: Arc<dyn Tool> = Arc::new(RecordingTool::new("Read", json!({})));
        let engine = build_engine(tool);
        let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let spec =
            WorkerSpec::new("prompt", "mock").with_allowed_tools(vec!["DoesNotExist".into()]);
        let err = spawn_worker(
            engine,
            SessionHandle::new(client),
            spec,
            PromptCancel::new(),
        )
        .err()
        .unwrap();
        assert!(matches!(err, WorkerSpawnError::NoMatchingTools(_)));
    }

    fn test_capability(root: &std::path::Path) -> CapabilityContext {
        let canonical = std::fs::canonicalize(root).unwrap();
        let mut capability = CapabilityContext {
            run_id: "run".into(),
            ledger_revision: 3,
            requirements_hash: "requirements".into(),
            session_id: "session".into(),
            cwd: canonical.to_string_lossy().to_string(),
            allowed_roots: vec![canonical.to_string_lossy().to_string()],
            read_allowed: true,
            write_allowed: false,
            shell_allowed: false,
            tool_ids: vec!["Read".into(), "Glob".into(), "Grep".into()],
            network: NetworkCapability::Denied,
            workspace_head: None,
            workspace_dirty: Some(false),
            provider: Some("mock".into()),
            model: Some("mock".into()),
            sub_agent_available: true,
            max_research_agents: 6,
            research_agents_used: 0,
            max_adversarial_reviews: 1,
            adversarial_reviews_used: 0,
            max_tool_error_retries: 1,
            capability_hash: String::new(),
        };
        capability.refresh_hash();
        capability
    }

    fn test_policy(capability: &CapabilityContext) -> ExecutionPolicy {
        let mut context = rebon_types::UltraplanContext::planning_turn(
            capability.run_id.clone(),
            "researching",
            rebon_types::PolicyMode::Enforce,
        );
        context.ledger_revision = capability.ledger_revision;
        context.requirements_hash = capability.requirements_hash.clone();
        ExecutionPolicy::ultraplan(context)
    }

    fn api_tool(name: &str) -> rebon_api::Tool {
        rebon_api::Tool {
            name: name.into(),
            description: name.into(),
            input_schema: json!({"type":"object"}),
        }
    }

    #[test]
    fn capability_preflight_accepts_narrow_child_root_and_read_tools() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let capability = test_capability(temp.path());
        let policy = test_policy(&capability);

        let result = preflight_worker_capabilities(
            Some(&capability),
            Some(&policy),
            Some("session"),
            child.to_str(),
            std::slice::from_ref(&child),
            &[api_tool("Read"), api_tool("Grep")],
            "researcher",
        );

        assert!(result.is_ok());
    }

    #[test]
    fn capability_preflight_rejects_session_and_root_expansion() {
        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let capability = test_capability(parent.path());
        let policy = test_policy(&capability);

        let session_error = preflight_worker_capabilities(
            Some(&capability),
            Some(&policy),
            Some("old-session"),
            Some(capability.cwd.as_str()),
            &[parent.path().to_path_buf()],
            &[api_tool("Read")],
            "researcher",
        )
        .unwrap_err();
        assert_eq!(
            session_error.class,
            CapabilityDiagnosticClass::SessionScopeMismatch
        );

        let root_error = preflight_worker_capabilities(
            Some(&capability),
            Some(&policy),
            Some("session"),
            outside.path().to_str(),
            &[outside.path().to_path_buf()],
            &[api_tool("Read")],
            "researcher",
        )
        .unwrap_err();
        assert_eq!(
            root_error.class,
            CapabilityDiagnosticClass::SessionScopeMismatch
        );
    }

    #[test]
    fn capability_preflight_rejects_write_and_stale_revision() {
        let temp = tempfile::tempdir().unwrap();
        let capability = test_capability(temp.path());
        let mut stale_policy = test_policy(&capability);
        stale_policy.ultraplan.as_mut().unwrap().ledger_revision += 1;

        let stale = preflight_worker_capabilities(
            Some(&capability),
            Some(&stale_policy),
            Some("session"),
            Some(capability.cwd.as_str()),
            &[temp.path().to_path_buf()],
            &[api_tool("Read")],
            "reviewer",
        )
        .unwrap_err();
        assert_eq!(stale.class, CapabilityDiagnosticClass::StaleRevision);

        let write = preflight_worker_capabilities(
            Some(&capability),
            Some(&test_policy(&capability)),
            Some("session"),
            Some(capability.cwd.as_str()),
            &[temp.path().to_path_buf()],
            &[api_tool("Write")],
            "reviewer",
        )
        .unwrap_err();
        assert_eq!(write.class, CapabilityDiagnosticClass::ToolUnavailable);
        assert_eq!(write.capability.as_deref(), Some("tool_ids"));
    }

    #[test]
    fn capability_preflight_rejects_outbound_tools_when_network_is_denied() {
        let temp = tempfile::tempdir().unwrap();
        let mut capability = test_capability(temp.path());
        capability.tool_ids = vec!["WebFetch".into(), "WebSearch".into(), "Mcp".into()];
        capability.refresh_hash();
        let policy = test_policy(&capability);

        for tool in ["WebFetch", "WebSearch", "Mcp"] {
            let error = preflight_worker_capabilities(
                Some(&capability),
                Some(&policy),
                Some("session"),
                Some(capability.cwd.as_str()),
                &[temp.path().to_path_buf()],
                &[api_tool(tool)],
                "researcher",
            )
            .unwrap_err();
            assert_eq!(
                error.class,
                CapabilityDiagnosticClass::ToolUnavailable,
                "{tool} must not reach the network under a denied capability"
            );
            assert_eq!(error.capability.as_deref(), Some("network"), "{tool}");
        }
    }

    #[test]
    fn capability_preflight_scopes_network_grant_to_the_granted_tool() {
        let temp = tempfile::tempdir().unwrap();
        let mut capability = test_capability(temp.path());
        capability.tool_ids = vec!["WebFetch".into(), "WebSearch".into()];
        capability.network = NetworkCapability::ToolScoped(vec!["WebSearch".into()]);
        capability.refresh_hash();
        let policy = test_policy(&capability);

        let granted = preflight_worker_capabilities(
            Some(&capability),
            Some(&policy),
            Some("session"),
            Some(capability.cwd.as_str()),
            &[],
            &[api_tool("WebSearch")],
            "researcher",
        );
        assert!(granted.is_ok(), "{granted:?}");

        let ungranted = preflight_worker_capabilities(
            Some(&capability),
            Some(&policy),
            Some("session"),
            Some(capability.cwd.as_str()),
            &[],
            &[api_tool("WebFetch")],
            "researcher",
        )
        .unwrap_err();
        assert_eq!(ungranted.capability.as_deref(), Some("network"));
    }

    #[test]
    fn worker_spec_builder_composes_fields() {
        let spec = WorkerSpec::new("hello", "mock")
            .with_system("system")
            .with_allowed_tools(vec!["Read".into()])
            .with_max_iterations(3)
            .with_max_tokens(2048);
        assert_eq!(spec.system.as_deref(), Some("system"));
        assert_eq!(spec.max_iterations, 3);
        assert_eq!(spec.max_tokens, 2048);
    }

    struct BudgetProbeClient {
        includes_reasoning: bool,
    }

    #[async_trait::async_trait]
    impl rebon_api::ModelClient for BudgetProbeClient {
        fn provider_name(&self) -> &'static str {
            "budget-probe"
        }

        fn output_budget_includes_reasoning(&self) -> bool {
            self.includes_reasoning
        }

        async fn create_message_stream(
            &self,
            _request: rebon_api::CreateMessageRequest,
        ) -> rebon_api::ModelResult<rebon_api::StreamEventStream> {
            unreachable!("budget probe never issues requests")
        }
    }

    #[test]
    fn worker_max_tokens_scales_up_when_reasoning_shares_output_budget() {
        let reasoning = BudgetProbeClient {
            includes_reasoning: true,
        };
        let separate = BudgetProbeClient {
            includes_reasoning: false,
        };
        assert_eq!(
            worker_max_tokens_for_client(&reasoning),
            REASONING_WORKER_MAX_TOKENS
        );
        assert_eq!(
            worker_max_tokens_for_client(&separate),
            DEFAULT_WORKER_MAX_TOKENS
        );
    }
}
