//! [`EngineSubAgentSpawner`] — concrete [`SubAgentSpawner`]
//! implementation that wires `AgentTool` to a real
//! [`rebon_core::Engine`] + [`rebon_api::ModelClient`] via
//! [`worker::spawn_worker`](crate::runtime::worker::spawn_worker).
//!
//! The tool layer (`rebon-tool::AgentTool`) reads a
//! `Arc<dyn SubAgentSpawner>` from its [`rebon_tool::ToolContext`]
//! and calls it without knowing anything about the engine or the
//! model client. This crate provides the adapter that plugs the
//! two sides together.
//!
//! To avoid a reference cycle (`Engine → AgentTool → Spawner →
//! Engine`), the spawner holds a [`std::sync::Weak`] reference to
//! the engine. When the engine is dropped, the weak reference
//! fails to upgrade and the spawner returns a clean error.

mod builder;
mod spawn_paths;
mod sub_agent_spawner;
mod worker_run;
mod worker_setup;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;
use tokio::sync::watch;

use rebon_api::{
    ContentBlock as ApiContentBlock, Message as ApiMessage, ModelClient, ReasoningEffort, Role,
    SessionHandle, TextBlock, ToolChoice, Usage,
};
use rebon_core::query::{
    filtered_tools_from_engine, tools_from_engine, AttachmentPollPhase, AttachmentPollRequest,
    AttachmentPoller, AttachmentPollerBinding,
};
use rebon_core::turn_hook::{TurnEndHookEvent, TurnHook, TurnHookContext, TurnHookState};
use rebon_core::Engine;
use rebon_tool::{
    ensure_agent_id, AutoApproveExceptSensitivePermissionBroker, ContextShareMode,
    EscalationRegistry, FileContextMode, SharedToolFilter, SubAgentGitMetadata,
    SubAgentProgressSender, SubAgentResult, SubAgentSpawner, SubAgentSpec, SubAgentTaskKind,
    ToolFilter, ToolResultMode,
};
use rebon_types::{
    CapabilityDiagnostic, CapabilityDiagnosticClass, ModelProfileMap, PromptCancel, ToolCallStatus,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::runtime::worker::{
    preflight_worker_capabilities, spawn_worker, WorkerEvent, WorkerHandle, WorkerResult,
    WorkerSpec, WorkerStatus,
};
use rebon_agent_core::model_router::{
    is_inherit_sentinel, parse_reasoning_effort, AgentModelRouter, ModelRouteRequest,
    SingleProviderModelRouter,
};
use rebon_plugin_tasks::runtime::{
    assistant_text_delta, describe_worker_after_tool_activity, describe_worker_tool_activity,
    push_bounded_agent_transcript, take_pending_local_agent_messages,
    upsert_bounded_agent_thinking, LocalAgentData, LocalAgentTranscriptEntry, TaskData, TaskId,
    TaskKind, TaskLiveEventKind, TaskRegistry, TaskSnapshot, TaskStatus, TaskTurnToken,
};
use rebon_tool::external_agent::{
    compose_external_first_prompt, external_route_for_spec, ExternalRoute, ExternalSubAgentRunner,
    ExternalTaskEvent, ExternalTaskRequest, ExternalTaskStatus,
};

/// Last-resort fallback used when an embedding does not provide the
/// parent model through [`EngineSubAgentSpawner::with_default_model`].
/// CLI/TUI wiring passes the main agent model there, so ordinary
/// sub-agents inherit the parent model by default.
pub const DEFAULT_SUB_AGENT_MODEL: &str = "claude-sonnet-4-6";
const PERSISTENT_AGENT_MANAGED_KEY: &str = "__persistent_agent_actor_managed";
/// Metadata key carrying the worker's resolved scratchpad directory
/// across the detached-spawn hop, where `spec.cwd` is rewritten to a
/// worktree path before `spawn_inner` gets to see it.
const SCRATCHPAD_DIR_METADATA_KEY: &str = "__scratchpad_dir";
const FOLLOWUP_CONTEXT_MAX_CHARS: usize = 64 * 1024;

// The config-side model types live in `rebon-types` so `rebon-config` can
// build them without depending on this crate. Every caller names them there
// too — this crate forwarded them for a while and the forward is gone.
use rebon_types::SubAgentModelConfig;

/// The model label an externally-routed task shows in the registry and
/// task UI: `acp:<agent>:<hint|agent-default>`.
fn external_registry_model_label(route: &ExternalRoute) -> String {
    format!(
        "acp:{}:{}",
        route.agent_id,
        route.model_hint.as_deref().unwrap_or("agent-default")
    )
}

fn metadata_string(metadata: &Value, key: &str) -> Option<String> {
    metadata
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn metadata_category(metadata: &Value) -> Option<String> {
    for key in ["category", "agent_category", "agentCategory"] {
        if let Some(value) = metadata_string(metadata, key) {
            return Some(value);
        }
    }
    None
}

fn metadata_reasoning_effort(metadata: &Value) -> Option<ReasoningEffort> {
    for key in ["reasoning_effort", "reasoningEffort", "effort", "variant"] {
        if let Some(effort) =
            metadata_string(metadata, key).and_then(|raw| parse_reasoning_effort(&raw))
        {
            return Some(effort);
        }
    }
    None
}

fn effective_worker_max_iterations(requested: usize, is_coordinator_mode: bool) -> usize {
    if is_coordinator_mode {
        requested.max(rebon_tool::DEFAULT_SUB_AGENT_MAX_ITERATIONS)
    } else {
        requested
    }
}

#[derive(Debug, Clone, Copy)]
struct RuntimeWorktreePolicy {
    require_implementation: bool,
    allow_explicit: bool,
}

fn runtime_worktree_policy(
    is_coordinator_mode: bool,
    coordinator_use_worktree: bool,
) -> RuntimeWorktreePolicy {
    if is_coordinator_mode {
        RuntimeWorktreePolicy {
            require_implementation: coordinator_use_worktree,
            allow_explicit: true,
        }
    } else {
        RuntimeWorktreePolicy {
            require_implementation: false,
            allow_explicit: true,
        }
    }
}

#[derive(Debug, Clone)]
struct CapabilityFailureState {
    attempts: u32,
    diagnostic: CapabilityDiagnostic,
}

/// Plug-in that lets `AgentTool` spawn sub-agent workers.
pub struct EngineSubAgentSpawner {
    engine: Weak<Engine>,
    client: Arc<dyn ModelClient>,
    default_model: String,
    base_filter: Option<SharedToolFilter>,
    coordinator_mode: Option<rebon_tool::SharedCoordinatorMode>,
    coordinator_use_worktree: bool,
    model_config: SubAgentModelConfig,
    model_profiles: ModelProfileMap,
    model_router: Option<Arc<dyn AgentModelRouter>>,
    // 同一任务的后续派发共享最终运行时，父任务与兄弟任务以身份键隔离。
    automatic_routes: Arc<
        Mutex<
            HashMap<
                (String, String),
                Arc<
                    tokio::sync::Mutex<
                        Option<rebon_agent_core::model_router::ResolvedModelRuntime>,
                    >,
                >,
            >,
        >,
    >,
    /// Resolver for the exact session task seat. A concrete registry is set
    /// only on a short-lived clone after a spawn spec supplies its session id.
    task_registry_resolver: Option<rebon_plugin_tasks::TaskRegistryResolver>,
    task_registry: Option<TaskRegistry>,
    escalation_registry: EscalationRegistry,
    /// Optional session file-history tracker. When present it is
    /// injected into every worker's `ToolContext` so Write/Edit made
    /// by sub-agents snapshot the pre-write file, and `/rewind` can
    /// roll back worker changes. Without it, worker writes were
    /// invisible to file history.
    file_history_tracker: Option<Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>>,
    capability_failures: Arc<Mutex<HashMap<String, CapabilityFailureState>>>,
    ultraplan_worker_reservations: Arc<Mutex<HashMap<String, u32>>>,
    /// Executes sub-agents whose model spec routes to a declared ACP
    /// agent (`<agentId>:<model>`). `None` means every such spec falls
    /// back to the local worker path unchanged.
    external_runner: Option<Arc<dyn ExternalSubAgentRunner>>,
    /// The session's policy-event subscribers. Every worker this spawner
    /// starts asks them, tagged with its own agent id, so a `PreToolUse`
    /// guard a user configured covers delegated work too. Default-empty
    /// is what a worker had before: nobody to ask.
    policy: rebon_core::policy_seat::PolicySources,
}

impl Clone for EngineSubAgentSpawner {
    fn clone(&self) -> Self {
        Self {
            engine: self.engine.clone(),
            client: self.client.clone(),
            default_model: self.default_model.clone(),
            base_filter: self.base_filter.clone(),
            coordinator_mode: self.coordinator_mode.clone(),
            coordinator_use_worktree: self.coordinator_use_worktree,
            model_config: self.model_config.clone(),
            model_profiles: self.model_profiles.clone(),
            model_router: self.model_router.clone(),
            automatic_routes: self.automatic_routes.clone(),
            policy: self.policy.clone(),
            task_registry_resolver: self.task_registry_resolver.clone(),
            task_registry: self.task_registry.clone(),
            escalation_registry: self.escalation_registry.clone(),
            file_history_tracker: self.file_history_tracker.clone(),
            capability_failures: self.capability_failures.clone(),
            ultraplan_worker_reservations: self.ultraplan_worker_reservations.clone(),
            external_runner: self.external_runner.clone(),
        }
    }
}

impl std::fmt::Debug for EngineSubAgentSpawner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineSubAgentSpawner")
            .field("engine_alive", &(self.engine.upgrade().is_some()))
            .field("provider", &self.client.provider_name())
            .field("default_model", &self.default_model)
            .field("coordinator_use_worktree", &self.coordinator_use_worktree)
            .field(
                "has_task_registry_resolver",
                &self.task_registry_resolver.is_some(),
            )
            .field("has_escalation_registry", &true)
            .finish()
    }
}

fn capability_failure_scope(
    spec: &SubAgentSpec,
    role: &str,
    effective_filter: Option<&ToolFilter>,
) -> Option<String> {
    let capability = spec.capability_context.as_ref()?;
    let mut allowed_roots = spec
        .allowed_roots
        .iter()
        .map(|root| root.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    allowed_roots.sort();
    allowed_roots.dedup();
    let allow_tools = effective_filter.and_then(ToolFilter::allow_list);
    let deny_tools = effective_filter
        .map(ToolFilter::deny_list)
        .unwrap_or_default();
    let payload = serde_json::json!({
        "session_id": spec.metadata.get("parent_session_id").and_then(Value::as_str),
        "cwd": spec.cwd.as_deref(),
        "allowed_roots": allowed_roots,
        "allow_tools": allow_tools,
        "deny_tools": deny_tools,
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    Some(format!(
        "{}:{}:{}:{:x}",
        capability.run_id,
        capability.capability_hash,
        role,
        Sha256::digest(bytes)
    ))
}

fn scoped_capability_failure_fingerprint(
    failure_scope: Option<&str>,
    diagnostic: &CapabilityDiagnostic,
) -> String {
    let Some(failure_scope) = failure_scope else {
        return diagnostic.fingerprint();
    };
    let details = serde_json::to_vec(&(
        diagnostic.class,
        diagnostic.root.as_deref(),
        diagnostic.capability.as_deref(),
        diagnostic.message.as_str(),
    ))
    .unwrap_or_default();
    format!("{failure_scope}:{:x}", Sha256::digest(details))
}

fn circuit_diagnostic(diagnostic: &CapabilityDiagnostic, attempts: u32) -> CapabilityDiagnostic {
    CapabilityDiagnostic {
        class: CapabilityDiagnosticClass::CircuitOpen,
        message: format!(
            "worker capability preflight failed repeatedly; circuit opened after {attempts} attempt(s): {}",
            diagnostic.message
        ),
        retryable: false,
        fallback_to_parent: true,
        ..diagnostic.clone()
    }
}

fn repair_spec_from_capability(spec: &mut SubAgentSpec) {
    let Some(capability) = spec.capability_context.as_ref() else {
        return;
    };
    if spec.cwd.is_none() {
        spec.cwd = Some(capability.cwd.clone());
    }
    if spec.allowed_roots.is_empty() {
        spec.allowed_roots = capability.allowed_roots.iter().map(PathBuf::from).collect();
    }
    if !spec.metadata.is_object() {
        spec.metadata = serde_json::json!({});
    }
    if let Some(metadata) = spec.metadata.as_object_mut() {
        metadata.insert(
            "parent_session_id".to_string(),
            Value::String(capability.session_id.clone()),
        );
        metadata.insert(
            "ultraplan_capability_hash".to_string(),
            Value::String(capability.capability_hash.clone()),
        );
        metadata.insert(
            "ultraplan_ledger_revision".to_string(),
            Value::Number(capability.ledger_revision.into()),
        );
    }
    if let Some(ultraplan) = spec
        .execution_policy
        .as_mut()
        .and_then(|policy| policy.ultraplan.as_mut())
    {
        ultraplan.run_id = capability.run_id.clone();
        ultraplan.ledger_revision = capability.ledger_revision;
        ultraplan.requirements_hash = capability.requirements_hash.clone();
    }
}

async fn run_persistent_agent_actor(
    spawner: EngineSubAgentSpawner,
    registry: TaskRegistry,
    task_id: TaskId,
    base_spec: SubAgentSpec,
) {
    let Some(wake) = registry.task_waker(&task_id) else {
        return;
    };
    let Some(cancel) = registry.cancel_handle(&task_id) else {
        return;
    };

    loop {
        if cancel.is_cancelled() {
            break;
        }
        let notified = wake.notified();
        tokio::pin!(notified);
        let _ = notified.as_mut().enable();

        let Some(snapshot) = registry.snapshot(&task_id) else {
            break;
        };
        if snapshot.status.is_terminal() {
            break;
        }
        if registry.task_turn_is_active(&task_id) {
            tokio::select! {
                _ = cancel.notified() => break,
                _ = &mut notified => continue,
            }
        }

        let Some(message_lease) = registry.lease_pending_local_agent_messages(&task_id) else {
            tokio::select! {
                _ = cancel.notified() => break,
                _ = &mut notified => continue,
            }
        };

        let Some(snapshot) = registry.snapshot(&task_id) else {
            break;
        };
        let mut followup_spec = base_spec.clone();
        followup_spec.prompt =
            persistent_agent_followup_prompt(&snapshot, message_lease.messages());
        if let Some(metadata) = followup_spec.metadata.as_object_mut() {
            metadata.insert(PERSISTENT_AGENT_MANAGED_KEY.into(), Value::Bool(true));
        }
        match spawner.spawn_inner(followup_spec, None).await {
            Ok(_) => message_lease.commit(),
            Err(error) => {
                let turn_started = registry.fail_active_task_turn(&task_id, error.clone());
                let message_generation = message_lease.message_generation();
                if turn_started {
                    message_lease.commit();
                } else {
                    drop(message_lease);
                    registry.fail_idle_task_turn(&task_id, error.clone());
                }
                tracing::warn!(
                    agent_id = %task_id,
                    error = %error,
                    turn_started,
                    "persistent agent follow-up turn failed"
                );
                if !turn_started {
                    drop(notified);
                    if !wait_for_new_local_agent_message(
                        &registry,
                        &task_id,
                        &wake,
                        &cancel,
                        message_generation,
                    )
                    .await
                    {
                        break;
                    }
                }
            }
        }
    }
}

async fn wait_for_new_local_agent_message(
    registry: &TaskRegistry,
    task_id: &TaskId,
    wake: &Arc<tokio::sync::Notify>,
    cancel: &PromptCancel,
    after_generation: u64,
) -> bool {
    loop {
        if cancel.is_cancelled() {
            return false;
        }
        let notified = wake.notified();
        tokio::pin!(notified);
        let _ = notified.as_mut().enable();
        let Some(snapshot) = registry.snapshot(task_id) else {
            return false;
        };
        if snapshot.status.is_terminal() {
            return false;
        }
        if registry
            .local_agent_message_generation(task_id)
            .is_some_and(|generation| generation > after_generation)
        {
            return true;
        }
        tokio::select! {
            _ = cancel.notified() => return false,
            _ = &mut notified => {}
        }
    }
}

fn persistent_agent_followup_prompt(snapshot: &TaskSnapshot, pending: &[String]) -> String {
    let mut entries = match &snapshot.data {
        TaskData::LocalAgent(data) => data.transcript.as_slice(),
        _ => &[],
    };
    let mut trim_pending = pending.iter().rev();
    while let (Some(LocalAgentTranscriptEntry::User { text }), Some(message)) =
        (entries.last(), trim_pending.next())
    {
        if text != message {
            break;
        }
        entries = &entries[..entries.len() - 1];
    }

    let mut history = String::new();
    for entry in entries {
        let line = match entry {
            LocalAgentTranscriptEntry::User { text } => format!("User: {text}"),
            LocalAgentTranscriptEntry::Thinking { .. } => continue,
            LocalAgentTranscriptEntry::Assistant { text } => format!("Assistant: {text}"),
            LocalAgentTranscriptEntry::ToolStart { name, input, .. } => {
                format!("Tool {name} input: {input}")
            }
            LocalAgentTranscriptEntry::ToolProgress { name, message, .. } => {
                format!("Tool {name} progress: {message}")
            }
            LocalAgentTranscriptEntry::ToolFinish {
                name,
                summary,
                outcome,
                ..
            } => match outcome {
                Ok(value) => format!("Tool {name} result ({summary}): {value}"),
                Err(error) => format!("Tool {name} error ({summary}): {error}"),
            },
        };
        history.push_str(&line);
        history.push('\n');
    }
    history = tail_chars(&history, FOLLOWUP_CONTEXT_MAX_CHARS);

    format!(
        "Continue the same agent task using the retained transcript below. Treat the new message as the next user turn; do not restart the investigation from scratch.\n\n<retained-agent-transcript>\n{history}</retained-agent-transcript>\n\n<new-message>\n{}\n</new-message>",
        pending.join("\n\n---\n\n")
    )
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    text.chars().skip(char_count - max_chars).collect()
}

/// What a finished worker run reports about itself.
struct WorkerAccounting {
    total_tokens: u64,
    usage_json: serde_json::Value,
    error: Option<String>,
    output_file: Option<String>,
}

/// Read the accounting off a finished worker run.
///
/// The error is the report validation's when the worker produced an invalid
/// report and the run's own otherwise: a worker that finished cleanly but
/// wrote an unusable report has still failed the caller, and saying so here
/// keeps the two failure kinds from racing.
fn worker_accounting(
    result: &WorkerResult,
    report_file_path: Option<String>,
    report_invalid: bool,
    report_validation: Option<rebon_core::coordinator_mode::WorkerReportValidation>,
) -> WorkerAccounting {
    // Extract usage information from the worker result.
    let (total_tokens, usage_json) = {
        let u = &result.total_usage;
        let total = u64::from(u.billed_input_tokens()) + u64::from(u.output_tokens);
        let usage = serde_json::json!({
            "input_tokens": u.billed_input_tokens(),
            "output_tokens": u.output_tokens,
            "cumulative_output_tokens": result.cumulative_output_tokens,
            "cache_creation_input_tokens": u.cache_creation_input_tokens,
            "cache_read_input_tokens": u.cache_read_input_tokens,
            "prompt_cache_hit_tokens": u.prompt_cache_hit_tokens,
            "prompt_cache_miss_tokens": u.prompt_cache_miss_tokens,
        });
        (total, usage)
    };

    // `agent_id` / `agent_type` were minted at the top of this
    // function so the TaskRegistry insert above could key on them.
    // Reuse them unchanged here.

    let error = if report_invalid {
        let validation_summary = report_validation
            .as_ref()
            .map(|validation| validation.failure_summary())
            .unwrap_or_else(|| "report is missing, empty, or structurally invalid".to_string());
        Some(format!(
            "Worker completed but did not leave a valid required report file at {} for this turn. \
             The harness coerced once and the file is still missing, empty, structurally invalid, \
             or unchanged from the previous turn. \
             Validation failure: {}. Coordinator should send this worker a corrected follow-up if \
             it is still continuable, otherwise spawn a fresh worker with a clearer prompt.",
            report_file_path.as_deref().unwrap_or("(unknown)"),
            validation_summary
        ))
    } else {
        result.error.clone()
    };

    let output_file = coordinator_output_file(report_file_path, report_invalid);
    WorkerAccounting {
        total_tokens,
        usage_json,
        error,
        output_file,
    }
}

/// What the finished worker leaves for the caller to return.
struct WorkerClosingState {
    diagnostics: serde_json::Value,
    sub_agent_tool_calls: Vec<serde_json::Value>,
}

/// The inputs the closing bookkeeping reads. A bundle because the list is the
/// shape of the thing: everything the run produced, plus the identity it ran
/// under.
struct WorkerClosingInputs<'a> {
    task_id: &'a TaskId,
    task_list_id: &'a str,
    agent_id: &'a str,
    status: &'a str,
    error: &'a Option<String>,
    output_file: &'a Option<String>,
    duration_ms: u64,
    total_tokens: u64,
    usage_json: &'a serde_json::Value,
    registry_model: &'a String,
    registry_provider: &'a String,
    git_metadata: &'a Option<SubAgentGitMetadata>,
    keep_runtime_resumable: bool,
    report_invalid: bool,
}

/// The worker's tool-facing context, and the ids it is filed under.
struct WorkerToolContext {
    child_context: rebon_tool::ToolContext,
    task_id: TaskId,
    task_list_id: String,
}

/// The prompt, the toolkit and the cache policy the worker will run under.
struct WorkerRunPlan {
    auto_report_from_final_text: bool,
    tool_filter: Option<ToolFilter>,
    effective_cache_strategy: rebon_tool::CacheStrategy,
    cache_context_policy: Option<String>,
    system: Option<String>,
    report_file_path: Option<String>,
    prompt: String,
}

/// Where the worker will run, and under whose lifetime.
struct WorkerPlacement {
    should_spawn_persistent_actor: bool,
    keep_runtime_resumable: bool,
    scratchpad_dir: Option<String>,
    scratchpad_root: Option<PathBuf>,
    is_verification_agent: bool,
    is_coord: bool,
    git_metadata: Option<SubAgentGitMetadata>,
}

/// How the worker is filed in the registry, and which runtime it resolved to.
struct WorkerRegistryPlan<'a> {
    persistent_actor_spec: Option<SubAgentSpec>,
    structured_output_channel: Option<Arc<rebon_tool::StructuredOutputChannel>>,
    registry_agent_type: String,
    registry_title: String,
    registry_prompt: String,
    requested_model_profile: Option<&'a str>,
    resolved_runtime: rebon_agent_core::model_router::ResolvedModelRuntime,
    registry_model: String,
    registry_model_for_snapshot: String,
    registry_provider: String,
    reasoning_effort: Option<ReasoningEffort>,
    registry_run_in_background: bool,
    permission_prompts_unavailable: bool,
}

/// Fold a deferred worktree integration into the worker's result.
///
/// Deferred integration is not worker failure: the task finished and the
/// changes are intact on the preserved branch. The status stays `completed`
/// and the notice is appended to the final text, so the parent relays the
/// worktree location instead of redoing the task in the shared tree.
fn finalize_agent_worktree_integration(
    result: &mut WorkerResult,
    git_metadata: &mut Option<SubAgentGitMetadata>,
    status: &mut String,
    agent_id: &String,
) {
    // Deferred integration is not worker failure: the task finished and
    // the changes are intact on the preserved branch. Keep `completed`
    // and append the notice to the final text so the parent relays the
    // worktree location instead of redoing the task in the shared tree.
    let integration_notice: Option<String> = if let Some(ref mut git) = git_metadata {
        if status == "completed" {
            match agent_worktree_info_from_metadata(git) {
                Ok(info) => {
                    let integration = rebon_tool::worktree::finalize_agent_worktree(
                        &info,
                        &format!("agent: integrate {agent_id} changes"),
                    );
                    git.apply_integration_result(&integration);
                    // A damaged worktree is not a clean integration
                    // either: nothing merged, and the directory is
                    // unusable. Only a removed worktree is "passed".
                    if integration.preserves_worktree() || integration.worktree_damaged() {
                        git.validation_status = Some("deferred".to_string());
                        git.validation_error = None;
                        Some(
                            integration.deferred_notice(&info.worktree_path, &info.worktree_branch),
                        )
                    } else {
                        git.validation_status = Some("passed".to_string());
                        git.validation_error = None;
                        None
                    }
                }
                Err(error) => {
                    git.integration_status = Some("metadata_invalid".to_string());
                    git.integration_error = Some(error.clone());
                    git.worktree_preserved = Some(true);
                    git.validation_status = Some("deferred".to_string());
                    git.validation_error = Some(error.clone());
                    Some(format!(
                        "worktree integration skipped (metadata_invalid): {error}. The \
                         agent's completed changes remain in its worktree and were NOT \
                         merged into the source branch."
                    ))
                }
            }
        } else {
            git.integration_status = Some("preserved_not_completed".to_string());
            git.integration_error = result.error.clone();
            git.worktree_preserved = Some(true);
            git.validation_status = Some("skipped".to_string());
            None
        }
    } else {
        None
    };
    if let Some(ref notice) = integration_notice {
        result.final_text = if result.final_text.trim().is_empty() {
            format!("[worktree] {notice}")
        } else {
            format!("{}\n\n[worktree] {notice}", result.final_text.trim_end())
        };
    }
}

/// The verdict on the worker's mandated report.
struct WorkerReportVerdict {
    report_validation: Option<rebon_core::coordinator_mode::WorkerReportValidation>,
    report_invalid: bool,
    status: String,
}

/// The worker handle's inputs, assembled.
struct WorkerLaunchSpec {
    start_time_ms: u64,
    session: Arc<SessionHandle>,
    registry_system: Option<String>,
    registry_allowed_tools: Option<Vec<String>>,
    worker_spec: WorkerSpec,
}

/// The running worker, and the bookkeeping the wait loop needs.
struct WorkerLaunch {
    start: Instant,
    handle: WorkerHandle,
    turn: Option<TaskTurnToken>,
    background_request: Option<tokio::sync::watch::Receiver<bool>>,
}

/// The turn-end contracts the worker must satisfy, and their diagnostics.
struct WorkerTurnContracts {
    validator_lifecycle: Arc<ValidatorLifecycleDiagnostics>,
    turn_hook: Option<Arc<dyn TurnHook>>,
}

/// The result a detached launch reports back immediately.
///
/// The worker keeps running; this call's job is over. `agent_id`,
/// `agent_type` and `git_metadata` are taken by value because the caller
/// returns right after handing them over.
fn detached_launch_result(
    spec: &SubAgentSpec,
    start: Instant,
    agent_id: String,
    agent_type: Option<String>,
    registry_provider: &String,
    registry_model: &String,
    git_metadata: Option<SubAgentGitMetadata>,
) -> SubAgentResult {
    SubAgentResult {
        final_text: String::new(),
        status: "async_launched".to_string(),
        tool_call_count: 0,
        sub_agent_tool_calls: None,
        read_file_count: Some(0),
        stop_reason: None,
        output_file: None,
        duration_ms: Some(start.elapsed().as_millis() as u64),
        error: None,
        agent_id: Some(agent_id),
        agent_type,
        provider: Some(registry_provider.clone()),
        model: Some(registry_model.clone()),
        total_tokens: None,
        output_tokens: None,
        usage: None,
        diagnostics: spec.capability_context.as_ref().map(|context| {
            serde_json::json!({
                "capability": {
                    "run_id": context.run_id,
                    "ledger_revision": context.ledger_revision,
                    "requirements_hash": context.requirements_hash,
                    "capability_hash": context.capability_hash,
                    "session_id": context.session_id,
                }
            })
        }),
        git: git_metadata,
    }
}

/// Scratchpad directory advertised in a worker's prompt and used for
/// the deletion permission carve-out. Keyed by the parent session id
/// when known — sharing the parent session's scratchpad so the
/// spawning conversation can inspect worker artifacts — else by the
/// worker's own agent id. Must be computed BEFORE worktree
/// preparation rewrites `spec.cwd`, so the key stays anchored to the
/// real project directory.
fn sub_agent_scratchpad_dir(
    spec: &SubAgentSpec,
    owner_session_id: Option<&str>,
    agent_id: &str,
) -> Option<String> {
    let cwd = spec.cwd.clone().or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|dir| dir.display().to_string())
    })?;
    Some(rebon_core::system_prompt::scratchpad_dir_for(
        &cwd,
        owner_session_id.unwrap_or(agent_id),
    ))
}

/// Sub-agent system prompt suffix: shared notes plus, when a
/// scratchpad could be resolved, the scratchpad pointer.
fn compose_sub_agent_prompt_suffix(scratchpad_dir: Option<&str>) -> String {
    match scratchpad_dir {
        Some(dir) => rebon_core::system_prompt::sub_agent_prompt_suffix(dir),
        None => rebon_core::system_prompt::sub_agent_notes_section().to_string(),
    }
}

fn build_worker_messages(spec: &SubAgentSpec, prompt: String) -> Vec<rebon_api::Message> {
    match spec.context.as_ref() {
        Some(context) if !matches!(context.mode, ContextShareMode::None) => {
            let capsule = spec
                .frozen_parent_context
                .as_ref()
                .map(|capsule| capsule.text.to_string())
                .unwrap_or_else(|| fallback_context_capsule(context));
            vec![
                rebon_api::Message::user_text(capsule),
                rebon_api::Message::user_text(prompt),
            ]
        }
        _ => vec![rebon_api::Message::user_text(prompt)],
    }
}

fn cache_api_path(provider_name: &str) -> &'static str {
    match provider_name {
        "openai-responses" => "openai_responses",
        "openai-compatible" => "openai_chat_completions",
        "anthropic" => "anthropic_messages",
        _ => "other",
    }
}

fn build_worker_cache_trace_context(
    cache_strategy: rebon_tool::CacheStrategy,
    context_policy: Option<String>,
    provider: &str,
    model: &str,
    model_profile: Option<&str>,
    system: Option<&str>,
    api_path: Option<&str>,
    tools_hash: String,
    schema_hash: String,
    capsule: Option<&rebon_tool::FrozenParentContextCapsule>,
    messages: &[rebon_api::Message],
) -> rebon_api::CacheTraceContext {
    let shared_preamble_hash = system.map(rebon_api::stable_hash_str);
    let profile_preamble = format!(
        "provider={provider};model={model};profile={}",
        model_profile.unwrap_or("")
    );
    let profile_preamble_hash = rebon_api::stable_hash_str(&profile_preamble);
    let stable_prefix_hash = rebon_api::stable_hash_str(&format!(
        "provider={provider};model={model};tools={tools_hash};schema={schema_hash};profile={profile_preamble_hash}"
    ));
    let task_hash = messages.last().and_then(|message| {
        message
            .content
            .first()
            .and_then(|block| block.as_text())
            .map(rebon_api::stable_hash_str)
    });
    let shared_profile_tokens = estimate_worker_tokens(system.unwrap_or(""))
        .saturating_add(estimate_worker_tokens(&profile_preamble));
    let stable_prefix_tokens = shared_profile_tokens;
    let tokens_before_capsule = Some(stable_prefix_tokens);
    let tokens_before_task = Some(
        stable_prefix_tokens
            .saturating_add(capsule.map(|capsule| capsule.token_estimate).unwrap_or(0)),
    );
    let prompt_cache_retention = match cache_strategy {
        rebon_tool::CacheStrategy::StableContextCapsule => Some("same_dispatch".to_string()),
        rebon_tool::CacheStrategy::ProviderNative
        | rebon_tool::CacheStrategy::ProviderNativeContinuation => {
            Some("provider_native".to_string())
        }
        rebon_tool::CacheStrategy::Auto
        | rebon_tool::CacheStrategy::Fresh
        | rebon_tool::CacheStrategy::NoCache => None,
    };
    let cache_key = match cache_strategy {
        rebon_tool::CacheStrategy::NoCache | rebon_tool::CacheStrategy::Fresh => None,
        rebon_tool::CacheStrategy::StableContextCapsule => capsule.map(|capsule| {
            format!(
                "rebon-worker-dispatch-{provider}-{model}-{stable_prefix_hash}-{}",
                capsule.hash
            )
        }),
        _ => Some(format!(
            "rebon-worker-{provider}-{model}-{stable_prefix_hash}"
        )),
    };
    rebon_api::CacheTraceContext {
        context_policy,
        prompt_cache_key: cache_key,
        prompt_cache_retention,
        api_path: api_path.map(str::to_string),
        previous_response_id_present: None,
        tools_hash: Some(tools_hash),
        schema_hash: Some(schema_hash),
        shared_preamble_hash,
        profile_preamble_hash: Some(profile_preamble_hash),
        capsule_hash: capsule.map(|capsule| capsule.hash.clone()),
        task_hash,
        tokens_before_capsule,
        tokens_before_task,
        cross_run_cache_eligible: Some(stable_prefix_tokens >= 1024),
        same_dispatch_cache_eligible: tokens_before_task.map(|tokens| tokens >= 1024),
    }
}

fn estimate_worker_tokens(value: &str) -> u32 {
    let chars = value.chars().count();
    chars.div_ceil(4).min(u32::MAX as usize) as u32
}

fn estimate_trace_tools_tokens(tools: &[rebon_api::Tool]) -> u32 {
    let serialized = serde_json::to_string(tools).unwrap_or_default();
    estimate_worker_tokens(&serialized)
}

fn fallback_context_capsule(context: &rebon_tool::ContextRequest) -> String {
    let mut payload = String::new();
    payload.push_str("Handoff model: fresh worker runtime with curated semantic context. Do not replay prior tool calls or side effects; treat any observations as facts only.\n");
    payload.push_str(&format!("Context mode: {}\n", context.mode.as_str()));
    if let Some(turns) = context.include_recent_turns {
        payload.push_str(&format!("Requested recent turns: {turns}\n"));
    }
    payload.push_str(&format!(
        "Tool results: {}\n",
        context.include_tool_results.as_str()
    ));
    payload.push_str(&format!(
        "File context: {}\n",
        context.include_files.as_str()
    ));
    if let Some(instructions) = context.instructions.as_deref() {
        payload.push_str("Additional handoff instructions:\n");
        payload.push_str(instructions.trim());
        payload.push('\n');
    }
    match context.mode {
        ContextShareMode::PlanOnly => {
            payload.push_str("Expected context: use only the explicit task prompt and any plan/instructions included there.\n");
        }
        ContextShareMode::Compact | ContextShareMode::CompactWithRecentTurns => {
            payload.push_str("Expected context: compact semantic handoff only. The parent did not attach a raw transcript in this MVP.\n");
        }
        ContextShareMode::TranscriptSlice | ContextShareMode::None => {}
    }
    if !matches!(context.include_tool_results, ToolResultMode::None) {
        payload.push_str("Tool fact policy: previously observed tool outputs may be summarized as facts, but historical Bash/Edit/Write/MCP/HTTP mutations must not be re-executed unless explicitly needed for this task.\n");
    }
    if !matches!(context.include_files, FileContextMode::None) {
        payload.push_str("File context policy: rely on explicit file paths and snippets in the task prompt; inspect current files before editing.\n");
    }
    let id = format!("sha256:{:x}", Sha256::digest(payload.as_bytes()));
    format!("<ContextCapsule id=\"{id}\" version=\"1\">\n{payload}</ContextCapsule>")
}

fn prepend_ultraplan_worker_preamble(
    base_system: Option<String>,
    execution_policy: Option<&rebon_tool::ExecutionPolicy>,
    metadata: &Value,
) -> Option<String> {
    let Some(context) = execution_policy.and_then(|policy| policy.ultraplan.as_ref()) else {
        return base_system;
    };
    let role = metadata
        .get("ultraplan_role")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("researcher");
    let mode = match context.mode {
        rebon_tool::PolicyMode::Observe => "observe",
        rebon_tool::PolicyMode::Enforce => "enforce",
    };
    let restrictions = if context.read_only {
        "Restrictions: this is a local-only, read-only planning/review worker. Do not use team/teammate behavior, remote agents, web, PR, teleport, or external/network behavior. Do not edit project files or run shell commands."
    } else if context.plan_fidelity {
        "Permissions: this implementation worker is in plan-fidelity execution. The approved plan's evidence is pre-verified: do NOT re-explore the repository. Follow only the relevant Execution Cards embedded below; before editing, Read only the listed target file/range plus necessary context. Broad Glob/Grep sweeps and Explore-agent fan-out are forbidden unless you first declare DEVIATION with the reason (hash drift or plan gap). If the assigned step is unimplementable as written, stop and ask rather than silently re-plan."
    } else {
        "Permissions: this is a writable implementation worker. It may edit project files and run shell commands within its assigned repository/worktree and tool allow-list, and may explore the repository (Read/Glob/Grep) as needed. Do not use team/teammate behavior, remote agents, web, PR, teleport, or external/network behavior."
    };
    let execution_cards = format_ultraplan_worker_execution_cards(context, metadata);
    let hash_drift = format_ultraplan_worker_hash_drift(context);
    let preamble = format!(
        "REBON LOCAL ULTRAPLAN WORKER CONTEXT\n\
         - ultraplan_id: {run_id}\n\
         - ultraplan_phase: {phase}\n\
         - ultraplan_role: {role}\n\
         - ultraplan_policy_mode: {mode}\n\
         {restrictions}\n\
         Evidence expectations: report concrete findings with absolute file paths, line numbers, and relevant snippets or command/test output. Do not rely on hidden parent context; include enough detail for the main planner to verify your evidence.\n\n\
         {execution_cards}\n\n\
         {hash_drift}",
        run_id = context.run_id,
        phase = context.phase,
        role = role,
        mode = mode,
        restrictions = restrictions,
        execution_cards = execution_cards,
        hash_drift = hash_drift,
    );
    Some(match base_system {
        Some(base) if !base.trim().is_empty() => format!("{preamble}\n\n{base}"),
        _ => preamble,
    })
}

fn ultraplan_selected_card_ids(metadata: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    for key in [
        "execution_card",
        "execution_card_step",
        "executionCard",
        "executionCardStep",
    ] {
        if let Some(value) = metadata.get(key).and_then(Value::as_str) {
            ids.push(value.trim().to_string());
        }
    }
    for key in [
        "execution_cards",
        "execution_card_steps",
        "executionCards",
        "executionCardSteps",
    ] {
        if let Some(values) = metadata.get(key).and_then(Value::as_array) {
            ids.extend(
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string),
            );
        }
    }
    ids.retain(|id| !id.is_empty());
    ids
}

fn format_ultraplan_worker_execution_cards(
    context: &rebon_tool::UltraplanContext,
    metadata: &Value,
) -> String {
    if context.execution_cards.is_empty() {
        return "Execution Cards: none supplied in run state. Declare DEVIATION before broad repository re-exploration.".to_string();
    }
    let selected = ultraplan_selected_card_ids(metadata);
    let cards: Vec<_> = if selected.is_empty() {
        context.execution_cards.iter().collect()
    } else {
        context
            .execution_cards
            .iter()
            .filter(|card| {
                selected.iter().any(|id| {
                    card.step == *id || card.covers.as_deref().is_some_and(|covers| covers == id)
                })
            })
            .collect()
    };
    let cards = if cards.is_empty() {
        context.execution_cards.iter().collect::<Vec<_>>()
    } else {
        cards
    };
    let mut text = String::from("Execution Cards from run state:\n");
    for card in cards {
        text.push_str(&format!("- Step {}", card.step));
        if let Some(covers) = card.covers.as_ref() {
            text.push_str(&format!(" [COVERS:{covers}]"));
        }
        text.push_str(&format!(
            "\n  files: {}\n  change: {}\n  verify: {}\n",
            card.files.join(", "),
            card.change,
            card.verify
        ));
    }
    text
}

fn format_ultraplan_worker_hash_drift(context: &rebon_tool::UltraplanContext) -> String {
    if context.hash_drift.is_empty() {
        return "Hash Drift Guard: no drift recorded for stored Execution Card files.".to_string();
    }
    let mut text = String::from("Hash Drift Guard: DEVIATION recorded before execution. Reconcile current file contents before editing.\n");
    for record in &context.hash_drift {
        text.push_str(&format!("- DEVIATION {:?}: {}", record.kind, record.path));
        if let Some(current) = record.current_sha256.as_ref() {
            text.push_str(&format!(
                " (stored {}, current {})",
                record.stored_sha256, current
            ));
        } else {
            text.push_str(&format!(" (stored {})", record.stored_sha256));
        }
        text.push('\n');
    }
    text
}

fn safe_report_file_stem(agent_id: &str) -> String {
    let mut stem = String::with_capacity(agent_id.len());
    for ch in agent_id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            stem.push(ch);
        } else {
            stem.push('_');
        }
    }
    if stem.is_empty() {
        "agent-report".to_string()
    } else {
        stem
    }
}

/// Which report path the task-notification hands to the coordinator —
/// which is also the path the coordinator is allowed to Read.
///
/// A structurally invalid report is exactly what the coordinator needs
/// to see: the failure message names the path and tells it to respawn
/// with a clearer prompt, and it cannot write a better prompt without
/// reading what the worker actually produced. Withhold the path only
/// when there is no file behind it.
fn coordinator_output_file(
    report_file_path: Option<String>,
    report_invalid: bool,
) -> Option<String> {
    report_file_path.filter(|path| {
        !report_invalid
            || std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() > 0)
    })
}

fn report_file_path_for_agent_id(agent_id: &str) -> String {
    let tasks_dir = tasks_output_dir();
    let _ = std::fs::create_dir_all(&tasks_dir);
    tasks_dir
        .join(format!("{}.report.md", safe_report_file_stem(agent_id)))
        .to_string_lossy()
        .to_string()
}

fn is_explore_agent_type(agent_type: Option<&str>) -> bool {
    agent_type.is_some_and(|agent_type| agent_type.trim().eq_ignore_ascii_case("Explore"))
}

fn should_auto_report_from_final_text(agent_type: Option<&str>) -> bool {
    is_explore_agent_type(agent_type)
}

fn auto_write_report_file(report_file_path: &str, final_text: &str) -> std::io::Result<()> {
    if let Some(parent) = std::path::Path::new(report_file_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let final_text = final_text.trim();
    let (summary, evidence) = if final_text.is_empty() {
        (
            "Auto-report was generated from empty final text; no worker summary was provided.",
            "Auto-report was generated from empty final text; no worker evidence was provided.",
        )
    } else {
        (final_text, final_text)
    };
    let report = format!(
        "# Explore Agent Report\n\n\
## Summary\n\n{summary}\n\n\
## Files Changed / Inspected\n\nNo files changed by read-only Explore worker. Files inspected are listed in the Evidence section when present in the final text.\n\n\
## Evidence\n\n{evidence}\n\n\
## Verification / Tests\n\nNo tests were run by the read-only Explore worker unless explicitly stated in the Evidence section.\n\n\
## Blockers / Assumptions\n\nNo blockers or assumptions were reported in the final text unless explicitly stated in the Evidence section.\n\n\
## Final Status\n\nCompleted Explore report auto-generated from the worker final text.\n"
    );
    std::fs::write(report_file_path, report)
}

fn effective_path_scope_roots(spec: &SubAgentSpec) -> Vec<PathBuf> {
    if !spec.allowed_roots.is_empty() {
        return spec.allowed_roots.clone();
    }
    spec.cwd.as_deref().map(PathBuf::from).into_iter().collect()
}

fn prepare_runtime_worktree(
    spec: &mut SubAgentSpec,
    agent_id: &str,
    task_kind: SubAgentTaskKind,
    policy: RuntimeWorktreePolicy,
) -> Result<Option<SubAgentGitMetadata>, String> {
    let explicit_worktree = spec
        .metadata
        .get("isolation")
        .and_then(Value::as_str)
        .is_some_and(|v| v.eq_ignore_ascii_case("worktree"));
    let required = policy.require_implementation && task_kind == SubAgentTaskKind::Implementation;
    if !required && !(policy.allow_explicit && explicit_worktree) {
        return Ok(None);
    }
    let cwd_base = rebon_tool::worktree::authorized_worktree_base(
        spec.cwd.as_deref().map(Path::new),
        &spec.allowed_roots,
    )
    .map_err(|err| err.to_string())?;
    spec.cwd = Some(cwd_base.to_string_lossy().to_string());
    // `cwd` no longer points at whatever tree the caller inherited, so any
    // inherited isolation claim is stale from here on. Only the success arm
    // below re-establishes it.
    spec.runtime_isolated_worktree = false;
    let slug = build_spawner_worktree_slug(agent_id);
    match rebon_tool::worktree::create_authorized_agent_worktree(
        &cwd_base,
        &spec.allowed_roots,
        &slug,
    ) {
        Ok(info) => {
            spec.cwd = Some(info.worktree_path.to_string_lossy().to_string());
            spec.allowed_roots = vec![info.worktree_path.clone()];
            // The runtime just created this tree for this worker alone.
            spec.runtime_isolated_worktree = true;
            Ok(Some(SubAgentGitMetadata::from_worktree_info(&info)))
        }
        Err(err) if required => Err(format!(
            "required implementation worktree creation failed for {agent_id}: {err}"
        )),
        Err(err) => {
            tracing::warn!(slug = %slug, error = %err, "agent worktree creation failed");
            // Degrading to the shared tree must not be silent: tell the
            // worker it is NOT isolated and record the reason so the
            // parent side can surface it.
            let notice = format!(
                "NOTE: worktree isolation was requested but could not be created ({err}). \
                 You are running directly in the shared working tree; treat pre-existing \
                 and concurrent changes as owned work and keep your edits minimal."
            );
            spec.system = Some(match spec.system.take() {
                Some(system) if !system.trim().is_empty() => format!("{system}\n\n{notice}"),
                _ => notice,
            });
            if let Some(obj) = spec.metadata.as_object_mut() {
                obj.insert(
                    "worktree_isolation_error".into(),
                    Value::String(err.to_string()),
                );
            }
            Ok(None)
        }
    }
}

fn build_spawner_worktree_slug(agent_id: &str) -> String {
    // Agent ids are usually already `agent-…`; strip the conventional
    // prefix before re-applying it so slugs don't come out `agent-agent-…`.
    let agent_id = agent_id.strip_prefix("agent-").unwrap_or(agent_id);
    let mut slug = String::from("agent-");
    for ch in agent_id.chars().take(48) {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            slug.push(ch);
        } else {
            slug.push('_');
        }
    }
    slug.truncate(64);
    slug
}

fn agent_worktree_info_from_metadata(
    git: &SubAgentGitMetadata,
) -> Result<rebon_tool::worktree::AgentWorktreeInfo, String> {
    Ok(rebon_tool::worktree::AgentWorktreeInfo {
        worktree_path: PathBuf::from(
            git.worktree_path
                .as_deref()
                .ok_or_else(|| "missing worktree_path for integration".to_string())?,
        ),
        worktree_branch: git
            .worktree_branch
            .clone()
            .ok_or_else(|| "missing worktree_branch for integration".to_string())?,
        head_commit: git
            .base_commit
            .clone()
            .ok_or_else(|| "missing base_commit for integration".to_string())?,
        source_worktree: PathBuf::from(
            git.source_worktree
                .as_deref()
                .ok_or_else(|| "missing source_worktree for integration".to_string())?,
        ),
        source_branch: git.source_branch.clone(),
        git_root: PathBuf::from(
            git.git_root
                .as_deref()
                .ok_or_else(|| "missing git_root for integration".to_string())?,
        ),
    })
}

#[cfg(test)]
fn validate_implementation_commit(
    git: &mut SubAgentGitMetadata,
    report_file_path: Option<&str>,
) -> Result<(), String> {
    let worktree_path = git
        .worktree_path
        .as_deref()
        .ok_or_else(|| "missing worktree_path for implementation validation".to_string())?;
    let branch = git
        .worktree_branch
        .clone()
        .ok_or_else(|| "missing worktree_branch for implementation validation".to_string())?;
    let base = git
        .base_commit
        .clone()
        .ok_or_else(|| "missing base_commit for implementation validation".to_string())?;
    let path = Path::new(worktree_path);
    let head = rebon_tool::worktree::git_current_head(path).map_err(|err| err.to_string())?;
    git.head_commit = Some(head.clone());
    let current_branch =
        rebon_tool::worktree::git_current_branch(path).map_err(|err| err.to_string())?;
    if current_branch != branch {
        return Err(format!(
            "implementation commit validation failed: current branch `{current_branch}` does not match required branch `{branch}`"
        ));
    }
    if head == base {
        return Err(
            "implementation commit validation failed: HEAD did not advance beyond base commit"
                .to_string(),
        );
    }
    if !rebon_tool::worktree::git_is_ancestor(path, &base, &head).map_err(|err| err.to_string())? {
        return Err(format!(
            "implementation commit validation failed: base commit {base} is not an ancestor of HEAD {head}"
        ));
    }
    let status = rebon_tool::worktree::git_status_porcelain(path).map_err(|err| err.to_string())?;
    git.status_output = Some(status.clone());
    let dirty = !status.trim().is_empty();
    git.dirty_after_commit = Some(dirty);
    if dirty {
        return Err(format!(
            "implementation commit validation failed: worktree has uncommitted changes after commit:\n{status}"
        ));
    }
    let report_hash = report_file_path.and_then(extract_commit_hash_from_report);
    let Some(report_hash) = report_hash else {
        return Err(
            "implementation commit validation failed: report file did not contain a commit hash"
                .to_string(),
        );
    };
    git.commit_hash = Some(report_hash.clone());
    if !(head == report_hash || head.starts_with(&report_hash)) {
        return Err(format!(
            "implementation commit validation failed: report commit hash {report_hash} does not match runtime HEAD {head}"
        ));
    }
    git.commit_hash = Some(head);
    Ok(())
}

#[cfg(test)]
fn extract_commit_hash_from_report(path: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    rebon_core::coordinator_mode::extract_commit_hash_from_implementation_section(&text)
}

fn now_wall_ms_for_spawner() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn child_task_cleanup_status(parent_status: &str) -> rebon_tool::tasks::TaskListStatus {
    if parent_status == "completed" {
        rebon_tool::tasks::TaskListStatus::Completed
    } else {
        rebon_tool::tasks::TaskListStatus::Pending
    }
}

fn cleanup_agent_created_tasks(
    task_list_id: &str,
    agent_id: &str,
    metadata: &Value,
    status: rebon_tool::tasks::TaskListStatus,
) {
    match rebon_tool::tasks::cleanup_in_progress_tasks_for_agent(
        task_list_id,
        agent_id,
        status.clone(),
    ) {
        Ok(count) if count > 0 => tracing::debug!(
            agent_id = %agent_id,
            task_list_id = %task_list_id,
            count,
            "cleaned up in-progress tasks created by terminal agent"
        ),
        Ok(_) => {}
        Err(err) => tracing::warn!(
            agent_id = %agent_id,
            task_list_id = %task_list_id,
            error = %err,
            "failed to clean up in-progress tasks created by terminal agent"
        ),
    }

    let linked_task_ids = linked_task_ids(metadata);
    if linked_task_ids.is_empty() {
        return;
    }
    match rebon_tool::tasks::cleanup_in_progress_tasks_by_ids(
        task_list_id,
        &linked_task_ids,
        status,
    ) {
        Ok(count) if count > 0 => tracing::debug!(
            agent_id = %agent_id,
            task_list_id = %task_list_id,
            count,
            "updated tasks explicitly linked to terminal agent"
        ),
        Ok(_) => {}
        Err(err) => tracing::warn!(
            agent_id = %agent_id,
            task_list_id = %task_list_id,
            error = %err,
            "failed to update tasks explicitly linked to terminal agent"
        ),
    }
}

fn linked_task_ids(metadata: &Value) -> Vec<String> {
    let mut task_ids = Vec::new();
    for key in ["task_id", "taskId", "task_ids", "taskIds"] {
        let Some(value) = metadata.get(key) else {
            continue;
        };
        match value {
            Value::Array(values) => {
                for value in values {
                    push_linked_task_id(&mut task_ids, value);
                }
            }
            value => push_linked_task_id(&mut task_ids, value),
        }
    }
    task_ids
}

fn push_linked_task_id(task_ids: &mut Vec<String>, value: &Value) {
    let task_id = match value {
        Value::String(value) => value.trim().to_string(),
        Value::Number(value) => value.to_string(),
        _ => return,
    };
    if !task_id.is_empty() && !task_ids.contains(&task_id) {
        task_ids.push(task_id);
    }
}

fn local_agent_task_snapshot(
    id: TaskId,
    status: TaskStatus,
    title: String,
    prompt: String,
    agent_type: String,
    model: String,
    system: Option<String>,
    allowed_tools: Option<Vec<String>>,
    is_backgrounded: bool,
    start_time_ms: u64,
    end_time_ms: Option<u64>,
    error: Option<String>,
    last_progress: Option<String>,
    result: Option<Value>,
    metadata: Value,
) -> TaskSnapshot {
    TaskSnapshot {
        id,
        kind: TaskKind::LocalAgent,
        status,
        title,
        last_progress,
        error,
        result,
        is_backgrounded,
        notified: false,
        start_time_ms,
        end_time_ms,
        metadata,
        data: TaskData::LocalAgent(LocalAgentData {
            prompt,
            agent_type,
            model: Some(model),
            system,
            allowed_tools,
            token_count: 0,
            tool_use_count: 0,
            transcript: Vec::new(),
            streaming_text: None,
            pending_messages: Vec::new(),
            retrieved: false,
        }),
    }
}

struct ForegroundWorkerCancelGuard {
    cancel: Option<PromptCancel>,
    background_request: Option<watch::Receiver<bool>>,
}

impl ForegroundWorkerCancelGuard {
    fn background_requested(&self) -> bool {
        self.background_request
            .as_ref()
            .is_some_and(|request| *request.borrow())
    }
}

impl Drop for ForegroundWorkerCancelGuard {
    fn drop(&mut self) {
        if !self.background_requested() {
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
        }
    }
}

async fn wait_worker_and_mirror_progress_detachable(
    handle: WorkerHandle,
    registry: Option<TaskRegistry>,
    task_id: TaskId,
    turn: Option<TaskTurnToken>,
    progress: Option<SubAgentProgressSender>,
    background_request: Option<watch::Receiver<bool>>,
    mirror_terminal_result: bool,
    keep_runtime_resumable: bool,
) -> (WorkerResult, bool) {
    let cancel = handle.cancel_handle();
    let (result_tx, mut result_rx) = tokio::sync::oneshot::channel();
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel::<()>();
    // 后创建的守卫先析构，确保先取消 worker，再通过通道关闭移交收尾。
    let mut cancel_guard = ForegroundWorkerCancelGuard {
        cancel: Some(cancel.clone()),
        background_request,
    };
    let mut worker_wait = tokio::spawn(async move {
        let result = wait_worker_and_mirror_progress(
            handle,
            registry.clone(),
            task_id.clone(),
            turn.clone(),
            progress,
        )
        .await;
        // 发送成功不代表调用方已取走结果；收到确认前，终态仍由此任务负责。
        drop(result_tx.send(result.clone()));
        if completion_rx.await.is_err() && (mirror_terminal_result || cancel.is_cancelled()) {
            let keep_runtime_resumable = keep_runtime_resumable && !cancel.is_cancelled();
            let mirrored = mirror_agent_terminal_result(
                &registry,
                &task_id,
                turn.as_ref(),
                &result,
                keep_runtime_resumable,
            );
            let status = if keep_runtime_resumable {
                TaskStatus::Running
            } else {
                match result.status {
                    WorkerStatus::Completed => TaskStatus::Completed,
                    WorkerStatus::Cancelled => TaskStatus::Killed,
                    WorkerStatus::Failed | WorkerStatus::Running => TaskStatus::Failed,
                }
            };
            if mirrored {
                record_agent_terminal_event(
                    &registry,
                    &task_id,
                    turn.as_ref(),
                    TaskLiveEventKind::Finished {
                        status,
                        error: result.error.clone(),
                    },
                );
            }
            if let (Some(registry), Some(turn)) = (&registry, &turn) {
                if keep_runtime_resumable {
                    registry.finish_task_turn(turn);
                } else {
                    registry.finish_terminal_task_turn(turn);
                }
            }
        }
        result
    });
    loop {
        if cancel_guard.background_requested() {
            cancel_guard.cancel = None;
            return (detached_worker_placeholder(), true);
        }
        tokio::select! {
            result = &mut result_rx => {
                return match result {
                    Ok(result) => {
                        completion_tx.send(()).expect("worker wait retains completion receiver");
                        cancel_guard.cancel = None;
                        (result, false)
                    }
                    Err(_) => (join_worker_wait(&mut worker_wait).await, false),
                };
            }
            changed = async {
                match cancel_guard.background_request.as_mut() {
                    Some(request) => request.changed().await,
                    None => std::future::pending().await,
                }
            } => {
                if changed.is_err() {
                    cancel_guard.background_request = None;
                }
            }
        }
    }
}

async fn join_worker_wait(worker_wait: &mut tokio::task::JoinHandle<WorkerResult>) -> WorkerResult {
    worker_wait.await.unwrap_or_else(worker_join_error)
}

fn worker_join_error(err: tokio::task::JoinError) -> WorkerResult {
    WorkerResult {
        status: WorkerStatus::Failed,
        final_text: String::new(),
        stop_reason: None,
        total_usage: rebon_api::Usage::default(),
        cumulative_output_tokens: 0,
        tool_calls: Vec::new(),
        context_reset_occurred: false,
        error: Some(format!("worker wait task failed: {err}")),
    }
}

fn detached_worker_placeholder() -> WorkerResult {
    WorkerResult {
        status: WorkerStatus::Running,
        final_text: String::new(),
        stop_reason: None,
        total_usage: rebon_api::Usage::default(),
        cumulative_output_tokens: 0,
        tool_calls: Vec::new(),
        context_reset_occurred: false,
        error: None,
    }
}

async fn wait_worker_and_mirror_progress(
    mut handle: WorkerHandle,
    registry: Option<TaskRegistry>,
    task_id: TaskId,
    turn: Option<TaskTurnToken>,
    progress: Option<SubAgentProgressSender>,
) -> WorkerResult {
    let mut fallback = WorkerResult {
        status: WorkerStatus::Running,
        final_text: String::new(),
        stop_reason: None,
        total_usage: rebon_api::Usage::default(),
        cumulative_output_tokens: 0,
        tool_calls: Vec::new(),
        context_reset_occurred: false,
        error: None,
    };
    let mut live_tool_count = 0_u64;
    let mut assistant_snapshot = String::new();
    let mut thinking_snapshot = String::new();

    while let Some(event) = handle.next_event().await {
        match event {
            WorkerEvent::AssistantText { text } => {
                let delta = assistant_text_delta(&assistant_snapshot, &text);
                assistant_snapshot = text.clone();
                fallback.final_text = text.clone();
                if !delta.is_empty() {
                    record_agent_live_event(
                        &registry,
                        &task_id,
                        turn.as_ref(),
                        TaskLiveEventKind::AssistantTextDelta {
                            delta,
                            snapshot: text.clone(),
                        },
                    );
                }
                mirror_agent_assistant_text(&registry, &task_id, turn.as_ref(), text);
            }
            WorkerEvent::Thinking { text } => {
                let delta = assistant_text_delta(&thinking_snapshot, &text);
                thinking_snapshot = text.clone();
                if !delta.is_empty() {
                    record_agent_live_event(
                        &registry,
                        &task_id,
                        turn.as_ref(),
                        TaskLiveEventKind::ThinkingDelta {
                            delta,
                            snapshot: text.clone(),
                        },
                    );
                }
                mirror_agent_thinking(&registry, &task_id, turn.as_ref(), text);
            }
            WorkerEvent::IterationComplete {
                text, total_usage, ..
            } => {
                assistant_snapshot.clear();
                thinking_snapshot.clear();
                fallback.final_text = text.clone();
                record_agent_live_event(
                    &registry,
                    &task_id,
                    turn.as_ref(),
                    TaskLiveEventKind::AssistantTurnComplete { text: text.clone() },
                );
                mirror_agent_iteration_complete(
                    &registry,
                    &task_id,
                    turn.as_ref(),
                    text,
                    total_usage,
                );
            }
            WorkerEvent::ToolStart {
                name,
                input,
                tool_use_id,
            } => {
                live_tool_count = live_tool_count.saturating_add(1);
                let activity = describe_worker_tool_activity(&name, &input);
                if let Some(progress) = progress.as_ref() {
                    progress.emit_activity_lines(parent_worker_tool_activity_lines(&name, &input));
                }
                record_agent_live_event(
                    &registry,
                    &task_id,
                    turn.as_ref(),
                    TaskLiveEventKind::ToolStart {
                        tool_use_id: tool_use_id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    },
                );
                mirror_agent_tool_start(
                    &registry,
                    &task_id,
                    turn.as_ref(),
                    tool_use_id,
                    name,
                    input,
                    activity,
                    live_tool_count,
                );
            }
            WorkerEvent::ToolProgress {
                tool_use_id,
                name,
                message,
            } => {
                if let Some(message) = message {
                    let activity = format!("{name}: {message}");
                    if let Some(progress) = progress.as_ref() {
                        progress.emit_activity(activity.clone());
                    }
                    record_agent_live_event(
                        &registry,
                        &task_id,
                        turn.as_ref(),
                        TaskLiveEventKind::ToolProgress {
                            tool_use_id: tool_use_id.clone(),
                            name: name.clone(),
                            message: message.clone(),
                        },
                    );
                    mirror_agent_tool_progress(
                        &registry,
                        &task_id,
                        turn.as_ref(),
                        tool_use_id,
                        name,
                        message,
                        activity,
                    );
                }
            }
            WorkerEvent::PermissionQuery(query) => {
                let _ = query
                    .response_tx
                    .send(rebon_core::permission::PermissionAnswer::Cancelled);
            }
            WorkerEvent::ToolFinish {
                tool_use_id,
                name,
                outcome,
            } => {
                record_agent_live_event(
                    &registry,
                    &task_id,
                    turn.as_ref(),
                    TaskLiveEventKind::ToolFinish {
                        tool_use_id: tool_use_id.clone(),
                        name: name.clone(),
                        outcome: outcome.clone(),
                    },
                );
                let ok = outcome.is_ok();
                let activity = match &outcome {
                    Ok(_) => format!("{name} ok"),
                    Err(err) => format!("{name} error: {err}"),
                };
                mirror_agent_tool_finish(
                    &registry,
                    &task_id,
                    turn.as_ref(),
                    tool_use_id,
                    name,
                    ok,
                    activity,
                    outcome,
                );
            }
            WorkerEvent::Completed(result) => return result,
        }
    }

    fallback.status = WorkerStatus::Failed;
    fallback.error = Some("worker event stream closed unexpectedly".into());
    fallback
}

fn mirror_agent_terminal_result(
    registry: &Option<TaskRegistry>,
    task_id: &TaskId,
    turn: Option<&TaskTurnToken>,
    result: &WorkerResult,
    keep_runtime_resumable: bool,
) -> bool {
    let Some(registry) = registry else {
        return false;
    };
    let update = |snap: &mut TaskSnapshot| {
        if snap.status == TaskStatus::Killed {
            return;
        }
        snap.status = if keep_runtime_resumable {
            TaskStatus::Running
        } else {
            match result.status {
                WorkerStatus::Completed => TaskStatus::Completed,
                WorkerStatus::Cancelled => TaskStatus::Killed,
                WorkerStatus::Failed | WorkerStatus::Running => TaskStatus::Failed,
            }
        };
        snap.error = result.error.clone();
        snap.last_progress = Some(result.final_text.clone());
        snap.end_time_ms = (!keep_runtime_resumable).then(now_wall_ms_for_spawner);
        let terminal_tool_use_count = result.tool_calls.len() as u64;
        let total_tokens = u64::from(result.total_usage.billed_input_tokens())
            + u64::from(result.total_usage.output_tokens);
        let sub_agent_tool_calls = worker_tool_calls_json(result);
        snap.result = Some(serde_json::json!({
            "status": match result.status {
                WorkerStatus::Completed => "completed",
                WorkerStatus::Cancelled => "cancelled",
                WorkerStatus::Failed => "failed",
                WorkerStatus::Running => "running",
            },
            "final_text": result.final_text.clone(),
            "tool_call_count": terminal_tool_use_count,
            "sub_agent_tool_calls": sub_agent_tool_calls,
            "subAgentToolCalls": sub_agent_tool_calls,
            "total_tokens": total_tokens,
            "usage": {
                "input_tokens": result.total_usage.billed_input_tokens(),
                "output_tokens": result.total_usage.output_tokens,
                "cumulative_output_tokens": result.cumulative_output_tokens,
                "cache_creation_input_tokens": result.total_usage.cache_creation_input_tokens,
                "cache_read_input_tokens": result.total_usage.cache_read_input_tokens,
                "prompt_cache_hit_tokens": result.total_usage.prompt_cache_hit_tokens,
                "prompt_cache_miss_tokens": result.total_usage.prompt_cache_miss_tokens,
            },
        }));
        if let TaskData::LocalAgent(data) = &mut snap.data {
            data.tool_use_count = terminal_tool_use_count;
            data.token_count = total_tokens;
            data.streaming_text = None;
            if !result.final_text.trim().is_empty()
                && !data.transcript.iter().any(|entry| {
                    matches!(
                        entry,
                        LocalAgentTranscriptEntry::Assistant { text }
                            if text == &result.final_text
                    )
                })
            {
                push_bounded_agent_transcript(
                    &mut data.transcript,
                    LocalAgentTranscriptEntry::Assistant {
                        text: result.final_text.clone(),
                    },
                );
            }
        }
    };
    if let Some(turn) = turn {
        registry.update_task_turn(turn, update)
    } else {
        registry.update(task_id, update);
        true
    }
}

fn mirror_agent_assistant_text(
    registry: &Option<TaskRegistry>,
    task_id: &TaskId,
    turn: Option<&TaskTurnToken>,
    text: String,
) {
    mirror_agent_transcript_update(registry, task_id, turn, |snap| {
        snap.last_progress = Some(text.clone());
        if let TaskData::LocalAgent(data) = &mut snap.data {
            data.streaming_text = Some(text);
        }
    });
}

fn mirror_agent_thinking(
    registry: &Option<TaskRegistry>,
    task_id: &TaskId,
    turn: Option<&TaskTurnToken>,
    text: String,
) {
    mirror_agent_transcript_update(registry, task_id, turn, |snap| {
        snap.last_progress = Some(format!("Thinking: {text}"));
        if let TaskData::LocalAgent(data) = &mut snap.data {
            upsert_bounded_agent_thinking(&mut data.transcript, text);
        }
    });
}

fn mirror_agent_iteration_complete(
    registry: &Option<TaskRegistry>,
    task_id: &TaskId,
    turn: Option<&TaskTurnToken>,
    text: String,
    total_usage: Usage,
) {
    let token_count = total_usage_tokens(&total_usage);
    mirror_agent_transcript_update(registry, task_id, turn, |snap| {
        snap.last_progress = Some(text.clone());
        if let TaskData::LocalAgent(data) = &mut snap.data {
            data.token_count = token_count;
            data.streaming_text = None;
            if !text.trim().is_empty() {
                push_bounded_agent_transcript(
                    &mut data.transcript,
                    LocalAgentTranscriptEntry::Assistant { text },
                );
            }
        }
    });
}

fn mirror_agent_tool_start(
    registry: &Option<TaskRegistry>,
    task_id: &TaskId,
    turn: Option<&TaskTurnToken>,
    tool_use_id: String,
    name: String,
    input: Value,
    activity: String,
    tool_use_count: u64,
) {
    mirror_agent_transcript_update(registry, task_id, turn, |snap| {
        snap.last_progress = Some(activity.clone());
        if let TaskData::LocalAgent(data) = &mut snap.data {
            data.tool_use_count = tool_use_count;
            push_bounded_agent_transcript(
                &mut data.transcript,
                LocalAgentTranscriptEntry::ToolStart {
                    tool_use_id,
                    name,
                    input,
                    activity,
                },
            );
        }
    });
}

fn mirror_agent_tool_progress(
    registry: &Option<TaskRegistry>,
    task_id: &TaskId,
    turn: Option<&TaskTurnToken>,
    tool_use_id: String,
    name: String,
    message: String,
    activity: String,
) {
    mirror_agent_transcript_update(registry, task_id, turn, |snap| {
        snap.last_progress = Some(activity);
        if let TaskData::LocalAgent(data) = &mut snap.data {
            push_bounded_agent_transcript(
                &mut data.transcript,
                LocalAgentTranscriptEntry::ToolProgress {
                    tool_use_id,
                    name,
                    message,
                },
            );
        }
    });
}

fn mirror_agent_tool_finish(
    registry: &Option<TaskRegistry>,
    task_id: &TaskId,
    turn: Option<&TaskTurnToken>,
    tool_use_id: String,
    name: String,
    ok: bool,
    summary: String,
    outcome: Result<Value, String>,
) {
    mirror_agent_transcript_update(registry, task_id, turn, |snap| {
        snap.last_progress = Some(describe_worker_after_tool_activity(&name));
        if let TaskData::LocalAgent(data) = &mut snap.data {
            push_bounded_agent_transcript(
                &mut data.transcript,
                LocalAgentTranscriptEntry::ToolFinish {
                    tool_use_id,
                    name,
                    ok,
                    summary,
                    outcome,
                },
            );
        }
    });
}

fn mirror_agent_transcript_update(
    registry: &Option<TaskRegistry>,
    task_id: &TaskId,
    turn: Option<&TaskTurnToken>,
    update: impl FnOnce(&mut TaskSnapshot),
) {
    let Some(registry) = registry else {
        return;
    };
    if let Some(turn) = turn {
        registry.update_task_turn(turn, update);
    } else {
        registry.update(task_id, update);
    }
}

fn record_agent_live_event(
    registry: &Option<TaskRegistry>,
    task_id: &TaskId,
    turn: Option<&TaskTurnToken>,
    event: TaskLiveEventKind,
) {
    let Some(registry) = registry else {
        return;
    };
    if let Some(turn) = turn {
        registry.record_task_turn_event(turn, event);
    } else {
        registry.record_live_event(task_id, event);
    }
}

fn record_agent_terminal_event(
    registry: &Option<TaskRegistry>,
    task_id: &TaskId,
    turn: Option<&TaskTurnToken>,
    event: TaskLiveEventKind,
) {
    let Some(registry) = registry else {
        return;
    };
    if let Some(turn) = turn {
        registry.record_task_turn_terminal_event(turn, event);
    } else {
        registry.record_live_event(task_id, event);
    }
}

fn total_usage_tokens(usage: &Usage) -> u64 {
    u64::from(usage.billed_input_tokens()) + u64::from(usage.output_tokens)
}

fn worker_tool_calls_json(result: &WorkerResult) -> Vec<Value> {
    result
        .tool_calls
        .iter()
        .map(|call| {
            serde_json::json!({
                "tool_use_id": call.tool_use_id,
                "toolUseId": call.tool_use_id,
                "name": call.name,
                "input": call.input.clone(),
                "ok": call.outcome.is_ok(),
            })
        })
        .collect()
}

fn parent_worker_tool_activity_lines(name: &str, input: &Value) -> Vec<String> {
    // The parent shows a worker's file traffic, so the question is whether
    // this call names a file — which the tool declares.
    if !rebon_tools_core::tool_kind_for_name(name).touches_a_file() {
        return Vec::new();
    }
    input_first_str_for_parent_activity(input, &["file_path", "notebook_path", "path"])
        .map(|path| vec![path.to_string()])
        .unwrap_or_default()
}

fn input_first_str_for_parent_activity<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Default)]
struct ValidatorLifecycleDiagnostics {
    coercions_emitted: AtomicUsize,
    report_file_failures: AtomicUsize,
    structured_output_failures: AtomicUsize,
    structured_output_attempts: AtomicUsize,
    structured_output_exhausted: AtomicBool,
}

impl ValidatorLifecycleDiagnostics {
    fn record_report_file_coercion(&self) {
        self.report_file_failures.fetch_add(1, Ordering::Relaxed);
        self.coercions_emitted.fetch_add(1, Ordering::Relaxed);
    }

    fn record_structured_output_attempt(&self) {
        self.structured_output_attempts
            .fetch_add(1, Ordering::Relaxed);
        self.structured_output_failures
            .fetch_add(1, Ordering::Relaxed);
        self.coercions_emitted.fetch_add(1, Ordering::Relaxed);
    }

    fn record_structured_output_exhausted(&self) {
        self.structured_output_exhausted
            .store(true, Ordering::Relaxed);
    }

    fn validator_coercions_emitted(&self) -> usize {
        self.coercions_emitted.load(Ordering::Relaxed)
    }

    fn structured_output_attempts(&self) -> usize {
        self.structured_output_attempts.load(Ordering::Relaxed)
    }

    fn structured_output_exhausted(&self) -> bool {
        self.structured_output_exhausted.load(Ordering::Relaxed)
    }

    fn validator_failures(
        &self,
        report_invalid_at_end: bool,
        structured_missing_at_end: bool,
    ) -> Vec<Value> {
        let mut failures = Vec::new();
        if self.report_file_failures.load(Ordering::Relaxed) > 0 || report_invalid_at_end {
            failures.push(Value::String("report_file_invalid".to_string()));
        }
        if self.structured_output_failures.load(Ordering::Relaxed) > 0 || structured_missing_at_end
        {
            failures.push(Value::String("structured_output_missing".to_string()));
        }
        failures
    }
}

struct LocalAgentPendingMessagePoller {
    registry: TaskRegistry,
    task_id: TaskId,
}

impl LocalAgentPendingMessagePoller {
    fn new(registry: TaskRegistry, task_id: TaskId) -> Self {
        Self { registry, task_id }
    }
}

impl AttachmentPoller for LocalAgentPendingMessagePoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        if request.phase == AttachmentPollPhase::Eager {
            return Vec::new();
        }
        take_pending_local_agent_messages(&self.registry, &self.task_id)
            .into_iter()
            .map(|text| ApiMessage {
                role: Role::User,
                content: vec![ApiContentBlock::Text(TextBlock { text })],
            })
            .collect()
    }
}

fn sub_agent_diagnostics(
    spec: &SubAgentSpec,
    structured_output_channel: Option<&Arc<rebon_tool::StructuredOutputChannel>>,
    report_validation: Option<&rebon_core::coordinator_mode::WorkerReportValidation>,
    sub_agent_tool_calls: &[Value],
    context_reset_occurred: bool,
    status: &str,
    error: Option<&str>,
    validator_lifecycle: &ValidatorLifecycleDiagnostics,
) -> Value {
    let schema_present = structured_output_channel.is_some();
    let provider_visible_tools_contains_structured_output =
        spec.execution_policy.as_ref().is_some_and(|policy| {
            policy
                .eager_promotions
                .contains(rebon_tool::STRUCTURED_OUTPUT_TOOL_NAME)
        });
    let execution_policy_has_structured_output_eager_promotion =
        provider_visible_tools_contains_structured_output;
    let structured_output_calls = sub_agent_tool_calls
        .iter()
        .filter(|call| {
            call.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name == rebon_tool::STRUCTURED_OUTPUT_TOOL_NAME)
        })
        .count();
    let successful_structured_output_calls = sub_agent_tool_calls
        .iter()
        .filter(|call| {
            call.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name == rebon_tool::STRUCTURED_OUTPUT_TOOL_NAME)
                && call.get("ok").and_then(Value::as_bool).unwrap_or(false)
        })
        .count();
    let report_installed = report_validation.is_some();
    let structured_output_installed = structured_output_channel.is_some();
    let mut installed = Vec::new();
    if report_installed {
        installed.push(Value::String("report_file".to_string()));
    }
    if structured_output_installed {
        installed.push(Value::String("structured_output".to_string()));
    }
    let failures = validator_lifecycle.validator_failures(
        report_validation.is_some_and(|validation| !validation.ok),
        structured_output_channel.is_some_and(|channel| !channel.is_satisfied()),
    );
    let structured_output_exhausted = structured_output_installed
        && !structured_output_channel.is_some_and(|channel| channel.is_satisfied())
        && validator_lifecycle.structured_output_exhausted();
    serde_json::json!({
        "structured_output": {
            "schema_present": schema_present,
            "tool_call_count": structured_output_calls,
            "successful_tool_call_count": successful_structured_output_calls,
            "missing_tool_call": schema_present && successful_structured_output_calls == 0,
            "coercion_attempts": validator_lifecycle.structured_output_attempts(),
            "coercion_exhausted": structured_output_exhausted,
            "accepted": structured_output_channel.and_then(|channel| channel.accepted()),
        },
        "tool_visibility": {
            "provider_visible_tools_contains_structured_output": provider_visible_tools_contains_structured_output,
            "execution_policy_has_structured_output_eager_promotion": execution_policy_has_structured_output_eager_promotion,
            "tool_search_enabled": rebon_tool::is_tool_search_enabled(),
            "context_reset_occurred": context_reset_occurred,
            "after_context_reset": context_reset_occurred,
        },
        "validators": {
            "installed_validators": installed,
            "report_file_validator_installed": report_installed,
            "structured_output_validator_installed": structured_output_installed,
            "validator_coercions_emitted": validator_lifecycle.validator_coercions_emitted(),
            "validator_failures": failures,
        },
        "agent_result": {
            "status": status,
            "error": error,
        },
        "capability": spec.capability_context.as_ref().map(|context| serde_json::json!({
            "run_id": context.run_id,
            "ledger_revision": context.ledger_revision,
            "requirements_hash": context.requirements_hash,
            "capability_hash": context.capability_hash,
            "session_id": context.session_id,
        }))
    })
}

/// Identity of a report file at a point in time, used to tell a report
/// written this turn from one left over by an earlier turn of the same
/// worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReportFileStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

fn report_file_stamp(path: &str) -> Option<ReportFileStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(ReportFileStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

/// Structural validation plus the freshness rule: a report byte-identical
/// to the one that existed before this turn is the previous turn's
/// deliverable, not this one's.
fn stale_aware_report_validation(
    path: &str,
    task_kind: SubAgentTaskKind,
    use_worktree: bool,
    baseline: Option<ReportFileStamp>,
) -> rebon_core::coordinator_mode::WorkerReportValidation {
    let validation =
        rebon_core::coordinator_mode::validate_worker_report_file_for_task_kind_with_options(
            path,
            Some(task_kind.as_str()),
            use_worktree,
        );
    if !validation.ok {
        return validation;
    }
    let Some(baseline) = baseline else {
        return validation;
    };
    if report_file_stamp(path) != Some(baseline) {
        return validation;
    }
    rebon_core::coordinator_mode::WorkerReportValidation {
        ok: false,
        size: validation.size,
        missing_sections: Vec::new(),
        reason: Some(
            "the report file still holds the previous turn's content — it was not rewritten \
             during this turn"
                .to_string(),
        ),
    }
}

/// One missing worker delivery contract.
struct WorkerDeliveryIssue {
    message: ApiMessage,
    force_structured_output: bool,
}

/// One-shot report-file contract. A missing, stale, or malformed report gets
/// one corrective turn; final settlement remains authoritative after that.
struct ReportFileContract {
    path: String,
    task_kind: SubAgentTaskKind,
    use_worktree: bool,
    baseline: Option<ReportFileStamp>,
    fired: AtomicBool,
    diagnostics: Arc<ValidatorLifecycleDiagnostics>,
}

impl ReportFileContract {
    fn new(
        path: String,
        task_kind: SubAgentTaskKind,
        use_worktree: bool,
        baseline: Option<ReportFileStamp>,
        diagnostics: Arc<ValidatorLifecycleDiagnostics>,
    ) -> Self {
        Self {
            path,
            task_kind,
            use_worktree,
            baseline,
            fired: AtomicBool::new(false),
            diagnostics,
        }
    }

    fn evaluate(&self) -> Option<WorkerDeliveryIssue> {
        if self.fired.swap(true, Ordering::Relaxed) {
            return None;
        }
        let validation = stale_aware_report_validation(
            &self.path,
            self.task_kind,
            self.use_worktree,
            self.baseline,
        );
        if validation.ok {
            return None;
        }
        tracing::info!(
            path = %self.path,
            reason = %validation.failure_summary(),
            "worker ended turn without a structurally valid report file — injecting coercion message"
        );
        self.diagnostics.record_report_file_coercion();
        let text = rebon_core::coordinator_mode::build_report_coercion_message(
            &self.path,
            Some(&validation),
        );
        Some(WorkerDeliveryIssue {
            message: ApiMessage {
                role: Role::User,
                content: vec![ApiContentBlock::Text(TextBlock { text })],
            },
            force_structured_output: false,
        })
    }
}

/// One-shot schema contract for workflow workers. A schema-valid tool result
/// ends the contract; otherwise the hook forces one StructuredOutput retry.
struct StructuredOutputContract {
    channel: Arc<rebon_tool::StructuredOutputChannel>,
    fired: AtomicBool,
    diagnostics: Arc<ValidatorLifecycleDiagnostics>,
}

impl StructuredOutputContract {
    fn new(
        channel: Arc<rebon_tool::StructuredOutputChannel>,
        diagnostics: Arc<ValidatorLifecycleDiagnostics>,
    ) -> Self {
        Self {
            channel,
            fired: AtomicBool::new(false),
            diagnostics,
        }
    }

    fn evaluate(&self) -> Option<WorkerDeliveryIssue> {
        if self.channel.is_satisfied() {
            return None;
        }
        if self.fired.swap(true, Ordering::Relaxed) {
            self.diagnostics.record_structured_output_exhausted();
            return None;
        }
        self.diagnostics.record_structured_output_attempt();
        let schema_text = self
            .channel
            .schema()
            .map(|schema| {
                serde_json::to_string_pretty(schema).expect("serde_json::Value always serializes")
            })
            .unwrap_or_default();
        let skeleton_text = self
            .channel
            .schema()
            .map(minimal_json_skeleton_from_schema)
            .map(|value| {
                serde_json::to_string_pretty(&value).expect("serde_json::Value always serializes")
            })
            .unwrap_or_else(|| "{}".to_string());
        tracing::info!(
            "workflow worker ended turn without a valid StructuredOutput call — injecting coercion message"
        );
        let text = if schema_text.is_empty() {
            format!(
                "StructuredOutput delivery is still missing.\n\n\
                 Call the StructuredOutput tool right now. Do not answer with prose. Do not summarize.\n\
                 The workflow runtime reads only the StructuredOutput tool call or schema-valid fallback JSON.\n\n\
                 Minimal valid JSON skeleton:\n{skeleton_text}"
            )
        } else {
            format!(
                "StructuredOutput delivery is still missing.\n\n\
                 Call the StructuredOutput tool right now. Do not answer with prose. Do not summarize.\n\
                 The workflow runtime reads only the StructuredOutput tool call or schema-valid fallback JSON.\n\n\
                 Call StructuredOutput with one JSON object matching this schema:\n\n{schema_text}\n\n\
                 Minimal valid JSON skeleton:\n{skeleton_text}"
            )
        };
        Some(WorkerDeliveryIssue {
            message: ApiMessage {
                role: Role::User,
                content: vec![ApiContentBlock::Text(TextBlock { text })],
            },
            force_structured_output: true,
        })
    }
}

enum WorkerDeliveryRule {
    Report(ReportFileContract),
    StructuredOutput(StructuredOutputContract),
}

impl WorkerDeliveryRule {
    fn evaluate(&self) -> Option<WorkerDeliveryIssue> {
        match self {
            Self::Report(contract) => contract.evaluate(),
            Self::StructuredOutput(contract) => contract.evaluate(),
        }
    }
}

/// The query-local hook that owns all worker terminal delivery policy.
struct WorkerDeliveryHook {
    rules: Vec<WorkerDeliveryRule>,
}

impl WorkerDeliveryHook {
    fn evaluate(&self) -> Option<WorkerDeliveryIssue> {
        let mut texts = Vec::new();
        let mut force_structured_output = false;
        for issue in self.rules.iter().filter_map(WorkerDeliveryRule::evaluate) {
            let text = api_message_text(&issue.message);
            if !text.trim().is_empty() {
                texts.push(text);
            }
            force_structured_output |= issue.force_structured_output;
        }
        if texts.is_empty() {
            return None;
        }
        let text = if texts.len() == 1 {
            texts.into_iter().next().unwrap_or_default()
        } else {
            format!(
                "Multiple required delivery contracts are still missing or invalid. Satisfy every section below in this same turn; fixing only one contract is not enough.\n\n{}",
                texts.join("\n\n---\n\n")
            )
        };
        Some(WorkerDeliveryIssue {
            message: ApiMessage {
                role: Role::User,
                content: vec![ApiContentBlock::Text(TextBlock { text })],
            },
            force_structured_output,
        })
    }
}

impl TurnHook for WorkerDeliveryHook {
    fn on_event(&self, _event: &rebon_core::query::QueryEvent, _context: &mut TurnHookContext) {}

    fn on_turn_end(
        &self,
        event: &TurnEndHookEvent<'_>,
        context: &mut TurnHookContext,
        _state: &mut TurnHookState,
    ) {
        let Some(issue) = self.evaluate() else {
            return;
        };
        context.append_history(ApiMessage {
            role: Role::Assistant,
            content: event.message.content.clone(),
        });
        context.append_history(issue.message);
        if issue.force_structured_output {
            context.update_params(|params| {
                params.next_tool_choice = Some(ToolChoice::Tool {
                    name: rebon_tool::STRUCTURED_OUTPUT_TOOL_NAME.to_string(),
                });
            });
        }
        context.request_continue_reserving(2);
    }
}

fn api_message_text(message: &ApiMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ApiContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn worker_delivery_hook(rules: Vec<WorkerDeliveryRule>) -> Option<Arc<dyn TurnHook>> {
    (!rules.is_empty()).then(|| Arc::new(WorkerDeliveryHook { rules }) as Arc<dyn TurnHook>)
}
fn minimal_json_skeleton_from_schema(schema: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Map, Value};
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let mut object = Map::new();
            let properties = schema.get("properties").and_then(Value::as_object);
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for key in required.iter().filter_map(Value::as_str) {
                    let value = properties
                        .and_then(|properties| properties.get(key))
                        .map(minimal_json_skeleton_from_schema)
                        .unwrap_or(Value::Null);
                    object.insert(key.to_string(), value);
                }
            }
            Value::Object(object)
        }
        Some("array") => Value::Array(Vec::new()),
        Some("string") => Value::String(String::new()),
        Some("integer") | Some("number") => Value::Number(0.into()),
        Some("boolean") => Value::Bool(false),
        Some("null") => Value::Null,
        _ => Value::Object(Map::new()),
    }
}

/// Directory where worker report files are written.
///
/// Uses `~/.rebon/tasks/` (or `%USERPROFILE%\.rebon\tasks\` on Windows).
/// Defined in `rebon_tool` so the Agent tool recognises the same path
/// and does not ask a caller to authorize a directory the runtime
/// already scopes per worker.
fn tasks_output_dir() -> std::path::PathBuf {
    rebon_tool::path_scope::worker_report_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures_util::StreamExt;
    use rebon_api::{
        events::{ContentBlockDelta, ContentBlockStart, MessageDeltaFields},
        MockModelClient, StopReason, StreamEvent, Usage,
    };
    use rebon_core::permission::{ChannelPermissionBroker, PermissionAnswer};
    use rebon_core::PermissionBroker;
    use rebon_tool::external_agent::ExternalTaskOutcome;
    use rebon_tool::{ExecutionPolicy, PolicyMode, Tool, ToolContext, UltraplanContext};
    use rebon_tools_core::{
        PermissionBehavior, PermissionDecision, PermissionRequest, ToolError, ToolId,
        ToolInputSchema, ToolResult, ValidationOutcome,
    };
    use serde_json::{json, Value};
    use std::process::Command;

    #[test]
    fn tool_finish_keeps_history_separate_from_running_activity() {
        let registry = TaskRegistry::new();
        let task_id = TaskId::new("agent-tool-finish");
        registry.insert(
            task_id.clone(),
            local_agent_task_snapshot(
                task_id.clone(),
                TaskStatus::Running,
                "Search code".into(),
                "Find the implementation".into(),
                "Explore".into(),
                "mock".into(),
                None,
                None,
                false,
                1,
                None,
                None,
                None,
                None,
                json!({}),
            ),
            PromptCancel::new(),
        );

        mirror_agent_tool_finish(
            &Some(registry.clone()),
            &task_id,
            None,
            "toolu-grep".into(),
            "Grep".into(),
            true,
            "Grep ok".into(),
            Ok(json!({ "matches": ["src/main.rs:1"] })),
        );

        let snapshot = registry.snapshot(&task_id).expect("task snapshot");
        assert_eq!(snapshot.status, TaskStatus::Running);
        assert_eq!(
            snapshot.last_progress.as_deref(),
            Some("processing Grep result")
        );
        let TaskData::LocalAgent(data) = snapshot.data else {
            panic!("expected local agent data");
        };
        assert!(matches!(
            data.transcript.last(),
            Some(LocalAgentTranscriptEntry::ToolFinish {
                name,
                ok: true,
                summary,
                outcome: Ok(output),
                ..
            }) if name == "Grep"
                && summary == "Grep ok"
                && output == &json!({ "matches": ["src/main.rs:1"] })
        ));

        mirror_agent_tool_finish(
            &Some(registry.clone()),
            &task_id,
            None,
            "toolu-bash".into(),
            "Bash".into(),
            false,
            "Bash error: boom".into(),
            Err("boom".into()),
        );
        let snapshot = registry.snapshot(&task_id).expect("task snapshot");
        let TaskData::LocalAgent(data) = snapshot.data else {
            panic!("expected local agent data");
        };
        assert!(matches!(
            data.transcript.last(),
            Some(LocalAgentTranscriptEntry::ToolFinish {
                name,
                ok: false,
                summary,
                outcome: Err(error),
                ..
            }) if name == "Bash" && summary == "Bash error: boom" && error == "boom"
        ));
    }

    #[test]
    fn ultraplan_preamble_preserves_base_system_without_ultraplan_policy() {
        assert_eq!(
            prepend_ultraplan_worker_preamble(Some("system-x".into()), None, &json!({})),
            Some("system-x".into())
        );
    }

    #[test]
    fn ultraplan_preamble_includes_all_cards_by_default() {
        let mut context =
            UltraplanContext::ultrawork_execution_controller_turn("run-1", PolicyMode::Enforce);
        context.execution_cards = vec![
            rebon_types::ExecutionCard {
                step: "1".into(),
                covers: Some("R1".into()),
                files: vec!["a.rs".into()],
                change: "change a".into(),
                verify: "test a".into(),
            },
            rebon_types::ExecutionCard {
                step: "2".into(),
                covers: Some("R2".into()),
                files: vec!["b.rs".into()],
                change: "change b".into(),
                verify: "test b".into(),
            },
        ];
        let policy = ExecutionPolicy::ultraplan(context);

        let system =
            prepend_ultraplan_worker_preamble(None, Some(&policy), &json!({})).expect("preamble");

        assert!(system.contains("Execution Cards from run state"));
        assert!(system.contains("Step 1 [COVERS:R1]"));
        assert!(system.contains("Step 2 [COVERS:R2]"));
        assert!(system.contains("Hash Drift Guard: no drift"));
    }

    #[test]
    fn ultraplan_preamble_filters_selected_card_and_reports_deviation() {
        let mut context =
            UltraplanContext::ultrawork_execution_controller_turn("run-1", PolicyMode::Enforce);
        context.execution_cards = vec![
            rebon_types::ExecutionCard {
                step: "1".into(),
                covers: Some("R1".into()),
                files: vec!["a.rs".into()],
                change: "change a".into(),
                verify: "test a".into(),
            },
            rebon_types::ExecutionCard {
                step: "2".into(),
                covers: Some("R2".into()),
                files: vec!["b.rs".into()],
                change: "change b".into(),
                verify: "test b".into(),
            },
        ];
        context.hash_drift.push(rebon_types::HashDriftRecord {
            path: "b.rs".into(),
            stored_sha256: "old".into(),
            current_sha256: Some("new".into()),
            kind: rebon_types::HashDriftKind::Changed,
        });
        let policy = ExecutionPolicy::ultraplan(context);

        let system = prepend_ultraplan_worker_preamble(
            None,
            Some(&policy),
            &json!({ "execution_card_step": "2" }),
        )
        .expect("preamble");

        assert!(!system.contains("Step 1 [COVERS:R1]"));
        assert!(system.contains("Step 2 [COVERS:R2]"));
        assert!(system.contains("DEVIATION"));
        assert!(system.contains("b.rs"));
    }

    #[test]
    fn worker_messages_put_frozen_capsule_before_task() {
        let mut spec = SubAgentSpec::new("task body");
        spec.context = Some(rebon_tool::ContextRequest {
            mode: rebon_tool::ContextShareMode::CompactWithRecentTurns,
            include_recent_turns: Some(2),
            include_tool_results: rebon_tool::ToolResultMode::Facts,
            include_files: rebon_tool::FileContextMode::References,
            instructions: None,
        });
        spec.frozen_parent_context = Some(rebon_tool::FrozenParentContextCapsule {
            text: Arc::<str>::from("<ContextCapsule>parent facts</ContextCapsule>"),
            hash: "hash".into(),
            token_estimate: 3,
        });

        let messages = build_worker_messages(&spec, spec.prompt.clone());

        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0].content[0].as_text(),
            Some("<ContextCapsule>parent facts</ContextCapsule>")
        );
        assert_eq!(messages[1].content[0].as_text(), Some("task body"));
    }

    #[test]
    fn worker_cache_key_excludes_child_task_and_capsule() {
        let capsule_a = rebon_tool::FrozenParentContextCapsule {
            text: Arc::<str>::from("capsule a"),
            hash: "capsule-a".into(),
            token_estimate: 10,
        };
        let capsule_b = rebon_tool::FrozenParentContextCapsule {
            text: Arc::<str>::from("capsule b"),
            hash: "capsule-b".into(),
            token_estimate: 20,
        };
        let messages_a = vec![rebon_api::Message::user_text("task a")];
        let messages_b = vec![rebon_api::Message::user_text("task b")];

        let trace_a = build_worker_cache_trace_context(
            rebon_tool::CacheStrategy::Auto,
            Some("compact".into()),
            "openai",
            "gpt-test",
            Some("Explore"),
            Some("shared system"),
            Some("openai_responses"),
            "tools-shared".into(),
            "schema-shared".into(),
            Some(&capsule_a),
            &messages_a,
        );
        let trace_b = build_worker_cache_trace_context(
            rebon_tool::CacheStrategy::Auto,
            Some("compact".into()),
            "openai",
            "gpt-test",
            Some("Explore"),
            Some("shared system"),
            Some("openai_responses"),
            "tools-shared".into(),
            "schema-shared".into(),
            Some(&capsule_b),
            &messages_b,
        );

        assert_eq!(trace_a.prompt_cache_key, trace_b.prompt_cache_key);
        assert_ne!(trace_a.capsule_hash, trace_b.capsule_hash);
        assert_ne!(trace_a.task_hash, trace_b.task_hash);
    }

    #[test]
    fn stable_context_capsule_strategy_uses_dispatch_scoped_key() {
        let capsule_a = rebon_tool::FrozenParentContextCapsule {
            text: Arc::<str>::from("capsule a"),
            hash: "capsule-a".into(),
            token_estimate: 10,
        };
        let capsule_b = rebon_tool::FrozenParentContextCapsule {
            text: Arc::<str>::from("capsule b"),
            hash: "capsule-b".into(),
            token_estimate: 20,
        };
        let messages = vec![rebon_api::Message::user_text("task")];

        let trace_a = build_worker_cache_trace_context(
            rebon_tool::CacheStrategy::StableContextCapsule,
            Some("compact".into()),
            "openai",
            "gpt-test",
            Some("Explore"),
            Some("shared system"),
            Some("openai_responses"),
            "tools-shared".into(),
            "schema-shared".into(),
            Some(&capsule_a),
            &messages,
        );
        let trace_b = build_worker_cache_trace_context(
            rebon_tool::CacheStrategy::StableContextCapsule,
            Some("compact".into()),
            "openai",
            "gpt-test",
            Some("Explore"),
            Some("shared system"),
            Some("openai_responses"),
            "tools-shared".into(),
            "schema-shared".into(),
            Some(&capsule_b),
            &messages,
        );

        assert_ne!(trace_a.prompt_cache_key, trace_b.prompt_cache_key);
        assert_eq!(
            trace_a.prompt_cache_retention.as_deref(),
            Some("same_dispatch")
        );
    }

    struct BlockingModelClient {
        started_tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release_rx: Arc<tokio::sync::Mutex<tokio::sync::oneshot::Receiver<()>>>,
    }

    impl BlockingModelClient {
        fn new() -> (
            Self,
            tokio::sync::oneshot::Receiver<()>,
            tokio::sync::oneshot::Sender<()>,
        ) {
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            (
                Self {
                    started_tx: std::sync::Mutex::new(Some(started_tx)),
                    release_rx: Arc::new(tokio::sync::Mutex::new(release_rx)),
                },
                started_rx,
                release_tx,
            )
        }
    }

    #[async_trait]
    impl ModelClient for BlockingModelClient {
        fn provider_name(&self) -> &'static str {
            "mock"
        }

        async fn create_message_stream(
            &self,
            _request: rebon_api::CreateMessageRequest,
        ) -> rebon_api::ModelResult<rebon_api::StreamEventStream> {
            let release_rx = self.release_rx.clone();
            let started = self
                .started_tx
                .lock()
                .expect("blocking mock poisoned")
                .take();
            let stream = futures_util::stream::once(async move {
                if let Some(tx) = started {
                    let _ = tx.send(());
                }
                {
                    let mut release = release_rx.lock().await;
                    let _ = (&mut *release).await;
                }
                Ok::<_, rebon_api::ModelError>(StreamEvent::MessageStart {
                    message_id: "msg_blocking".into(),
                    model: "mock".into(),
                    usage: Usage::default(),
                })
            })
            .chain(futures_util::stream::iter(
                text_turn("background unblocked")
                    .into_iter()
                    .skip(1)
                    .map(Ok),
            ));
            Ok(Box::pin(stream))
        }
    }

    struct SensitiveShellTool {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for SensitiveShellTool {
        fn id(&self) -> ToolId {
            ToolId::new("Bash")
        }

        fn description(&self) -> &str {
            "sensitive shell test tool"
        }

        fn input_schema(&self) -> ToolInputSchema {
            json!({ "type": "object", "additionalProperties": true })
        }

        fn needs_permission(&self, _input: &Value) -> bool {
            true
        }

        async fn check_permissions(
            &self,
            input: &Value,
            _context: &ToolContext,
        ) -> ToolResult<PermissionDecision> {
            Ok(PermissionDecision::ask(
                PermissionRequest::new("Run shell command", "Approve sensitive command?"),
                Some(input.clone()),
            ))
        }

        async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(input)
        }
    }

    struct NullTool;

    #[async_trait]
    impl Tool for NullTool {
        fn id(&self) -> ToolId {
            ToolId::new("NullTool")
        }
        fn description(&self) -> &str {
            "null"
        }
        fn input_schema(&self) -> ToolInputSchema {
            json!({})
        }
        async fn validate_input(
            &self,
            _input: &Value,
            _context: &ToolContext,
        ) -> ToolResult<ValidationOutcome> {
            Ok(ValidationOutcome::valid())
        }
        async fn check_permissions(
            &self,
            _input: &Value,
            _context: &ToolContext,
        ) -> ToolResult<PermissionDecision> {
            Ok(PermissionDecision::allow(Value::Null))
        }
        async fn call(&self, _input: Value, _context: &ToolContext) -> ToolResult<Value> {
            Ok(Value::Null)
        }
    }

    struct ReadLikeTool;

    #[async_trait]
    impl Tool for ReadLikeTool {
        fn id(&self) -> ToolId {
            ToolId::new("Read")
        }
        fn description(&self) -> &str {
            "read"
        }
        fn input_schema(&self) -> ToolInputSchema {
            json!({ "type": "object", "additionalProperties": true })
        }
        async fn validate_input(
            &self,
            _input: &Value,
            _context: &ToolContext,
        ) -> ToolResult<ValidationOutcome> {
            Ok(ValidationOutcome::valid())
        }
        async fn check_permissions(
            &self,
            _input: &Value,
            _context: &ToolContext,
        ) -> ToolResult<PermissionDecision> {
            Ok(PermissionDecision::allow(Value::Null))
        }
        async fn call(&self, _input: Value, _context: &ToolContext) -> ToolResult<Value> {
            Ok(json!({ "content": "workspace" }))
        }
    }

    struct CreateAndStartTaskTool;

    #[async_trait]
    impl Tool for CreateAndStartTaskTool {
        fn id(&self) -> ToolId {
            ToolId::new("CreateAndStartTask")
        }
        fn description(&self) -> &str {
            "create and start task"
        }
        fn input_schema(&self) -> ToolInputSchema {
            json!({})
        }
        async fn validate_input(
            &self,
            _input: &Value,
            _context: &ToolContext,
        ) -> ToolResult<ValidationOutcome> {
            Ok(ValidationOutcome::valid())
        }
        async fn check_permissions(
            &self,
            _input: &Value,
            _context: &ToolContext,
        ) -> ToolResult<PermissionDecision> {
            Ok(PermissionDecision::allow(Value::Null))
        }
        async fn call(&self, _input: Value, context: &ToolContext) -> ToolResult<Value> {
            let task_id = rebon_tool::tasks::create_task(
                &context.task_list_id(),
                rebon_tool::tasks::NewTask {
                    subject: "child task".into(),
                    description: "agent-created task".into(),
                    active_form: None,
                    owner: None,
                    status: rebon_tool::tasks::TaskListStatus::InProgress,
                    blocks: Vec::new(),
                    blocked_by: Vec::new(),
                    metadata: Some({
                        let mut metadata = serde_json::Map::new();
                        metadata.insert(
                            "agent_id".into(),
                            json!(context.agent_id().unwrap_or("missing-agent")),
                        );
                        metadata
                    }),
                },
            )
            .map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: err.into(),
            })?;
            Ok(json!({ "task_id": task_id }))
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
                    reason: "denied".into(),
                });
            }
            tool.call(input, context).await
        }
    }

    fn text_turn(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message_id: "msg_1".into(),
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
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Usage::default(),
                },
            },
            StreamEvent::MessageStop,
        ]
    }

    struct TestTaskHome {
        _lock: crate::runtime::TestEnvLock,
        _dir: tempfile::TempDir,
        task_list_id: String,
        old_config_dir: Option<String>,
        old_task_list_id: Option<String>,
        old_team_name: Option<String>,
        old_session_id: Option<String>,
    }

    impl TestTaskHome {
        fn new(prefix: &str) -> Self {
            let lock = crate::runtime::test_config_env_lock();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let dir = tempfile::Builder::new()
                .prefix(&format!("rebon-coordinator-task-tests-{prefix}-"))
                .tempdir()
                .unwrap();
            let task_list_id = format!("{prefix}-{nanos:x}");
            let old_config_dir = std::env::var("REBON_CONFIG_DIR").ok();
            let old_task_list_id = std::env::var("REBON_TASK_LIST_ID").ok();
            let old_team_name = std::env::var("REBON_TEAM_NAME").ok();
            let old_session_id = std::env::var("REBON_SESSION_ID").ok();
            std::env::set_var("REBON_CONFIG_DIR", dir.path());
            std::env::set_var("REBON_TASK_LIST_ID", &task_list_id);
            std::env::remove_var("REBON_TEAM_NAME");
            std::env::remove_var("REBON_SESSION_ID");
            Self {
                _lock: lock,
                _dir: dir,
                task_list_id,
                old_config_dir,
                old_task_list_id,
                old_team_name,
                old_session_id,
            }
        }

        fn task_list_id(&self) -> &str {
            &self.task_list_id
        }
    }

    impl Drop for TestTaskHome {
        fn drop(&mut self) {
            restore_env("REBON_CONFIG_DIR", self.old_config_dir.as_deref());
            restore_env("REBON_TASK_LIST_ID", self.old_task_list_id.as_deref());
            restore_env("REBON_TEAM_NAME", self.old_team_name.as_deref());
            restore_env("REBON_SESSION_ID", self.old_session_id.as_deref());
        }
    }

    fn restore_env(name: &str, value: Option<&str>) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    #[test]
    fn metadata_reasoning_effort_accepts_config_aliases() {
        assert_eq!(
            metadata_reasoning_effort(&json!({ "effort": "high" })),
            Some(ReasoningEffort::High)
        );
        assert_eq!(
            metadata_reasoning_effort(&json!({ "reasoning_effort": "xhigh" })),
            Some(ReasoningEffort::XHigh)
        );
        assert_eq!(
            metadata_reasoning_effort(&json!({ "reasoningEffort": "xhigh" })),
            Some(ReasoningEffort::XHigh)
        );
        assert_eq!(
            metadata_reasoning_effort(&json!({ "effort": "bogus" })),
            None
        );
    }

    #[test]
    fn local_agent_pending_message_poller_drains_queued_messages() {
        let registry = TaskRegistry::new();
        let task_id = TaskId::new("agent-queued");
        let snapshot = local_agent_task_snapshot(
            task_id.clone(),
            TaskStatus::Running,
            "queued".into(),
            "prompt".into(),
            "general-purpose".into(),
            "mock".into(),
            None,
            None,
            true,
            1,
            None,
            None,
            None,
            None,
            serde_json::json!({}),
        );
        registry.insert(task_id.clone(), snapshot, PromptCancel::new());
        assert!(
            rebon_plugin_tasks::runtime::inject_user_message_to_local_agent(
                &registry,
                &task_id,
                "include this finding".into()
            )
        );

        let poller = LocalAgentPendingMessagePoller::new(registry.clone(), task_id.clone());
        let messages = poller.poll(AttachmentPollRequest::new(
            "agent-queued",
            "agent-queued",
            1,
            AttachmentPollPhase::Regular,
        ));
        assert_eq!(messages.len(), 1);
        match &messages[0].content[0] {
            ApiContentBlock::Text(block) => assert_eq!(block.text, "include this finding"),
            other => panic!("unexpected message block: {other:?}"),
        }
        assert!(poller
            .poll(AttachmentPollRequest::new(
                "agent-queued",
                "agent-queued",
                2,
                AttachmentPollPhase::Regular,
            ))
            .is_empty());
    }

    #[test]
    fn coordinator_workers_raise_tiny_iteration_caps() {
        assert_eq!(effective_worker_max_iterations(8, false), 8);
        assert_eq!(
            effective_worker_max_iterations(8, true),
            rebon_tool::DEFAULT_SUB_AGENT_MAX_ITERATIONS
        );
        assert_eq!(
            effective_worker_max_iterations(rebon_tool::DEFAULT_SUB_AGENT_MAX_ITERATIONS + 1, true),
            rebon_tool::DEFAULT_SUB_AGENT_MAX_ITERATIONS + 1
        );
    }

    /// The coordinator's Read allowlist is built from the notification's
    /// output file. Dropping the path for an invalid report left it with
    /// a failure message naming a file it was then forbidden to open, so
    /// it could only guess at what went wrong and respawn blind.
    #[test]
    fn coordinator_output_file_survives_an_invalid_but_present_report() {
        let dir = tempfile::Builder::new()
            .prefix("rebon-output-file-")
            .tempdir()
            .unwrap();
        let present = dir.path().join("agent-present.report.md");
        std::fs::write(&present, "## Summary\n\nhalf a report\n").unwrap();
        let present = present.to_string_lossy().to_string();

        assert_eq!(
            coordinator_output_file(Some(present.clone()), true),
            Some(present.clone()),
            "an invalid report that exists must stay readable"
        );
        assert_eq!(
            coordinator_output_file(Some(present), false),
            Some(
                dir.path()
                    .join("agent-present.report.md")
                    .to_string_lossy()
                    .to_string()
            )
        );

        let missing = dir
            .path()
            .join("agent-missing.report.md")
            .to_string_lossy()
            .to_string();
        assert_eq!(coordinator_output_file(Some(missing.clone()), true), None);

        let empty = dir.path().join("agent-empty.report.md");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(
            coordinator_output_file(Some(empty.to_string_lossy().to_string()), true),
            None,
            "an empty file teaches the coordinator nothing"
        );
        assert_eq!(coordinator_output_file(None, false), None);
    }

    #[test]
    fn coordinator_report_file_stem_is_derived_from_agent_id() {
        assert_eq!(safe_report_file_stem("agent-b92f9ff7"), "agent-b92f9ff7");
        assert_eq!(safe_report_file_stem("agent:bad/path"), "agent_bad_path");
        assert!(report_file_path_for_agent_id("agent-b92f9ff7")
            .replace('\\', "/")
            .ends_with("/agent-b92f9ff7.report.md"));
    }

    #[test]
    fn only_explore_auto_reports_from_final_text() {
        assert!(should_auto_report_from_final_text(Some("Explore")));
        assert!(should_auto_report_from_final_text(Some("explore")));
        assert!(!should_auto_report_from_final_text(Some("Plan")));
        assert!(!should_auto_report_from_final_text(None));
    }

    #[test]
    fn implementation_worktree_failure_fails_closed_before_worker_execution() {
        let mut spec = SubAgentSpec::new("fix bug");
        spec.cwd = Some(
            std::env::temp_dir()
                .join("rebon-non-git-cwd")
                .to_string_lossy()
                .to_string(),
        );
        spec.task_kind = Some(SubAgentTaskKind::Implementation);
        spec.metadata = json!({ "coordinator_task_kind": "implementation" });

        let policy = runtime_worktree_policy(true, true);
        assert!(policy.require_implementation);
        assert!(policy.allow_explicit);

        let err = prepare_runtime_worktree(
            &mut spec,
            "agent-non-git",
            SubAgentTaskKind::Implementation,
            policy,
        )
        .unwrap_err();
        assert!(err.contains("required implementation worktree creation failed"));
        assert!(err.contains("not in a git repository"));
    }

    #[test]
    fn implementation_worktree_disabled_skips_required_worktree_creation() {
        let mut spec = SubAgentSpec::new("fix bug");
        spec.cwd = Some(
            std::env::temp_dir()
                .join("rebon-non-git-cwd")
                .to_string_lossy()
                .to_string(),
        );
        spec.task_kind = Some(SubAgentTaskKind::Implementation);
        spec.metadata = json!({ "coordinator_task_kind": "implementation" });

        let git = prepare_runtime_worktree(
            &mut spec,
            "agent-non-git",
            SubAgentTaskKind::Implementation,
            runtime_worktree_policy(true, false),
        )
        .unwrap();
        assert!(git.is_none());
    }

    #[test]
    fn coordinator_worktree_disabled_allows_explicit_isolation_request() {
        let policy = runtime_worktree_policy(true, false);
        assert!(!policy.require_implementation);
        assert!(policy.allow_explicit);

        let tmp = tempfile::Builder::new()
            .prefix("rebon-spawner-explicit-worktree-")
            .tempdir()
            .unwrap();
        Command::new("git")
            .current_dir(tmp.path())
            .args(["init", "-q"])
            .status()
            .unwrap();
        Command::new("git")
            .current_dir(tmp.path())
            .args(["checkout", "-b", "main"])
            .status()
            .unwrap();
        std::fs::write(tmp.path().join("README.md"), "hi\n").unwrap();
        Command::new("git")
            .current_dir(tmp.path())
            .args(["add", "."])
            .status()
            .unwrap();
        Command::new("git")
            .current_dir(tmp.path())
            .args([
                "-c",
                "user.email=a@b.c",
                "-c",
                "user.name=test",
                "commit",
                "-qm",
                "seed",
            ])
            .status()
            .unwrap();

        let scoped_root = tmp.path().join("crates/rebon-tool");
        std::fs::create_dir_all(&scoped_root).unwrap();
        let mut scoped_spec = SubAgentSpec::new("fix one crate");
        scoped_spec.cwd = Some(scoped_root.to_string_lossy().to_string());
        scoped_spec.allowed_roots = vec![scoped_root.clone()];
        scoped_spec.metadata = json!({ "isolation": "worktree" });

        let scoped_git = prepare_runtime_worktree(
            &mut scoped_spec,
            "agent-scoped-subdir",
            SubAgentTaskKind::Other,
            policy,
        )
        .expect("optional worktree should fall back inside the authorized subtree");
        assert!(scoped_git.is_none());
        assert_eq!(scoped_spec.cwd.as_deref(), scoped_root.to_str());
        assert_eq!(scoped_spec.allowed_roots, vec![scoped_root.clone()]);

        let mut required_scoped_spec = SubAgentSpec::new("fix one crate");
        required_scoped_spec.cwd = Some(scoped_root.to_string_lossy().to_string());
        required_scoped_spec.allowed_roots = vec![scoped_root];
        required_scoped_spec.task_kind = Some(SubAgentTaskKind::Implementation);
        let err = prepare_runtime_worktree(
            &mut required_scoped_spec,
            "agent-required-scoped-subdir",
            SubAgentTaskKind::Implementation,
            runtime_worktree_policy(true, true),
        )
        .unwrap_err();
        assert!(err.contains("required implementation worktree creation failed"));
        assert!(err.contains("containing Git root"));

        let mut spec = SubAgentSpec::new("fix bug");
        spec.cwd = Some(tmp.path().to_string_lossy().to_string());
        spec.allowed_roots = vec![tmp.path().to_path_buf()];
        spec.task_kind = Some(SubAgentTaskKind::Implementation);
        spec.metadata = json!({ "isolation": "worktree" });

        let git = prepare_runtime_worktree(
            &mut spec,
            "agent-explicit",
            SubAgentTaskKind::Implementation,
            policy,
        )
        .expect("explicit worktree attempt should not error")
        .expect("explicit worktree isolation creates a worktree");
        let worktree_path = git.worktree_path.as_ref().expect("worktree path");
        assert_eq!(spec.cwd.as_deref(), Some(worktree_path.as_str()));
        assert_eq!(spec.allowed_roots, vec![PathBuf::from(worktree_path)]);

        let info = rebon_tool::worktree::AgentWorktreeInfo {
            worktree_path: PathBuf::from(worktree_path),
            worktree_branch: git.worktree_branch.expect("worktree branch"),
            head_commit: git.base_commit.expect("base commit"),
            source_worktree: PathBuf::from(git.source_worktree.expect("source worktree")),
            source_branch: git.source_branch,
            git_root: PathBuf::from(git.git_root.expect("git root")),
        };
        let _ = rebon_tool::worktree::remove_agent_worktree(&info);
    }

    #[test]
    fn non_coordinator_explicit_isolation_still_allows_worktree_attempt() {
        let mut spec = SubAgentSpec::new("fix bug");
        spec.cwd = Some(
            std::env::temp_dir()
                .join("rebon-non-git-cwd")
                .to_string_lossy()
                .to_string(),
        );
        spec.metadata = json!({ "isolation": "worktree" });

        let git = prepare_runtime_worktree(
            &mut spec,
            "agent-explicit",
            SubAgentTaskKind::Other,
            runtime_worktree_policy(false, false),
        )
        .unwrap();
        assert!(git.is_none());
    }

    #[test]
    fn runtime_worktree_uses_explicit_root_instead_of_process_cwd() {
        let root = tempfile::Builder::new()
            .prefix("rebon-runtime-worktree-explicit-root-")
            .tempdir()
            .unwrap();
        let mut spec = SubAgentSpec::new("fix bug");
        spec.allowed_roots = vec![root.path().to_path_buf()];
        spec.metadata = json!({ "isolation": "worktree" });

        let git = prepare_runtime_worktree(
            &mut spec,
            "agent-explicit-root",
            SubAgentTaskKind::Other,
            runtime_worktree_policy(true, false),
        )
        .unwrap();

        assert!(git.is_none());
        assert_eq!(spec.cwd.as_deref(), root.path().to_str());
        assert_eq!(spec.allowed_roots, vec![root.path().to_path_buf()]);
    }

    /// A throwaway repository with two commits and a clean worktree.
    ///
    /// [`validate_implementation_commit`] shells out to git in whatever
    /// directory the metadata names. These tests used to name the process
    /// cwd, which made them depend on the checkout they happened to run in:
    /// CI clones with `--depth=1`, where the `HEAD^` they ask for does not
    /// exist at all, and a dirty tree sent several of them down the
    /// "uncommitted changes" branch where the interesting assertions are
    /// skipped. A fixture repo makes both knowns.
    struct ImplementationRepo {
        dir: tempfile::TempDir,
    }

    impl ImplementationRepo {
        fn new(prefix: &str) -> Self {
            let dir = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
            let repo = ImplementationRepo { dir };
            repo.git(&["init", "--quiet"]);
            repo.git(&["config", "user.name", "Rebon Test"]);
            repo.git(&["config", "user.email", "test@example.invalid"]);
            repo.git(&["config", "commit.gpgsign", "false"]);
            repo.commit("first.txt", "one");
            // The second commit is what makes `HEAD^` resolvable.
            repo.commit("second.txt", "two");
            repo
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        fn git(&self, args: &[&str]) {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(self.dir.path())
                .output()
                .unwrap_or_else(|err| panic!("git {args:?} could not run: {err}"));
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        fn commit(&self, file: &str, body: &str) {
            std::fs::write(self.dir.path().join(file), body).unwrap();
            self.git(&["add", file]);
            self.git(&["commit", "--quiet", "-m", body]);
        }
    }

    #[test]
    fn implementation_validation_fails_when_head_equals_base() {
        let repo = ImplementationRepo::new("rebon-impl-head-equals-base-");
        let cwd = repo.path().to_path_buf();
        let head = rebon_tool::worktree::git_current_head(&cwd).unwrap();
        let branch = rebon_tool::worktree::git_current_branch(&cwd).unwrap();
        let mut git = SubAgentGitMetadata {
            worktree_path: Some(cwd.to_string_lossy().to_string()),
            worktree_branch: Some(branch),
            base_commit: Some(head),
            ..Default::default()
        };

        let err = validate_implementation_commit(&mut git, None).unwrap_err();
        assert!(err.contains("HEAD did not advance beyond base commit"));
        assert_eq!(git.head_commit, git.base_commit);
    }

    #[test]
    fn implementation_validation_records_dirty_status_before_report_hash_check() {
        let repo = ImplementationRepo::new("rebon-impl-dirty-status-");
        let cwd = repo.path().to_path_buf();
        let head = rebon_tool::worktree::git_current_head(&cwd).unwrap();
        let branch = rebon_tool::worktree::git_current_branch(&cwd).unwrap();
        let mut git = SubAgentGitMetadata {
            worktree_path: Some(cwd.to_string_lossy().to_string()),
            worktree_branch: Some(branch),
            base_commit: Some(format!("{head}^")),
            ..Default::default()
        };

        let err = validate_implementation_commit(&mut git, None).unwrap_err();
        if let Some(status) = git.status_output.as_ref() {
            if !status.trim().is_empty() {
                assert!(err.contains("worktree has uncommitted changes"));
                assert_eq!(git.dirty_after_commit, Some(true));
                return;
            }
        }
        assert!(
            err.contains("report file did not contain a commit hash"),
            "clean worktree should progress to report hash validation, got {err}"
        );
        assert_eq!(git.dirty_after_commit, Some(false));
    }

    #[test]
    fn implementation_validation_fails_when_report_hash_missing() {
        let repo = ImplementationRepo::new("rebon-impl-hash-missing-");
        let cwd = repo.path().to_path_buf();
        let head = rebon_tool::worktree::git_current_head(&cwd).unwrap();
        let branch = rebon_tool::worktree::git_current_branch(&cwd).unwrap();
        let mut git = SubAgentGitMetadata {
            worktree_path: Some(cwd.to_string_lossy().to_string()),
            worktree_branch: Some(branch),
            base_commit: Some(format!("{head}^")),
            ..Default::default()
        };

        let err = validate_implementation_commit(&mut git, None).unwrap_err();
        if git.dirty_after_commit == Some(false) {
            assert!(err.contains("report file did not contain a commit hash"));
        }
    }

    #[test]
    fn implementation_validation_fails_when_report_hash_mismatches() {
        let repo = ImplementationRepo::new("rebon-impl-hash-mismatch-");
        let cwd = repo.path().to_path_buf();
        let head = rebon_tool::worktree::git_current_head(&cwd).unwrap();
        let branch = rebon_tool::worktree::git_current_branch(&cwd).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("rebon-mismatch-report.md");
        std::fs::write(
            &report,
            "## Implementation Commit\n\nCommit hash: 0000000\n",
        )
        .unwrap();
        let mut git = SubAgentGitMetadata {
            worktree_path: Some(cwd.to_string_lossy().to_string()),
            worktree_branch: Some(branch),
            base_commit: Some(format!("{head}^")),
            ..Default::default()
        };

        let err = validate_implementation_commit(&mut git, report.to_str()).unwrap_err();
        if git.dirty_after_commit == Some(false) {
            assert!(err.contains("report commit hash 0000000 does not match runtime HEAD"));
            assert_eq!(git.commit_hash.as_deref(), Some("0000000"));
        }
    }

    #[test]
    fn implementation_validation_passes_with_clean_head_and_matching_report_hash() {
        let repo = ImplementationRepo::new("rebon-impl-matching-hash-");
        let cwd = repo.path().to_path_buf();
        let head = rebon_tool::worktree::git_current_head(&cwd).unwrap();
        let branch = rebon_tool::worktree::git_current_branch(&cwd).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("rebon-matching-report.md");
        std::fs::write(
            &report,
            format!("## Implementation Commit\n\nCommit hash: {head}\n"),
        )
        .unwrap();
        let mut git = SubAgentGitMetadata {
            worktree_path: Some(cwd.to_string_lossy().to_string()),
            worktree_branch: Some(branch),
            base_commit: Some(format!("{head}^")),
            ..Default::default()
        };

        let result = validate_implementation_commit(&mut git, report.to_str());
        if git.dirty_after_commit == Some(false) {
            result.unwrap();
            assert_eq!(git.head_commit.as_deref(), Some(head.as_str()));
            assert_eq!(git.commit_hash.as_deref(), Some(head.as_str()));
            assert_eq!(git.dirty_after_commit, Some(false));
            assert_eq!(git.status_output.as_deref(), Some(""));
        }
    }

    #[tokio::test]
    async fn foreground_wait_cancellation_stops_worker_and_finishes_registry_turn() {
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("foreground-cancel");
        for agent_type in ["Explore", "general-purpose"] {
            for (timed_out, backgrounded) in [(false, false), (true, false), (false, true)] {
                let engine =
                    Arc::new(Engine::new().with_permission_broker(Arc::new(ApproveBroker)));
                let (blocking, started_rx, release_tx) = BlockingModelClient::new();
                let registry = TaskRegistry::new();
                let spawner =
                    EngineSubAgentSpawner::new(Arc::downgrade(&engine), Arc::new(blocking))
                        .with_default_model("mock")
                        .with_coordinator_mode(false)
                        .with_test_task_registry(registry.clone());
                let mut spec = SubAgentSpec::new("wait until cancelled");
                spec.metadata = json!({
                    "agent_id": "foreground-cancel",
                    "agent_type": agent_type,
                });
                let task_id = TaskId::new("foreground-cancel");
                let mut spawn = Box::pin(spawner.spawn_with_progress(spec, None));
                tokio::select! {
                    started = started_rx => started.expect("worker started"),
                    result = &mut spawn => panic!("worker unexpectedly returned: {result:?}"),
                }
                let cancel = registry
                    .cancel_handle(&task_id)
                    .expect("registered cancel handle");
                assert!(registry.task_turn_is_active(&task_id));
                if backgrounded {
                    assert!(registry.set_backgrounded(&task_id));
                }
                if timed_out {
                    assert!(tokio::time::timeout(std::time::Duration::ZERO, spawn)
                        .await
                        .is_err());
                } else {
                    drop(spawn);
                }
                assert_eq!(
                    cancel.is_cancelled(),
                    !backgrounded,
                    "{agent_type} timeout={timed_out} background={backgrounded}"
                );
                if backgrounded {
                    release_tx.send(()).expect("background worker stays alive");
                }
                tokio::time::timeout(std::time::Duration::from_secs(3), async {
                    while registry.task_turn_is_active(&task_id) {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("cancelled worker releases its turn");
                let snapshot = registry
                    .snapshot(&task_id)
                    .expect("terminal snapshot retained");
                let status = if !backgrounded {
                    TaskStatus::Killed
                } else if agent_type == "Explore" {
                    TaskStatus::Completed
                } else {
                    TaskStatus::Running
                };
                assert_eq!(snapshot.status, status);
                assert_eq!(
                    snapshot.result.as_ref().unwrap()["status"],
                    if backgrounded {
                        "completed"
                    } else {
                        "cancelled"
                    }
                );
                assert_eq!(
                    snapshot.end_time_ms.is_some(),
                    status != TaskStatus::Running
                );
                let finished = registry
                    .task_live_events(&task_id, None)
                    .unwrap()
                    .events
                    .into_iter()
                    .filter_map(|event| match event.kind {
                        TaskLiveEventKind::Finished { status, .. } => Some(status),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(finished, vec![status]);
            }
        }
    }

    #[tokio::test]
    async fn foreground_wait_cancellation_after_result_ready_preserves_terminal_result() {
        struct ResultReady(tokio::sync::Notify);
        impl std::task::Wake for ResultReady {
            fn wake(self: Arc<Self>) {
                self.0.notify_one();
            }
        }

        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("foreground-ready");
        let engine = Arc::new(Engine::new().with_permission_broker(Arc::new(ApproveBroker)));
        let (blocking, started_rx, release_tx) = BlockingModelClient::new();
        let registry = TaskRegistry::new();
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), Arc::new(blocking))
            .with_default_model("mock")
            .with_coordinator_mode(false)
            .with_test_task_registry(registry.clone());
        let mut spec = SubAgentSpec::new("finish before the caller resumes");
        spec.metadata = json!({"agent_id": "foreground-ready", "agent_type": "Explore"});
        let task_id = TaskId::new("foreground-ready");
        let mut spawn = Box::pin(spawner.spawn_with_progress(spec, None));
        tokio::select! {
            started = started_rx => started.expect("worker started"),
            result = &mut spawn => panic!("worker unexpectedly returned: {result:?}"),
        }
        let ready = Arc::new(ResultReady(tokio::sync::Notify::new()));
        let waker = std::task::Waker::from(ready.clone());
        assert!(std::future::Future::poll(
            spawn.as_mut(),
            &mut std::task::Context::from_waker(&waker)
        )
        .is_pending());
        release_tx.send(()).expect("worker is waiting for release");
        tokio::time::timeout(std::time::Duration::from_secs(3), ready.0.notified())
            .await
            .expect("result delivery wakes the caller");
        assert!(registry.task_turn_is_active(&task_id));
        drop(spawn);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while registry.task_turn_is_active(&task_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("unclaimed result is finalized");
        let snapshot = registry
            .snapshot(&task_id)
            .expect("terminal snapshot retained");
        assert_eq!(snapshot.status, TaskStatus::Completed);
        assert_eq!(
            snapshot.result.as_ref().unwrap()["final_text"],
            "background unblocked"
        );
        assert_eq!(
            registry
                .task_live_events(&task_id, None)
                .unwrap()
                .events
                .into_iter()
                .filter(|event| matches!(event.kind, TaskLiveEventKind::Finished { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn foreground_wait_without_background_sender_returns_normally() {
        for closed_sender in [false, true] {
            let engine = Arc::new(Engine::new());
            let mock = MockModelClient::new();
            mock.push_turn(text_turn("normal result"));
            let cancel = PromptCancel::new();
            let handle = spawn_worker(
                engine,
                SessionHandle::new(Arc::new(mock)),
                WorkerSpec::new("run normally", "mock"),
                cancel.clone(),
            )
            .unwrap();
            let (sender, receiver) = watch::channel(false);
            drop(sender);
            let (result, detached) = wait_worker_and_mirror_progress_detachable(
                handle,
                None,
                TaskId::new("no-background-sender"),
                None,
                None,
                closed_sender.then_some(receiver),
                true,
                false,
            )
            .await;
            assert!(!detached);
            assert!(!cancel.is_cancelled());
            assert_eq!(result.status, WorkerStatus::Completed);
            assert_eq!(result.final_text, "normal result");
        }
    }

    #[tokio::test]
    async fn engine_sub_agent_spawner_detaches_foreground_task_when_backgrounded() {
        let _coord = CoordModeGuard::set(false);
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        let (blocking, started_rx, release_tx) = BlockingModelClient::new();
        let client: Arc<dyn ModelClient> = Arc::new(blocking);
        let registry = TaskRegistry::new();
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone());

        let mut spec = SubAgentSpec::new("run slowly");
        spec.metadata = json!({
            "agent_id": "agent-detach-test",
            "agent_type": "general-purpose",
            "description": "detach test"
        });
        let task_id = TaskId::new("agent-detach-test");
        let spawn_task = tokio::spawn(async move { spawner.spawn_with_progress(spec, None).await });

        started_rx.await.expect("worker should start");
        let snap = registry.snapshot(&task_id).expect("task registered");
        assert_eq!(snap.status, TaskStatus::Running);
        assert!(!snap.is_backgrounded);

        assert!(registry.set_backgrounded(&task_id));
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), spawn_task)
            .await
            .expect("foreground spawn should detach promptly")
            .expect("spawn task should not panic")
            .expect("spawn should succeed");
        assert_eq!(result.status, "async_launched");
        assert_eq!(result.agent_id.as_deref(), Some("agent-detach-test"));
        let snap = registry.snapshot(&task_id).expect("task still registered");
        assert_eq!(snap.status, TaskStatus::Running);
        assert!(snap.is_backgrounded);

        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if registry
                    .snapshot(&task_id)
                    .is_some_and(|snap| rebon_plugin_tasks::runtime::is_agent_snapshot_idle(&snap))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("detached mirror should complete registry task");
        let snap = registry.snapshot(&task_id).unwrap();
        assert!(snap.is_backgrounded);
        assert_eq!(snap.status, TaskStatus::Running);
        assert!(rebon_plugin_tasks::runtime::is_agent_snapshot_idle(&snap));
        assert_eq!(snap.last_progress.as_deref(), Some("background unblocked"));
        let finished_events = registry
            .task_live_events(&task_id, None)
            .expect("task event journal")
            .events
            .into_iter()
            .filter_map(|event| match event.kind {
                TaskLiveEventKind::Finished { status, .. } => Some(status),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(finished_events, vec![TaskStatus::Running]);
    }

    #[tokio::test]
    async fn engine_sub_agent_spawner_runs_worker_and_returns_result() {
        let _coord = CoordModeGuard::set(false);
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        let mock = MockModelClient::new();
        mock.push_turn(text_turn("sub-agent says hi"));
        let client: Arc<dyn ModelClient> = Arc::new(mock);

        let spawner =
            EngineSubAgentSpawner::new(Arc::downgrade(&engine), client).with_default_model("mock");
        let result = spawner
            .spawn(SubAgentSpec::new("run the task"))
            .await
            .unwrap();
        assert_eq!(result.status, "completed");
        assert_eq!(result.final_text, "sub-agent says hi");
        assert!(result.stop_reason.is_some());
    }

    #[tokio::test]
    async fn engine_sub_agent_spawner_appends_scratchpad_to_worker_system_prompt() {
        let _coord = CoordModeGuard::set(false);
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn("done"));
        let client: Arc<dyn ModelClient> = mock.clone();

        let spawner =
            EngineSubAgentSpawner::new(Arc::downgrade(&engine), client).with_default_model("mock");
        let mut spec = SubAgentSpec::new("verify the change");
        spec.system = Some("You are a verification specialist.".to_string());
        let result = spawner.spawn(spec).await.unwrap();
        assert_eq!(result.status, "completed");

        let captured = mock.captured_requests();
        assert_eq!(captured.len(), 1);
        let system = captured[0]
            .system
            .as_deref()
            .expect("worker with a base system prompt gets the suffix appended");
        assert!(system.starts_with("You are a verification specialist."));
        // Shared notes stay...
        assert!(system.contains("Notes:"));
        // ...and the scratchpad pointer now rides along, aimed at a
        // path ending in `scratchpad` outside the project tree.
        assert!(system.contains("# Scratchpad Directory"));
        assert!(system.replace('\\', "/").contains("/scratchpad"));
    }

    #[tokio::test]
    async fn engine_sub_agent_spawner_carries_ultraplan_policy_into_child_query() {
        let _coord = CoordModeGuard::set(false);
        let engine =
            Arc::new(Engine::with_builtin_tools().with_permission_broker(Arc::new(ApproveBroker)));

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn("policy-aware worker"));
        let client: Arc<dyn ModelClient> = mock.clone();

        let spawner =
            EngineSubAgentSpawner::new(Arc::downgrade(&engine), client).with_default_model("mock");
        let policy = ExecutionPolicy::ultraplan(UltraplanContext::planning_turn(
            "run-policy",
            "plan_mode_active",
            PolicyMode::Observe,
        ));
        let mut policy = policy;
        if let Some(ultraplan) = policy.ultraplan.as_mut() {
            ultraplan.allowed_tools = vec![
                "Read".to_string(),
                "Glob".to_string(),
                "Grep".to_string(),
                "ToolSearch".to_string(),
            ];
        }
        let mut spec = SubAgentSpec::new("run with policy");
        spec.execution_policy = Some(policy.clone());
        spec.tool_filter = Some(ToolFilter::allow_only([
            "Read".to_string(),
            "Glob".to_string(),
            "Grep".to_string(),
            "ToolSearch".to_string(),
        ]));
        spec.metadata = json!({
            "ultraplan_id": "run-policy",
            "ultraplan_phase": "plan_mode_active",
            "ultraplan_role": "researcher",
            "ultraplan_policy_mode": "enforce"
        });

        let result = spawner.spawn(spec).await.unwrap();
        assert_eq!(result.status, "completed");

        let captured = mock.captured_requests();
        assert_eq!(captured.len(), 1);
        let tool_names: Vec<_> = captured[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert!(tool_names.contains(&"Read"));
        assert!(tool_names.contains(&"Glob"));
        assert!(tool_names.contains(&"Grep"));
        assert!(tool_names.contains(&"ToolSearch"));
        assert!(!tool_names.contains(&"Write"));
        assert!(!tool_names.contains(&"Bash"));
        let system = captured[0]
            .system
            .as_deref()
            .expect("ultraplan worker should receive a policy preamble");
        assert!(system.contains("REBON LOCAL ULTRAPLAN WORKER CONTEXT"));
        assert!(system.contains("ultraplan_id: run-policy"));
        assert!(system.contains("ultraplan_role: researcher"));
        assert!(system.contains("Do not edit project files or run shell commands"));
    }

    #[tokio::test]
    async fn engine_sub_agent_spawner_allows_writable_ultrawork_implementation_tools() {
        let _coord = CoordModeGuard::set(true);
        let engine =
            Arc::new(Engine::with_builtin_tools().with_permission_broker(Arc::new(ApproveBroker)));

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn("implementation worker"));
        let client: Arc<dyn ModelClient> = mock.clone();

        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_base_filter(rebon_core::coordinator_mode::async_agent_filter())
            .with_coordinator_mode(true)
            .with_coordinator_use_worktree(false);
        let mut context =
            UltraplanContext::planning_turn("run-policy", "ultrawork", PolicyMode::Enforce);
        context.read_only = false;
        context.allowed_tools = rebon_core::coordinator_mode::ASYNC_AGENT_ALLOWED_TOOLS
            .iter()
            .map(|tool| (*tool).to_string())
            .collect();
        context.denied_tools.clear();
        let policy = ExecutionPolicy::ultraplan(context);
        let mut spec = SubAgentSpec::new("implement with policy");
        spec.execution_policy = Some(policy);
        spec.task_kind = Some(SubAgentTaskKind::Implementation);
        spec.metadata = json!({
            "agent_type": "Explore",
            "coordinator_task_kind": "implementation",
            "ultraplan_id": "run-policy",
            "ultraplan_phase": "ultrawork",
            "ultraplan_role": "implementer",
            "ultraplan_policy_mode": "enforce"
        });

        let result = spawner.spawn(spec).await.unwrap();
        assert_eq!(result.status, "completed");

        let captured = mock.captured_requests();
        assert_eq!(captured.len(), 1);
        let tool_names: Vec<_> = captured[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert!(tool_names.contains(&"Read"));
        assert!(tool_names.contains(&"Edit"));
        assert!(tool_names.contains(&"Write"));
        assert!(tool_names.contains(&"Bash"));
        assert!(!tool_names.contains(&"Agent"));
        assert!(!tool_names.contains(&"TeamCreate"));
        let system = captured[0]
            .system
            .as_deref()
            .expect("ultraplan worker should receive a policy preamble");
        assert!(system.contains("writable implementation worker"));
    }

    #[tokio::test]
    async fn engine_sub_agent_spawner_injects_plan_fidelity_worker_contract() {
        let _coord = CoordModeGuard::set(true);
        let engine =
            Arc::new(Engine::with_builtin_tools().with_permission_broker(Arc::new(ApproveBroker)));

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn("implementation worker"));
        mock.push_turn(text_turn("implementation worker done"));
        let client: Arc<dyn ModelClient> = mock.clone();

        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_base_filter(rebon_core::coordinator_mode::async_agent_filter())
            .with_coordinator_mode(true)
            .with_coordinator_use_worktree(false);
        let mut context = UltraplanContext::planning_turn("run-policy", "ceo", PolicyMode::Enforce);
        context.read_only = false;
        context.plan_fidelity = true;
        context.allowed_tools = rebon_core::coordinator_mode::ASYNC_AGENT_ALLOWED_TOOLS
            .iter()
            .map(|tool| (*tool).to_string())
            .collect();
        context.denied_tools.clear();
        let policy = ExecutionPolicy::ultraplan(context);
        let mut spec = SubAgentSpec::new("implement step from Execution Cards");
        spec.execution_policy = Some(policy);
        spec.task_kind = Some(SubAgentTaskKind::Implementation);
        spec.metadata = json!({
            "ultraplan_role": "implementer",
        });

        let _result = spawner.spawn(spec).await.unwrap();

        let captured = mock.captured_requests();
        let system = captured[0]
            .system
            .as_deref()
            .expect("ultraplan worker should receive a policy preamble");
        assert!(system.contains("plan-fidelity execution"));
        assert!(system.contains("do NOT re-explore the repository"));
        assert!(system.contains("Follow only the relevant Execution Cards"));
    }

    #[tokio::test]
    async fn foreground_spawn_cleans_up_agent_created_in_progress_tasks() {
        let _coord = CoordModeGuard::set(false);
        let task_home = TestTaskHome::new("agent-child-cleanup");
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(CreateAndStartTaskTool));
        let engine = Arc::new(engine);

        let mock = MockModelClient::new();
        mock.push_turn(tool_use_turn(
            "create-task",
            "CreateAndStartTask",
            "toolu_create_task",
            "{}",
        ));
        mock.push_turn(text_turn("done"));
        let client: Arc<dyn ModelClient> = Arc::new(mock);
        let registry = TaskRegistry::new();

        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone());
        let linked_task = rebon_tool::tasks::create_task(
            task_home.task_list_id(),
            rebon_tool::tasks::NewTask {
                subject: "parent task".into(),
                description: "delegated before the agent started".into(),
                owner: Some("reader-edge-fixer".into()),
                status: rebon_tool::tasks::TaskListStatus::InProgress,
                ..Default::default()
            },
        )
        .unwrap();
        let mut spec = SubAgentSpec::new("create a task then finish");
        spec.task_list_id = Some(task_home.task_list_id().to_string());
        spec.metadata = json!({
            "agent_id": "agent-clean-child",
            "description": "Clean child tasks",
            "task_ids": [linked_task],
        });

        let result = spawner.spawn(spec).await.unwrap();
        assert_eq!(result.status, "completed");
        let tasks = rebon_tool::tasks::list_tasks(task_home.task_list_id()).unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(tasks
            .iter()
            .all(|task| task.status == rebon_tool::tasks::TaskListStatus::Completed));
        let child_task = tasks
            .iter()
            .find(|task| task.metadata.is_some())
            .expect("agent-created task");
        assert_eq!(
            child_task.metadata.as_ref().unwrap()["agent_id"],
            "agent-clean-child"
        );
    }

    #[tokio::test]
    async fn foreground_spawn_requeues_agent_created_in_progress_tasks_on_failure() {
        let _coord = CoordModeGuard::set(false);
        let task_home = TestTaskHome::new("agent-child-requeue");
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(CreateAndStartTaskTool));
        let engine = Arc::new(engine);

        let mock = MockModelClient::new();
        mock.push_turn(tool_use_turn(
            "create-task",
            "CreateAndStartTask",
            "toolu_create_task",
            "{}",
        ));
        let client: Arc<dyn ModelClient> = Arc::new(mock);
        let registry = TaskRegistry::new();

        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone());
        let linked_task = rebon_tool::tasks::create_task(
            task_home.task_list_id(),
            rebon_tool::tasks::NewTask {
                subject: "parent task".into(),
                description: "delegated before the agent started".into(),
                owner: Some("reader-edge-fixer".into()),
                status: rebon_tool::tasks::TaskListStatus::InProgress,
                ..Default::default()
            },
        )
        .unwrap();
        let mut spec = SubAgentSpec::new("create a task then fail");
        spec.task_list_id = Some(task_home.task_list_id().to_string());
        spec.metadata = json!({
            "agent_id": "agent-requeue-child",
            "description": "Requeue child tasks",
            "task_id": linked_task,
        });

        let result = spawner.spawn(spec).await.unwrap();
        assert_eq!(result.status, "failed");
        let tasks = rebon_tool::tasks::list_tasks(task_home.task_list_id()).unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(tasks
            .iter()
            .all(|task| task.status == rebon_tool::tasks::TaskListStatus::Pending));
        let child_task = tasks
            .iter()
            .find(|task| task.metadata.is_some())
            .expect("agent-created task");
        assert_eq!(
            child_task.metadata.as_ref().unwrap()["agent_id"],
            "agent-requeue-child"
        );
    }

    #[tokio::test]
    async fn foreground_spawn_snapshot_metadata_keeps_generated_agent_id() {
        let _coord = CoordModeGuard::set(false);
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        let mock = MockModelClient::new();
        mock.push_turn(text_turn("foreground done"));
        let client: Arc<dyn ModelClient> = Arc::new(mock);
        let registry = TaskRegistry::new();

        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone());
        let mut spec = SubAgentSpec::new("run in foreground");
        spec.metadata = json!({
            "description": "Foreground generated id",
            "ultraplan_id": "run-active",
            "ultraplan_phase": "plan_mode_active"
        });

        let result = spawner.spawn(spec).await.unwrap();
        let agent_id = result
            .agent_id
            .expect("foreground spawn should return generated agent id");
        let snap = registry
            .snapshot(&TaskId::new(agent_id.clone()))
            .expect("foreground task should be registered");

        assert_eq!(snap.status, TaskStatus::Running);
        assert!(rebon_plugin_tasks::runtime::is_agent_snapshot_idle(&snap));
        assert_eq!(snap.title, "Foreground generated id");
        assert_eq!(snap.metadata["agent_id"], agent_id);
        assert_eq!(snap.metadata["description"], "Foreground generated id");
        assert_eq!(snap.metadata["ultraplan_id"], "run-active");
        assert_eq!(snap.metadata["ultraplan_phase"], "plan_mode_active");
    }

    #[tokio::test]
    async fn spawn_detached_preserves_foreground_resumable_runtime() {
        let _coord = CoordModeGuard::set(false);
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn("detached first turn"));
        mock.push_turn(text_turn("detached follow-up"));
        let client: Arc<dyn ModelClient> = mock;
        let registry = TaskRegistry::new();
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone());
        let mut spec = SubAgentSpec::new("start detached but foreground");
        spec.metadata = json!({
            "agent_id": "agent-detached-foreground",
            "description": "Detached foreground agent",
        });

        let id = spawner.spawn_detached(spec).await.unwrap();
        let task_id = TaskId::new(id);
        assert!(
            !registry
                .snapshot(&task_id)
                .expect("detached task should be registered")
                .is_backgrounded
        );

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if registry.snapshot(&task_id).is_some_and(|snapshot| {
                    rebon_plugin_tasks::runtime::is_agent_snapshot_idle(&snapshot)
                        && snapshot
                            .result
                            .as_ref()
                            .and_then(|result| result.get("final_text"))
                            .and_then(Value::as_str)
                            == Some("detached first turn")
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("detached first turn should become idle");

        rebon_plugin_tasks::runtime::send_message_to_local_agent_task(
            &registry,
            task_id.as_str(),
            "continue the detached task".into(),
        )
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if registry.snapshot(&task_id).is_some_and(|snapshot| {
                    rebon_plugin_tasks::runtime::is_agent_snapshot_idle(&snapshot)
                        && snapshot
                            .result
                            .as_ref()
                            .and_then(|result| result.get("final_text"))
                            .and_then(Value::as_str)
                            == Some("detached follow-up")
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("detached follow-up should complete");
    }

    #[tokio::test]
    async fn spawn_background_immediately_registers_snapshot() {
        let _coord = CoordModeGuard::set(false);
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        let (blocking, _started_rx, release_tx) = BlockingModelClient::new();
        let client: Arc<dyn ModelClient> = Arc::new(blocking);
        let registry = TaskRegistry::new();
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone());
        let mut spec = SubAgentSpec::new("run in background and wait");
        spec.metadata = json!({
            "agent_id": "agent-bg-immediate-snapshot",
            "description": "Background immediate snapshot",
        });

        let id = spawner.spawn_background(spec).await.unwrap();
        let task_id = TaskId::new(id);
        let snap = registry
            .snapshot(&task_id)
            .expect("background task should be registered before spawn_background returns");
        assert_eq!(snap.status, TaskStatus::Running);
        assert_eq!(snap.title, "Background immediate snapshot");
        assert!(snap.is_backgrounded);
        assert!(matches!(snap.data, TaskData::LocalAgent(_)));
        let _ = release_tx.send(());
    }

    #[tokio::test]
    async fn spawn_background_immediate_stop_hits_runtime_task() {
        let _coord = CoordModeGuard::set(false);
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        let (blocking, started_rx, release_tx) = BlockingModelClient::new();
        let client: Arc<dyn ModelClient> = Arc::new(blocking);
        let registry = TaskRegistry::new();
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone());
        let mut spec = SubAgentSpec::new("run in background then stop");
        spec.metadata = json!({
            "agent_id": "agent-bg-immediate-stop",
            "description": "Background immediate stop",
        });
        spec.workflow_nesting_depth = 1;

        let id = spawner.spawn_background(spec).await.unwrap();
        started_rx.await.unwrap();
        let outcome = rebon_plugin_tasks::runtime::stop_task(&registry, &TaskId::new(id.clone()))
            .expect("stop_task should hit the pre-registered local agent");
        assert_eq!(outcome.task_id, id);
        assert_eq!(outcome.task_type, "local_agent");
        let snap = registry.snapshot(&TaskId::new(id.clone())).unwrap();
        assert_eq!(snap.status, TaskStatus::Killed);
        let _ = release_tx.send(());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            registry.snapshot(&TaskId::new(id.clone())).unwrap().status,
            TaskStatus::Killed
        );
        let finished_events = registry
            .task_live_events(&TaskId::new(id), None)
            .expect("task event journal")
            .events
            .into_iter()
            .filter_map(|event| match event.kind {
                TaskLiveEventKind::Finished { status, .. } => Some(status),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(finished_events, vec![TaskStatus::Killed]);
    }

    #[tokio::test]
    async fn spawn_background_immediate_send_message_queues_for_runtime_task() {
        let _coord = CoordModeGuard::set(false);
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        let (blocking, _started_rx, release_tx) = BlockingModelClient::new();
        let client: Arc<dyn ModelClient> = Arc::new(blocking);
        let registry = TaskRegistry::new();
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone());
        let mut spec = SubAgentSpec::new("run in background then receive message");
        spec.metadata = json!({
            "agent_id": "agent-bg-immediate-message",
            "description": "Background immediate message",
        });

        let id = spawner.spawn_background(spec).await.unwrap();
        rebon_plugin_tasks::runtime::send_message_to_local_agent_task(
            &registry,
            &id,
            "please include queued follow-up".to_string(),
        )
        .expect("send_message_to_task should queue on the pre-registered local agent");
        let snap = registry.snapshot(&TaskId::new(id.clone())).unwrap();
        match snap.data {
            TaskData::LocalAgent(data) => {
                assert_eq!(
                    data.pending_messages,
                    vec!["please include queued follow-up"]
                );
            }
            other => panic!("expected local agent snapshot, got {other:?}"),
        }
        let _ = release_tx.send(());
    }

    #[tokio::test]
    async fn completed_non_explore_local_agent_stays_idle_and_resumes_on_send_message() {
        let _coord = CoordModeGuard::set(false);
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn("first turn done"));
        mock.push_turn(text_turn("follow-up done"));
        let client: Arc<dyn ModelClient> = mock.clone();
        let registry = TaskRegistry::new();
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone());
        let mut spec = SubAgentSpec::new("inspect the old response fields");
        spec.metadata = json!({
            "agent_id": "resumable-worker",
            "agent_type": "general-purpose",
            "description": "Resumable worker",
        });

        let result = spawner.spawn(spec).await.unwrap();
        assert_eq!(result.status, "completed");
        let task_id = TaskId::new("resumable-worker");
        let first_snapshot = registry.snapshot(&task_id).unwrap();
        assert_eq!(first_snapshot.status, TaskStatus::Running);
        assert!(rebon_plugin_tasks::runtime::is_agent_snapshot_idle(
            &first_snapshot
        ));

        rebon_plugin_tasks::runtime::send_message_to_local_agent_task(
            &registry,
            task_id.as_str(),
            "verify the legacy Responses field names".into(),
        )
        .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if registry.snapshot(&task_id).is_some_and(|snapshot| {
                    rebon_plugin_tasks::runtime::is_agent_snapshot_idle(&snapshot)
                        && snapshot
                            .result
                            .as_ref()
                            .and_then(|result| result.get("final_text"))
                            .and_then(Value::as_str)
                            == Some("follow-up done")
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("follow-up turn should complete");

        let captured = mock.captured_requests();
        assert_eq!(captured.len(), 2);
        let followup_text = captured[1].messages[0].content[0].as_text().unwrap();
        assert!(followup_text.contains("first turn done"));
        assert!(followup_text.contains("verify the legacy Responses field names"));
        let finished_statuses = registry
            .task_live_events(&task_id, None)
            .expect("task events")
            .events
            .into_iter()
            .filter_map(|event| match event.kind {
                TaskLiveEventKind::Finished { status, .. } => Some(status),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            finished_statuses,
            vec![TaskStatus::Running, TaskStatus::Running]
        );
    }

    #[tokio::test]
    async fn completed_explore_agent_closes_instead_of_staying_idle() {
        let _coord = CoordModeGuard::set(false);
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        let mock = MockModelClient::new();
        mock.push_turn(text_turn("exploration done"));
        let client: Arc<dyn ModelClient> = Arc::new(mock);
        let registry = TaskRegistry::new();
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone());
        let mut spec = SubAgentSpec::new("inspect the query flow");
        spec.metadata = json!({
            "agent_id": "one-shot-explorer",
            "agent_type": "Explore",
            "description": "One-shot explorer",
        });

        let result = spawner.spawn(spec).await.unwrap();
        assert_eq!(result.status, "completed");

        let task_id = TaskId::new("one-shot-explorer");
        let snapshot = registry.snapshot(&task_id).unwrap();
        assert_eq!(snapshot.status, TaskStatus::Completed);
        assert!(!rebon_plugin_tasks::runtime::is_agent_snapshot_idle(
            &snapshot
        ));
        assert!(snapshot.end_time_ms.is_some());

        let error = rebon_plugin_tasks::runtime::send_message_to_local_agent_task(
            &registry,
            task_id.as_str(),
            "continue".into(),
        )
        .expect_err("completed Explore agent should be closed");
        assert_eq!(error.code, "agent_closed");

        let finished_statuses = registry
            .task_live_events(&task_id, None)
            .expect("task events")
            .events
            .into_iter()
            .filter_map(|event| match event.kind {
                TaskLiveEventKind::Finished { status, .. } => Some(status),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(finished_statuses, vec![TaskStatus::Completed]);
    }

    #[tokio::test]
    async fn resumed_foreground_agent_keeps_leader_permission_route() {
        let _coord = CoordModeGuard::set(false);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(SensitiveShellTool {
            calls: calls.clone(),
        }));
        let engine = Arc::new(engine);

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn("first turn done"));
        mock.push_turn(tool_use_turn(
            "m2",
            "Bash",
            "tid-sensitive",
            r#"{"command":"rm old.log"}"#,
        ));
        mock.push_turn(text_turn("follow-up done"));
        let client: Arc<dyn ModelClient> = mock;
        let registry = TaskRegistry::new();
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone());
        let (permission_broker, mut permission_rx) = ChannelPermissionBroker::new("sess-leader");
        let mut spec = SubAgentSpec::new("inspect safely");
        spec.metadata = json!({
            "agent_id": "permission-resumable-agent",
            "description": "Permission resumable agent",
            "parent_session_id": "sess-leader",
        });
        spec.permission_broker = Some(Arc::new(permission_broker));

        spawner.spawn(spec).await.unwrap();
        rebon_plugin_tasks::runtime::send_message_to_local_agent_task(
            &registry,
            "permission-resumable-agent",
            "remove the obsolete log".into(),
        )
        .unwrap();

        let query = tokio::time::timeout(std::time::Duration::from_secs(2), permission_rx.recv())
            .await
            .expect("resumed foreground turn should request leader permission")
            .expect("permission channel should remain open");
        assert_eq!(query.tool_name, "Bash");
        query
            .response_tx
            .send(PermissionAnswer::Selected {
                option_id: "allow_once".into(),
                updated_input: None,
                extra_text: None,
            })
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if registry
                    .snapshot(&TaskId::new("permission-resumable-agent"))
                    .is_some_and(|snapshot| {
                        rebon_plugin_tasks::runtime::is_agent_snapshot_idle(&snapshot)
                            && snapshot
                                .result
                                .as_ref()
                                .and_then(|result| result.get("final_text"))
                                .and_then(Value::as_str)
                                == Some("follow-up done")
                    })
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("resumed foreground turn should finish after approval");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn resumed_background_agent_denies_sensitive_tools_without_prompting() {
        let _coord = CoordModeGuard::set(false);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(SensitiveShellTool {
            calls: calls.clone(),
        }));
        let engine = Arc::new(engine);

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn("first turn done"));
        // A non-deletion sensitive command: deletions now float to the
        // frontend as a permission ask (covered by the companion test
        // below); everything else keeps the silent background deny.
        mock.push_turn(tool_use_turn(
            "m2",
            "Bash",
            "tid-sensitive-bg",
            r#"{"command":"git stash drop"}"#,
        ));
        mock.push_turn(text_turn("continued without command"));
        let client: Arc<dyn ModelClient> = mock;
        let registry = TaskRegistry::new();
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone());
        let (permission_broker, mut permission_rx) = ChannelPermissionBroker::new("sess-leader");
        let mut spec = SubAgentSpec::new("inspect in background");
        spec.metadata = json!({
            "agent_id": "permission-background-agent",
            "description": "Permission background agent",
            "parent_session_id": "sess-leader",
        });
        spec.permission_broker = Some(Arc::new(permission_broker));

        spawner.spawn_background(spec).await.unwrap();
        let task_id = TaskId::new("permission-background-agent");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if registry.snapshot(&task_id).is_some_and(|snapshot| {
                    rebon_plugin_tasks::runtime::is_agent_snapshot_idle(&snapshot)
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background first turn should become idle");

        rebon_plugin_tasks::runtime::send_message_to_local_agent_task(
            &registry,
            task_id.as_str(),
            "remove the obsolete log".into(),
        )
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if registry.snapshot(&task_id).is_some_and(|snapshot| {
                    rebon_plugin_tasks::runtime::is_agent_snapshot_idle(&snapshot)
                        && snapshot
                            .result
                            .as_ref()
                            .and_then(|result| result.get("final_text"))
                            .and_then(Value::as_str)
                            == Some("continued without command")
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background follow-up should finish without permission prompt");

        assert!(matches!(
            permission_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resumed_background_agent_deletion_floats_permission_ask_to_parent() {
        let _coord = CoordModeGuard::set(false);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(SensitiveShellTool {
            calls: calls.clone(),
        }));
        let engine = Arc::new(engine);

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn("first turn done"));
        mock.push_turn(tool_use_turn(
            "m2",
            "Bash",
            "tid-deletion-bg",
            r#"{"command":"rm old.log"}"#,
        ));
        mock.push_turn(text_turn("continued after ask"));
        let client: Arc<dyn ModelClient> = mock;
        let registry = TaskRegistry::new();
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone());
        let (permission_broker, mut permission_rx) = ChannelPermissionBroker::new("sess-leader");
        let mut spec = SubAgentSpec::new("clean up in background");
        spec.metadata = json!({
            "agent_id": "deletion-ask-background-agent",
            "description": "Deletion ask background agent",
            "parent_session_id": "sess-leader",
        });
        spec.permission_broker = Some(Arc::new(permission_broker));

        spawner.spawn_background(spec).await.unwrap();
        let task_id = TaskId::new("deletion-ask-background-agent");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if registry.snapshot(&task_id).is_some_and(|snapshot| {
                    rebon_plugin_tasks::runtime::is_agent_snapshot_idle(&snapshot)
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background first turn should become idle");

        rebon_plugin_tasks::runtime::send_message_to_local_agent_task(
            &registry,
            task_id.as_str(),
            "remove the obsolete log".into(),
        )
        .unwrap();

        // The deletion ask must FLOAT to the parent permission channel
        // instead of being silently denied.
        let query = tokio::time::timeout(std::time::Duration::from_secs(2), permission_rx.recv())
            .await
            .expect("deletion ask should reach the parent permission channel")
            .expect("permission channel should stay open");
        assert_eq!(query.tool_name, "Bash");

        // The user dismisses the ask; the worker resumes without
        // having run the deletion.
        let _ = query
            .response_tx
            .send(rebon_core::permission::PermissionAnswer::Cancelled);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if registry.snapshot(&task_id).is_some_and(|snapshot| {
                    rebon_plugin_tasks::runtime::is_agent_snapshot_idle(&snapshot)
                        && snapshot
                            .result
                            .as_ref()
                            .and_then(|result| result.get("final_text"))
                            .and_then(Value::as_str)
                            == Some("continued after ask")
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background worker should resume after the ask is answered");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn engine_sub_agent_spawner_background_returns_before_completion_notification() {
        let _coord = CoordModeGuard::set(false);
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        let mock = MockModelClient::new();
        mock.push_turn(text_turn("background done"));
        let client: Arc<dyn ModelClient> = Arc::new(mock);
        let registry = TaskRegistry::new();

        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone());
        let mut spec = SubAgentSpec::new("run in background");
        spec.run_in_background = true;
        spec.metadata = json!({
            "agent_id": "agent-bg",
            "description": "Background task",
        });

        let id = spawner.spawn_background(spec).await.unwrap();
        assert_eq!(id, "agent-bg");

        for _ in 0..50 {
            if registry
                .snapshot(&TaskId::new("agent-bg"))
                .is_some_and(|snap| rebon_plugin_tasks::runtime::is_agent_snapshot_idle(&snap))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let snap = registry.snapshot(&TaskId::new("agent-bg")).unwrap();
        assert_eq!(snap.status, TaskStatus::Running);
        assert!(rebon_plugin_tasks::runtime::is_agent_snapshot_idle(&snap));
        assert!(snap.is_backgrounded);
        let notifications = registry.take_unnotified_terminal_agent_notifications();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].contains("<task-id>agent-bg</task-id>"));
        assert!(notifications[0].contains("<status>completed</status>"));
        assert!(notifications[0].contains("background done"));
    }

    #[tokio::test]
    async fn coordinator_background_agent_notification_includes_registered_output_file() {
        // Isolates the worker report directory: without this the test writes
        // into the user's real `<config home>/tasks`, where a parallel test
        // sharing that directory can delete the file mid-run.
        let _coord = CoordModeGuard::set(true);
        let _task_home = TestTaskHome::new("report-output");
        let agent_id = "agent-bg-report-output";
        let report_path = report_file_path_for_agent_id(agent_id);
        let _ = std::fs::remove_file(&report_path);

        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        let mock = MockModelClient::new();
        mock.push_turn(text_turn("background report done"));
        let client: Arc<dyn ModelClient> = Arc::new(mock);
        let registry = TaskRegistry::new();

        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone())
            .with_coordinator_mode(true);
        let mut spec = SubAgentSpec::new("write report in background");
        spec.metadata = json!({
            "agent_id": agent_id,
            "agent_type": "Explore",
            "coordinator_task_kind": "research",
            "description": "Background report worker",
        });

        let id = spawner.spawn_background(spec).await.unwrap();
        assert_eq!(id, agent_id);

        for _ in 0..50 {
            if registry
                .snapshot(&TaskId::new(agent_id))
                .is_some_and(|snap| {
                    snap.status == TaskStatus::Completed
                        && snap
                            .result
                            .as_ref()
                            .and_then(|value| value.get("output_file"))
                            .and_then(Value::as_str)
                            == Some(report_path.as_str())
                })
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let snap = registry.snapshot(&TaskId::new(agent_id)).unwrap();
        assert_eq!(snap.error.as_deref(), None);
        assert_eq!(snap.status, TaskStatus::Completed);
        assert!(!rebon_plugin_tasks::runtime::is_agent_snapshot_idle(&snap));
        assert_eq!(
            snap.result
                .as_ref()
                .and_then(|value| value.get("output_file"))
                .and_then(Value::as_str),
            Some(report_path.as_str())
        );

        let notifications = registry.unnotified_terminal_agent_notifications();
        assert_eq!(notifications.len(), 1);
        assert_eq!(
            notifications[0].output_file.as_deref(),
            Some(report_path.as_str())
        );
        assert!(notifications[0]
            .message
            .contains("<task-id>agent-bg-report-output</task-id>"));
        assert!(notifications[0].message.contains("<output-file>"));
        assert!(notifications[0].message.contains(&report_path));

        let report = std::fs::read_to_string(&report_path).unwrap();
        assert!(report.contains("background report done"));
        let _ = std::fs::remove_file(&report_path);
    }

    #[tokio::test]
    async fn engine_sub_agent_spawner_background_records_failed_snapshot_before_worker_start() {
        use rebon_tool::ToolFilter;

        let _coord = CoordModeGuard::set(false);
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let registry = TaskRegistry::new();

        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_base_filter(ToolFilter::deny_all())
            .with_test_task_registry(registry.clone());
        let mut spec = SubAgentSpec::new("run in background with no tools");
        spec.metadata = json!({
            "agent_id": "agent-bg-fail",
            "description": "Background failing task",
        });

        let id = spawner.spawn_background(spec).await.unwrap();
        assert_eq!(id, "agent-bg-fail");

        for _ in 0..50 {
            if registry
                .snapshot(&TaskId::new("agent-bg-fail"))
                .is_some_and(|snap| snap.status == TaskStatus::Failed)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let snap = registry.snapshot(&TaskId::new("agent-bg-fail")).unwrap();
        assert_eq!(snap.status, TaskStatus::Failed);
        assert!(snap.is_backgrounded);
        assert!(snap.error.as_deref().is_some_and(|err| {
            err.contains("NoMatchingTools") || err.to_ascii_lowercase().contains("no")
        }));

        let notifications = registry.take_unnotified_terminal_agent_notifications();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].contains("<task-id>agent-bg-fail</task-id>"));
        assert!(notifications[0].contains("<status>failed</status>"));
        assert!(notifications[0].contains("Background failing task"));
    }

    #[tokio::test]
    async fn engine_sub_agent_spawner_errors_when_engine_was_dropped() {
        let engine = Arc::new(Engine::new());
        let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let spawner =
            EngineSubAgentSpawner::new(Arc::downgrade(&engine), client).with_default_model("mock");
        drop(engine); // weak reference should now fail to upgrade
        let err = spawner
            .spawn(SubAgentSpec::new("late call"))
            .await
            .unwrap_err();
        assert!(err.contains("engine"));
    }

    #[tokio::test]
    async fn persistent_actor_restores_messages_when_followup_fails_before_turn_start() {
        let engine = Arc::new(Engine::new());
        let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let registry = TaskRegistry::new();
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone());
        let task_id = TaskId::new("agent-early-followup-failure");
        let metadata = json!({
            "agent_id": task_id.as_str(),
            "description": "Early follow-up failure",
            "agent_type": "Explore",
        });
        let snapshot = local_agent_task_snapshot(
            task_id.clone(),
            TaskStatus::Running,
            "Early follow-up failure".into(),
            "initial task".into(),
            "Explore".into(),
            "mock".into(),
            None,
            None,
            true,
            now_wall_ms_for_spawner(),
            None,
            None,
            None,
            None,
            metadata.clone(),
        );
        registry.insert(task_id.clone(), snapshot, PromptCancel::new());
        let mut base_spec = SubAgentSpec::new("initial task");
        base_spec.metadata = metadata;
        if let Some(metadata) = base_spec.metadata.as_object_mut() {
            metadata.insert(PERSISTENT_AGENT_MANAGED_KEY.into(), Value::Bool(true));
        }
        drop(engine);

        let actor_registry = registry.clone();
        let actor_task_id = task_id.clone();
        let actor = tokio::spawn(async move {
            run_persistent_agent_actor(spawner, actor_registry, actor_task_id, base_spec).await;
        });
        rebon_plugin_tasks::runtime::send_message_to_local_agent_task(
            &registry,
            task_id.as_str(),
            "first follow-up".into(),
        )
        .unwrap();

        let first_generation = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(notification) = registry
                    .unnotified_terminal_notifications()
                    .into_iter()
                    .find(|notification| notification.task_id == task_id)
                {
                    break notification.generation;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("early failure should leave an idle notification");
        let first_snapshot = registry.snapshot(&task_id).expect("task");
        let TaskData::LocalAgent(first_data) = first_snapshot.data else {
            panic!("expected local agent");
        };
        assert_eq!(first_data.pending_messages, vec!["first follow-up"]);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            registry
                .unnotified_terminal_notifications()
                .into_iter()
                .find(|notification| notification.task_id == task_id)
                .expect("notification should remain pending")
                .generation,
            first_generation
        );

        rebon_plugin_tasks::runtime::send_message_to_local_agent_task(
            &registry,
            task_id.as_str(),
            "second follow-up".into(),
        )
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if registry
                    .unnotified_terminal_notifications()
                    .into_iter()
                    .find(|notification| notification.task_id == task_id)
                    .is_some_and(|notification| notification.generation > first_generation)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("new message should trigger exactly one retry attempt");
        let second_snapshot = registry.snapshot(&task_id).expect("task");
        let TaskData::LocalAgent(second_data) = second_snapshot.data else {
            panic!("expected local agent");
        };
        assert_eq!(
            second_data.pending_messages,
            vec!["first follow-up", "second follow-up"]
        );

        assert!(registry.cancel(&task_id));
        tokio::time::timeout(std::time::Duration::from_secs(2), actor)
            .await
            .expect("actor should stop after cancellation")
            .expect("actor task should not panic");
    }

    // ── End-to-end: AgentTool → EngineSubAgentSpawner → sub-query ──
    //
    use crate::AgentTool;

    fn tool_use_turn(
        id: &str,
        tool_name: &str,
        tool_use_id: &str,
        input_json: &str,
    ) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message_id: id.into(),
                model: "mock".into(),
                usage: Usage::default(),
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::ToolUse {
                    id: tool_use_id.into(),
                    name: tool_name.into(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::InputJsonDelta {
                    partial_json: input_json.into(),
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

    #[tokio::test]
    async fn engine_sub_agent_spawner_records_successful_read_tool_history() {
        let _coord = CoordModeGuard::set(false);
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(ReadLikeTool));
        let engine = Arc::new(engine);

        let mock = MockModelClient::new();
        mock.push_turn(tool_use_turn(
            "read_1",
            "Read",
            "toolu_read_1",
            "{\"file_path\":\"Cargo.toml\"}",
        ));
        mock.push_turn(text_turn("done"));
        let client: Arc<dyn ModelClient> = Arc::new(mock);

        let spawner =
            EngineSubAgentSpawner::new(Arc::downgrade(&engine), client).with_default_model("mock");
        let result = spawner
            .spawn(SubAgentSpec::new("read Cargo.toml"))
            .await
            .unwrap();

        assert_eq!(result.tool_call_count, 1);
        assert_eq!(result.read_file_count, None);
        let result_calls = result.sub_agent_tool_calls.as_ref().unwrap();
        assert_eq!(result_calls.len(), 1);
        assert_eq!(result_calls[0]["name"], "Read");
        assert_eq!(result_calls[0]["ok"], true);

        let registry = TaskRegistry::new();
        let mut spec = SubAgentSpec::new("read Cargo.toml");
        spec.metadata = json!({ "agent_id": "agent-read-history" });
        let spawner = EngineSubAgentSpawner::new(
            Arc::downgrade(&engine),
            Arc::new({
                let mock = MockModelClient::new();
                mock.push_turn(tool_use_turn(
                    "read_2",
                    "Read",
                    "toolu_read_2",
                    "{\"file_path\":\"Cargo.toml\"}",
                ));
                mock.push_turn(text_turn("done again"));
                mock
            }),
        )
        .with_default_model("mock")
        .with_test_task_registry(registry.clone());
        let _ = spawner.spawn(spec).await.unwrap();
        let snap = registry
            .snapshot(&TaskId::new("agent-read-history"))
            .unwrap();
        let calls = snap
            .result
            .as_ref()
            .and_then(|result| result.get("sub_agent_tool_calls"))
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "Read");
        assert_eq!(calls[0]["ok"], true);
    }

    #[tokio::test]
    async fn agent_tool_spawns_sub_agent_and_returns_structured_result() {
        let _coord = CoordModeGuard::set(false);
        // Build an engine with AgentTool registered.
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(AgentTool::new()));
        let engine = Arc::new(engine);

        // Mock client that plays the sub-agent's single turn.
        let sub_client = MockModelClient::new();
        sub_client.push_turn(text_turn("sub-agent reports success"));
        let sub_client: Arc<dyn ModelClient> = Arc::new(sub_client);

        // Spawner with a weak engine reference — this is what gets
        // injected into every AgentTool invocation.
        let spawner = Arc::new(
            EngineSubAgentSpawner::new(Arc::downgrade(&engine), sub_client)
                .with_default_model("mock"),
        );

        // Invoke AgentTool directly via the engine's dispatch path,
        // seeding the ToolContext with the spawner.
        let context = rebon_tool::ToolContext::new()
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);
        let out = engine
            .invoke_tool(
                "Agent",
                serde_json::json!({ "prompt": "do the sub task" }),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(out["status"], "completed");
        assert_eq!(out["final_text"], "sub-agent reports success");
        assert_eq!(out["tool_call_count"], 0);
    }

    #[tokio::test]
    async fn agent_tool_in_full_query_loop_spawns_sub_agent() {
        // Build parent engine with AgentTool + one dummy tool so the
        // sub-agent has at least one allowed tool when requested.
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(AgentTool::new()));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        // Parent client: one turn asking for Agent{prompt:"summarize X"}, one final turn.
        let parent_client = MockModelClient::new();
        parent_client.push_turn(tool_use_turn(
            "parent_1",
            "Agent",
            "toolu_agent",
            "{\"prompt\":\"summarize X\"}",
        ));
        parent_client.push_turn(text_turn("delegated successfully"));

        // Sub-agent client: one text turn.
        let sub_client = MockModelClient::new();
        sub_client.push_turn(text_turn("X summary from worker"));

        let parent_client: Arc<dyn ModelClient> = Arc::new(parent_client);
        let sub_client: Arc<dyn ModelClient> = Arc::new(sub_client);

        let spawner = Arc::new(
            EngineSubAgentSpawner::new(Arc::downgrade(&engine), sub_client)
                .with_default_model("mock"),
        );

        let params = rebon_core::query::QueryParams::new(
            "mock",
            vec![rebon_api::Message::user_text("please delegate")],
        )
        .with_tools(rebon_core::query::tools_from_engine(&engine))
        .with_max_iterations(5);

        let context = rebon_tool::ToolContext::new()
            .with_sub_agent_spawner(spawner as Arc<dyn SubAgentSpawner>);

        let mut rx = rebon_core::query::run_query(
            engine,
            SessionHandle::new(parent_client),
            params,
            context,
            PromptCancel::new(),
        );

        // Drain events until Done.
        let mut final_text = String::new();
        let mut saw_tool_dispatch = false;
        while let Some(event) = rx.recv().await {
            match event {
                rebon_core::query::QueryEvent::ToolDispatchStart { name, .. } => {
                    if name == "Agent" {
                        saw_tool_dispatch = true;
                    }
                }
                rebon_core::query::QueryEvent::Done { final_message, .. } => {
                    final_text = final_message.text();
                    break;
                }
                rebon_core::query::QueryEvent::Error(msg) => panic!("unexpected error: {msg}"),
                _ => {}
            }
        }

        assert!(saw_tool_dispatch, "parent should have dispatched AgentTool");
        assert_eq!(final_text, "delegated successfully");
    }

    // ── base_filter + per-spec filter (non-coordinator mode) ─────────

    /// Outside coordinator mode the spawner intersects the base filter
    /// with the per-spec filter — the most restrictive combination
    /// wins.
    #[tokio::test]
    async fn spawner_base_filter_intersects_with_spec_allowed_tools() {
        use rebon_tool::ToolFilter;

        // Ensure coordinator mode is off so intersection semantics apply.
        let _coord = CoordModeGuard::set(false);

        // Engine with three tools so we can verify the intersection.
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        // Add two additional recording tools. We reuse NullTool
        // as a stand-in since we only care about which tools the
        // filter exposes, not their behaviour.
        struct StubTool(&'static str);
        #[async_trait]
        impl Tool for StubTool {
            fn id(&self) -> ToolId {
                ToolId::new(self.0)
            }
            fn description(&self) -> &str {
                "stub"
            }
            fn input_schema(&self) -> ToolInputSchema {
                json!({ "type": "object" })
            }
            async fn validate_input(
                &self,
                _input: &Value,
                _context: &ToolContext,
            ) -> ToolResult<ValidationOutcome> {
                Ok(ValidationOutcome::valid())
            }
            async fn check_permissions(
                &self,
                _input: &Value,
                _context: &ToolContext,
            ) -> ToolResult<PermissionDecision> {
                Ok(PermissionDecision::allow(Value::Null))
            }
            async fn call(&self, _input: Value, _context: &ToolContext) -> ToolResult<Value> {
                Ok(Value::Null)
            }
        }
        engine.register_tool(Arc::new(StubTool("Read")));
        engine.register_tool(Arc::new(StubTool("Bash")));
        engine.register_tool(Arc::new(StubTool("Write")));
        let engine = Arc::new(engine);

        // Base filter: allow only [Read, Bash]. Without coordinator
        // mode, the spawner intersects this with the per-spec filter.
        let base = ToolFilter::allow_only(["Read", "Bash"]);
        // Spec filter: allow only [Read, Write].
        let spec_allowed = vec!["Read".to_string(), "Write".to_string()];

        // Keep a typed handle to the mock so we can assert the
        // tool list reached the sub-agent.
        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn("ok"));
        let client: Arc<dyn ModelClient> = mock.clone();

        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_base_filter(base);

        let result = spawner
            .spawn(SubAgentSpec {
                prompt: "go".into(),
                model: None,
                model_profile: None,
                provider: None,
                context: None,
                cache_strategy: None,
                frozen_parent_context: None,
                system: None,
                tool_filter: Some(ToolFilter::allow_only(spec_allowed)),
                max_iterations: 3,
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
            })
            .await
            .unwrap();
        assert_eq!(result.status, "completed");

        // Intersection of [Read, Bash] and [Read, Write] is [Read].
        // Only one request was captured (single-turn mock).
        let captured = mock.captured_requests();
        assert_eq!(captured.len(), 1);
        let tool_names: Vec<_> = captured[0].tools.iter().map(|t| t.name.clone()).collect();
        assert_eq!(tool_names, vec!["Read".to_string()]);
    }

    // ── coordinator-mode: spec filter takes precedence ─────────────

    /// Stub tool with a configurable name, for filter tests.
    struct NamedTool(&'static str);

    #[async_trait]
    impl Tool for NamedTool {
        fn id(&self) -> ToolId {
            ToolId::new(self.0)
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn input_schema(&self) -> ToolInputSchema {
            json!({ "type": "object" })
        }
        async fn validate_input(
            &self,
            _input: &Value,
            _context: &ToolContext,
        ) -> ToolResult<ValidationOutcome> {
            Ok(ValidationOutcome::valid())
        }
        async fn check_permissions(
            &self,
            _input: &Value,
            _context: &ToolContext,
        ) -> ToolResult<PermissionDecision> {
            Ok(PermissionDecision::allow(Value::Null))
        }
        async fn call(&self, _input: Value, _context: &ToolContext) -> ToolResult<Value> {
            Ok(Value::Null)
        }
    }

    /// RAII guard: sets `REBON_COORDINATOR_MODE` to a chosen state
    /// for the lifetime of the guard, restores the previous value on
    /// drop.
    ///
    /// Takes the crate-wide [`test_config_env_lock`](crate::runtime::test_config_env_lock)
    /// — the same one `TestTaskHome` takes — rather than a second mutex of
    /// its own. Two locks meant the order they were acquired in mattered,
    /// and a test that took them the other way round deadlocked the suite.
    /// One re-entrant lock makes that impossible to express.
    struct CoordModeGuard {
        previous: Option<String>,
        _lock: crate::runtime::TestEnvLock,
    }

    impl CoordModeGuard {
        fn set(enabled: bool) -> Self {
            let lock = crate::runtime::test_config_env_lock();
            let previous = std::env::var("REBON_COORDINATOR_MODE").ok();
            if enabled {
                std::env::set_var("REBON_COORDINATOR_MODE", "1");
            } else {
                std::env::remove_var("REBON_COORDINATOR_MODE");
            }
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for CoordModeGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => std::env::set_var("REBON_COORDINATOR_MODE", v),
                None => std::env::remove_var("REBON_COORDINATOR_MODE"),
            }
        }
    }

    #[test]
    fn report_file_contract_coerces_when_implementation_commit_section_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("rebon-spawner-implementation-coercion.md");
        std::fs::write(
            &report_path,
            "# Worker Report\n\n\
## Summary\n\nDone.\n\n\
## Files Changed / Inspected\n\n- src/lib.rs:1\n\n\
## Evidence\n\n- evidence\n\n\
## Verification / Tests\n\n- not run\n\n\
## Blockers / Assumptions\n\nNone.\n\n\
## Final Status\n\nComplete.\n",
        )
        .unwrap();
        let report_path = report_path.to_string_lossy().to_string();

        let validator = ReportFileContract::new(
            report_path.clone(),
            SubAgentTaskKind::Implementation,
            true,
            None,
            Arc::new(ValidatorLifecycleDiagnostics::default()),
        );
        let msg = validator
            .evaluate()
            .expect("missing Implementation Commit should coerce for implementation tasks")
            .message;
        let text = msg.content[0].as_text().unwrap();
        assert!(text.contains("Validation failure detected"));
        assert!(text.contains("Implementation Commit"));
        assert!(text.contains("missing/invalid sections"));
        assert!(validator.evaluate().is_none());
    }

    #[test]
    fn report_file_contract_allows_implementation_without_commit_when_worktree_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir
            .path()
            .join("rebon-spawner-implementation-no-worktree.md");
        std::fs::write(
            &report_path,
            "# Worker Report\n\n\
## Summary\n\nDone.\n\n\
## Files Changed / Inspected\n\n- src/lib.rs:1\n\n\
## Evidence\n\n- evidence\n\n\
## Verification / Tests\n\n- not run\n\n\
## Blockers / Assumptions\n\nNone.\n\n\
## Final Status\n\nComplete.\n",
        )
        .unwrap();
        let report_path = report_path.to_string_lossy().to_string();

        let validator = ReportFileContract::new(
            report_path.clone(),
            SubAgentTaskKind::Implementation,
            false,
            None,
            Arc::new(ValidatorLifecycleDiagnostics::default()),
        );
        assert!(validator.evaluate().is_none());
    }

    #[test]
    fn report_file_contract_rejects_a_report_left_over_from_the_previous_turn() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("rebon-spawner-stale-report.md");
        let report = "# Worker Report\n\n\
## Summary\n\nDone.\n\n\
## Files Changed / Inspected\n\n- src/lib.rs:1\n\n\
## Evidence\n\n- evidence\n\n\
## Verification / Tests\n\n- not run\n\n\
## Blockers / Assumptions\n\nNone.\n\n\
## Final Status\n\nComplete.\n";
        std::fs::write(&report_path, report).unwrap();
        let report_path = report_path.to_string_lossy().to_string();
        // What a follow-up turn on an idle worker starts with: a valid
        // report the previous turn wrote.
        let baseline = report_file_stamp(&report_path);
        assert!(baseline.is_some());

        let validator = ReportFileContract::new(
            report_path.clone(),
            SubAgentTaskKind::Implementation,
            false,
            baseline,
            Arc::new(ValidatorLifecycleDiagnostics::default()),
        );
        let msg = validator
            .evaluate()
            .expect("an untouched report is not this turn's deliverable")
            .message;
        let text = msg.content[0].as_text().unwrap();
        assert!(
            text.contains("was not rewritten during this turn"),
            "{text}"
        );

        // Rewriting it clears the objection.
        std::fs::write(&report_path, format!("{report}\nFollow-up turn.\n")).unwrap();
        let validator = ReportFileContract::new(
            report_path,
            SubAgentTaskKind::Implementation,
            false,
            baseline,
            Arc::new(ValidatorLifecycleDiagnostics::default()),
        );
        assert!(validator.evaluate().is_none());
    }

    /// In coordinator mode, Explore is read-only and cannot write the
    /// report file itself. The spawner should not inject the Write-based
    /// directive or coercion retry; it should persist the final text for
    /// the coordinator to Read from output_file.
    #[tokio::test]
    async fn spawner_coordinator_mode_explore_auto_writes_report_from_final_text() {
        // Isolates the worker report directory: without this the test writes
        // into the user's real `<config home>/tasks`, where a parallel test
        // sharing that directory can delete the file mid-run.
        let _coord = CoordModeGuard::set(true);
        let _task_home = TestTaskHome::new("report-output");
        let agent_id = "agent-explore-auto-report";
        let report_path = report_file_path_for_agent_id(agent_id);
        let _ = std::fs::remove_file(&report_path);

        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NamedTool("Read")));
        let engine = Arc::new(engine);

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn(
            "Found src/lib.rs:42 as the relevant entry point.",
        ));
        let client: Arc<dyn ModelClient> = mock.clone();

        let spawner =
            EngineSubAgentSpawner::new(Arc::downgrade(&engine), client).with_default_model("mock");
        let mut spec = SubAgentSpec::new("search the codebase");
        spec.metadata = json!({
            "agent_id": agent_id,
            "agent_type": "Explore",
            "description": "Explore auto report",
        });

        let result = spawner.spawn(spec).await.unwrap();
        assert_eq!(result.status, "completed");
        assert_eq!(result.output_file.as_deref(), Some(report_path.as_str()));

        let report = std::fs::read_to_string(&report_path).unwrap();
        assert!(report.contains("# Explore Agent Report"));
        assert!(report.contains("## Summary"));
        assert!(report.contains("## Files Changed / Inspected"));
        assert!(report.contains("## Evidence"));
        assert!(report.contains("## Verification / Tests"));
        assert!(report.contains("## Blockers / Assumptions"));
        assert!(report.contains("## Final Status"));
        assert!(report.contains("Found src/lib.rs:42 as the relevant entry point."));
        let validation = rebon_core::coordinator_mode::validate_worker_report_file(&report_path);
        assert!(validation.ok, "{validation:?}");

        let captured = mock.captured_requests();
        assert_eq!(captured.len(), 1);
        let first_user_text = captured[0].messages[0].content[0].as_text().unwrap();
        assert_eq!(first_user_text, "search the codebase");
        assert!(!first_user_text.contains("Required Deliverable"));

        let _ = std::fs::remove_file(&report_path);
    }

    #[test]
    fn auto_write_report_file_whitespace_final_text_still_validates() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("rebon-spawner-empty-final-auto-report.md");
        let report_path = report_path.to_string_lossy().to_string();

        auto_write_report_file(&report_path, " \t\n\r\n ").expect("auto report should be written");

        let report = std::fs::read_to_string(&report_path).unwrap();
        assert!(report.contains(
            "Auto-report was generated from empty final text; no worker summary was provided."
        ));
        assert!(report.contains(
            "Auto-report was generated from empty final text; no worker evidence was provided."
        ));
        let validation = rebon_core::coordinator_mode::validate_worker_report_file(&report_path);
        assert!(validation.ok, "{validation:?}\n{report}");
    }

    /// In coordinator mode, an explicit `spec.tool_filter` takes full
    /// precedence over the spawner's `base_filter` — the worker sees
    /// only the tools the caller explicitly requested, even when
    /// those tools are not in the default allow-list.
    ///
    /// Without this fix, the intersection of [Read, Bash] and
    /// [CustomMcp] would be empty → `NoMatchingTools`.
    #[tokio::test]
    async fn spawner_coordinator_mode_spec_filter_overrides_base_filter() {
        use rebon_tool::ToolFilter;

        let _coord = CoordModeGuard::set(true);

        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NamedTool("Read")));
        engine.register_tool(Arc::new(NamedTool("Bash")));
        engine.register_tool(Arc::new(NamedTool("CustomMcp")));
        let engine = Arc::new(engine);

        // Base filter: [Read, Bash] (simulating async_agent_filter).
        let base = ToolFilter::allow_only(["Read", "Bash"]);
        // Spec filter: [CustomMcp] — NOT in the base filter.
        let spec_filter = ToolFilter::allow_only(["CustomMcp"]);

        let mock = Arc::new(MockModelClient::new());
        // Two turns: one for the main prompt, one for the report-file
        // coercion retry that fires because mock workers can't write files.
        mock.push_turn(text_turn("ok"));
        mock.push_turn(text_turn("coercion ack"));
        let client: Arc<dyn ModelClient> = mock.clone();

        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_base_filter(base);

        let result = spawner
            .spawn(SubAgentSpec {
                prompt: "go".into(),
                model: None,
                model_profile: None,
                provider: None,
                context: None,
                cache_strategy: None,
                frozen_parent_context: None,
                system: None,
                tool_filter: Some(spec_filter),
                max_iterations: 3,
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
            })
            .await
            .unwrap();
        // Mock workers can't write report files, so the post-wait
        // fallback downgrades the status. This test only validates
        // filter behaviour — the status is expected to be "failed".
        assert_eq!(result.status, "failed");

        // In coordinator mode the spec filter wins: worker sees
        // only [CustomMcp], not the empty intersection.
        let captured = mock.captured_requests();
        assert!(!captured.is_empty());
        let tool_names: Vec<_> = captured[0].tools.iter().map(|t| t.name.clone()).collect();
        assert_eq!(tool_names, vec!["CustomMcp".to_string()]);
    }

    /// When no explicit `spec.tool_filter` is provided in coordinator
    /// mode, the spawner falls back to its `base_filter` so that
    /// agents cannot escape the default allow-list.
    #[tokio::test]
    async fn spawner_coordinator_mode_falls_back_to_base_filter_when_no_spec_filter() {
        use rebon_tool::ToolFilter;

        let _coord = CoordModeGuard::set(true);

        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NamedTool("Read")));
        engine.register_tool(Arc::new(NamedTool("Bash")));
        engine.register_tool(Arc::new(NamedTool("Write")));
        let engine = Arc::new(engine);

        // Base filter: [Read, Bash] only.
        let base = ToolFilter::allow_only(["Read", "Bash"]);

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn("ok"));
        mock.push_turn(text_turn("coercion ack"));
        let client: Arc<dyn ModelClient> = mock.clone();

        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_base_filter(base);

        let result = spawner
            .spawn(SubAgentSpec {
                prompt: "go".into(),
                model: None,
                model_profile: None,
                provider: None,
                context: None,
                cache_strategy: None,
                frozen_parent_context: None,
                system: None,
                tool_filter: None, // ← no explicit filter
                max_iterations: 3,
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
            })
            .await
            .unwrap();
        assert_eq!(result.status, "failed");

        // No spec filter → base filter used as fallback.
        // Worker sees [Bash, Read] (sorted), not all three tools.
        let captured = mock.captured_requests();
        assert!(!captured.is_empty());
        let mut tool_names: Vec<_> = captured[0].tools.iter().map(|t| t.name.clone()).collect();
        tool_names.sort();
        assert_eq!(tool_names, vec!["Bash".to_string(), "Read".to_string()]);
    }

    /// In coordinator mode with partial overlap between base and spec
    /// filters, the spec filter still takes full precedence — no
    /// intersection. Worker sees the complete set the caller asked for.
    #[tokio::test]
    async fn spawner_coordinator_mode_partial_overlap_uses_full_spec_filter() {
        use rebon_tool::ToolFilter;

        let _coord = CoordModeGuard::set(true);

        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NamedTool("Read")));
        engine.register_tool(Arc::new(NamedTool("Bash")));
        engine.register_tool(Arc::new(NamedTool("Write")));
        engine.register_tool(Arc::new(NamedTool("CustomMcp")));
        let engine = Arc::new(engine);

        // Base filter: [Read, Bash, Write].
        let base = ToolFilter::allow_only(["Read", "Bash", "Write"]);
        // Spec filter: [Read, CustomMcp] — only Read overlaps.
        let spec_filter = ToolFilter::allow_only(["Read", "CustomMcp"]);

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn("ok"));
        mock.push_turn(text_turn("coercion ack"));
        let client: Arc<dyn ModelClient> = mock.clone();

        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_base_filter(base);

        let result = spawner
            .spawn(SubAgentSpec {
                prompt: "go".into(),
                model: None,
                model_profile: None,
                provider: None,
                context: None,
                cache_strategy: None,
                frozen_parent_context: None,
                system: None,
                tool_filter: Some(spec_filter),
                max_iterations: 3,
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
            })
            .await
            .unwrap();
        assert_eq!(result.status, "failed");

        // Spec filter wins in full: [CustomMcp, Read], not just [Read].
        let captured = mock.captured_requests();
        assert!(!captured.is_empty());
        let mut tool_names: Vec<_> = captured[0].tools.iter().map(|t| t.name.clone()).collect();
        tool_names.sort();
        assert_eq!(
            tool_names,
            vec!["CustomMcp".to_string(), "Read".to_string()]
        );
    }

    // ── fork_for_sub_agent ────────────────────────────────────────

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// ModelClient wrapper that implements `fork_for_sub_agent` and
    /// tracks how many times it was called. The forked child is a
    /// distinct MockModelClient so we can verify the sub-agent used
    /// the forked client, not the parent.
    struct ForkTrackingClient {
        parent_mock: Arc<MockModelClient>,
        child_mock: Arc<MockModelClient>,
        fork_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModelClient for ForkTrackingClient {
        fn provider_name(&self) -> &'static str {
            "fork-tracking"
        }

        async fn create_message_stream(
            &self,
            request: rebon_api::CreateMessageRequest,
        ) -> rebon_api::ModelResult<rebon_api::StreamEventStream> {
            self.parent_mock.create_message_stream(request).await
        }

        fn fork_for_sub_agent(&self) -> Option<Arc<dyn ModelClient>> {
            self.fork_count.fetch_add(1, Ordering::Relaxed);
            Some(self.child_mock.clone() as Arc<dyn ModelClient>)
        }
    }

    #[tokio::test]
    async fn spawner_calls_fork_for_sub_agent_when_client_supports_it() {
        let _coord = CoordModeGuard::set(false);
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        // Parent mock: not used by the sub-agent (should remain at 0 calls).
        let parent_mock = Arc::new(MockModelClient::new());

        // Child mock: the forked client the sub-agent will use.
        let child_mock = Arc::new(MockModelClient::new());
        child_mock.push_turn(text_turn("forked reply"));

        let fork_count = Arc::new(AtomicUsize::new(0));
        let client: Arc<dyn ModelClient> = Arc::new(ForkTrackingClient {
            parent_mock: parent_mock.clone(),
            child_mock: child_mock.clone(),
            fork_count: fork_count.clone(),
        });

        let spawner =
            EngineSubAgentSpawner::new(Arc::downgrade(&engine), client).with_default_model("mock");

        let result = spawner
            .spawn(SubAgentSpec::new("do the task"))
            .await
            .unwrap();

        // fork_for_sub_agent was called exactly once.
        assert_eq!(fork_count.load(Ordering::Relaxed), 1);
        // The sub-agent used the child mock (text came from there).
        assert_eq!(result.final_text, "forked reply");
        assert_eq!(result.status, "completed");
        // Parent mock was never called.
        assert_eq!(parent_mock.call_count(), 0);
        // Child mock was called once.
        assert_eq!(child_mock.call_count(), 1);
        // A genuinely isolated child owns its own session lifecycle,
        // so its turn end lands on the child and nowhere else.
        assert_eq!(child_mock.end_turn_count(), 1);
        assert_eq!(parent_mock.end_turn_count(), 0);
    }

    #[tokio::test]
    async fn spawner_falls_back_to_clone_when_fork_returns_none() {
        let _coord = CoordModeGuard::set(false);
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        // MockModelClient.fork_for_sub_agent() returns None →
        // spawner should fall back to Arc::clone and use this mock.
        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn("fallback reply"));
        let client: Arc<dyn ModelClient> = mock.clone();

        let spawner =
            EngineSubAgentSpawner::new(Arc::downgrade(&engine), client).with_default_model("mock");

        let result = spawner.spawn(SubAgentSpec::new("task")).await.unwrap();

        assert_eq!(result.final_text, "fallback reply");
        assert_eq!(mock.call_count(), 1);
        // Sharing the transport must not share the session lifecycle:
        // the sub-agent's turn ending is not the parent's turn ending,
        // and on a provider whose `endTurn` is connection-scoped
        // (external plugins) forwarding it would reset a session the
        // sub-agent does not own.
        assert_eq!(
            mock.end_turn_count(),
            0,
            "a sub-agent sharing its parent's client must not end the parent's turn"
        );
        assert_eq!(mock.reset_count(), 0);
    }

    #[tokio::test]
    async fn spawner_base_filter_used_alone_when_spec_has_no_allowed_tools() {
        use rebon_tool::ToolFilter;

        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(NullTool));
        let engine = Arc::new(engine);

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn("ok"));
        let client: Arc<dyn ModelClient> = mock.clone();
        let registry = TaskRegistry::new();

        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_base_filter(ToolFilter::deny_all())
            .with_test_task_registry(registry.clone());

        let err = spawner
            .spawn(SubAgentSpec {
                prompt: "go".into(),
                model: None,
                model_profile: None,
                provider: None,
                context: None,
                cache_strategy: None,
                frozen_parent_context: None,
                system: None,
                tool_filter: None,
                max_iterations: 3,
                metadata: json!({ "agent_id": "agent-no-tools" }),
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
            })
            .await
            .err()
            .expect("deny_all should produce empty tool list and fail spawn");
        assert!(err.contains("NoMatchingTools") || err.contains("no"));
        assert!(
            registry.snapshot(&TaskId::new("agent-no-tools")).is_none(),
            "failed worker creation must not leave a running task snapshot"
        );
    }

    /// Verifies that [`SubAgentSpec::cwd`] propagates end-to-end into
    /// the worker's [`ToolContext`] — regression guard for the fix
    /// that ensures worktree-isolated sub-agents' tool calls land in
    /// the requested cwd rather than leaking out to the parent
    /// process cwd.
    #[tokio::test]
    async fn spawner_propagates_spec_cwd_into_worker_tool_context() {
        use std::sync::Mutex;

        let _coord = CoordModeGuard::set(false);
        let _home = TestTaskHome::new("spawner-cwd-context");

        #[derive(Clone)]
        struct SeenContext {
            cwd: Option<String>,
            path_scope_roots: Vec<PathBuf>,
            write_scope_roots: Option<Vec<PathBuf>>,
            auto_approved_write_roots: Vec<PathBuf>,
            permission_prompts_unavailable: bool,
        }

        /// Records the cwd and file scopes observed on each call.
        struct CwdReporter(Arc<Mutex<Option<SeenContext>>>);
        #[async_trait]
        impl Tool for CwdReporter {
            fn id(&self) -> ToolId {
                ToolId::new("ReportCwd")
            }
            fn description(&self) -> &str {
                "record cwd"
            }
            fn input_schema(&self) -> ToolInputSchema {
                json!({ "type": "object" })
            }
            async fn validate_input(
                &self,
                _input: &Value,
                _context: &ToolContext,
            ) -> ToolResult<ValidationOutcome> {
                Ok(ValidationOutcome::valid())
            }
            async fn check_permissions(
                &self,
                _input: &Value,
                _context: &ToolContext,
            ) -> ToolResult<PermissionDecision> {
                Ok(PermissionDecision::allow(Value::Null))
            }
            async fn call(&self, _input: Value, context: &ToolContext) -> ToolResult<Value> {
                *self.0.lock().unwrap() = Some(SeenContext {
                    cwd: context.cwd().map(|s| s.to_string()),
                    path_scope_roots: context.path_scope_roots().to_vec(),
                    write_scope_roots: context.write_scope_roots().map(|roots| roots.to_vec()),
                    auto_approved_write_roots: context.auto_approved_write_roots().to_vec(),
                    permission_prompts_unavailable: context.permission_prompts_unavailable(),
                });
                Ok(json!({"cwd": context.cwd()}))
            }
        }

        let recorded: Arc<Mutex<Option<SeenContext>>> = Arc::new(Mutex::new(None));
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(CwdReporter(recorded.clone())));
        let engine = Arc::new(engine);

        // Mock: first tool_use → then text turn.
        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(tool_use_turn("m1", "ReportCwd", "tid1", "{}"));
        mock.push_turn(text_turn("done"));
        let client: Arc<dyn ModelClient> = mock.clone();

        let spawner =
            EngineSubAgentSpawner::new(Arc::downgrade(&engine), client).with_default_model("mock");

        let expected_cwd = if cfg!(windows) {
            "C:\\tmp\\rebon-test-cwd".to_string()
        } else {
            "/tmp/rebon-test-cwd".to_string()
        };

        let _ = spawner
            .spawn(SubAgentSpec {
                prompt: "go".into(),
                model: None,
                model_profile: None,
                provider: None,
                context: None,
                cache_strategy: None,
                frozen_parent_context: None,
                system: None,
                tool_filter: None,
                max_iterations: 3,
                metadata: json!({
                    "agent_id": "agent-verification-scope",
                    "agent_type": "verification",
                }),
                run_in_background: false,
                permission_prompts_unavailable: true,
                workflow_nesting_depth: 0,
                cwd: Some(expected_cwd.clone()),
                runtime_isolated_worktree: false,
                allowed_roots: Vec::new(),
                capability_context: None,
                ultraplan_run_repository: None,
                task_list_id: None,
                execution_policy: None,
                task_kind: None,
                permission_broker: None,
            })
            .await
            .unwrap();

        let seen = recorded.lock().unwrap().clone().unwrap();
        assert_eq!(
            seen.cwd,
            Some(expected_cwd.clone()),
            "sub-agent tool should observe the cwd the spawner was given"
        );
        let scratchpad_key = std::env::var("REBON_SESSION_ID")
            .ok()
            .filter(|session_id| !session_id.trim().is_empty())
            .unwrap_or_else(|| "agent-verification-scope".to_string());
        let expected_scratchpad = PathBuf::from(rebon_core::system_prompt::scratchpad_dir_for(
            &expected_cwd,
            &scratchpad_key,
        ));
        assert_eq!(
            seen.path_scope_roots,
            vec![PathBuf::from(&expected_cwd), expected_scratchpad.clone(),]
        );
        assert_eq!(
            seen.write_scope_roots,
            Some(vec![expected_scratchpad.clone()])
        );
        assert_eq!(seen.auto_approved_write_roots, vec![expected_scratchpad]);
        assert!(seen.permission_prompts_unavailable);

        *recorded.lock().unwrap() = None;
        mock.push_turn(tool_use_turn("m2", "ReportCwd", "tid2", "{}"));
        mock.push_turn(text_turn("done"));
        let _ = spawner
            .spawn(SubAgentSpec {
                prompt: "go".into(),
                model: None,
                model_profile: None,
                provider: None,
                context: None,
                cache_strategy: None,
                frozen_parent_context: None,
                system: None,
                tool_filter: None,
                max_iterations: 3,
                metadata: json!({
                    "agent_id": "agent-general-scope",
                    "agent_type": "general-purpose",
                }),
                run_in_background: false,
                permission_prompts_unavailable: true,
                workflow_nesting_depth: 0,
                cwd: Some(expected_cwd.clone()),
                runtime_isolated_worktree: false,
                allowed_roots: Vec::new(),
                capability_context: None,
                ultraplan_run_repository: None,
                task_list_id: None,
                execution_policy: None,
                task_kind: None,
                permission_broker: None,
            })
            .await
            .unwrap();

        let seen = recorded.lock().unwrap().clone().unwrap();
        let scratchpad_key = std::env::var("REBON_SESSION_ID")
            .ok()
            .filter(|session_id| !session_id.trim().is_empty())
            .unwrap_or_else(|| "agent-general-scope".to_string());
        let expected_scratchpad = PathBuf::from(rebon_core::system_prompt::scratchpad_dir_for(
            &expected_cwd,
            &scratchpad_key,
        ));
        assert_eq!(
            seen.path_scope_roots,
            vec![PathBuf::from(&expected_cwd), expected_scratchpad.clone(),]
        );
        assert_eq!(seen.write_scope_roots, None);
        assert_eq!(seen.auto_approved_write_roots, vec![expected_scratchpad]);
        assert!(seen.permission_prompts_unavailable);
    }

    /// The report path the worker is ORDERED to write lives in the
    /// config home, outside the project and outside any worktree, and
    /// `allowed_roots` cannot reach it (worktree preparation replaces
    /// them). Unless the runtime carves the file out, every coordinator
    /// worker fails with "report file is missing".
    #[tokio::test]
    async fn coordinator_worker_may_write_its_own_report_file() {
        use std::sync::Mutex;

        let _coord = CoordModeGuard::set(true);
        let _home = TestTaskHome::new("spawner-report-scope");

        #[derive(Clone)]
        struct SeenContext {
            path_scope_roots: Vec<PathBuf>,
            write_scope_roots: Option<Vec<PathBuf>>,
            auto_approved_write_roots: Vec<PathBuf>,
        }

        struct ScopeReporter(Arc<Mutex<Option<SeenContext>>>);
        #[async_trait]
        impl Tool for ScopeReporter {
            fn id(&self) -> ToolId {
                ToolId::new("ReportScope")
            }
            fn description(&self) -> &str {
                "record file scopes"
            }
            fn input_schema(&self) -> ToolInputSchema {
                json!({ "type": "object" })
            }
            async fn validate_input(
                &self,
                _input: &Value,
                _context: &ToolContext,
            ) -> ToolResult<ValidationOutcome> {
                Ok(ValidationOutcome::valid())
            }
            async fn check_permissions(
                &self,
                _input: &Value,
                _context: &ToolContext,
            ) -> ToolResult<PermissionDecision> {
                Ok(PermissionDecision::allow(Value::Null))
            }
            async fn call(&self, _input: Value, context: &ToolContext) -> ToolResult<Value> {
                *self.0.lock().unwrap() = Some(SeenContext {
                    path_scope_roots: context.path_scope_roots().to_vec(),
                    write_scope_roots: context.write_scope_roots().map(|roots| roots.to_vec()),
                    auto_approved_write_roots: context.auto_approved_write_roots().to_vec(),
                });
                Ok(json!({}))
            }
        }

        let cwd = if cfg!(windows) {
            "C:\\tmp\\rebon-test-report-scope".to_string()
        } else {
            "/tmp/rebon-test-report-scope".to_string()
        };

        let spawn_and_record = |agent_id: &'static str, agent_type: &'static str| {
            let cwd = cwd.clone();
            async move {
                let recorded: Arc<Mutex<Option<SeenContext>>> = Arc::new(Mutex::new(None));
                let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
                engine.register_tool(Arc::new(ScopeReporter(recorded.clone())));
                let engine = Arc::new(engine);
                let mock = Arc::new(MockModelClient::new());
                mock.push_turn(tool_use_turn("m1", "ReportScope", "tid1", "{}"));
                for _ in 0..4 {
                    mock.push_turn(text_turn("done"));
                }
                let client: Arc<dyn ModelClient> = mock.clone();
                let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
                    .with_default_model("mock");
                // Research, not implementation: keeps the runtime from
                // preparing a worktree for a cwd that does not exist.
                let _ = spawner
                    .spawn(SubAgentSpec {
                        prompt: "go".into(),
                        model: None,
                        model_profile: None,
                        provider: None,
                        context: None,
                        cache_strategy: None,
                        frozen_parent_context: None,
                        system: None,
                        tool_filter: None,
                        max_iterations: 3,
                        metadata: json!({
                            "agent_id": agent_id,
                            "agent_type": agent_type,
                            "coordinator_task_kind": "research",
                        }),
                        run_in_background: false,
                        permission_prompts_unavailable: true,
                        workflow_nesting_depth: 0,
                        cwd: Some(cwd.clone()),
                        runtime_isolated_worktree: false,
                        allowed_roots: Vec::new(),
                        capability_context: None,
                        ultraplan_run_repository: None,
                        task_list_id: None,
                        execution_policy: None,
                        task_kind: None,
                        permission_broker: None,
                    })
                    .await;
                let seen = recorded.lock().unwrap().clone();
                seen.expect("tool observed")
            }
        };

        let report = PathBuf::from(report_file_path_for_agent_id("agent-report-scope"));
        let seen = spawn_and_record("agent-report-scope", "general-purpose").await;
        assert!(
            seen.path_scope_roots.contains(&report),
            "worker must be able to read/write its own report: {:?}",
            seen.path_scope_roots
        );
        assert!(
            seen.auto_approved_write_roots.contains(&report),
            "writing the mandated report must not raise a permission prompt: {:?}",
            seen.auto_approved_write_roots
        );

        // Verification workers carry an explicit write scope, which
        // takes priority over `path_scope_roots` — it has to list the
        // report too or they can never satisfy the same contract.
        let verification_report =
            PathBuf::from(report_file_path_for_agent_id("agent-report-scope-verify"));
        let seen = spawn_and_record("agent-report-scope-verify", "verification").await;
        let write_roots = seen
            .write_scope_roots
            .expect("verification workers keep an explicit write scope");
        assert!(
            write_roots.contains(&verification_report),
            "verification worker must still be able to write its report: {write_roots:?}"
        );
    }

    /// Worktree isolation reaches the worker through the spec flag the
    /// runtime sets when it creates the tree — never through the shape of
    /// `cwd`, which the spawning agent chooses. Otherwise a parent could
    /// name a lookalike `.rebon/worktrees/…` directory and strip the
    /// shared-worktree Git gate off its own child.
    #[tokio::test]
    async fn spawner_takes_worktree_isolation_from_the_spec_not_the_cwd_shape() {
        use std::sync::Mutex;

        let _coord = CoordModeGuard::set(false);

        struct IsolationReporter(Arc<Mutex<Option<(bool, usize)>>>);
        #[async_trait]
        impl Tool for IsolationReporter {
            fn id(&self) -> ToolId {
                ToolId::new("ReportIsolation")
            }
            fn description(&self) -> &str {
                "record worktree isolation"
            }
            fn input_schema(&self) -> ToolInputSchema {
                json!({ "type": "object" })
            }
            async fn validate_input(
                &self,
                _input: &Value,
                _context: &ToolContext,
            ) -> ToolResult<ValidationOutcome> {
                Ok(ValidationOutcome::valid())
            }
            async fn check_permissions(
                &self,
                _input: &Value,
                _context: &ToolContext,
            ) -> ToolResult<PermissionDecision> {
                Ok(PermissionDecision::allow(Value::Null))
            }
            async fn call(&self, _input: Value, context: &ToolContext) -> ToolResult<Value> {
                *self.0.lock().unwrap() = Some((
                    context.is_isolated_worktree(),
                    context.workflow_nesting_depth(),
                ));
                Ok(json!({ "isolated": context.is_isolated_worktree() }))
            }
        }

        let recorded: Arc<Mutex<Option<(bool, usize)>>> = Arc::new(Mutex::new(None));
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(IsolationReporter(recorded.clone())));
        let engine = Arc::new(engine);

        let mock = Arc::new(MockModelClient::new());
        let client: Arc<dyn ModelClient> = mock.clone();
        let spawner =
            EngineSubAgentSpawner::new(Arc::downgrade(&engine), client).with_default_model("mock");

        // A path that merely looks like a runtime worktree.
        let spoofed_cwd = if cfg!(windows) {
            "C:\\repo\\.rebon\\worktrees\\agent-x".to_string()
        } else {
            "/repo/.rebon/worktrees/agent-x".to_string()
        };

        let mut spec = SubAgentSpec::new("go");
        spec.max_iterations = 3;
        spec.cwd = Some(spoofed_cwd.clone());
        spec.metadata = json!({ "agent_id": "agent-spoofed", "agent_type": "general-purpose" });
        // Workflow lineage: the depth the gate keys off must survive the
        // hop into the worker's own context alongside the isolation flag.
        spec.workflow_nesting_depth = 1;

        mock.push_turn(tool_use_turn("m1", "ReportIsolation", "tid1", "{}"));
        mock.push_turn(text_turn("done"));
        let _ = spawner.spawn(spec.clone()).await.unwrap();
        assert_eq!(
            recorded.lock().unwrap().take(),
            Some((false, 1)),
            "a worktree-shaped cwd alone must not confer isolation"
        );

        // Same cwd, but now the runtime vouches for having created it.
        spec.runtime_isolated_worktree = true;
        mock.push_turn(tool_use_turn("m2", "ReportIsolation", "tid2", "{}"));
        mock.push_turn(text_turn("done"));
        let _ = spawner.spawn(spec).await.unwrap();
        assert_eq!(
            recorded.lock().unwrap().take(),
            Some((true, 1)),
            "a runtime-created worktree must reach the worker context"
        );
    }

    /// When [`SubAgentSpec::cwd`] is `None`, the worker's
    /// [`ToolContext`] should have no cwd set either — tools fall back
    /// to the process cwd as before, preserving today's default.
    #[tokio::test]
    async fn spawner_leaves_worker_cwd_unset_when_spec_cwd_is_none() {
        use std::sync::Mutex;

        let _coord = CoordModeGuard::set(false);

        struct CwdReporter(Arc<Mutex<Option<String>>>);
        #[async_trait]
        impl Tool for CwdReporter {
            fn id(&self) -> ToolId {
                ToolId::new("ReportCwd")
            }
            fn description(&self) -> &str {
                "record cwd"
            }
            fn input_schema(&self) -> ToolInputSchema {
                json!({ "type": "object" })
            }
            async fn validate_input(
                &self,
                _input: &Value,
                _context: &ToolContext,
            ) -> ToolResult<ValidationOutcome> {
                Ok(ValidationOutcome::valid())
            }
            async fn check_permissions(
                &self,
                _input: &Value,
                _context: &ToolContext,
            ) -> ToolResult<PermissionDecision> {
                Ok(PermissionDecision::allow(Value::Null))
            }
            async fn call(&self, _input: Value, context: &ToolContext) -> ToolResult<Value> {
                *self.0.lock().unwrap() = context.cwd().map(|s| s.to_string());
                Ok(Value::Null)
            }
        }

        let recorded: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(CwdReporter(recorded.clone())));
        let engine = Arc::new(engine);

        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(tool_use_turn("m1", "ReportCwd", "tid1", "{}"));
        mock.push_turn(text_turn("done"));
        let client: Arc<dyn ModelClient> = mock.clone();

        let spawner =
            EngineSubAgentSpawner::new(Arc::downgrade(&engine), client).with_default_model("mock");

        let _ = spawner
            .spawn(SubAgentSpec {
                prompt: "go".into(),
                model: None,
                model_profile: None,
                provider: None,
                context: None,
                cache_strategy: None,
                frozen_parent_context: None,
                system: None,
                tool_filter: None,
                max_iterations: 3,
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
            })
            .await
            .unwrap();

        let seen = recorded.lock().unwrap().clone();
        assert_eq!(
            seen, None,
            "worker cwd should stay unset when spec.cwd is None"
        );
    }

    fn valid_worker_report() -> String {
        "# Worker Report\n\n\
## Summary\n\nDone.\n\n\
## Files Changed / Inspected\n\n- src/lib.rs:1\n\n\
## Evidence\n\n- evidence\n\n\
## Verification / Tests\n\n- not run\n\n\
## Blockers / Assumptions\n\nNone.\n\n\
## Final Status\n\nComplete.\n"
            .to_string()
    }

    #[test]
    fn real_report_valid_structured_output_missing_records_structured_coercion_only() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("rebon-spawner-report-valid-so-missing.md");
        std::fs::write(&report_path, valid_worker_report()).unwrap();
        let diagnostics = Arc::new(ValidatorLifecycleDiagnostics::default());
        let channel = Arc::new(rebon_tool::StructuredOutputChannel::new(Some(json!({
            "type": "object",
            "required": ["score"],
            "properties": { "score": { "type": "number" } }
        }))));
        let hook = WorkerDeliveryHook {
            rules: vec![
                WorkerDeliveryRule::Report(ReportFileContract::new(
                    report_path.to_string_lossy().to_string(),
                    SubAgentTaskKind::Research,
                    true,
                    None,
                    diagnostics.clone(),
                )),
                WorkerDeliveryRule::StructuredOutput(StructuredOutputContract::new(
                    channel,
                    diagnostics.clone(),
                )),
            ],
        };

        let first = hook.evaluate().expect("structured coercion");
        let text = api_message_text(&first.message);
        assert!(!text.contains("Multiple required delivery contracts"));
        assert!(text.contains("StructuredOutput delivery is still missing"));
        assert!(first.force_structured_output);
        assert_eq!(diagnostics.validator_coercions_emitted(), 1);
        assert_eq!(diagnostics.structured_output_attempts(), 1);
        assert!(!diagnostics.structured_output_exhausted());
    }

    #[tokio::test]
    async fn real_structured_output_valid_report_missing_records_report_coercion_only() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("rebon-spawner-so-valid-report-missing.md");
        let diagnostics = Arc::new(ValidatorLifecycleDiagnostics::default());
        let channel = Arc::new(rebon_tool::StructuredOutputChannel::new(Some(json!({
            "type": "object",
            "required": ["score"],
            "properties": { "score": { "type": "number" } }
        }))));
        let ctx = ToolContext::new().with_structured_output_channel(channel.clone());
        rebon_plugin_structured_output::StructuredOutputTool
            .call(json!({ "score": 7 }), &ctx)
            .await
            .unwrap();
        let hook = WorkerDeliveryHook {
            rules: vec![
                WorkerDeliveryRule::Report(ReportFileContract::new(
                    report_path.to_string_lossy().to_string(),
                    SubAgentTaskKind::Research,
                    true,
                    None,
                    diagnostics.clone(),
                )),
                WorkerDeliveryRule::StructuredOutput(StructuredOutputContract::new(
                    channel,
                    diagnostics.clone(),
                )),
            ],
        };

        let first = hook.evaluate().expect("report coercion");
        let text = api_message_text(&first.message);
        assert!(!text.contains("Multiple required delivery contracts"));
        assert!(text.contains("report file"));
        assert!(!first.force_structured_output);
        assert_eq!(diagnostics.validator_coercions_emitted(), 1);
        assert_eq!(diagnostics.structured_output_attempts(), 0);
    }

    #[tokio::test]
    async fn real_report_and_structured_output_valid_emit_no_coercion() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("rebon-spawner-both-valid.md");
        std::fs::write(&report_path, valid_worker_report()).unwrap();
        let diagnostics = Arc::new(ValidatorLifecycleDiagnostics::default());
        let channel = Arc::new(rebon_tool::StructuredOutputChannel::new(Some(json!({
            "type": "object",
            "required": ["score"],
            "properties": { "score": { "type": "number" } }
        }))));
        let ctx = ToolContext::new().with_structured_output_channel(channel.clone());
        rebon_plugin_structured_output::StructuredOutputTool
            .call(json!({ "score": 7 }), &ctx)
            .await
            .unwrap();
        let hook = WorkerDeliveryHook {
            rules: vec![
                WorkerDeliveryRule::Report(ReportFileContract::new(
                    report_path.to_string_lossy().to_string(),
                    SubAgentTaskKind::Research,
                    true,
                    None,
                    diagnostics.clone(),
                )),
                WorkerDeliveryRule::StructuredOutput(StructuredOutputContract::new(
                    channel,
                    diagnostics.clone(),
                )),
            ],
        };

        assert!(hook.evaluate().is_none());
        assert_eq!(diagnostics.validator_coercions_emitted(), 0);
        assert_eq!(diagnostics.structured_output_attempts(), 0);
    }

    #[test]
    fn real_report_and_structured_output_missing_combines_and_exhausts_without_passing() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("rebon-spawner-both-missing.md");
        let diagnostics = Arc::new(ValidatorLifecycleDiagnostics::default());
        let channel = Arc::new(rebon_tool::StructuredOutputChannel::new(Some(json!({
            "type": "object",
            "required": ["score"],
            "properties": { "score": { "type": "number" } }
        }))));
        let hook = WorkerDeliveryHook {
            rules: vec![
                WorkerDeliveryRule::Report(ReportFileContract::new(
                    report_path.to_string_lossy().to_string(),
                    SubAgentTaskKind::Research,
                    true,
                    None,
                    diagnostics.clone(),
                )),
                WorkerDeliveryRule::StructuredOutput(StructuredOutputContract::new(
                    channel,
                    diagnostics.clone(),
                )),
            ],
        };

        let first = hook.evaluate().expect("combined coercion");
        let text = api_message_text(&first.message);
        assert!(text.contains("Multiple required delivery contracts"));
        assert!(text.contains("report file"));
        assert!(text.contains("StructuredOutput delivery is still missing"));
        assert!(first.force_structured_output);
        assert_eq!(diagnostics.validator_coercions_emitted(), 2);
        assert_eq!(diagnostics.structured_output_attempts(), 1);

        assert!(hook.evaluate().is_none());
        assert!(diagnostics.structured_output_exhausted());
        let failures = diagnostics.validator_failures(true, true);
        assert!(failures.contains(&Value::String("report_file_invalid".into())));
        assert!(failures.contains(&Value::String("structured_output_missing".into())));
    }
    #[tokio::test]
    async fn spawner_diagnostics_include_validator_lifecycle_and_visibility_evidence() {
        let _coord = CoordModeGuard::set(false);
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(
            rebon_plugin_structured_output::StructuredOutputTool,
        ));
        let engine = Arc::new(engine);
        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn("not structured"));
        mock.push_turn(text_turn("still not structured"));
        let client: Arc<dyn ModelClient> = mock;
        let spawner =
            EngineSubAgentSpawner::new(Arc::downgrade(&engine), client).with_default_model("mock");
        let mut spec = SubAgentSpec::new("return structured data");
        spec.execution_policy = Some(
            rebon_tool::ExecutionPolicy::default()
                .with_eager_promotions([rebon_tool::STRUCTURED_OUTPUT_TOOL_NAME]),
        );
        spec.metadata = json!({
            rebon_tool::WORKFLOW_STRUCTURED_OUTPUT_SCHEMA_KEY: {
                "type": "object",
                "required": ["score"],
                "properties": { "score": { "type": "number" } }
            }
        });

        let result = spawner.spawn(spec).await.unwrap();
        assert_eq!(result.status, "completed");
        let diagnostics = result.diagnostics.as_ref().expect("diagnostics");
        assert_eq!(
            diagnostics["validators"]["installed_validators"],
            json!(["structured_output"])
        );
        assert_eq!(diagnostics["validators"]["validator_coercions_emitted"], 1);
        assert_eq!(diagnostics["structured_output"]["coercion_attempts"], 1);
        assert_eq!(diagnostics["structured_output"]["coercion_exhausted"], true);
        assert_eq!(
            diagnostics["tool_visibility"]["provider_visible_tools_contains_structured_output"],
            true
        );
    }

    #[test]
    fn structured_output_contract_coerces_once_when_unsatisfied() {
        let channel = Arc::new(rebon_tool::StructuredOutputChannel::new(Some(json!({
            "type": "object",
            "required": ["score"],
            "properties": { "score": { "type": "number" } }
        }))));
        let validator = StructuredOutputContract::new(
            channel.clone(),
            Arc::new(ValidatorLifecycleDiagnostics::default()),
        );

        // First end-of-turn with no result -> inject one coercion message
        // that names the StructuredOutput tool and embeds the schema.
        let first = validator
            .evaluate()
            .expect("an unsatisfied worker should be coerced")
            .message;
        assert_eq!(first.role, Role::User);
        let ApiContentBlock::Text(TextBlock { text }) = &first.content[0] else {
            panic!("coercion message must be text");
        };
        assert!(text.contains("StructuredOutput"));
        assert!(text.contains("Call the StructuredOutput tool right now"));
        assert!(text.contains("Do not answer with prose"));
        assert!(text.contains("schema-valid fallback JSON"));
        assert!(text.contains("score"), "schema should be embedded: {text}");

        // Second call must NOT loop forever — one coercion only.
        assert!(
            validator.evaluate().is_none(),
            "validator must fire at most once"
        );
    }

    #[derive(Clone)]
    struct PersistedRunRepository {
        state: Arc<Mutex<rebon_types::UltraplanRunState>>,
    }

    impl rebon_tool::UltraplanRunRepository for PersistedRunRepository {
        fn load_current(
            &self,
        ) -> Result<rebon_types::UltraplanRunState, rebon_tool::UltraplanRepositoryError> {
            Ok(self.state.lock().unwrap().clone())
        }

        fn load_run(
            &self,
            run_id: &str,
        ) -> Result<Option<rebon_types::UltraplanRunState>, rebon_tool::UltraplanRepositoryError>
        {
            let state = self.state.lock().unwrap();
            Ok((state.run_id == run_id).then(|| state.clone()))
        }

        fn compare_and_swap(
            &self,
            expected_revision: u64,
            state: &rebon_types::UltraplanRunState,
        ) -> Result<(), rebon_tool::UltraplanRepositoryError> {
            let mut current = self.state.lock().unwrap();
            if current.state_revision != expected_revision {
                return Err(rebon_tool::UltraplanRepositoryError::StaleRevision {
                    expected: expected_revision,
                    actual: current.state_revision,
                });
            }
            *current = state.clone();
            Ok(())
        }

        fn create_run(
            &self,
            _state: &rebon_types::UltraplanRunState,
        ) -> Result<(), rebon_tool::UltraplanRepositoryError> {
            Err(rebon_tool::UltraplanRepositoryError::Storage(
                "not supported".into(),
            ))
        }
    }

    fn capability_spec(root: &Path, run_id: &str, parent_session_id: &str) -> SubAgentSpec {
        let canonical = std::fs::canonicalize(root).unwrap();
        let mut capability = rebon_types::CapabilityContext {
            run_id: run_id.into(),
            ledger_revision: 2,
            requirements_hash: "requirements".into(),
            session_id: "session".into(),
            cwd: canonical.to_string_lossy().to_string(),
            allowed_roots: vec![canonical.to_string_lossy().to_string()],
            read_allowed: true,
            write_allowed: false,
            shell_allowed: false,
            tool_ids: vec!["Read".into()],
            network: rebon_types::NetworkCapability::Denied,
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
        let mut policy =
            UltraplanContext::planning_turn(run_id, "researching", PolicyMode::Enforce);
        policy.ledger_revision = capability.ledger_revision;
        policy.requirements_hash = capability.requirements_hash.clone();
        let mut spec = SubAgentSpec::new("research");
        spec.cwd = Some(capability.cwd.clone());
        spec.allowed_roots = vec![canonical];
        spec.tool_filter = Some(ToolFilter::allow_only(["Read"]));
        spec.execution_policy = Some(ExecutionPolicy::ultraplan(policy));
        spec.capability_context = Some(capability);
        spec.metadata = json!({
            "agent_type": "Explore",
            "ultraplan_role": "researcher",
            "parent_session_id": parent_session_id,
        });
        spec
    }

    #[test]
    fn capability_preflight_repairs_parent_session_once() {
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(ReadLikeTool));
        let engine = Arc::new(engine);
        let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client);
        let root = tempfile::tempdir().unwrap();
        let mut spec = capability_spec(root.path(), "run-a", "old-session");

        spawner.preflight(&mut spec).unwrap();

        assert_eq!(spec.metadata["parent_session_id"], "session");
        assert_eq!(
            spec.metadata["ultraplan_ledger_revision"],
            serde_json::Value::Number(2.into())
        );
    }

    #[test]
    fn capability_preflight_uses_the_runtime_execution_policy_tool_intersection() {
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(ReadLikeTool));
        let engine = Arc::new(engine);
        let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client);
        let root = tempfile::tempdir().unwrap();
        let mut spec = capability_spec(root.path(), "run-a", "session");
        spec.tool_filter = Some(ToolFilter::allow_only(["Read", "ToolSearch"]));
        let capability = spec.capability_context.as_mut().unwrap();
        capability.tool_ids.push("ToolSearch".into());
        capability.refresh_hash();

        spawner.preflight(&mut spec).unwrap();
    }

    #[test]
    fn capability_preflight_opens_circuit_after_repeat_and_isolates_new_run() {
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(ReadLikeTool));
        let engine = Arc::new(engine);
        let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client);
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        let mut first = capability_spec(root.path(), "run-a", "session");
        first.cwd = Some(missing.to_string_lossy().to_string());
        first.allowed_roots = vec![missing.clone()];

        let first_error = spawner.preflight(&mut first).unwrap_err();
        let first_diagnostic: CapabilityDiagnostic = serde_json::from_str(&first_error).unwrap();
        assert_eq!(
            first_diagnostic.class,
            CapabilityDiagnosticClass::MissingRoot
        );

        let second_error = spawner.preflight(&mut first).unwrap_err();
        let second_diagnostic: CapabilityDiagnostic = serde_json::from_str(&second_error).unwrap();
        assert_eq!(
            second_diagnostic.class,
            CapabilityDiagnosticClass::CircuitOpen
        );
        assert!(second_diagnostic.fallback_to_parent);

        let mut other_run = capability_spec(root.path(), "run-b", "session");
        other_run.cwd = Some(missing.to_string_lossy().to_string());
        other_run.allowed_roots = vec![missing];
        let other_error = spawner.preflight(&mut other_run).unwrap_err();
        let other_diagnostic: CapabilityDiagnostic = serde_json::from_str(&other_error).unwrap();
        assert_eq!(
            other_diagnostic.class,
            CapabilityDiagnosticClass::MissingRoot
        );
    }

    #[test]
    fn capability_circuit_is_scoped_to_the_exact_worker_request() {
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(ReadLikeTool));
        let engine = Arc::new(engine);
        let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client);
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        let mut failing = capability_spec(root.path(), "run-a", "session");
        failing.cwd = Some(missing.to_string_lossy().to_string());
        failing.allowed_roots = vec![missing];

        let _ = spawner.preflight(&mut failing).unwrap_err();
        let opened: CapabilityDiagnostic =
            serde_json::from_str(&spawner.preflight(&mut failing).unwrap_err()).unwrap();
        assert_eq!(opened.class, CapabilityDiagnosticClass::CircuitOpen);

        let mut valid = capability_spec(root.path(), "run-a", "session");
        spawner.preflight(&mut valid).unwrap();
    }

    #[test]
    fn capability_circuit_survives_recreation_but_rechecks_recovered_failure() {
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
        engine.register_tool(Arc::new(ReadLikeTool));
        let engine = Arc::new(engine);
        let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        let repository = PersistedRunRepository {
            state: Arc::new(Mutex::new(rebon_types::UltraplanRunState::new(
                "run-a".into(),
                "session".into(),
                "task".into(),
                None,
                1,
            ))),
        };

        let make_spec = || {
            let mut spec = capability_spec(root.path(), "run-a", "session");
            spec.cwd = Some(missing.to_string_lossy().to_string());
            spec.allowed_roots = vec![missing.clone()];
            spec.ultraplan_run_repository = Some(Arc::new(repository.clone()));
            spec
        };
        let first_spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client.clone());
        let first: CapabilityDiagnostic =
            serde_json::from_str(&first_spawner.preflight(&mut make_spec()).unwrap_err()).unwrap();
        assert_eq!(first.class, CapabilityDiagnosticClass::MissingRoot);

        let second_spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client.clone());
        let second: CapabilityDiagnostic =
            serde_json::from_str(&second_spawner.preflight(&mut make_spec()).unwrap_err()).unwrap();
        assert_eq!(second.class, CapabilityDiagnosticClass::CircuitOpen);

        std::fs::create_dir(&missing).unwrap();
        let third_spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client);
        third_spawner.preflight(&mut make_spec()).unwrap();
        assert!(repository
            .state
            .lock()
            .unwrap()
            .tool_error_attempts
            .is_empty());
    }

    #[test]
    fn ultraplan_worker_budget_caps_research_and_isolates_runs() {
        let engine = Arc::new(Engine::new());
        let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client);
        let root = tempfile::tempdir().unwrap();
        let spec = capability_spec(root.path(), "run-a", "session");

        for _ in 0..6 {
            spawner.reserve_ultraplan_worker_slot(&spec).unwrap();
        }
        let error = spawner.reserve_ultraplan_worker_slot(&spec).unwrap_err();
        let diagnostic: CapabilityDiagnostic = serde_json::from_str(&error).unwrap();
        assert_eq!(diagnostic.class, CapabilityDiagnosticClass::BudgetExhausted);
        assert_eq!(diagnostic.capability.as_deref(), Some("research_agent"));
        assert!(diagnostic.fallback_to_parent);

        let other_run = capability_spec(root.path(), "run-b", "session");
        spawner.reserve_ultraplan_worker_slot(&other_run).unwrap();
    }

    #[test]
    fn ultraplan_worker_budget_resumes_from_persisted_usage() {
        let engine = Arc::new(Engine::new());
        let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client);
        let root = tempfile::tempdir().unwrap();
        let mut spec = capability_spec(root.path(), "run-a", "session");
        let capability = spec.capability_context.as_mut().unwrap();
        capability.research_agents_used = 5;
        capability.refresh_hash();

        spawner.reserve_ultraplan_worker_slot(&spec).unwrap();
        let error = spawner.reserve_ultraplan_worker_slot(&spec).unwrap_err();
        let diagnostic: CapabilityDiagnostic = serde_json::from_str(&error).unwrap();
        assert_eq!(diagnostic.class, CapabilityDiagnosticClass::BudgetExhausted);
    }

    #[test]
    fn ultraplan_worker_budget_allows_only_one_adversarial_review() {
        let engine = Arc::new(Engine::new());
        let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client);
        let root = tempfile::tempdir().unwrap();
        let mut spec = capability_spec(root.path(), "run-a", "session");
        spec.metadata["ultraplan_role"] = json!("reviewer");

        spawner.reserve_ultraplan_worker_slot(&spec).unwrap();
        let error = spawner.reserve_ultraplan_worker_slot(&spec).unwrap_err();
        let diagnostic: CapabilityDiagnostic = serde_json::from_str(&error).unwrap();
        assert_eq!(diagnostic.class, CapabilityDiagnosticClass::BudgetExhausted);
        assert_eq!(diagnostic.capability.as_deref(), Some("adversarial_review"));
        assert!(!diagnostic.fallback_to_parent);
    }

    #[test]
    fn ultraplan_pre_reserved_review_uses_persisted_slot() {
        let engine = Arc::new(Engine::new());
        let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client);
        let root = tempfile::tempdir().unwrap();
        let mut spec = capability_spec(root.path(), "run-a", "session");
        spec.metadata["ultraplan_role"] = json!("reviewer");
        spec.metadata["ultraplan_budget_pre_reserved"] = json!(true);
        let capability = spec.capability_context.as_mut().unwrap();
        capability.adversarial_reviews_used = 1;
        capability.refresh_hash();

        spawner.reserve_ultraplan_worker_slot(&spec).unwrap();
    }

    #[tokio::test]
    async fn structured_output_contract_stays_quiet_when_satisfied() {
        let channel = Arc::new(rebon_tool::StructuredOutputChannel::new(Some(json!({
            "type": "object",
            "required": ["score"],
            "properties": { "score": { "type": "number" } }
        }))));
        let validator = StructuredOutputContract::new(
            channel.clone(),
            Arc::new(ValidatorLifecycleDiagnostics::default()),
        );

        // Worker returns a schema-valid result through the tool, which
        // records it on the channel and marks it satisfied.
        let ctx = ToolContext::new().with_structured_output_channel(channel.clone());
        rebon_plugin_structured_output::StructuredOutputTool
            .call(json!({ "score": 7 }), &ctx)
            .await
            .expect("valid result should be accepted");

        assert!(channel.is_satisfied());
        assert!(
            validator.evaluate().is_none(),
            "a satisfied worker must be left alone"
        );
    }

    // ── external sub-agent routing ────────────────────────────────

    #[derive(Debug, Clone)]
    struct RecordedExternalRequest {
        agent_id: String,
        model_hint: Option<String>,
        task_session_id: String,
        prompt: String,
        cwd: Option<String>,
    }

    struct MockExternalRunner {
        known: Vec<&'static str>,
        requests: Arc<Mutex<Vec<RecordedExternalRequest>>>,
        outcomes: Mutex<std::collections::VecDeque<Result<ExternalTaskOutcome, String>>>,
        events: Mutex<Vec<ExternalTaskEvent>>,
    }

    impl MockExternalRunner {
        fn new(known: Vec<&'static str>) -> Arc<Self> {
            Arc::new(Self {
                known,
                requests: Arc::new(Mutex::new(Vec::new())),
                outcomes: Mutex::new(std::collections::VecDeque::new()),
                events: Mutex::new(Vec::new()),
            })
        }

        fn push_outcome(&self, outcome: Result<ExternalTaskOutcome, String>) {
            self.outcomes.lock().unwrap().push_back(outcome);
        }

        fn set_events(&self, events: Vec<ExternalTaskEvent>) {
            *self.events.lock().unwrap() = events;
        }

        fn requests(&self) -> Vec<RecordedExternalRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ExternalSubAgentRunner for MockExternalRunner {
        fn resolve_agent(&self, prefix: &str) -> Option<String> {
            let folded = prefix.trim().to_ascii_lowercase();
            self.known
                .iter()
                .find(|id| id.to_ascii_lowercase() == folded)
                .map(|id| id.to_string())
        }

        async fn run_task(
            &self,
            request: ExternalTaskRequest,
        ) -> Result<ExternalTaskOutcome, String> {
            self.requests.lock().unwrap().push(RecordedExternalRequest {
                agent_id: request.agent_id.clone(),
                model_hint: request.model_hint.clone(),
                task_session_id: request.task_session_id.clone(),
                prompt: request.prompt.clone(),
                cwd: request.cwd.clone(),
            });
            if let Some(progress) = &request.progress {
                for event in self.events.lock().unwrap().clone() {
                    progress(event);
                }
            }
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    Ok(ExternalTaskOutcome {
                        final_text: "external answer".into(),
                        status: ExternalTaskStatus::Completed,
                        tool_call_count: 2,
                        acp_session_id: Some("agent-sess-9".into()),
                    })
                })
        }
    }

    fn external_test_spawner(
        runner: Arc<MockExternalRunner>,
    ) -> (EngineSubAgentSpawner, TaskRegistry, Arc<Engine>) {
        let engine = Arc::new(Engine::with_builtin_tools());
        let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let registry = TaskRegistry::new();
        let spawner = EngineSubAgentSpawner::new(Arc::downgrade(&engine), client)
            .with_default_model("mock")
            .with_test_task_registry(registry.clone())
            .with_external_runner(runner);
        (spawner, registry, engine)
    }

    #[tokio::test]
    async fn an_external_spec_routes_to_the_runner_and_maps_the_result() {
        let task_home = TestTaskHome::new("external-linked-task");
        let linked_task = rebon_tool::tasks::create_task(
            task_home.task_list_id(),
            rebon_tool::tasks::NewTask {
                subject: "external parent task".into(),
                description: "delegated to external agent".into(),
                owner: Some("claudecode".into()),
                status: rebon_tool::tasks::TaskListStatus::InProgress,
                ..Default::default()
            },
        )
        .unwrap();
        let runner = MockExternalRunner::new(vec!["claudecode"]);
        let (spawner, registry, _engine) = external_test_spawner(runner.clone());

        let mut spec = SubAgentSpec::new("Find the bug in auth.rs");
        spec.model = Some("ClaudeCode:claude-opus-5".into());
        spec.system = Some("Only ever read files.".into());
        spec.task_list_id = Some(task_home.task_list_id().to_string());
        spec.metadata = json!({
            "agent_type": "Explore",
            "description": "hunt the bug",
            "taskId": linked_task,
        });
        let result = spawner.spawn(spec).await.expect("external spawn");

        // Runner saw one task with the composed brief and the hint.
        let requests = runner.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].agent_id, "claudecode");
        assert_eq!(requests[0].model_hint.as_deref(), Some("claude-opus-5"));
        assert!(requests[0].prompt.starts_with("<rebon-subagent-brief>"));
        assert!(requests[0].prompt.contains("Only ever read files."));
        assert!(requests[0].prompt.ends_with("Find the bug in auth.rs"));
        assert!(requests[0].task_session_id.starts_with("subagent-agent-"));
        assert!(requests[0].cwd.is_none());

        // Result is honest about what an external agent can report.
        assert_eq!(result.status, "completed");
        assert_eq!(result.final_text, "external answer");
        assert_eq!(result.tool_call_count, 2);
        assert_eq!(result.provider.as_deref(), Some("acp:claudecode"));
        assert_eq!(result.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(result.total_tokens, None);
        assert_eq!(result.output_tokens, None);
        assert_eq!(result.usage.as_ref().unwrap()["reported"], false);
        assert_eq!(
            result.diagnostics.as_ref().unwrap()["external"]["acpSessionId"],
            "agent-sess-9"
        );
        assert_eq!(
            rebon_tool::tasks::get_task(task_home.task_list_id(), &linked_task)
                .unwrap()
                .unwrap()
                .status,
            rebon_tool::tasks::TaskListStatus::Completed
        );

        // Registry reached a terminal snapshot with the acp model label
        // and no dangling turn.
        let task_id = TaskId::new(result.agent_id.clone().unwrap());
        let snapshot = registry.snapshot(&task_id).expect("task snapshot");
        assert_eq!(snapshot.status, TaskStatus::Completed);
        match &snapshot.data {
            TaskData::LocalAgent(data) => {
                assert_eq!(data.model.as_deref(), Some("acp:claudecode:claude-opus-5"));
            }
            other => panic!("expected a LocalAgent task, got {other:?}"),
        }
        assert!(!registry.task_turn_is_active(&task_id));
    }

    #[tokio::test]
    async fn external_progress_populates_the_live_task_stream() {
        let runner = MockExternalRunner::new(vec!["claudecode"]);
        runner.set_events(vec![
            ExternalTaskEvent::AgentText("preface ".into()),
            ExternalTaskEvent::Thinking("checking auth".into()),
            ExternalTaskEvent::ThinkingEnd,
            ExternalTaskEvent::AgentText("live ".into()),
            ExternalTaskEvent::ToolCall {
                tool_call_id: "call-1".into(),
                title: "Reading auth.rs".into(),
                input: Some(json!({"file_path": "auth.rs"})),
            },
            ExternalTaskEvent::ToolCallUpdate {
                tool_call_id: "call-1".into(),
                title: "Reading auth.rs".into(),
                status: Some(ToolCallStatus::Completed),
                output: Some(json!({"lines": 42})),
            },
            ExternalTaskEvent::Thinking("verify result".into()),
            ExternalTaskEvent::ThinkingEnd,
            ExternalTaskEvent::AgentText("answer".into()),
        ]);
        let (spawner, registry, _engine) = external_test_spawner(runner);

        let mut spec = SubAgentSpec::new("Find the bug in auth.rs");
        spec.model = Some("claudecode:".into());
        spec.metadata = json!({"agent_id": "agent-ext-live", "agent_type": "Explore"});
        spawner.spawn(spec).await.expect("external spawn");

        let task_id = TaskId::new("agent-ext-live");
        let snapshot = registry.snapshot(&task_id).expect("task snapshot");
        assert_eq!(snapshot.status, TaskStatus::Completed);
        assert_eq!(snapshot.last_progress.as_deref(), Some("external answer"));
        let TaskData::LocalAgent(data) = snapshot.data else {
            panic!("expected local agent snapshot");
        };
        assert_eq!(data.tool_use_count, 1);
        assert_eq!(data.streaming_text, None);
        assert_eq!(data.transcript.len(), 7);
        assert!(matches!(
            &data.transcript[0],
            LocalAgentTranscriptEntry::Assistant { text } if text == "preface "
        ));
        assert!(matches!(
            &data.transcript[1],
            LocalAgentTranscriptEntry::Thinking { text } if text == "checking auth"
        ));
        assert!(matches!(
            &data.transcript[2],
            LocalAgentTranscriptEntry::Assistant { text } if text == "live "
        ));
        assert!(matches!(
            &data.transcript[3],
            LocalAgentTranscriptEntry::ToolStart { tool_use_id, .. } if tool_use_id == "call-1"
        ));
        assert!(matches!(
            &data.transcript[4],
            LocalAgentTranscriptEntry::ToolFinish { ok: true, .. }
        ));
        assert!(matches!(
            &data.transcript[5],
            LocalAgentTranscriptEntry::Thinking { text } if text == "verify result"
        ));
        assert!(matches!(
            &data.transcript[6],
            LocalAgentTranscriptEntry::Assistant { text } if text == "answer"
        ));

        let events = registry
            .task_live_events(&task_id, None)
            .expect("live events")
            .events;
        assert_eq!(events.len(), 11);
        assert!(matches!(events[0].kind, TaskLiveEventKind::Started));
        assert!(matches!(
            events[1].kind,
            TaskLiveEventKind::AssistantTextDelta {
                ref delta,
                ref snapshot,
            } if delta == "preface " && snapshot == "preface "
        ));
        assert!(matches!(
            events[2].kind,
            TaskLiveEventKind::ThinkingDelta {
                ref delta,
                ref snapshot,
            } if delta == "checking auth" && snapshot == "checking auth"
        ));
        assert!(matches!(events[3].kind, TaskLiveEventKind::ThinkingEnd));
        assert!(matches!(
            events[4].kind,
            TaskLiveEventKind::AssistantTextDelta {
                ref delta,
                ref snapshot,
            } if delta == "live " && snapshot == "live "
        ));
        assert!(matches!(
            events[5].kind,
            TaskLiveEventKind::ToolStart { .. }
        ));
        assert!(matches!(
            events[6].kind,
            TaskLiveEventKind::ToolFinish { .. }
        ));
        assert!(matches!(
            events[7].kind,
            TaskLiveEventKind::ThinkingDelta {
                ref delta,
                ref snapshot,
            } if delta == "verify result" && snapshot == "verify result"
        ));
        assert!(matches!(events[8].kind, TaskLiveEventKind::ThinkingEnd));
        assert!(matches!(
            events[9].kind,
            TaskLiveEventKind::AssistantTextDelta {
                ref delta,
                ref snapshot,
            } if delta == "answer" && snapshot == "answer"
        ));
        assert!(matches!(
            events[10].kind,
            TaskLiveEventKind::Finished { .. }
        ));
    }

    #[tokio::test]
    async fn an_undeclared_prefix_never_reaches_the_runner() {
        let runner = MockExternalRunner::new(vec!["claudecode"]);
        let (spawner, _registry, _engine) = external_test_spawner(runner.clone());

        let mut spec = SubAgentSpec::new("just answer");
        spec.model = Some("llama3:8b".into());
        // The local path runs against the mock model client; whatever
        // it answers, the runner must not have been consulted.
        let _ = spawner.spawn(spec).await;

        assert!(
            runner.requests().is_empty(),
            "`llama3` is not a declared agent — the task must stay local"
        );
    }

    #[tokio::test]
    async fn a_runner_error_marks_the_task_failed() {
        let task_home = TestTaskHome::new("external-linked-task-failure");
        let linked_task = rebon_tool::tasks::create_task(
            task_home.task_list_id(),
            rebon_tool::tasks::NewTask {
                subject: "external parent task".into(),
                description: "delegated to external agent".into(),
                owner: Some("claudecode".into()),
                status: rebon_tool::tasks::TaskListStatus::InProgress,
                ..Default::default()
            },
        )
        .unwrap();
        let runner = MockExternalRunner::new(vec!["claudecode"]);
        runner.push_outcome(Err("agent exploded".into()));
        let (spawner, registry, _engine) = external_test_spawner(runner.clone());

        let mut spec = SubAgentSpec::new("do a thing");
        spec.model = Some("claudecode:".into());
        spec.task_list_id = Some(task_home.task_list_id().to_string());
        spec.metadata = json!({"agent_id": "agent-ext-fail", "taskIds": [linked_task]});
        let err = spawner.spawn(spec).await.expect_err("must fail");
        assert!(err.contains("agent exploded"));
        assert_eq!(
            rebon_tool::tasks::get_task(task_home.task_list_id(), &linked_task)
                .unwrap()
                .unwrap()
                .status,
            rebon_tool::tasks::TaskListStatus::Pending
        );

        let task_id = TaskId::new("agent-ext-fail");
        let snapshot = registry.snapshot(&task_id).expect("task snapshot");
        assert_eq!(snapshot.status, TaskStatus::Failed);
        assert!(!registry.task_turn_is_active(&task_id));
        let events = registry
            .task_live_events(&task_id, None)
            .expect("live events")
            .events;
        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(TaskLiveEventKind::Finished {
                status: TaskStatus::Failed,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_cancelled_outcome_maps_to_killed() {
        let runner = MockExternalRunner::new(vec!["claudecode"]);
        runner.push_outcome(Ok(ExternalTaskOutcome {
            final_text: String::new(),
            status: ExternalTaskStatus::Cancelled,
            tool_call_count: 0,
            acp_session_id: None,
        }));
        let (spawner, registry, _engine) = external_test_spawner(runner.clone());

        let mut spec = SubAgentSpec::new("do a thing");
        spec.model = Some("claudecode:".into());
        spec.metadata = json!({"agent_id": "agent-ext-cancel"});
        let result = spawner.spawn(spec).await.expect("cancel is an outcome");

        assert_eq!(result.status, "cancelled");
        let task_id = TaskId::new("agent-ext-cancel");
        let snapshot = registry.snapshot(&task_id).expect("task snapshot");
        assert_eq!(snapshot.status, TaskStatus::Killed);
        let events = registry
            .task_live_events(&task_id, None)
            .expect("live events")
            .events;
        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(TaskLiveEventKind::Finished {
                status: TaskStatus::Killed,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn workflow_structured_output_is_refused_loudly() {
        let runner = MockExternalRunner::new(vec!["claudecode"]);
        let (spawner, _registry, _engine) = external_test_spawner(runner.clone());

        let mut spec = SubAgentSpec::new("produce json");
        spec.model = Some("claudecode:claude-opus-5".into());
        spec.metadata = json!({
            rebon_tool::WORKFLOW_STRUCTURED_OUTPUT_SCHEMA_KEY: {"type": "object"},
        });
        let err = spawner.spawn(spec).await.expect_err("must refuse");

        assert!(err.contains("structured output"), "{err}");
        assert!(
            runner.requests().is_empty(),
            "the refusal must come before any agent contact"
        );
    }

    #[tokio::test]
    async fn without_a_runner_colon_specs_stay_local_byte_for_byte() {
        // The regression fence: a workspace with no ACP wiring must
        // treat `claudecode:claude-opus-5` exactly as before — a
        // literal model id handed to the local provider.
        let _coord = CoordModeGuard::set(false);
        let engine = Arc::new(Engine::with_builtin_tools());
        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(text_turn("local answer"));
        let client: Arc<dyn ModelClient> = mock.clone();
        let spawner =
            EngineSubAgentSpawner::new(Arc::downgrade(&engine), client).with_default_model("mock");

        let mut spec = SubAgentSpec::new("just answer");
        spec.model = Some("claudecode:claude-opus-5".into());
        let result = spawner.spawn(spec).await.expect("local spawn");

        assert_eq!(result.status, "completed");
        assert_eq!(result.model.as_deref(), Some("claudecode:claude-opus-5"));
        let captured = mock.captured_requests();
        assert_eq!(captured[0].model, "claudecode:claude-opus-5");
    }
}
