//! The `Agent` tool (aliases `AgentTool`, `Task`).
//!
//! The sub-agent domain it speaks —
//! [`SubAgentSpawner`], [`SubAgentSpec`], [`SubAgentResult`], the option
//! enums, the [`AgentRegistry`] — stayed in `rebon_tool::agent` and
//! `rebon_tool::agent_registry`, because this crate's runtime, `rebon-core`
//! and the TUI read all of it without going through this tool.

use async_trait::async_trait;
use rebon_tool::{
    agent_input_may_write, agent_registry_selection, append_workflow_agent_shared_worktree_prompt,
    ensure_session_default_team, is_session_default_team_name, set_current_team_name,
    sub_agents_enabled, AgentRegistry, CacheStrategy, ContextRequest, ContextShareMode,
    ExecutionPolicy, FileContextMode, FrozenParentContextCapsule, PolicyMode, SubAgentGitMetadata,
    SubAgentProgressSender, SubAgentSpec, SubAgentTaskKind, TeammateSpawnSpec, Tool, ToolContext,
    ToolFilter, ToolResultMode, UltraplanContext, WorkflowNesting,
    AGENT_EXTERNAL_PATH_AUTHORIZATION_KIND, AGENT_TOOL_NAME, DEFAULT_SUB_AGENT_MAX_ITERATIONS,
    DEFAULT_SUB_AGENT_TYPE,
};
use rebon_tools_core::{
    PermissionDecision, PermissionRequest, ToolError, ToolId, ToolInputSchema, ToolResult,
    ValidationOutcome,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const INVALID_INPUT_CODE: i64 = 400;
const PLAN_SUB_AGENT_TYPE: &str = "Plan";
const PLAN_MODE_PLAN_AGENT_REJECTION: &str = "The Plan agent cannot be invoked while the parent session is already in Plan Mode. Use Explore for delegated codebase research and keep synthesis, trade-offs, and the final plan in the parent agent.";

/// The declared external agent a model spec would route to, if any.
fn external_model_target(context: &ToolContext, model: Option<&str>) -> Option<String> {
    let (prefix, _) = model?.trim().split_once(':')?;
    let prefix = prefix.trim();
    if prefix.is_empty() || prefix.contains('/') {
        return None;
    }
    context.sub_agent_spawner()?.resolve_external_agent(prefix)
}

/// Tool that spawns a sub-agent via [`SubAgentSpawner`](rebon_tool::SubAgentSpawner).
///
/// The registry is the module that
/// merges built-in agents with user / project / flag / managed
/// overrides — see [`AgentRegistry`].
///
/// Three constructors:
/// - [`AgentTool::from_process_registry`] — production path. Reads the
///   registry the host published with
///   [`rebon_tool::set_agent_registry_selection`], so user / project agent
///   overrides propagate into both the description shown to the parent model
///   and the per-spawn resolution. This is what the plugin puts on the seat.
/// - [`AgentTool::new`] — built-ins only. Used by test suites that don't go
///   through the disk loader.
/// - [`AgentTool::with_registry`] / [`AgentTool::with_registry_and_options`]
///   — an explicit registry, for tests that want a scripted one.
#[derive(Debug, Clone)]
pub struct AgentTool {
    description: String,
    model_description: String,
    registry: Arc<AgentRegistry>,
    use_worktree: bool,
    /// The [`rebon_tool::AgentRegistrySelection`] generation this tool was
    /// built from, so the seat provider knows when the host has published a
    /// newer registry. `None` for an explicitly-supplied registry, which no
    /// host publication should ever replace.
    selection_generation: Option<u64>,
    /// The kernel scope to resolve the `agent-memory-prompt` seam from, when
    /// this tool was built by the plugin. Held as a scope rather than as a
    /// resolved provider so a `memory` plugin that unloads mid-session stops
    /// answering; a captured `Arc` would keep working after it left. `None`
    /// on a kernel-less host, where a sub-agent gets no agent memory.
    kernel_scope: Option<rebon_kernel::Context>,
}

impl Default for AgentTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentTool {
    /// Construct an `AgentTool` with a registry that contains only
    /// the compiled-in built-in agents; production callers should use
    /// [`Self::from_process_registry`].
    pub fn new() -> Self {
        Self::with_registry(Arc::new(AgentRegistry::builtins_only()))
    }

    /// Construct an `AgentTool` from the registry the host published.
    ///
    /// Falls back to the built-ins when no host has published one, which is
    /// exactly what [`Self::new`] gives.
    pub fn from_process_registry() -> Self {
        let selection = agent_registry_selection();
        let mut tool =
            Self::with_registry_and_options(selection.registry, selection.coordinator_use_worktree);
        tool.selection_generation = Some(selection.generation);
        tool
    }

    /// The published-registry generation this tool was built from, or `None`
    /// when its registry was supplied directly.
    pub fn selection_generation(&self) -> Option<u64> {
        self.selection_generation
    }

    pub fn with_coordinator_use_worktree(use_worktree: bool) -> Self {
        Self::with_registry_and_options(Arc::new(AgentRegistry::builtins_only()), use_worktree)
    }

    /// Construct an `AgentTool` backed by the supplied registry. The
    /// description seen by the parent model is computed once from
    /// `registry.format_lines()`, so any later mutation of the
    /// registry will not be reflected in the description until a new
    /// `AgentTool` is constructed.
    pub fn with_registry(registry: Arc<AgentRegistry>) -> Self {
        Self::with_registry_and_options(registry, false)
    }

    pub fn with_registry_and_options(registry: Arc<AgentRegistry>, use_worktree: bool) -> Self {
        let agent_lines = if use_worktree {
            registry.format_lines()
        } else {
            registry.format_lines_without_worktree_agents()
        };
        let description = format!(
            "Launch a new agent to handle complex, multi-step tasks autonomously.\n\
             \n\
             The Agent tool launches specialized agents (subprocesses) that autonomously \
             handle complex tasks. Each agent type has specific capabilities and tools \
             available to it.\n\
             \n\
             Available agent types and the tools they have access to:\n\
             {agent_lines}\n\
             \n\
             When using the Agent tool, specify a subagent_type parameter to select which \
             agent type to use, especially when a listed specialized agent matches the task. \
             If omitted, the general-purpose agent is used. Give a local agent a stable `name` \
             only when you expect to reuse its accumulated context in later calls. Do not add a \
             name merely to label a one-off search, review, verification, or implementation; omit \
             `name` for ordinary one-shot work. The first named call creates a session teammate, \
             and later calls with the same name dispatch new work to that teammate. Named \
             teammates run in the background by default and report completion automatically; set \
             `run_in_background` to false only when the Agent call itself must block for the \
             handoff. Named teammates use the execution boundary fixed by their teammate/session: \
             never combine `name` with per-call `cwd`, `allowed_roots`, `isolation`, or \
             `allowed_tools`; omit `name` when those settings are required.\n\
             \n\
             ## When NOT to use the Agent tool:\n\
             \n\
             - If you want to read a specific file path, use the Read tool or Glob tool \
             instead of the Agent tool, to find the match more quickly\n\
             - If you are searching for a specific class definition like \"class Foo\", \
             use the Grep tool instead, to find the match more quickly\n\
             - If you are searching for code within a specific file or set of 2-3 files, \
             use the Read tool instead of the Agent tool, to find the match more quickly\n\
             - Other tasks that are not related to the agent descriptions above\n\
             \n\
             ## Usage notes\n\
             \n\
             - Always include a short description (3-5 words) summarizing what the agent \
             will do\n\
             - `provider`, `model`, and `modelProfile` are optional routing overrides. Normally \
             omit them to inherit the selected agent definition and current session/provider \
             defaults; set them only when intentionally targeting exact configured identifiers. \
             Do not specify both `model` and `modelProfile`.\n\
             - Launch multiple agents concurrently whenever possible, to maximize performance; \
             to do that, use a single message with multiple tool uses\n\
             - Concurrent and background write-capable agents are automatically isolated in \
             runtime-created worktrees and their changes are merged back into the source branch \
             serially once they complete. If tasks must see each other's edits while running, \
             run them serially instead of in parallel. Named teammates cannot be concurrent \
             writers in a shared worktree; such calls are rejected — use one-shot agents for \
             parallel implementation work\n\
             - When the agent is done, it will return a single message back to you. The result \
             returned by the agent is not visible to the user. To show the user the result, you \
             should send a text message back to the user with a concise summary of the result.\n\
             - You can optionally run one-shot agents in the background using the \
             run_in_background parameter. Named teammates and external ACP agents run in the \
             background by default when this parameter is omitted. Before invoking an external \
             ACP agent, first send the user a short visible status message explaining what you \
             are delegating and that it will continue in the background. When an agent runs in \
             the background, you will be automatically notified when it completes; do NOT sleep, \
             poll, or proactively check on its progress. Continue with other work or respond to \
             the user instead.\n\
             - Foreground vs background: One-shot local agents use foreground by default; named \
             teammates and external ACP agents use background by default. Keep a one-shot Explore \
             agent in the foreground when you need its findings before proceeding. Do not force a \
             named teammate into the foreground merely to wait for its result; completion is \
             delivered automatically.\n\
             - Clearly tell the agent whether you expect it to write code or just to do research \
             (search, file reads, web fetches, etc.), since it is not aware of the user's intent\n\
             - If the agent description mentions that it should be used proactively, then you \
             should try your best to use it without the user having to ask for it first. Use \
             your judgement.\n\
             \n\
             ## Writing the prompt\n\
             \n\
             Brief the agent like a smart colleague who just walked into the room \u{2014} it hasn't \
             seen this conversation, doesn't know what you've tried, doesn't understand why this \
             task matters.\n\
             - Explain what you're trying to accomplish and why.\n\
             - Describe what you've already learned or ruled out.\n\
             - Give enough context about the surrounding problem that the agent can make judgment \
             calls rather than just following a narrow instruction.\n\
             - If you need a short response, say so (\"report in under 200 words\").\n\
             - Lookups: hand over the exact command. Investigations: hand over the question; \
             prescribed steps become dead weight when the premise is wrong.\n\
             \n\
             Never delegate understanding. Do not write \"based on your findings, fix the bug\" \
             or \"based on the research, implement it.\" Those phrases push synthesis onto the \
             agent instead of doing it yourself. Write prompts that prove you understood: \
             include file paths, line numbers, and what specifically to change.\n\
             \n\
             Example usage:\n\
             \n\
             <example>\n\
             user: \"Where do we register the Read tool's permission rules?\"\n\
             assistant: <thinking>The file isn't obvious from the name and could live in tools-core, tool, or engine — unknown scope across at least three crates. Delegate to Explore instead of running my own Glob/Grep loop.</thinking>\n\
             assistant: Uses the Agent tool to launch the Explore agent with a prompt asking which crate registers Read's permission behavior and the exact file:line where it's wired in.\n\
             <commentary>\n\
             Codebase question with unknown scope across multiple crates. Explore beats running Grep yourself — it parallelizes the search and returns a single concise answer instead of polluting the parent context with raw matches.\n\
             </commentary>\n\
             </example>\n\
             \n\
             <example>\n\
             user: \"Why does the streaming overlay flicker when a tool result is large?\"\n\
             assistant: <thinking>I have one concrete lead (streaming overlay) but the cause could be in render, message accumulation, or the publisher — scope still unclear. One cheap probe first, then delegate.</thinking>\n\
             assistant: Uses Grep to confirm \"streaming overlay\" lives in rebon-tui, then uses the Agent tool to launch Explore with: \"Trace how a tool_result block flows from rebon-core through rebon-render into rebon-tui's streaming overlay. Identify which component decides when to repaint and report file:line for each hop. Under 300 words.\"\n\
             <commentary>\n\
             One probe was enough to pin the entry point; tracing across three crates is exactly the unknown-scope flow Explore is designed for. Don't continue the Grep search yourself.\n\
             </commentary>\n\
             </example>\n\
             \n\
             <example>\n\
             user: \"Read crates/rebon-core/src/query.rs and explain the agentic loop\"\n\
             assistant: Uses the Read tool directly on the named file.\n\
             <commentary>\n\
             Specific file path is given. No exploration needed — reading via Agent would be slower and lossier than Read.\n\
             </commentary>\n\
             </example>"
        );
        let model_description = format!(
            "Launch specialized sub-agents for complex tasks. Use subagent_type for a matching role; omit for general-purpose. `provider`, `model`, and `modelProfile` are optional routing overrides. Normally omit them to inherit the selected agent definition and current session/provider defaults; set them only when intentionally targeting exact configured identifiers. Do not specify both `model` and `modelProfile`. A named local agent is a reusable session teammate: the first call creates it, and later calls with the same name dispatch follow-up work while preserving its context. Set `name` only when that accumulated context is likely to be reused; do not name one-off searches, reviews, verifications, or implementations merely to label them. Named teammates use the execution boundary fixed by their teammate/session: never combine `name` with per-call `cwd`, `allowed_roots`, `isolation`, or `allowed_tools`. Concurrent/background write agents are auto-isolated in runtime-created worktrees and merged back serially; named teammates cannot be concurrent writers. Named teammates default to background and report completion automatically; omit `run_in_background` unless the Agent call itself must block for the handoff. Omit name for a one-shot sub-agent. Prefer a relevant teammate from the current roster for context-heavy work. Use SendMessage only to supplement or correct work already in progress. One-shot local agents default to foreground; named teammates and external ACP agents default to background. Before invoking an external ACP agent, first send the user a short visible status message describing the delegation. Keep one-shot Explore agents in the foreground when their findings determine your next step; do not force named Explore teammates foreground merely to wait for them.\n\nAvailable agent types:\n{agent_lines}"
        );
        Self {
            description,
            model_description,
            registry,
            use_worktree,
            selection_generation: None,
            kernel_scope: None,
        }
    }

    /// Resolve this tool's agent memory through `scope`.
    pub fn with_kernel_scope(mut self, scope: rebon_kernel::Context) -> Self {
        self.kernel_scope = Some(scope);
        self
    }

    fn permission_scope(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> Result<AgentPermissionScope, String> {
        let parsed = parse_input(input)?;
        let requested_type = parsed
            .subagent_type
            .as_deref()
            .unwrap_or(DEFAULT_SUB_AGENT_TYPE);
        let resolved = self
            .registry
            .resolve(requested_type)
            .and_then(|definition| {
                if !self.use_worktree
                    && definition
                        .isolation
                        .as_deref()
                        .is_some_and(|mode| mode.eq_ignore_ascii_case("worktree"))
                {
                    self.registry.resolve(DEFAULT_SUB_AGENT_TYPE)
                } else {
                    Some(definition)
                }
            });
        let effective_isolation = parsed.isolation.as_deref().or_else(|| {
            if self.use_worktree {
                resolved
                    .as_ref()
                    .and_then(|definition| definition.isolation.as_deref())
            } else {
                None
            }
        });
        let may_create_worktree =
            requests_worktree_mutation(effective_isolation, parsed.sub_agent.task_kind);
        let (agent_type, tool_filter) = match resolved {
            Some(definition) => {
                let filter = parsed
                    .sub_agent
                    .tool_filter
                    .as_ref()
                    .map(|caller| caller.intersect(&definition.tool_filter))
                    .unwrap_or_else(|| definition.tool_filter.clone());
                (definition.agent_type.clone(), Some(filter))
            }
            None => (
                requested_type.to_string(),
                parsed.sub_agent.tool_filter.clone(),
            ),
        };
        Ok(AgentPermissionScope {
            agent_type,
            tool_filter,
            external_roots: external_child_roots(context, &parsed.sub_agent)?,
            may_create_worktree,
        })
    }

    /// Borrow the underlying registry.
    pub fn registry(&self) -> &Arc<AgentRegistry> {
        &self.registry
    }
}

#[derive(Debug, Clone)]
struct ParsedAgentToolInput {
    sub_agent: SubAgentSpec,
    subagent_type: Option<String>,
    subagent_type_explicit: bool,
    description: Option<String>,
    name: Option<String>,
    team_name: Option<String>,
    mode: Option<String>,
    effort: Option<String>,
    run_in_background: bool,
    run_in_background_explicit: bool,
    isolation: Option<String>,
}

#[derive(Debug)]
struct AgentPermissionScope {
    agent_type: String,
    tool_filter: Option<ToolFilter>,
    external_roots: Vec<PathBuf>,
    may_create_worktree: bool,
}

fn parent_path_resolution_base(context: &ToolContext) -> Result<PathBuf, String> {
    if let Some(cwd) = context.cwd() {
        return Ok(PathBuf::from(cwd));
    }
    std::env::current_dir().map_err(|err| {
        format!("Agent path authorization could not determine the parent working directory: {err}")
    })
}

fn parent_authorized_roots(context: &ToolContext, parent_cwd: &Path) -> Vec<PathBuf> {
    let mut roots = if context.path_scope_roots().is_empty() {
        vec![parent_cwd.to_path_buf()]
    } else {
        context.path_scope_roots().to_vec()
    };
    for directory in context.additional_working_directories() {
        let resolved =
            rebon_tool::path_scope::resolve_context_path(Path::new(directory), parent_cwd, context);
        if !roots.contains(&resolved) {
            roots.push(resolved);
        }
    }
    roots
}

fn external_child_roots(
    context: &ToolContext,
    spec: &SubAgentSpec,
) -> Result<Vec<PathBuf>, String> {
    let parent_cwd = parent_path_resolution_base(context)?;
    let parent_roots = parent_authorized_roots(context, &parent_cwd);
    let mut external = Vec::new();
    if let Some(cwd) = spec.cwd.as_deref() {
        let resolved =
            rebon_tool::path_scope::resolve_context_path(Path::new(cwd), &parent_cwd, context);
        if !parent_roots
            .iter()
            .any(|root| rebon_tool::path_scope::path_is_within_root(&resolved, root))
        {
            push_minimal_root(&mut external, resolved);
        }
    }
    for root in &spec.allowed_roots {
        let resolved = rebon_tool::path_scope::resolve_context_path(root, &parent_cwd, context);
        // The worker report directory is not the caller's to authorize —
        // see `is_worker_report_path`. Coordinators ask for it on nearly
        // every spawn because their workers are ordered to write a report
        // there, so leaving it in would put a prompt in front of every
        // delegation while changing nothing about what the worker can do.
        if rebon_tool::path_scope::is_worker_report_path(&resolved) {
            continue;
        }
        let overlaps_parent_scope = parent_roots.iter().any(|parent| {
            rebon_tool::path_scope::path_is_within_root(&resolved, parent)
                || rebon_tool::path_scope::path_is_within_root(parent, &resolved)
        });
        if !overlaps_parent_scope {
            push_minimal_root(&mut external, resolved);
        }
    }
    Ok(external)
}

fn push_minimal_root(roots: &mut Vec<PathBuf>, candidate: PathBuf) {
    if roots
        .iter()
        .any(|root| rebon_tool::path_scope::path_is_within_root(&candidate, root))
    {
        return;
    }
    roots.retain(|root| !rebon_tool::path_scope::path_is_within_root(root, &candidate));
    roots.push(candidate);
}

fn is_strict_read_only_explore(agent_type: Option<&str>, filter: Option<&ToolFilter>) -> bool {
    if !agent_type.is_some_and(|agent_type| agent_type.eq_ignore_ascii_case("Explore")) {
        return false;
    }
    filter
        .and_then(ToolFilter::allow_list)
        .is_some_and(|tools| tools.iter().all(|tool| is_explore_read_tool(tool)))
}

fn automatic_worktree_isolation_required(
    context: &ToolContext,
    parsed: &ParsedAgentToolInput,
    may_write: bool,
    external_delegation: bool,
) -> bool {
    parsed.name.is_none()
        && may_write
        && !external_delegation
        && (context.parallel_agent_write_batch()
            || parsed.run_in_background
            || context.coordinator_mode()
            || WorkflowNesting::of(context).is_within_workflow())
}

fn requests_worktree_mutation(
    isolation: Option<&str>,
    task_kind: Option<SubAgentTaskKind>,
) -> bool {
    isolation.is_some_and(|mode| mode.eq_ignore_ascii_case("worktree"))
        || task_kind == Some(SubAgentTaskKind::Implementation)
}

fn spec_requests_worktree_mutation(spec: &SubAgentSpec) -> bool {
    requests_worktree_mutation(
        spec.metadata.get("isolation").and_then(Value::as_str),
        spec.task_kind,
    )
}

fn plan_agent_is_disallowed_in_parent_mode(
    parsed: &ParsedAgentToolInput,
    context: &ToolContext,
) -> bool {
    parsed
        .subagent_type
        .as_deref()
        .is_some_and(|agent_type| agent_type.eq_ignore_ascii_case(PLAN_SUB_AGENT_TYPE))
        && context
            .permission_mode()
            .as_deref()
            .is_some_and(|mode| mode.eq_ignore_ascii_case("plan"))
}

fn is_explore_read_tool(tool: &str) -> bool {
    matches!(
        tool.to_ascii_lowercase().as_str(),
        "read" | "filereadtool" | "glob" | "globtool" | "grep" | "greptool"
    )
}

#[async_trait]
impl Tool for AgentTool {
    fn id(&self) -> ToolId {
        ToolId::new(AGENT_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["AgentTool", "Task"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::Agent
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn model_description(&self) -> &str {
        &self.model_description
    }

    fn is_enabled(&self) -> bool {
        sub_agents_enabled()
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn search_hint(&self) -> Option<&str> {
        Some("sub-agent delegate specialized agents parallel exploration planning review")
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string"
                },
                "subagent_type": {
                    "type": "string"
                },
                "provider": {
                    "type": "string",
                    "description": "Optional routing override. Normally omit to inherit the selected agent definition or current session provider. Set only to an exact configured provider ID; execution labels such as `local` are not provider shorthands."
                },
                "model": {
                    "type": "string",
                    "description": "Optional routing override. Normally omit to inherit the selected agent definition, model profile, or provider default. Set only to an exact supported model identifier. Do not combine with `modelProfile`."
                },
                "modelProfile": {
                    "type": "string",
                    "description": "Optional routing override. Normally omit to inherit the selected agent definition or provider default. Set only to an exact configured model profile key for the effective provider. Do not combine with `model`."
                },
                "context": {
                    "type": "object",
                    "properties": {
                        "mode": {
                            "type": "string",
                            "enum": ["none", "plan_only", "compact", "compact_with_recent_turns", "transcript_slice"]
                        },
                        "include_recent_turns": { "type": "integer", "minimum": 0 },
                        "include_tool_results": {
                            "type": "string",
                            "enum": ["none", "facts", "summary"]
                        },
                        "include_files": {
                            "type": "string",
                            "enum": ["none", "references", "contents"]
                        },
                        "instructions": { "type": "string" }
                    },
                    "additionalProperties": false
                },
                "cacheStrategy": {
                    "type": "string",
                    "enum": ["auto", "fresh", "stable_context_capsule", "provider_native", "provider_native_continuation", "no_cache"]
                },
                "reasoningEffort": {
                    "type": "string",
                    "enum": ["max", "xhigh", "high", "medium", "low"]
                },
                "description": {
                    "type": "string"
                },
                "system": {
                    "type": "string"
                },
                "name": {
                    "type": "string",
                    "description": "Stable teammate identity for this session. Use it only when the teammate's accumulated context is likely to be reused; do not name one-off searches, reviews, verifications, or implementations merely to label them. Reusing the same name dispatches the new prompt to the existing teammate. Named teammates run in the background by default and use the execution boundary fixed by their teammate/session, so `name` cannot be combined with per-call `cwd`, `allowed_roots`, `isolation`, or `allowed_tools`. Omit `name` for ordinary one-shot work."
                },
                "team_name": {
                    "type": "string",
                    "description": "Optional team for a named reusable teammate. Omit together with `name` for a one-shot sub-agent."
                },
                "mode": {
                    "type": "string"
                },
                "allowed_tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Per-call execution boundary for one-shot sub-agents; cannot be combined with `name`."
                },
                "task_kind": {
                    "type": "string",
                    "enum": ["research", "implementation", "verification", "other"]
                },
                "coordinator_task_kind": {
                    "type": "string",
                    "enum": ["research", "implementation", "verification", "other"]
                },
                "metadata": {
                    "type": "object"
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "One-shot local agents default to foreground; named teammates and external ACP agents default to background. Keep one-shot Explore agents in the foreground when their findings determine your next step. Omit this field for named teammates unless the Agent call itself must block for the handoff."
                },
                "isolation": {
                    "type": "string",
                    "description": "Per-call execution boundary for one-shot sub-agents; cannot be combined with `name`."
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory for a one-shot sub-agent; cannot be combined with `name`. External non-read-only access requires approval."
                },
                "allowed_roots": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional child roots for a one-shot sub-agent; cannot be combined with `name`. External non-read-only access requires approval."
                }
            },
            "required": ["prompt"],
            "additionalProperties": false
        })
    }

    fn needs_permission(&self, input: &Value) -> bool {
        input.get("cwd").is_some()
            || input
                .get("allowed_roots")
                .and_then(Value::as_array)
                .is_some_and(|roots| !roots.is_empty())
    }

    async fn validate_input(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        match parse_input(input) {
            Ok(parsed) if plan_agent_is_disallowed_in_parent_mode(&parsed, context) => Ok(
                ValidationOutcome::invalid(PLAN_MODE_PLAN_AGENT_REJECTION, INVALID_INPUT_CODE),
            ),
            Ok(_) => Ok(ValidationOutcome::valid()),
            Err(message) => Ok(ValidationOutcome::invalid(message, INVALID_INPUT_CODE)),
        }
    }

    async fn check_permissions(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        let scope =
            self.permission_scope(input, context)
                .map_err(|reason| ToolError::InvalidInput {
                    tool: self.id(),
                    reason,
                    error_code: Some(INVALID_INPUT_CODE),
                })?;
        if scope.external_roots.is_empty()
            || (!scope.may_create_worktree
                && is_strict_read_only_explore(
                    Some(scope.agent_type.as_str()),
                    scope.tool_filter.as_ref(),
                ))
        {
            return Ok(PermissionDecision::allow(input.clone()));
        }

        let paths = scope
            .external_roots
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        let path_list = paths
            .iter()
            .map(|path| format!("- {path}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tools = scope
            .tool_filter
            .as_ref()
            .map(ToolFilter::describe_allowed)
            .unwrap_or_else(|| "all available tools".to_string());
        let worktree_notice = if scope.may_create_worktree {
            "\n\nWorktree isolation may create a local Git branch, commit the agent's changes, and merge them back into the source branch when the source worktree is still clean and unchanged."
        } else {
            ""
        };
        Ok(PermissionDecision::ask(
            PermissionRequest::new(
                "Authorize agent directory",
                format!(
                    "{} wants to run outside the current authorized roots:\n{}\n\nAvailable tools: {}{}",
                    scope.agent_type, path_list, tools, worktree_notice
                ),
            )
            // `allow_always` is scoped to these roots by the rule the
            // frontend stores (`Agent(<root>/**)`), not to the Agent
            // tool as a whole. Without it a coordinator that delegates
            // dozens of workers into the same authorized directory has
            // to be re-approved for every single spawn.
            .with_options(["allow_once", "allow_always", "reject_once"])
            .with_metadata(json!({
                "kind": AGENT_EXTERNAL_PATH_AUTHORIZATION_KIND,
                "authorized_path_roots": paths,
                "agent_type": scope.agent_type,
            })),
            Some(input.clone()),
        ))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let mut parsed = parse_input(&input).map_err(|reason| ToolError::InvalidInput {
            tool: self.id(),
            reason,
            error_code: Some(INVALID_INPUT_CODE),
        })?;
        parsed.sub_agent.workflow_nesting_depth = context.workflow_nesting_depth();
        parsed.sub_agent.permission_prompts_unavailable = context.permission_prompts_unavailable();
        if plan_agent_is_disallowed_in_parent_mode(&parsed, context) {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: PLAN_MODE_PLAN_AGENT_REJECTION.into(),
                error_code: Some(INVALID_INPUT_CODE),
            });
        }
        if parsed.subagent_type.is_none() {
            parsed.subagent_type = Some(DEFAULT_SUB_AGENT_TYPE.to_string());
        }

        // ── Resolve agent definition ───────────────────────────────
        // When `subagent_type` matches a registry entry — built-in or
        // user/project/flag/managed override — overlay its system
        // prompt and tool filter onto the spec. Explicit caller
        // values (`system`, `allowed_tools`) take precedence.
        //
        // The worker tool registry includes built-in definitions plus
        // user/project/flag/managed overrides, so a `~/.rebon/agents/
        // <type>.md` file shadows the compiled-in definition with
        // matching `agent_type`.
        let mut resolved_agent_type = parsed.subagent_type.clone();
        self.resolve_agent_definition(&mut parsed, context, &mut resolved_agent_type)?;

        if !parsed.run_in_background_explicit
            && external_model_target(context, parsed.sub_agent.model.as_deref()).is_some()
        {
            parsed.run_in_background = true;
            parsed.sub_agent.run_in_background = true;
        }
        if parsed.name.is_some() && !parsed.run_in_background_explicit {
            parsed.run_in_background = true;
            parsed.sub_agent.run_in_background = true;
        }

        let external_delegation =
            external_model_target(context, parsed.sub_agent.model.as_deref()).is_some();
        let may_write = agent_input_may_write(&input);
        if context.parallel_agent_write_batch() && may_write && parsed.name.is_some() {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: "named teammates cannot be concurrent writers in a shared worktree; use one-shot Agent calls with worktree isolation or run the writes serially".into(),
                error_code: Some(INVALID_INPUT_CODE),
            });
        }
        if automatic_worktree_isolation_required(context, &parsed, may_write, external_delegation) {
            parsed
                .isolation
                .get_or_insert_with(|| "worktree".to_string());
        }

        self.stamp_agent_spec_metadata(
            &mut parsed,
            context,
            &resolved_agent_type,
            may_write,
            external_delegation,
        );

        let current_ultraplan_state = context.load_ultraplan_run_state().ok().flatten();
        if let (Some(name), Some(external)) = (
            parsed.name.as_deref(),
            external_model_target(context, parsed.sub_agent.model.as_deref()),
        ) {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: format!(
                    "agent `{name}` would run on the external `{external}` agent, which cannot join a team as a teammate yet; drop `name`/`team_name` to run it as a plain sub-agent"
                ),
                error_code: Some(INVALID_INPUT_CODE),
            });
        }
        if context
            .effective_ultraplan_context()
            .is_some_and(|ultraplan| ultraplan.local_only)
            && (parsed.name.is_some() || parsed.team_name.is_some())
        {
            return Err(ToolError::PermissionDenied {
                tool: self.id(),
                reason: "ultraplan local-only policy forbids Agent teammate/team spawning; use local one-shot Agent workers without name or team_name".into(),
            });
        }
        let resolved_team_name = self.resolve_team_name(&parsed, context)?;

        if let Some(ultraplan) = context.effective_ultraplan_context() {
            if let Some(policy) = context.execution_policy() {
                let mut policy = policy.clone();
                if let Some(policy_context) = policy.ultraplan.as_mut() {
                    *policy_context = ultraplan.clone();
                }
                enrich_ultraplan_local_agent_spec(&mut parsed.sub_agent, &policy, &ultraplan);
            }
        }

        if let Some(output) = self
            .spawn_named_teammate(&mut parsed, context, &input, resolved_team_name)
            .await?
        {
            return Ok(output);
        }

        let spawner = context
            .sub_agent_spawner()
            .ok_or_else(|| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(
                    "AgentTool requires a SubAgentSpawner to be set on the ToolContext"
                ),
            })?
            .clone();
        let inherited_cwd = context.cwd().map(|cwd| cwd.to_string());
        parsed.sub_agent.frozen_parent_context = context.frozen_parent_context().cloned();
        parsed.sub_agent.permission_broker = context.permission_broker().cloned();
        parsed.sub_agent.capability_context = current_ultraplan_state
            .as_ref()
            .and_then(|state| state.capability_context.clone())
            .or_else(|| context.capability_context().cloned());
        parsed.sub_agent.ultraplan_run_repository = context.ultraplan_run_repository().cloned();

        // Store display fields for the output before moving the spec.
        let agent_type = parsed.subagent_type.clone();
        let display_name = parsed
            .sub_agent
            .metadata
            .get("display_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        let agent_type_label = agent_type.as_deref().unwrap_or(DEFAULT_SUB_AGENT_TYPE);
        tracing::info!(
            agent_type = agent_type_label,
            background = parsed.run_in_background,
            description = parsed.description.as_deref().unwrap_or(""),
            "Agent tool call starting"
        );

        // Coordinator mode always uses the async/background interaction model:
        // the coordinator receives an immediate launch result, then a
        // task-notification when the worker reaches a terminal state.
        if context.coordinator_mode() {
            parsed.run_in_background = true;
            parsed.sub_agent.run_in_background = true;
        }

        if parsed.sub_agent.frozen_parent_context.is_none() {
            parsed.sub_agent.frozen_parent_context = build_frozen_parent_context_from_metadata(
                parsed.sub_agent.context.as_ref(),
                &parsed.sub_agent.metadata,
            );
        }

        // Background mode: return immediately with an agent ID.
        // The coordinator spawner owns authoritative worktree lifecycle
        // for background workers when that mode is enabled.
        if parsed.run_in_background {
            return self
                .spawn_background_agent(
                    parsed,
                    context,
                    &spawner,
                    &agent_type,
                    &display_name,
                    &inherited_cwd,
                )
                .await;
        }

        // Compatibility fallback for non-coordinator foreground calls:
        // the coordinator spawner now owns authoritative worktree lifecycle
        // so background workers and validation share one path.
        let mut worktree_info: Option<rebon_tool::worktree::AgentWorktreeInfo> = None;
        let mut worktree_warning: Option<String> = None;
        self.prepare_foreground_worktree(
            &mut parsed,
            context,
            &spawner,
            &mut worktree_info,
            &mut worktree_warning,
        )
        .await?;
        if parsed.sub_agent.cwd.is_none() {
            parsed.sub_agent.cwd = inherited_cwd;
            // Same creation-chain inheritance as the background path
            // above: unchanged tree, so unchanged isolation.
            parsed.sub_agent.runtime_isolated_worktree = context.is_isolated_worktree();
        }
        let worktree_scope_context = worktree_info.as_ref().map(|info| {
            let mut directories = context.additional_working_directories().to_vec();
            directories.push(info.worktree_path.to_string_lossy().to_string());
            context
                .clone()
                .with_additional_working_directories(directories)
        });
        enforce_child_path_scope(
            self.id(),
            worktree_scope_context.as_ref().unwrap_or(context),
            &mut parsed.sub_agent,
            agent_type.as_deref(),
        )?;
        spawner
            .preflight(&mut parsed.sub_agent)
            .map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(err),
            })?;
        reserve_ultraplan_research_slot(context, &mut parsed.sub_agent).map_err(|err| {
            ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(err),
            }
        })?;
        spawner
            .preflight(&mut parsed.sub_agent)
            .map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(err),
            })?;

        let mut result = spawner
            .spawn_with_progress(parsed.sub_agent, Some(SubAgentProgressSender::new(context)))
            .await
            .map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(err),
            })?;

        if result.status == "async_launched" {
            let agent_id = result.agent_id.unwrap_or_else(|| "".to_string());
            return Ok(json!({
                "status": "async_launched",
                "agent_id": agent_id,
                "agentId": agent_id,
                "task_id": agent_id,
                "taskId": agent_id,
                "agent_type": agent_type,
                "agentType": agent_type,
                "display_name": display_name,
                "displayName": display_name,
                "description": parsed.description,
                "tool_call_count": 0,
                "toolCallCount": 0,
            }));
        }

        let integration_notice =
            self.agent_worktree_integration_notice(&mut result, worktree_info, agent_type_label);

        let output =
            self.finish_agent_output(result, agent_type, worktree_warning, integration_notice);
        Ok(output)
    }
}

fn enforce_child_path_scope(
    tool: ToolId,
    context: &ToolContext,
    spec: &mut SubAgentSpec,
    agent_type: Option<&str>,
) -> ToolResult<()> {
    let parent_cwd =
        parent_path_resolution_base(context).map_err(|reason| ToolError::InvalidInput {
            tool: tool.clone(),
            reason,
            error_code: Some(INVALID_INPUT_CODE),
        })?;
    let parent_roots = parent_authorized_roots(context, &parent_cwd);
    let allow_external_roots = is_strict_read_only_explore(agent_type, spec.tool_filter.as_ref())
        && !spec_requests_worktree_mutation(spec);
    let child_cwd = spec
        .cwd
        .as_deref()
        .map(Path::new)
        .map(|path| rebon_tool::path_scope::resolve_context_path(path, &parent_cwd, context));

    if let Some(child_cwd) = child_cwd.as_ref() {
        if !allow_external_roots {
            ensure_path_under_roots(tool.clone(), "cwd", child_cwd, &parent_roots)?;
        }
    }
    let mut resolved_allowed_roots = Vec::with_capacity(spec.allowed_roots.len());
    for root in &spec.allowed_roots {
        let resolved = rebon_tool::path_scope::resolve_context_path(root, &parent_cwd, context);
        // Dropped rather than validated: the runtime scopes each worker to
        // its own report file regardless of what the caller asked for, so
        // carrying the directory through would either fail this check or
        // widen the worker to every other worker's report.
        if rebon_tool::path_scope::is_worker_report_path(&resolved) {
            continue;
        }
        if allow_external_roots {
            resolved_allowed_roots.push(resolved);
            continue;
        }
        if parent_roots
            .iter()
            .any(|parent| rebon_tool::path_scope::path_is_within_root(&resolved, parent))
        {
            resolved_allowed_roots.push(resolved);
            continue;
        }

        let inherited_roots = parent_roots
            .iter()
            .filter(|parent| rebon_tool::path_scope::path_is_within_root(parent, &resolved))
            .cloned()
            .collect::<Vec<_>>();
        if inherited_roots.is_empty() {
            ensure_path_under_roots(tool.clone(), "allowed_roots", &resolved, &parent_roots)?;
        }
        for inherited_root in inherited_roots {
            if !resolved_allowed_roots.contains(&inherited_root) {
                resolved_allowed_roots.push(inherited_root);
            }
        }
    }
    if spec.allowed_roots.is_empty() {
        spec.allowed_roots = child_cwd
            .map(|cwd| vec![cwd])
            .unwrap_or_else(|| parent_roots.clone());
    } else if !resolved_allowed_roots.is_empty() {
        spec.allowed_roots = resolved_allowed_roots;
    }
    Ok(())
}

fn ensure_path_under_roots(
    tool: ToolId,
    field: &str,
    path: &Path,
    roots: &[PathBuf],
) -> ToolResult<()> {
    if roots
        .iter()
        .any(|root| rebon_tool::path_scope::path_is_within_root(path, root))
    {
        return Ok(());
    }
    Err(ToolError::InvalidInput {
        tool,
        reason: format!(
            "Agent `{field}` must stay within parent authorized roots {}; `{}` is outside that scope.",
            rebon_tool::path_scope::format_roots(roots),
            path.display()
        ),
        error_code: Some(INVALID_INPUT_CODE),
    })
}

fn metadata_object_mut(metadata: &mut Value) -> &mut serde_json::Map<String, Value> {
    if !metadata.is_object() {
        *metadata = json!({});
    }
    metadata
        .as_object_mut()
        .expect("metadata was normalized to an object")
}

fn reserve_ultraplan_research_slot(
    context: &ToolContext,
    spec: &mut SubAgentSpec,
) -> Result<(), String> {
    let Some(active_context) = context.effective_ultraplan_context() else {
        return Ok(());
    };
    let Some(repository) = context.ultraplan_run_repository() else {
        return Ok(());
    };
    for attempt in 0..=1 {
        let mut state = repository.load_current().map_err(|err| err.to_string())?;
        if state.run_id != active_context.run_id {
            return Err(format!(
                "ultraplan run mismatch while reserving research budget: expected `{}`, got `{}`",
                active_context.run_id, state.run_id
            ));
        }
        let expected_revision = state.state_revision;
        state
            .consume_research_agent()
            .map_err(|err| err.to_string())?;
        match repository.compare_and_swap(expected_revision, &state) {
            Ok(()) => {
                if let Some(policy_context) = spec
                    .execution_policy
                    .as_mut()
                    .and_then(|policy| policy.ultraplan.as_mut())
                {
                    *policy_context = policy_context.clone().with_run_head(&state.head());
                }
                spec.capability_context = state.capability_context.clone();
                let metadata = metadata_object_mut(&mut spec.metadata);
                metadata.insert("ultraplan_budget_pre_reserved".into(), Value::Bool(true));
                metadata.insert(
                    "ultraplan_ledger_revision".into(),
                    Value::Number(state.ledger_revision.into()),
                );
                metadata.insert(
                    "ultraplan_requirements_hash".into(),
                    Value::String(state.requirements_hash.clone()),
                );
                if let Some(capability) = state.capability_context.as_ref() {
                    metadata.insert(
                        "ultraplan_capability_hash".into(),
                        Value::String(capability.capability_hash.clone()),
                    );
                }
                return Ok(());
            }
            Err(rebon_tool::UltraplanRepositoryError::StaleRevision { .. }) if attempt == 0 => {
                continue;
            }
            Err(err) => return Err(err.to_string()),
        }
    }
    Err("ultraplan research budget remained stale after one reload".into())
}

fn enrich_ultraplan_local_agent_spec(
    spec: &mut SubAgentSpec,
    policy: &ExecutionPolicy,
    context: &UltraplanContext,
) {
    let mut child_policy = policy.clone();
    let allowed_promotions = child_policy.ultraplan.as_mut().map(|ultraplan| {
        ultraplan
            .allowed_tools
            .retain(|tool| matches!(tool.as_str(), "Read" | "Glob" | "Grep" | "ToolSearch"));
        for denied in [
            "Agent",
            "AskUserQuestion",
            "ExitPlanMode",
            "PlanLedger",
            "Workflow",
            "RunWorkflow",
        ] {
            if !ultraplan.denied_tools.iter().any(|tool| tool == denied) {
                ultraplan.denied_tools.push(denied.into());
            }
        }
        ultraplan.allowed_tools.clone()
    });
    if let Some(allowed_promotions) = allowed_promotions {
        child_policy
            .eager_promotions
            .retain(|tool| allowed_promotions.iter().any(|allowed| allowed == tool));
    }
    spec.execution_policy = Some(child_policy);
    let metadata = metadata_object_mut(&mut spec.metadata);
    metadata.insert("ultraplan_id".into(), Value::String(context.run_id.clone()));
    metadata.insert(
        "ultraplan_phase".into(),
        Value::String(context.phase.clone()),
    );
    metadata.insert(
        "ultraplan_policy_mode".into(),
        Value::String(match context.mode {
            PolicyMode::Observe => "observe".into(),
            PolicyMode::Enforce => "enforce".into(),
        }),
    );
    metadata.remove("ultraplan_budget_pre_reserved");
    metadata.insert("ultraplan_role".into(), Value::String("researcher".into()));
}

fn build_frozen_parent_context_from_metadata(
    context: Option<&ContextRequest>,
    metadata: &Value,
) -> Option<FrozenParentContextCapsule> {
    let context = context?;
    if matches!(context.mode, ContextShareMode::None) {
        return None;
    }
    let mut body = String::new();
    body.push_str("handoff_model: frozen_parent_context_capsule\n");
    body.push_str(&format!("context_mode: {}\n", context.mode.as_str()));
    body.push_str(&format!(
        "tool_results: {}\n",
        context.include_tool_results.as_str()
    ));
    body.push_str(&format!(
        "file_context: {}\n",
        context.include_files.as_str()
    ));
    if let Some(instructions) = context.instructions.as_deref() {
        body.push_str("instructions:\n");
        body.push_str(instructions.trim());
        body.push('\n');
    }
    if let Some(description) = metadata.get("description").and_then(Value::as_str) {
        body.push_str("parent_task_description:\n");
        body.push_str(description.trim());
        body.push('\n');
    }
    body.push_str("policy: Observations below are parent-context facts only. Do not replay historical tool calls, tool results, or side effects. Inspect current files before editing.\n");
    let hash = stable_hash_str(&body);
    let text =
        format!("<ContextCapsule id=\"stable:{hash}\" version=\"1\">\n{body}</ContextCapsule>");
    Some(FrozenParentContextCapsule {
        token_estimate: estimate_text_tokens(&text),
        text: Arc::<str>::from(text),
        hash,
    })
}

fn stable_hash_str(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn estimate_text_tokens(value: &str) -> u32 {
    let mut ascii = 0usize;
    let mut non_ascii = 0usize;
    for ch in value.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii
        .div_ceil(4)
        .saturating_add(non_ascii)
        .min(u32::MAX as usize) as u32
}

fn metadata_effort(metadata: &Value) -> Option<String> {
    for key in ["reasoning_effort", "reasoningEffort", "effort", "variant"] {
        if let Some(value) = metadata
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(value.to_string());
        }
    }
    None
}

/// Pick a filesystem-safe slug for this agent's worktree. Prefers
/// a caller-supplied `agent_id` (in metadata) so the path is stable
/// across resumes; falls back to a timestamp suffix when missing.
///
/// Agent ids are usually already `agent-…`, so the conventional prefix
/// is stripped before re-applying it — otherwise the slug came out as
/// `agent-agent-<2 chars>`, leaving only 256 distinct values and letting
/// concurrent agents silently share a worktree via the `--force` reuse
/// path in `create_authorized_agent_worktree`.
fn build_worktree_slug(parsed: &ParsedAgentToolInput) -> String {
    let base = parsed
        .sub_agent
        .metadata
        .get("agent_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            format!(
                "{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    & 0xFFFFFFFF
            )
        });
    let base = base.strip_prefix("agent-").unwrap_or(&base);
    // Sanitise to the slug alphabet the worktree module accepts.
    let mut slug = String::with_capacity(24);
    slug.push_str("agent-");
    for c in base.chars().take(16) {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            slug.push(c);
        } else {
            slug.push('_');
        }
    }
    slug
}

/// If `memory_scope` is `Some(...)` and parses, load the per-agent
/// memory prompt for `agent_type` and concatenate it to
/// `current_system`. Returns the final system prompt (or `None`
/// when neither memory nor an existing system prompt is present).
/// Memory-enabled agents append the loaded per-agent memory prompt
/// after the existing system prompt, separated by a blank line.
fn inject_agent_memory_raw(
    kernel_scope: Option<&rebon_kernel::Context>,
    agent_type: &str,
    memory_scope: Option<&str>,
    current_system: Option<String>,
    cwd: &std::path::Path,
) -> Option<String> {
    let Some(scope_raw) = memory_scope else {
        return current_system;
    };
    // Who stores agent memory is the `memory` plugin's business, and it may
    // be switched off. No provider, and a scope no provider recognises, are
    // the same answer: this sub-agent gets no memory.
    let memory_prompt = kernel_scope.and_then(|scope| {
        rebon_instructions::agent_documents::agent_memory_prompt(scope, agent_type, scope_raw, cwd)
    });
    let Some(memory_prompt) = memory_prompt else {
        tracing::warn!(
            agent = %agent_type,
            scope = %scope_raw,
            "no agent memory available for this scope; skipping memory injection"
        );
        return current_system;
    };
    Some(match current_system {
        Some(existing) if !existing.trim().is_empty() => {
            format!("{existing}\n\n{memory_prompt}")
        }
        _ => memory_prompt,
    })
}

fn parse_input(input: &Value) -> Result<ParsedAgentToolInput, String> {
    let object = input
        .as_object()
        .ok_or_else(|| "AgentTool input must be a JSON object".to_string())?;

    let prompt = object
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "`prompt` is required".to_string())?
        .trim()
        .to_string();
    if prompt.is_empty() {
        return Err("`prompt` must not be empty".to_string());
    }

    let subagent_type_explicit = object.contains_key("subagent_type");
    let subagent_type = object
        .get("subagent_type")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let provider = object
        .get("provider")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let model = object
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let model_profile = object
        .get("modelProfile")
        .or_else(|| object.get("model_profile"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let description = object
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let system = object
        .get("system")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let name = object
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let team_name = object
        .get("team_name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mode = object
        .get("mode")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Convert the JSON `allowed_tools` array into a `ToolFilter`.
    let tool_filter = match object.get("allowed_tools") {
        Some(Value::Array(items)) => {
            let names: Result<Vec<String>, String> = items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(|s| s.to_string())
                        .ok_or_else(|| "`allowed_tools` entries must be strings".to_string())
                })
                .collect();
            Some(ToolFilter::allow_only(names?))
        }
        Some(_) => return Err("`allowed_tools` must be an array of strings".to_string()),
        None => None,
    };

    let max_iterations = match object.get("max_iterations") {
        Some(Value::Number(n)) => n
            .as_u64()
            .ok_or_else(|| "`max_iterations` must be a non-negative integer".to_string())?
            as usize,
        Some(_) => return Err("`max_iterations` must be an integer".to_string()),
        None => DEFAULT_SUB_AGENT_MAX_ITERATIONS,
    };

    let (run_in_background, run_in_background_explicit) = match object.get("run_in_background") {
        Some(Value::Bool(b)) => (*b, true),
        Some(_) => return Err("`run_in_background` must be a boolean".to_string()),
        None => (false, false),
    };

    let isolation = object
        .get("isolation")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let cwd = object
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let allowed_roots = parse_allowed_roots(object)?;

    let context_request = parse_context_request(object)?;
    let cache_strategy = parse_cache_strategy(object)?;

    let mut metadata = object.get("metadata").cloned().unwrap_or(Value::Null);
    if metadata.is_null() {
        metadata = json!({});
    }
    if !metadata.is_object() {
        return Err("`metadata` must be an object".to_string());
    }
    let explicit_task_kind = parse_task_kind_field(object, "task_kind")?
        .or(parse_task_kind_field(object, "coordinator_task_kind")?);
    if let Some(obj) = metadata.as_object_mut() {
        // `__`-prefixed keys are coordinator-internal (scratchpad root, runtime
        // git, structured-output schema). Model input must not set them: the
        // scratchpad root doubles as the deletion-exemption root, so a caller
        // that smuggles one in widens what a sub-agent may delete unprompted.
        obj.retain(|key, _| !key.starts_with("__"));
        if let Some(desc) = description.as_ref() {
            obj.entry("description")
                .or_insert_with(|| Value::String(desc.clone()));
        }
        if let Some(name) = name.as_ref() {
            obj.entry("display_name")
                .or_insert_with(|| Value::String(name.clone()));
        }
        for key in ["reasoning_effort", "reasoningEffort", "effort", "variant"] {
            if let Some(value) = object
                .get(key)
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                obj.entry("reasoning_effort".to_string())
                    .or_insert_with(|| Value::String(value.to_string()));
                break;
            }
        }
        if let Some(kind) = explicit_task_kind {
            obj.insert(
                "coordinator_task_kind".to_string(),
                Value::String(kind.as_str().to_string()),
            );
            obj.entry("task_kind".to_string())
                .or_insert_with(|| Value::String(kind.as_str().to_string()));
        }
    }
    let effort = metadata_effort(&metadata);
    let task_kind =
        Some(explicit_task_kind.unwrap_or_else(|| SubAgentTaskKind::from_metadata(&metadata)));

    Ok(ParsedAgentToolInput {
        sub_agent: SubAgentSpec {
            prompt,
            model,
            model_profile,
            provider,
            context: context_request,
            frozen_parent_context: None,
            cache_strategy,
            system,
            tool_filter,
            max_iterations,
            metadata,
            run_in_background,
            permission_prompts_unavailable: false,
            workflow_nesting_depth: 0,
            cwd,
            // Model input can never claim worktree isolation; only the
            // runtime paths that create a worktree set this.
            runtime_isolated_worktree: false,
            allowed_roots,
            capability_context: None,
            ultraplan_run_repository: None,
            task_list_id: None,
            execution_policy: None,
            task_kind,
            permission_broker: None,
        },
        subagent_type,
        subagent_type_explicit,
        description,
        name,
        team_name,
        mode,
        effort,
        run_in_background,
        run_in_background_explicit,
        isolation,
    })
}

fn parse_allowed_roots(object: &serde_json::Map<String, Value>) -> Result<Vec<PathBuf>, String> {
    let value = object
        .get("allowed_roots")
        .or_else(|| object.get("allowedRoots"));
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from)
                    .ok_or_else(|| "`allowed_roots` entries must be non-empty strings".to_string())
            })
            .collect(),
        Some(_) => Err("`allowed_roots` must be an array of strings".to_string()),
        None => Ok(Vec::new()),
    }
}

fn parse_cache_strategy(
    object: &serde_json::Map<String, Value>,
) -> Result<Option<CacheStrategy>, String> {
    let value = object
        .get("cacheStrategy")
        .or_else(|| object.get("cache_strategy"));
    match value {
        Some(Value::String(raw)) => CacheStrategy::parse(raw)
            .ok_or_else(|| {
                "`cacheStrategy` must be one of auto, fresh, stable_context_capsule, \
                 provider_native, provider_native_continuation, or no_cache"
                    .to_string()
            })
            .map(Some),
        Some(_) => Err("`cacheStrategy` must be a string".to_string()),
        None => Ok(None),
    }
}

fn parse_context_request(
    object: &serde_json::Map<String, Value>,
) -> Result<Option<ContextRequest>, String> {
    let Some(value) = object.get("context") else {
        return Ok(None);
    };
    let obj = value
        .as_object()
        .ok_or_else(|| "`context` must be an object".to_string())?;

    let mode = match obj.get("mode") {
        Some(Value::String(raw)) => ContextShareMode::parse(raw).ok_or_else(|| {
            "`context.mode` must be one of none, plan_only, compact, \
             compact_with_recent_turns, or transcript_slice"
                .to_string()
        })?,
        Some(_) => return Err("`context.mode` must be a string".to_string()),
        None => ContextShareMode::Compact,
    };
    if matches!(mode, ContextShareMode::TranscriptSlice) {
        return Err("`context.mode=transcript_slice` is unsupported in this MVP".to_string());
    }

    let include_recent_turns = match obj
        .get("include_recent_turns")
        .or_else(|| obj.get("includeRecentTurns"))
    {
        Some(Value::Number(n)) => Some(n.as_u64().ok_or_else(|| {
            "`context.include_recent_turns` must be a non-negative integer".to_string()
        })? as usize),
        Some(_) => return Err("`context.include_recent_turns` must be an integer".to_string()),
        None => None,
    };

    let include_tool_results = match obj
        .get("include_tool_results")
        .or_else(|| obj.get("includeToolResults"))
    {
        Some(Value::String(raw)) => ToolResultMode::parse(raw).ok_or_else(|| {
            "`context.include_tool_results` must be one of none, facts, or summary".to_string()
        })?,
        Some(_) => return Err("`context.include_tool_results` must be a string".to_string()),
        None => ToolResultMode::Facts,
    };

    let include_files = match obj.get("include_files").or_else(|| obj.get("includeFiles")) {
        Some(Value::String(raw)) => FileContextMode::parse(raw).ok_or_else(|| {
            "`context.include_files` must be one of none, references, or contents".to_string()
        })?,
        Some(_) => return Err("`context.include_files` must be a string".to_string()),
        None => FileContextMode::References,
    };

    let instructions = obj
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    Ok(Some(ContextRequest {
        mode,
        include_recent_turns,
        include_tool_results,
        include_files,
        instructions,
    }))
}

fn parse_task_kind_field(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<SubAgentTaskKind>, String> {
    match object.get(key) {
        Some(Value::String(raw)) => SubAgentTaskKind::parse(raw)
            .ok_or_else(|| {
                format!("`{key}` must be one of research, implementation, verification, or other")
            })
            .map(Some),
        Some(_) => Err(format!("`{key}` must be a string")),
        None => Ok(None),
    }
}

impl AgentTool {
    /// Fold a finished agent's worktree back into the parent tree.
    ///
    /// A deferred integration is not a failure: the notice travels back with
    /// the result so the caller relays the worktree location rather than
    /// redoing the work in the shared tree.
    fn agent_worktree_integration_notice(
        &self,
        result: &mut rebon_tool::SubAgentResult,
        worktree_info: Option<rebon_tool::worktree::AgentWorktreeInfo>,
        agent_type_label: &str,
    ) -> Option<String> {
        let mut integration_notice: Option<String> = None;
        if let Some(info) = worktree_info {
            let mut git = SubAgentGitMetadata::from_worktree_info(&info);
            if result.status == "completed" {
                let integration = rebon_tool::worktree::finalize_agent_worktree(
                    &info,
                    &format!("agent: integrate {agent_type_label} changes"),
                );
                git.apply_integration_result(&integration);
                // A damaged worktree is not a clean integration either:
                // nothing merged, and the directory is unusable. Only a
                // removed worktree means "integrated and cleaned up".
                if integration.preserves_worktree() || integration.worktree_damaged() {
                    // Deferred integration is not agent failure: the task
                    // finished and the changes are intact on the preserved
                    // branch. Keep `completed` and surface the state loudly
                    // so the caller relays it instead of redoing the task.
                    git.validation_status = Some("deferred".to_string());
                    git.validation_error = None;
                    integration_notice = Some(
                        integration.deferred_notice(&info.worktree_path, &info.worktree_branch),
                    );
                } else {
                    git.validation_status = Some("passed".to_string());
                }
            } else {
                git.integration_status = Some("preserved_not_completed".to_string());
                git.integration_error = result.error.clone();
                git.worktree_preserved = Some(true);
            }
            if result.git.is_none() {
                result.git = Some(git);
            }
        }
        integration_notice
    }

    /// Assemble the tool's JSON result from a finished sub-agent run.
    ///
    /// `result` arrives whole because the display fields are taken out of it
    /// here, immediately before the payload that carries them.
    fn finish_agent_output(
        &self,
        result: rebon_tool::SubAgentResult,
        agent_type: Option<String>,
        worktree_warning: Option<String>,
        integration_notice: Option<String>,
    ) -> Value {
        let result_agent_id = result.agent_id;
        let result_provider = result.provider;
        let result_model = result.model;
        let result_git = result.git.as_ref().map(SubAgentGitMetadata::to_json);
        let result_read_file_count = result.read_file_count;
        let result_sub_agent_tool_calls = result.sub_agent_tool_calls;
        let mut output = json!({
            "final_text": result.final_text,
            "status": result.status,
            "tool_call_count": result.tool_call_count,
            "stop_reason": result.stop_reason,
            "error": result.error,
            "output_file": result.output_file,
            "duration_ms": result.duration_ms,
            "agent_id": result_agent_id,
            "agentId": result_agent_id,
            "task_id": result_agent_id,
            "taskId": result_agent_id,
            "agent_type": agent_type,
            "agentType": agent_type,
            "provider": result_provider,
            "model": result_model,
            "total_tokens": result.total_tokens,
            "usage": result.usage,
            "git": result_git,
            "worktree_path": result.git.as_ref().and_then(|g| g.worktree_path.clone()),
            "worktree_branch": result.git.as_ref().and_then(|g| g.worktree_branch.clone()),
        });
        if let Some(sub_agent_tool_calls) = result_sub_agent_tool_calls {
            if let Some(obj) = output.as_object_mut() {
                obj.insert(
                    "sub_agent_tool_calls".to_string(),
                    json!(sub_agent_tool_calls.clone()),
                );
                obj.insert("subAgentToolCalls".to_string(), json!(sub_agent_tool_calls));
            }
        }
        if let Some(read_file_count) = result_read_file_count {
            if let Some(obj) = output.as_object_mut() {
                obj.insert("read_file_count".to_string(), json!(read_file_count));
                obj.insert("readFileCount".to_string(), json!(read_file_count));
            }
        }
        if let Some(warning) = worktree_warning {
            if let Some(obj) = output.as_object_mut() {
                obj.insert("worktree_warning".to_string(), json!(warning.clone()));
                obj.insert("worktreeWarning".to_string(), json!(warning));
            }
        }
        if let Some(notice) = integration_notice {
            if let Some(obj) = output.as_object_mut() {
                obj.insert("integration_notice".to_string(), json!(notice.clone()));
                obj.insert("integrationNotice".to_string(), json!(notice));
            }
        }
        output
    }
}

impl AgentTool {
    /// Overlay a registry agent definition onto the parsed spec.
    ///
    /// When `subagent_type` matches a registry entry -- built-in or a
    /// user/project/flag/managed override -- its system prompt and tool filter
    /// are overlaid onto the spec. Explicit caller values (`system`,
    /// `allowed_tools`) take precedence, and a `~/.rebon/agents/<type>.md`
    /// file shadows the compiled-in definition with a matching `agent_type`.
    fn resolve_agent_definition(
        &self,
        parsed: &mut ParsedAgentToolInput,
        context: &ToolContext,
        resolved_agent_type: &mut Option<String>,
    ) -> ToolResult<()> {
        if let Some(agent_type) = parsed.subagent_type.clone() {
            let resolved = self.registry.resolve(&agent_type).and_then(|def| {
                if !self.use_worktree
                    && def
                        .isolation
                        .as_deref()
                        .is_some_and(|mode| mode.eq_ignore_ascii_case("worktree"))
                {
                    self.registry.resolve(DEFAULT_SUB_AGENT_TYPE)
                } else {
                    Some(def)
                }
            });
            if let Some(def) = resolved {
                if def.is_disabled() {
                    return Err(ToolError::InvalidInput {
                        tool: self.id(),
                        reason: format!("agent type `{agent_type}` is disabled"),
                        error_code: Some(INVALID_INPUT_CODE),
                    });
                }
                let mut external_delegation = false;
                if def.runtime.is_external() {
                    // Delegation needs a spawner that can actually run
                    // tasks on the agent's CLI. Without one, falling
                    // through would spawn a general-purpose local
                    // worker under this agent's name — a different
                    // agent than the caller asked for, with none of
                    // the configuration that made it worth naming.
                    let external_target = context
                        .sub_agent_spawner()
                        .and_then(|spawner| spawner.resolve_external_agent(&def.agent_type));
                    let Some(canonical) = external_target else {
                        return Err(ToolError::InvalidInput {
                            tool: self.id(),
                            reason: format!(
                                "agent type `{agent_type}` runs on the `{}` runtime and cannot \
                             be spawned as a sub-agent here: no external sub-agent runner \
                             is wired for it",
                                def.runtime.as_str()
                            ),
                            error_code: Some(INVALID_INPUT_CODE),
                        });
                    };
                    // Route the whole task to the declared agent by
                    // spelling the model slot as the external spec.
                    // A caller-supplied model (else the definition's)
                    // rides along as the best-effort hint; a caller
                    // that already spelled the full spec keeps it.
                    let hint_source = parsed.sub_agent.model.take().or_else(|| def.model.clone());
                    let hint = hint_source
                        .as_deref()
                        .map(str::trim)
                        .filter(|model| {
                            !model.is_empty()
                                && !model.eq_ignore_ascii_case("inherit")
                                && !model.eq_ignore_ascii_case("default")
                        })
                        .unwrap_or("");
                    let prefix = format!("{}:", canonical.to_ascii_lowercase());
                    let spec_string = if hint.to_ascii_lowercase().starts_with(&prefix) {
                        hint.to_string()
                    } else {
                        format!("{canonical}:{hint}")
                    };
                    parsed.sub_agent.model = Some(spec_string);
                    external_delegation = true;
                    metadata_object_mut(&mut parsed.sub_agent.metadata)
                        .insert("external_agent".into(), Value::String(canonical));
                }

                // Snapshot memory-related fields before we overlay —
                // the injection step at the end needs the agent's
                // canonical type + memory scope, not whatever the
                // caller passed in.
                let def_agent_type = def.agent_type.clone();
                let def_memory_scope = def.memory.clone();
                *resolved_agent_type = Some(def_agent_type.clone());
                parsed.subagent_type = Some(def_agent_type.clone());

                // System prompt: use caller's if provided, else def.
                if parsed.sub_agent.system.is_none() {
                    parsed.sub_agent.system = Some(def.system_prompt.clone());
                }
                // Tool filter: intersect the def's filter with any
                // caller-supplied filter so the most restrictive
                // combination wins. An externally-delegated task runs
                // on the agent's own tools — a filter cannot bind on
                // somebody else's process, so it is dropped and the
                // drop is recorded rather than silently pretended.
                if external_delegation {
                    parsed.sub_agent.tool_filter = None;
                    metadata_object_mut(&mut parsed.sub_agent.metadata)
                        .insert("external_tools_ignored".into(), Value::Bool(true));
                } else {
                    parsed.sub_agent.tool_filter =
                        Some(match parsed.sub_agent.tool_filter.take() {
                            Some(caller_filter) => caller_filter.intersect(&def.tool_filter),
                            None => def.tool_filter.clone(),
                        });
                }
                // Model: use caller's if provided, else def.
                if parsed.sub_agent.model.is_none() {
                    parsed.sub_agent.model = def.model.clone();
                }
                if parsed.sub_agent.model_profile.is_none() && !external_delegation {
                    parsed.sub_agent.model_profile = def.model_profile.clone();
                }
                if parsed.sub_agent.provider.is_none() && !external_delegation {
                    parsed.sub_agent.provider = def.provider.clone();
                }
                // Effort: use caller metadata if present, else def.
                if parsed.effort.is_none() {
                    parsed.effort = def.effort.clone();
                }
                // Background: use the definition's default unless the caller overrode it.
                if def.background && !parsed.run_in_background_explicit {
                    parsed.run_in_background = true;
                    parsed.sub_agent.run_in_background = true;
                }
                // Isolation: caller override takes precedence over the
                // def's default.
                if parsed.isolation.is_none() && self.use_worktree {
                    parsed.isolation = def.isolation.clone();
                }
                // Per-agent memory: delegate to the helper so the
                // concatenation rule can be unit-tested independently.
                let cwd = context
                    .cwd()
                    .map(std::path::PathBuf::from)
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                parsed.sub_agent.system = inject_agent_memory_raw(
                    self.kernel_scope.as_ref(),
                    &def_agent_type,
                    def_memory_scope.as_deref(),
                    parsed.sub_agent.system.take(),
                    &cwd,
                );
            }
        }
        Ok(())
    }

    /// Stamp the spec's metadata with everything downstream needs.
    ///
    /// Workflow lineage is written only for a writing, locally executed agent:
    /// a `Some(system)` fully replaces the spawner's base-prompt chain, so
    /// promoting `None` to `Some` here would strip those fallbacks.
    fn stamp_agent_spec_metadata(
        &self,
        parsed: &mut ParsedAgentToolInput,
        context: &ToolContext,
        resolved_agent_type: &Option<String>,
        may_write: bool,
        external_delegation: bool,
    ) {
        // Workflow lineage only. A `Some(system)` fully replaces the
        // spawner's base-prompt chain (caller override > coordinator worker
        // prompt > engine default), so promoting `None` to `Some` here would
        // strip those fallbacks. Read-only and externally delegated agents
        // never write this tree, so they skip the block entirely.
        if WorkflowNesting::of(context)
            .forbids_shared_worktree_writes(may_write, external_delegation)
        {
            parsed.sub_agent.system = Some(append_workflow_agent_shared_worktree_prompt(
                parsed.sub_agent.system.take(),
            ));
        }

        if let Some(ref agent_type) = resolved_agent_type {
            metadata_object_mut(&mut parsed.sub_agent.metadata)
                .insert("agent_type".into(), Value::String(agent_type.clone()));
        }

        if let Some(ref profile) = parsed.sub_agent.model_profile {
            metadata_object_mut(&mut parsed.sub_agent.metadata)
                .entry("modelProfile")
                .or_insert_with(|| Value::String(profile.clone()));
        }

        if let Some(ref provider) = parsed.sub_agent.provider {
            metadata_object_mut(&mut parsed.sub_agent.metadata)
                .entry("provider")
                .or_insert_with(|| Value::String(provider.clone()));
        }

        if let Some(ref effort) = parsed.effort {
            metadata_object_mut(&mut parsed.sub_agent.metadata)
                .entry("reasoning_effort")
                .or_insert_with(|| Value::String(effort.clone()));
        }

        // Store isolation in metadata for downstream use.
        if let Some(ref iso) = parsed.isolation {
            metadata_object_mut(&mut parsed.sub_agent.metadata)
                .insert("isolation".into(), Value::String(iso.clone()));
        }
        if parsed.sub_agent.task_kind.is_none() {
            parsed.sub_agent.task_kind =
                Some(SubAgentTaskKind::from_metadata(&parsed.sub_agent.metadata));
        }
        if let Some(kind) = parsed.sub_agent.task_kind {
            metadata_object_mut(&mut parsed.sub_agent.metadata)
                .entry("coordinator_task_kind")
                .or_insert_with(|| Value::String(kind.as_str().to_string()));
        }
        if let Some(session_id) = context
            .session_id()
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty())
        {
            metadata_object_mut(&mut parsed.sub_agent.metadata).insert(
                "parent_session_id".into(),
                Value::String(session_id.to_string()),
            );
        }
        if let Some(tool_call_id) = context
            .tool_use_id()
            .map(str::trim)
            .filter(|tool_call_id| !tool_call_id.is_empty())
        {
            metadata_object_mut(&mut parsed.sub_agent.metadata).insert(
                "parent_tool_call_id".into(),
                Value::String(tool_call_id.to_string()),
            );
        }

        if parsed.sub_agent.task_list_id.is_none() {
            parsed.sub_agent.task_list_id = Some(context.task_list_id());
        }
    }

    /// Decide which team a named agent joins.
    fn resolve_team_name(
        &self,
        parsed: &ParsedAgentToolInput,
        context: &ToolContext,
    ) -> ToolResult<Option<String>> {
        Ok(if let Some(name) = parsed.name.as_deref() {
            match parsed
                .team_name
                .clone()
                .or_else(|| context.current_team_name())
            {
                Some(team_name) => Some(team_name),
                None => {
                    let session_id = context
                        .session_id()
                        .map(str::trim)
                        .filter(|session_id| !session_id.is_empty());
                    match session_id {
                        // A default-team failure must not silently
                        // demote the named agent to a one-shot
                        // sub-agent: the caller was promised a
                        // reusable teammate.
                        Some(session_id) => {
                            Some(ensure_session_default_team(session_id).map_err(|err| {
                                ToolError::Execution {
                                    tool: self.id(),
                                    source: anyhow::anyhow!(
                                        "failed to prepare the session default team for teammate \
                                     `{name}`: {err}; drop `name` to run a one-shot sub-agent"
                                    ),
                                }
                            })?)
                        }
                        None => None,
                    }
                }
            }
        } else {
            parsed.team_name.clone()
        })
    }

    /// Spawn or message a named teammate.
    ///
    /// `Some` means the call is finished: a named teammate is addressed by
    /// name for the rest of the session, so this path returns its own result
    /// rather than falling through to the one-shot sub-agent spawn.
    async fn spawn_named_teammate(
        &self,
        parsed: &mut ParsedAgentToolInput,
        context: &ToolContext,
        input: &Value,
        resolved_team_name: Option<String>,
    ) -> ToolResult<Option<Value>> {
        if let Some(name) = parsed.name.clone() {
            if let Some(team_name) = resolved_team_name {
                let manager = context
                .team_manager()
                .ok_or_else(|| ToolError::Execution {
                    tool: self.id(),
                    source: anyhow::anyhow!(
                        "AgentTool teammate spawning requires a TeamManager to be set on the ToolContext"
                    ),
                })?
                .clone();
                let workflow_lineage = WorkflowNesting::at(parsed.sub_agent.workflow_nesting_depth)
                    .is_within_workflow();
                // A teammate does not honor per-call execution
                // boundaries (`TeammateSpawnSpec` has no slots for
                // them), so reject rather than silently ignore what
                // the caller — and possibly a permission prompt —
                // was told would apply. `system` is forwarded for
                // workflow lineage only.
                let mut unsupported: Vec<&str> =
                    ["cwd", "allowed_roots", "isolation", "allowed_tools"]
                        .into_iter()
                        .filter(|key| input.get(*key).is_some())
                        .collect();
                if !workflow_lineage && input.get("system").is_some() {
                    unsupported.push("system");
                }
                if !unsupported.is_empty() {
                    return Err(ToolError::InvalidInput {
                        tool: self.id(),
                        reason: format!(
                            "agent `{name}` is a reusable teammate, but this call also set \
                         unsupported per-call setting(s): `{}`. Named teammates use their \
                         session-inherited working directory and cannot use per-call `cwd`, \
                         `allowed_roots`, `isolation`, or `allowed_tools`; `system` is also \
                         rejected outside workflow lineage. Retry in exactly one form: keep \
                         `name` and remove every unsupported per-call setting (do not replace \
                         one with another), or omit `name`/`team_name` and keep those settings \
                         for a one-shot sub-agent",
                            unsupported.join("`/`")
                        ),
                        error_code: Some(INVALID_INPUT_CODE),
                    });
                }
                let result = manager
                    .spawn_teammate(TeammateSpawnSpec {
                        team_name,
                        name,
                        prompt: parsed.sub_agent.prompt.clone(),
                        agent_type: parsed.subagent_type.clone().or_else(|| {
                            parsed
                                .sub_agent
                                .metadata
                                .get("agent_type")
                                .and_then(|v| v.as_str())
                                .map(|v| v.to_string())
                        }),
                        model: parsed.sub_agent.model.clone(),
                        model_profile: parsed.sub_agent.model_profile.clone(),
                        provider: parsed.sub_agent.provider.clone(),
                        mode: parsed.mode.clone(),
                        effort: parsed.effort.clone(),
                        description: parsed.description.clone(),
                        system: workflow_lineage
                            .then(|| parsed.sub_agent.system.clone())
                            .flatten(),
                        cwd: context.cwd().map(str::to_string),
                        additional_working_directories: context
                            .additional_working_directories()
                            .to_vec(),
                        workflow_nesting_depth: parsed.sub_agent.workflow_nesting_depth,
                        parent_session_id: context.session_id().unwrap_or_default().to_string(),
                        agent_type_explicit: parsed.subagent_type_explicit,
                        wait_for_completion: !parsed.run_in_background,
                        permission_broker: workflow_lineage
                            .then(|| context.permission_broker().cloned())
                            .flatten(),
                        permission_prompts_unavailable: workflow_lineage,
                    })
                    .await
                    .map_err(|err| ToolError::Execution {
                        tool: self.id(),
                        source: anyhow::anyhow!(err),
                    })?;
                // Keep the process-wide environment variable only as a
                // compatibility fallback for callers that provide no session id.
                // Session-aware hosts resolve the team from ToolContext instead.
                if context.session_id().is_none()
                    && context.current_team_name().is_none()
                    && !is_session_default_team_name(&result.team_name)
                {
                    set_current_team_name(&result.team_name);
                }
                let display_team_name = if is_session_default_team_name(&result.team_name) {
                    "default".to_string()
                } else {
                    result.team_name.clone()
                };
                let mut output = json!({
                    "status": if result.reused { "teammate_dispatched" } else { "teammate_spawned" },
                    "team_name": display_team_name,
                    "teamName": display_team_name,
                    "agent_id": result.agent_id,
                    "agentId": result.agent_id,
                    "task_id": result.task_id,
                    "taskId": result.task_id,
                    "reused": result.reused,
                });
                if let Some(handoff) = result.handoff.as_ref() {
                    output["handoff"] = json!({
                        "status": handoff.status,
                        "summary": handoff.summary,
                        "error": handoff.error,
                    });
                }
                return Ok(Some(output));
            }
        }
        Ok(None)
    }

    /// Background mode: return immediately with an agent id.
    ///
    /// `Some` means the call is finished -- the worker keeps running and the
    /// caller is told where to find it.
    async fn spawn_background_agent(
        &self,
        mut parsed: ParsedAgentToolInput,
        context: &ToolContext,
        spawner: &std::sync::Arc<dyn rebon_tool::SubAgentSpawner>,
        agent_type: &Option<String>,
        display_name: &Option<String>,
        inherited_cwd: &Option<String>,
    ) -> ToolResult<Value> {
        if parsed.sub_agent.cwd.is_none() {
            parsed.sub_agent.cwd = inherited_cwd.clone();
            // The child runs in the parent's tree verbatim, so it
            // inherits the parent's isolation along the creation
            // chain. A caller-supplied `cwd` inherits nothing.
            parsed.sub_agent.runtime_isolated_worktree = context.is_isolated_worktree();
        }
        enforce_child_path_scope(
            self.id(),
            context,
            &mut parsed.sub_agent,
            agent_type.as_deref(),
        )?;
        spawner
            .preflight(&mut parsed.sub_agent)
            .map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(err),
            })?;
        reserve_ultraplan_research_slot(context, &mut parsed.sub_agent).map_err(|err| {
            ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(err),
            }
        })?;
        spawner
            .preflight(&mut parsed.sub_agent)
            .map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(err),
            })?;
        let agent_id = spawner
            .spawn_background(parsed.sub_agent)
            .await
            .map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(err),
            })?;

        Ok(json!({
            "status": "async_launched",
            "agent_id": agent_id,
            "agentId": agent_id,
            "task_id": agent_id,
            "taskId": agent_id,
            "agent_type": agent_type,
            "agentType": agent_type,
            "display_name": display_name,
            "displayName": display_name,
            "description": parsed.description,
            "tool_call_count": 0,
            "toolCallCount": 0,
        }))
    }

    /// Compatibility fallback for non-coordinator foreground calls.
    ///
    /// The coordinator spawner owns authoritative worktree lifecycle, so
    /// background workers and validation share one path; this is the path a
    /// plain foreground call still takes.
    async fn prepare_foreground_worktree(
        &self,
        parsed: &mut ParsedAgentToolInput,
        context: &ToolContext,
        spawner: &std::sync::Arc<dyn rebon_tool::SubAgentSpawner>,
        worktree_info: &mut Option<rebon_tool::worktree::AgentWorktreeInfo>,
        worktree_warning: &mut Option<String>,
    ) -> ToolResult<()> {
        if !context.coordinator_mode()
            && !spawner.manages_worktree_lifecycle()
            && parsed.isolation.as_deref() == Some("worktree")
            && parsed.sub_agent.cwd.is_none()
        {
            let slug = build_worktree_slug(parsed);
            let parent_cwd =
                parent_path_resolution_base(context).map_err(|reason| ToolError::InvalidInput {
                    tool: self.id(),
                    reason,
                    error_code: Some(INVALID_INPUT_CODE),
                })?;
            let parent_roots = parent_authorized_roots(context, &parent_cwd);
            let cwd_base = rebon_tool::worktree::authorized_worktree_base(
                context.cwd().map(Path::new),
                &parent_roots,
            )
            .map_err(|err| ToolError::InvalidInput {
                tool: self.id(),
                reason: err.to_string(),
                error_code: Some(INVALID_INPUT_CODE),
            })?;
            parsed.sub_agent.cwd = Some(cwd_base.to_string_lossy().to_string());
            match rebon_tool::worktree::create_authorized_agent_worktree(
                &cwd_base,
                &parent_roots,
                &slug,
            ) {
                Ok(info) => {
                    parsed.sub_agent.cwd = Some(info.worktree_path.to_string_lossy().to_string());
                    if parsed.sub_agent.allowed_roots.is_empty() {
                        parsed.sub_agent.allowed_roots = vec![info.worktree_path.clone()];
                    }
                    // We created this tree, so the child owns it alone.
                    parsed.sub_agent.runtime_isolated_worktree = true;
                    *worktree_info = Some(info);
                }
                Err(err) => {
                    tracing::warn!(
                        slug = %slug,
                        error = %err,
                        "agent worktree creation failed — running without isolation"
                    );
                    // Degrading to the shared tree must not be silent:
                    // tell the child it is NOT isolated and surface the
                    // reason to the caller in the tool result.
                    let notice = format!(
                        "NOTE: worktree isolation was requested but could not be created \
                     ({err}). You are running directly in the shared working tree; \
                     treat pre-existing and concurrent changes as owned work and keep \
                     your edits minimal."
                    );
                    parsed.sub_agent.system = Some(match parsed.sub_agent.system.take() {
                        Some(system) if !system.trim().is_empty() => {
                            format!("{system}\n\n{notice}")
                        }
                        _ => notice,
                    });
                    *worktree_warning = Some(format!(
                        "worktree isolation failed ({err}); agent ran in the shared \
                     working tree without isolation"
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::agent_registry::{AgentGroups, AgentRuntime, AgentSource, ResolvedAgentDef};
    use rebon_tool::tasks::test_support::TestConfigHome;
    use rebon_tool::{
        set_sub_agents_enabled, SubAgentResult, SubAgentSpawner, TeammateHandoff,
        TeammateSpawnResult, UltraplanRunRepository, WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT,
    };
    use rebon_types::{
        CapabilityContext, ExecutionPolicy, PolicyMode, UltraplanContext, UltraplanRunState,
    };
    use std::sync::Arc;
    use std::sync::Mutex;

    #[test]
    fn shared_worktree_prompt_targets_the_agent_not_the_caller() {
        assert!(WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT.contains("do not bypass"));
        assert!(WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT.contains("modified-since-read"));
        // Caller-side orchestration rules (parallel writer isolation, named
        // teammate limits) live in the tool description, not in the prompt
        // injected into the agent that is being spawned.
        assert!(!WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT.contains("Parallel implementation agents"));
        assert!(!WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT.contains("Named teammates"));
    }

    #[test]
    fn agent_tool_description_carries_concurrent_writer_guidance() {
        let tool = AgentTool::new();
        assert!(tool
            .description()
            .contains("automatically isolated in runtime-created worktrees"));
        assert!(tool
            .description()
            .contains("Named teammates cannot be concurrent writers"));
        assert!(tool
            .model_description()
            .contains("named teammates cannot be concurrent writers"));
    }

    #[test]
    fn parse_input_accepts_provider_context_and_cache_strategy() {
        let parsed = parse_input(&json!({
            "prompt": "inspect routing",
            "provider": "anthropic",
            "modelProfile": "explore",
            "context": {
                "mode": "compact",
                "includeRecentTurns": 4,
                "includeToolResults": "facts",
                "includeFiles": "references",
                "instructions": "Use the supplied plan."
            },
            "cacheStrategy": "stable_context_capsule"
        }))
        .unwrap();

        assert_eq!(parsed.sub_agent.provider.as_deref(), Some("anthropic"));
        assert_eq!(parsed.sub_agent.model_profile.as_deref(), Some("explore"));
        let context = parsed.sub_agent.context.unwrap();
        assert_eq!(context.mode, ContextShareMode::Compact);
        assert_eq!(context.include_recent_turns, Some(4));
        assert_eq!(context.include_tool_results, ToolResultMode::Facts);
        assert_eq!(context.include_files, FileContextMode::References);
        assert_eq!(
            context.instructions.as_deref(),
            Some("Use the supplied plan.")
        );
        assert_eq!(
            parsed.sub_agent.cache_strategy,
            Some(CacheStrategy::StableContextCapsule)
        );
    }

    #[test]
    fn parse_input_keeps_explore_foreground_by_default() {
        let parsed = parse_input(&json!({
            "prompt": "trace the startup path",
            "subagent_type": "Explore"
        }))
        .unwrap();

        assert!(!parsed.run_in_background);
        assert!(!parsed.run_in_background_explicit);
        assert!(!parsed.sub_agent.run_in_background);
    }

    #[test]
    fn parse_input_allows_explicit_background_explore() {
        let parsed = parse_input(&json!({
            "prompt": "trace an independent path",
            "subagent_type": "Explore",
            "run_in_background": true
        }))
        .unwrap();

        assert!(parsed.run_in_background);
        assert!(parsed.run_in_background_explicit);
        assert!(parsed.sub_agent.run_in_background);
    }

    #[test]
    fn parse_input_rejects_transcript_slice_context_mvp() {
        let err = parse_input(&json!({
            "prompt": "inspect routing",
            "context": { "mode": "transcript_slice" }
        }))
        .unwrap_err();
        assert!(err.contains("unsupported in this MVP"));
    }

    #[test]
    fn estimate_text_tokens_counts_non_ascii_more_conservatively() {
        assert_eq!(estimate_text_tokens("abcdefghijklmnop"), 4);
        assert_eq!(estimate_text_tokens("缓存命中率"), 5);
        assert_eq!(estimate_text_tokens("abcd缓存"), 3);
    }

    #[test]
    fn parse_input_accepts_allowed_roots() {
        let parsed = parse_input(&json!({
            "prompt": "inspect routing",
            "cwd": "F:/repo/crates/rebon-cli",
            "allowed_roots": ["F:/repo/crates/rebon-cli", "F:/repo/crates/rebon-tool"]
        }))
        .unwrap();

        assert_eq!(
            parsed.sub_agent.cwd.as_deref(),
            Some("F:/repo/crates/rebon-cli")
        );
        assert_eq!(
            parsed.sub_agent.allowed_roots,
            vec![
                PathBuf::from("F:/repo/crates/rebon-cli"),
                PathBuf::from("F:/repo/crates/rebon-tool")
            ]
        );
    }

    #[test]
    fn parse_input_rejects_invalid_allowed_roots() {
        let err = parse_input(&json!({
            "prompt": "inspect routing",
            "allowed_roots": "F:/repo"
        }))
        .unwrap_err();

        assert!(err.contains("`allowed_roots` must be an array of strings"));
    }

    #[test]
    fn input_schema_advertises_provider_context_and_cache_strategy() {
        let schema = AgentTool::new().input_schema();
        assert!(schema["properties"].get("provider").is_some());
        assert!(schema["properties"].get("context").is_some());
        assert!(schema["properties"].get("cacheStrategy").is_some());
        assert!(schema["properties"].get("allowed_roots").is_some());
        assert!(schema["properties"]["run_in_background"]
            .get("default")
            .is_none());
        let background_description = schema["properties"]["run_in_background"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(background_description.contains("One-shot local agents default to foreground"));
        assert!(background_description.contains("named teammates"));
        assert!(background_description.contains("external ACP agents default to background"));
        assert!(background_description.contains("one-shot Explore agents"));
    }

    #[test]
    fn input_schema_explains_model_routing_overrides() {
        let schema = AgentTool::new().input_schema();
        let properties = &schema["properties"];

        for field in ["provider", "model", "modelProfile"] {
            let description = properties[field]["description"]
                .as_str()
                .unwrap_or_default();
            assert!(description.contains("Optional routing override"));
            assert!(description.contains("Normally omit"));
        }

        let provider_description = properties["provider"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(provider_description.contains("exact configured provider ID"));
        assert!(provider_description.contains("`local`"));

        let model_description = properties["model"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(model_description.contains("exact supported model identifier"));
        assert!(model_description.contains("Do not combine with `modelProfile`"));

        let profile_description = properties["modelProfile"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(profile_description.contains("exact configured model profile key"));
        assert!(profile_description.contains("Do not combine with `model`"));
    }

    #[test]
    fn agent_descriptions_explain_model_routing_overrides() {
        let tool = AgentTool::new();
        for description in [tool.description(), tool.model_description()] {
            assert!(description.contains("optional routing overrides"));
            assert!(description.contains("Normally omit them"));
            assert!(description.contains("exact configured identifiers"));
            assert!(description.contains("Do not specify both `model` and `modelProfile`"));
        }
    }

    #[test]
    fn input_schema_explains_named_agent_execution_boundaries() {
        let schema = AgentTool::new().input_schema();
        let name_description = schema["properties"]["name"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(name_description.contains("cannot be combined"));
        assert!(name_description.contains("ordinary one-shot work"));
        assert!(name_description.contains("do not name one-off"));
        for setting in ["cwd", "allowed_roots", "isolation", "allowed_tools"] {
            assert!(name_description.contains(setting), "{name_description}");
            let description = schema["properties"][setting]["description"]
                .as_str()
                .unwrap_or_default();
            assert!(description.contains("cannot be combined with `name`"));
        }
    }

    #[test]
    fn parse_input_accepts_explicit_task_kind_field() {
        let parsed = parse_input(&json!({
            "prompt": "fix the bug",
            "task_kind": "implementation",
            "metadata": { "coordinator_task_kind": "research" }
        }))
        .unwrap();
        assert_eq!(
            parsed.sub_agent.task_kind,
            Some(SubAgentTaskKind::Implementation)
        );
        assert_eq!(
            parsed.sub_agent.metadata["coordinator_task_kind"],
            "implementation"
        );
        assert_eq!(parsed.sub_agent.metadata["task_kind"], "implementation");
    }

    #[test]
    fn parse_input_keeps_metadata_task_kind_fallback() {
        let parsed = parse_input(&json!({
            "prompt": "check the fix",
            "metadata": { "coordinator_task_kind": "verification" }
        }))
        .unwrap();
        assert_eq!(
            parsed.sub_agent.task_kind,
            Some(SubAgentTaskKind::Verification)
        );
    }

    #[test]
    fn parse_input_strips_coordinator_reserved_metadata_keys() {
        let parsed = parse_input(&json!({
            "prompt": "clean up",
            "metadata": {
                "__scratchpad_dir": "C:/",
                "__runtime_git": { "branch": "main" },
                "display_name": "cleaner"
            }
        }))
        .unwrap();
        let obj = parsed.sub_agent.metadata.as_object().unwrap();
        assert!(!obj.contains_key("__scratchpad_dir"));
        assert!(!obj.contains_key("__runtime_git"));
        assert_eq!(parsed.sub_agent.metadata["display_name"], "cleaner");
    }

    #[test]
    fn parse_input_rejects_invalid_task_kind_field() {
        let err = parse_input(&json!({
            "prompt": "do work",
            "task_kind": "writer"
        }))
        .unwrap_err();
        assert!(err.contains("`task_kind` must be one of"));
    }

    #[test]
    fn input_schema_advertises_task_kind_field() {
        let schema = AgentTool::new().input_schema();
        assert!(schema["properties"].get("task_kind").is_some());
        assert!(schema["properties"].get("coordinator_task_kind").is_some());
    }

    struct ScriptedSpawner {
        result: SubAgentResult,
        calls: Mutex<Vec<SubAgentSpec>>,
        background_calls: Mutex<Vec<SubAgentSpec>>,
    }

    #[derive(Clone)]
    struct TestRunRepository {
        state: Arc<Mutex<UltraplanRunState>>,
    }

    impl rebon_tool::UltraplanRunRepository for TestRunRepository {
        fn load_current(&self) -> Result<UltraplanRunState, rebon_tool::UltraplanRepositoryError> {
            Ok(self.state.lock().unwrap().clone())
        }

        fn load_run(
            &self,
            run_id: &str,
        ) -> Result<Option<UltraplanRunState>, rebon_tool::UltraplanRepositoryError> {
            let state = self.state.lock().unwrap();
            Ok((state.run_id == run_id).then(|| state.clone()))
        }

        fn compare_and_swap(
            &self,
            expected_revision: u64,
            state: &UltraplanRunState,
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
            state: &UltraplanRunState,
        ) -> Result<(), rebon_tool::UltraplanRepositoryError> {
            *self.state.lock().unwrap() = state.clone();
            Ok(())
        }
    }

    struct ScriptedTeamManager {
        result: TeammateSpawnResult,
        calls: Mutex<Vec<TeammateSpawnSpec>>,
    }

    #[async_trait]
    impl rebon_tool::TeamManager for ScriptedTeamManager {
        async fn spawn_teammate(
            &self,
            spec: TeammateSpawnSpec,
        ) -> Result<TeammateSpawnResult, String> {
            self.calls.lock().unwrap().push(spec);
            Ok(self.result.clone())
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
            _team_name: &str,
            _agent_name: &str,
            _plan_content: String,
        ) -> Result<String, String> {
            Ok("plan".into())
        }

        async fn delete_team(&self, _team_name: &str) -> Result<(), String> {
            Ok(())
        }
    }

    impl ScriptedSpawner {
        fn new(result: SubAgentResult) -> Self {
            Self {
                result,
                calls: Mutex::new(Vec::new()),
                background_calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SubAgentSpawner for ScriptedSpawner {
        async fn spawn(&self, spec: SubAgentSpec) -> Result<SubAgentResult, String> {
            self.calls.lock().unwrap().push(spec);
            Ok(self.result.clone())
        }

        async fn spawn_background(&self, spec: SubAgentSpec) -> Result<String, String> {
            self.background_calls.lock().unwrap().push(spec);
            Ok("background-agent-id".into())
        }
    }

    fn success_result() -> SubAgentResult {
        SubAgentResult {
            final_text: "done".into(),
            status: "completed".into(),
            tool_call_count: 2,
            stop_reason: Some("EndTurn".into()),
            error: None,
            output_file: None,
            duration_ms: Some(1000),
            agent_id: Some("test-agent-id".into()),
            agent_type: None,
            provider: Some("test-provider".into()),
            model: Some("test-model".into()),
            sub_agent_tool_calls: Some(vec![
                json!({ "tool_use_id": "read-1", "name": "Read", "input": { "file_path": "Cargo.toml" }, "ok": true }),
                json!({ "tool_use_id": "grep-1", "name": "Grep", "input": { "pattern": "Agent", "path": "crates" }, "ok": true }),
            ]),
            read_file_count: Some(1),
            total_tokens: Some(1234),
            output_tokens: Some(434),
            usage: Some(json!({ "input_tokens": 800, "output_tokens": 434 })),
            diagnostics: None,
            git: None,
        }
    }

    #[test]
    fn agent_tool_description_matches_src_routing_guidance() {
        let tool = AgentTool::new();
        let desc = tool.description();
        assert!(
            desc.contains("Launch a new agent to handle complex, multi-step tasks autonomously")
        );
        assert!(desc.contains("Available agent types"));
        assert!(desc.contains("- Explore:"));
        assert!(desc.contains("When NOT to use the Agent tool"));
        assert!(desc.contains("searching for code within a specific file or set of 2-3 files"));
        assert!(desc.contains("especially when a listed specialized agent matches the task"));
        // The Plan agent is not in the roster. Plan mode forbids it by name,
        // so the only sessions that could run it are the ones with no plan to
        // make — where offering a planning agent is a pull toward planning.
        // It still resolves and still runs when named; `agent_registry` owns
        // that half of the claim.
        assert!(!desc.contains("- Plan:"));
        assert!(!desc.contains("Do not invoke this from Plan Mode"));
        assert!(!tool.model_description().contains("- Plan:"));
        assert!(
            desc.contains("If the agent description mentions that it should be used proactively")
        );
        assert!(desc.contains("Never delegate understanding"));
        assert!(desc.contains("never combine `name` with per-call `cwd`"));
        assert!(desc.contains("Do not add a name merely to label a one-off"));
        assert!(desc.contains("Named teammates run in the background by default"));
        assert!(!desc.contains("batch-worker"));
        // Worktree-isolated agent types stay hidden without the flag, but
        // the automatic-isolation contract for concurrent writers (which
        // applies in every mode) is always documented.
        assert!(desc.contains("automatically isolated in runtime-created worktrees"));
        assert!(tool
            .model_description()
            .contains("Launch specialized sub-agents"));
        assert!(tool
            .model_description()
            .contains("never combine `name` with per-call `cwd`"));
        assert!(tool
            .model_description()
            .contains("do not name one-off searches"));
        assert!(tool.model_description().contains("- Explore:"));
        assert!(tool
            .model_description()
            .contains("Keep one-shot Explore agents in the foreground"));
        assert!(tool
            .model_description()
            .contains("named teammates and external ACP agents default to background"));
        assert!(tool
            .model_description()
            .contains("first send the user a short visible status message"));
        assert!(desc.contains("Before invoking an external ACP agent"));
        assert!(!tool.model_description().contains("Example usage"));
    }

    #[test]
    fn agent_tool_description_mentions_worktree_when_enabled() {
        let tool = AgentTool::with_coordinator_use_worktree(true);
        let desc = tool.description();
        assert!(desc.contains("batch-worker"));
        assert!(desc.contains("worktree"));
        assert!(tool.model_description().contains("batch-worker"));
        assert!(tool.model_description().contains("worktree"));
    }

    #[test]
    fn agent_tool_input_schema_does_not_advertise_max_iterations() {
        let tool = AgentTool::new();
        let schema = tool.input_schema();
        assert!(schema
            .get("properties")
            .and_then(|properties| properties.get("max_iterations"))
            .is_none());
    }

    #[tokio::test]
    async fn agent_tool_parses_input_and_calls_spawner() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let context = ToolContext::new()
            .with_session_id("session-parent")
            .with_tool_use_id("tool-agent-parent")
            .with_workflow_nesting_depth(2)
            .with_permission_prompts_unavailable(true)
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        let input = json!({
            "prompt": "do the thing",
            "allowed_tools": ["Read", "Grep"],
            "max_iterations": 5,
            "metadata": { "agent_type": "researcher" }
        });
        let validation = tool.validate_input(&input, &context).await.unwrap();
        assert!(validation.is_valid());

        let out = tool.call(input, &context).await.unwrap();
        assert_eq!(out["final_text"], "done");
        assert_eq!(out["status"], "completed");
        assert_eq!(out["tool_call_count"], 2);
        let calls = out["sub_agent_tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["name"], "Read");
        assert_eq!(out["subAgentToolCalls"].as_array().unwrap().len(), 2);

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].prompt, "do the thing");
        assert_eq!(recorded[0].metadata["agent_type"], DEFAULT_SUB_AGENT_TYPE);
        assert_eq!(recorded[0].metadata["parent_session_id"], "session-parent");
        assert_eq!(
            recorded[0].metadata["parent_tool_call_id"],
            "tool-agent-parent"
        );
        let filter = recorded[0].tool_filter.as_ref().unwrap();
        assert!(filter.allows("Read", &["FileReadTool"]));
        assert!(filter.allows("Grep", &["GrepTool"]));
        assert!(!filter.allows("Bash", &["BashTool"]));
        assert_eq!(recorded[0].max_iterations, 5);
        assert_eq!(recorded[0].workflow_nesting_depth, 2);
        assert!(recorded[0].permission_prompts_unavailable);
        // Read-only allowed_tools → no shared-worktree block, even inside
        // a workflow: the agent cannot write the tree.
        let system = recorded[0].system.as_deref().unwrap_or_default();
        assert!(system.starts_with("You are an agent for Rebon"));
        assert!(!system.contains("SHARED WORKTREE SAFETY:"));
    }

    #[tokio::test]
    async fn workflow_write_agent_gets_shared_worktree_safety() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let context = ToolContext::new()
            .with_workflow_nesting_depth(1)
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({ "prompt": "edit the parser", "run_in_background": true }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.background_calls.lock().unwrap();
        assert_eq!(recorded[0].workflow_nesting_depth, 1);
        assert_eq!(recorded[0].metadata["isolation"], "worktree");
        assert!(recorded[0]
            .system
            .as_deref()
            .unwrap_or_default()
            .contains(WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT));
    }

    #[tokio::test]
    async fn agent_tool_leaves_system_untouched_outside_workflows() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(json!({ "prompt": "edit the parser" }), &context)
            .await
            .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].workflow_nesting_depth, 0);
        let system = recorded[0].system.as_deref().unwrap_or_default();
        assert!(system.starts_with("You are an agent for Rebon"));
        assert!(!system.contains("SHARED WORKTREE SAFETY:"));
    }

    fn external_def(agent_type: &str, model: Option<&str>) -> ResolvedAgentDef {
        ResolvedAgentDef {
            agent_type: agent_type.to_string(),
            when_to_use: "when testing external delegation".to_string(),
            system_prompt: "Only ever read files.".to_string(),
            tool_filter: Default::default(),
            model: model.map(str::to_string),
            model_profile: None,
            provider: None,
            effort: None,
            background: false,
            isolation: None,
            memory: None,
            permission_mode: None,
            runtime: AgentRuntime::Acp {
                command: "claude-agent-acp".into(),
                args: Vec::new(),
            },
            source: AgentSource::BuiltIn,
            file_stem: None,
        }
    }

    fn external_registry(model: Option<&str>) -> Arc<AgentRegistry> {
        Arc::new(
            AgentRegistry::from_groups(AgentGroups {
                user: vec![external_def("claudecode", model)],
                ..AgentGroups::default()
            })
            .with_external_agents_spawnable(),
        )
    }

    /// A spawner that recognises `claudecode` as a declared agent.
    struct ExternalAwareSpawner(ScriptedSpawner);

    #[async_trait]
    impl SubAgentSpawner for ExternalAwareSpawner {
        fn resolve_external_agent(&self, prefix: &str) -> Option<String> {
            prefix
                .trim()
                .eq_ignore_ascii_case("claudecode")
                .then(|| "claudecode".to_string())
        }

        async fn spawn(&self, spec: SubAgentSpec) -> Result<SubAgentResult, String> {
            self.0.spawn(spec).await
        }
    }

    #[tokio::test]
    async fn an_external_agent_type_defaults_to_background_when_the_spawner_can_route_it() {
        let spawner = Arc::new(ExternalAwareSpawner(ScriptedSpawner::new(success_result())));
        let tool = AgentTool::with_registry(external_registry(Some("claude-opus-5")));
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        let out = tool
            .call(
                json!({"prompt": "find the bug", "subagent_type": "claudecode"}),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(out["status"], "async_launched");
        assert_eq!(out["agent_id"], "test-agent-id");

        let recorded = spawner.0.calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].run_in_background);
        assert_eq!(
            recorded[0].model.as_deref(),
            Some("claudecode:claude-opus-5"),
            "the definition's model rides as the hint of the external spec"
        );
        assert_eq!(
            recorded[0].system.as_deref(),
            Some("Only ever read files."),
            "the definition's system prompt still overlays (it becomes the brief); \
             external delegation gets no shared-worktree block"
        );
        assert!(
            recorded[0].tool_filter.is_none(),
            "a tool filter cannot bind on somebody else's process"
        );
        assert_eq!(recorded[0].metadata["external_agent"], "claudecode");
        assert_eq!(recorded[0].metadata["external_tools_ignored"], true);
    }

    #[tokio::test]
    async fn an_external_agent_type_allows_explicit_foreground_override() {
        let spawner = Arc::new(ExternalAwareSpawner(ScriptedSpawner::new(success_result())));
        let tool = AgentTool::with_registry(external_registry(None));
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        let out = tool
            .call(
                json!({
                    "prompt": "find the bug",
                    "subagent_type": "claudecode",
                    "run_in_background": false
                }),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(out["status"], "completed");

        let recorded = spawner.0.calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(!recorded[0].run_in_background);
    }

    #[tokio::test]
    async fn an_external_model_target_defaults_to_background() {
        let spawner = Arc::new(ExternalAwareSpawner(ScriptedSpawner::new(success_result())));
        let tool = AgentTool::new();
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        let out = tool
            .call(
                json!({"prompt": "find the bug", "model": "claudecode:sonnet"}),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(out["status"], "async_launched");
        assert!(spawner.0.calls.lock().unwrap()[0].run_in_background);
    }

    #[tokio::test]
    async fn an_external_agent_type_is_refused_when_no_runner_is_wired() {
        // ScriptedSpawner keeps the default resolve_external_agent
        // (None) — the shape of a host without ACP wiring.
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::with_registry(external_registry(None));
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        let err = tool
            .call(
                json!({"prompt": "find the bug", "subagent_type": "claudecode"}),
                &context,
            )
            .await
            .unwrap_err();
        let reason = err.to_string();
        assert!(reason.contains("no external sub-agent runner"), "{reason}");
        assert!(
            spawner.calls.lock().unwrap().is_empty(),
            "a refusal must not silently degrade into a local worker"
        );
    }

    #[tokio::test]
    async fn an_externally_routed_agent_cannot_join_a_team_as_a_teammate() {
        let _home = TestConfigHome::new("named-external-default-team");
        // Without this guard, an external agent named into a team
        // would silently run the in-process teammate loop under the
        // external agent's name — a different agent than asked for.
        let spawner = Arc::new(ExternalAwareSpawner(ScriptedSpawner::new(success_result())));
        let manager = Arc::new(ScriptedTeamManager {
            result: TeammateSpawnResult {
                team_name: "alpha".into(),
                agent_id: "ext-mate@alpha".into(),
                task_id: "text1234".into(),
                reused: false,
                handoff: None,
            },
            calls: Mutex::new(Vec::new()),
        });
        let tool = AgentTool::with_registry(external_registry(None));
        let context = ToolContext::new()
            .with_session_id("external-session")
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>)
            .with_team_manager(manager.clone() as Arc<dyn rebon_tool::TeamManager>);

        let err = tool
            .call(
                json!({
                    "prompt": "join the effort",
                    "subagent_type": "claudecode",
                    "name": "ext-mate",
                }),
                &context,
            )
            .await
            .unwrap_err();
        let reason = err.to_string();
        assert!(
            reason.contains("cannot join a team as a teammate"),
            "{reason}"
        );
        assert!(
            manager.calls.lock().unwrap().is_empty(),
            "the teammate path must not have been entered"
        );
        assert!(spawner.0.calls.lock().unwrap().is_empty());
        assert!(
            !rebon_tool::team_files::team_dir(&rebon_tool::default_team_name("external-session"))
                .exists(),
            "rejecting a named external agent must not create a default team"
        );
    }

    #[test]
    fn parse_input_copies_display_metadata_without_overwriting() {
        let parsed = parse_input(&json!({
            "prompt": "inspect the code",
            "description": "Inspect parser",
            "name": "explore-parser",
            "metadata": { "ultraplan_id": "ultraplan-test" }
        }))
        .unwrap();
        let preserved = parse_input(&json!({
            "prompt": "inspect the code again",
            "description": "Input description",
            "name": "input-name",
            "metadata": {
                "description": "Metadata description",
                "display_name": "metadata-name"
            }
        }))
        .unwrap();

        assert_eq!(parsed.sub_agent.metadata["description"], "Inspect parser");
        assert_eq!(parsed.sub_agent.metadata["display_name"], "explore-parser");
        assert_eq!(parsed.sub_agent.metadata["ultraplan_id"], "ultraplan-test");
        assert_eq!(
            preserved.sub_agent.metadata["description"],
            "Metadata description"
        );
        assert_eq!(
            preserved.sub_agent.metadata["display_name"],
            "metadata-name"
        );
    }

    #[tokio::test]
    async fn agent_tool_enriches_ultraplan_metadata_and_policy_for_local_worker() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let policy = ExecutionPolicy::ultraplan(UltraplanContext::planning_turn(
            "run-active",
            "plan_mode_active",
            PolicyMode::Enforce,
        ));
        let context = ToolContext::new()
            .with_execution_policy(policy.clone())
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "inspect the code",
                "description": "Inspect code",
                "metadata": {
                    "ultraplan_id": "stale-run",
                    "ultraplan_phase": "stale-phase",
                    "ultraplan_role": "reviewer",
                    "ultraplan_budget_pre_reserved": true,
                    "custom": "keep-me"
                }
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        let child_policy = recorded[0]
            .execution_policy
            .as_ref()
            .and_then(|policy| policy.ultraplan.as_ref())
            .expect("child ultraplan policy");
        assert_eq!(child_policy.run_id, "run-active");
        assert!(!child_policy
            .allowed_tools
            .iter()
            .any(|tool| tool == "Agent"));
        assert!(!child_policy
            .allowed_tools
            .iter()
            .any(|tool| tool == "PlanLedger"));
        assert!(child_policy.denied_tools.iter().any(|tool| tool == "Agent"));
        assert!(child_policy
            .denied_tools
            .iter()
            .any(|tool| tool == "PlanLedger"));
        assert_eq!(recorded[0].metadata["ultraplan_id"], "run-active");
        assert_eq!(recorded[0].metadata["ultraplan_phase"], "plan_mode_active");
        assert_eq!(recorded[0].metadata["ultraplan_policy_mode"], "enforce");
        assert_eq!(recorded[0].metadata["ultraplan_role"], "researcher");
        assert!(recorded[0]
            .metadata
            .get("ultraplan_budget_pre_reserved")
            .is_none());
        assert_eq!(recorded[0].metadata["custom"], "keep-me");
        assert_eq!(recorded[0].metadata["description"], "Inspect code");
        assert!(recorded[0].metadata.get("agent_id").is_none());
    }

    #[tokio::test]
    async fn agent_tool_defaults_ultraplan_role_to_researcher() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let policy = ExecutionPolicy::ultraplan(UltraplanContext::planning_turn(
            "run-active",
            "review_phase",
            PolicyMode::Observe,
        ));
        let context = ToolContext::new()
            .with_execution_policy(policy)
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "inspect the code",
                "metadata": { "custom": "keep-me" }
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].metadata["ultraplan_id"], "run-active");
        assert_eq!(recorded[0].metadata["ultraplan_phase"], "review_phase");
        assert_eq!(recorded[0].metadata["ultraplan_policy_mode"], "observe");
        assert_eq!(recorded[0].metadata["ultraplan_role"], "researcher");
        assert_eq!(recorded[0].metadata["custom"], "keep-me");
    }

    #[tokio::test]
    async fn agent_tool_rejects_teammate_spawn_under_ultraplan_local_only_policy() {
        let manager = Arc::new(ScriptedTeamManager {
            result: TeammateSpawnResult {
                team_name: "alpha".into(),
                agent_id: "alice@alpha".into(),
                task_id: "tabc1234".into(),
                reused: false,
                handoff: None,
            },
            calls: Mutex::new(Vec::new()),
        });
        let tool = AgentTool::new();
        let policy = ExecutionPolicy::ultraplan(UltraplanContext::planning_turn(
            "run-active",
            "plan_mode_active",
            PolicyMode::Enforce,
        ));
        let context = ToolContext::new()
            .with_execution_policy(policy)
            .with_team_manager(manager.clone() as Arc<dyn rebon_tool::TeamManager>);

        let err = tool
            .call(
                json!({
                    "prompt": "implement the parser",
                    "name": "alice",
                    "team_name": "alpha"
                }),
                &context,
            )
            .await
            .unwrap_err();

        match err {
            ToolError::PermissionDenied { reason, .. } => {
                assert!(reason.contains("ultraplan local-only policy"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(manager.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn agent_input_write_classification_is_conservative() {
        assert!(!agent_input_may_write(&json!({
            "prompt": "inspect",
            "task_kind": "research"
        })));
        assert!(!agent_input_may_write(&json!({
            "prompt": "verify",
            "subagent_type": "verification"
        })));
        assert!(agent_input_may_write(&json!({
            "prompt": "implement",
            "task_kind": "implementation"
        })));
        assert!(agent_input_may_write(&json!({
            "prompt": "unspecified",
            "task_kind": "other"
        })));
        assert!(agent_input_may_write(&json!({
            "prompt": "general work"
        })));
        assert!(!agent_input_may_write(&json!({
            "prompt": "inspect",
            "allowed_tools": ["Read", "Grep"]
        })));
    }

    #[test]
    fn automatic_worktree_isolation_covers_concurrent_writer_modes() {
        let foreground = parse_input(&json!({
            "prompt": "implement",
            "task_kind": "implementation"
        }))
        .unwrap();
        let background = parse_input(&json!({
            "prompt": "implement",
            "task_kind": "implementation",
            "run_in_background": true
        }))
        .unwrap();

        assert!(!automatic_worktree_isolation_required(
            &ToolContext::new(),
            &foreground,
            true,
            false
        ));
        assert!(automatic_worktree_isolation_required(
            &ToolContext::new().with_parallel_agent_write_batch(true),
            &foreground,
            true,
            false
        ));
        assert!(automatic_worktree_isolation_required(
            &ToolContext::new(),
            &background,
            true,
            false
        ));
        assert!(automatic_worktree_isolation_required(
            &ToolContext::new().with_coordinator_mode(true),
            &foreground,
            true,
            false
        ));
        assert!(!automatic_worktree_isolation_required(
            &ToolContext::new().with_parallel_agent_write_batch(true),
            &foreground,
            false,
            false
        ));
        assert!(!automatic_worktree_isolation_required(
            &ToolContext::new().with_parallel_agent_write_batch(true),
            &foreground,
            true,
            true
        ));
    }

    #[tokio::test]
    async fn background_implementation_agent_automatically_requests_worktree() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        AgentTool::new()
            .call(
                json!({
                    "prompt": "implement safely",
                    "task_kind": "implementation",
                    "run_in_background": true
                }),
                &context,
            )
            .await
            .unwrap();

        let recorded = spawner.background_calls.lock().unwrap();
        assert_eq!(recorded[0].metadata["isolation"], "worktree");
    }

    #[tokio::test]
    async fn background_explore_agent_remains_unisolated() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        AgentTool::new()
            .call(
                json!({
                    "prompt": "inspect only",
                    "subagent_type": "Explore",
                    "run_in_background": true
                }),
                &context,
            )
            .await
            .unwrap();

        let recorded = spawner.background_calls.lock().unwrap();
        assert!(recorded[0].metadata.get("isolation").is_none());
    }

    #[tokio::test]
    async fn parallel_named_writer_is_rejected_before_shared_tree_spawn() {
        let context = ToolContext::new().with_parallel_agent_write_batch(true);

        let error = AgentTool::new()
            .call(
                json!({
                    "prompt": "implement concurrently",
                    "name": "writer",
                    "task_kind": "implementation"
                }),
                &context,
            )
            .await
            .unwrap_err();

        match error {
            ToolError::InvalidInput { reason, .. } => {
                assert!(reason.contains("named teammates cannot be concurrent writers"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn agent_tool_forwards_explicit_task_kind_to_spawner() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "fix the code",
                "subagent_type": "worker",
                "task_kind": "implementation"
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(
            recorded[0].task_kind,
            Some(SubAgentTaskKind::Implementation)
        );
        assert_eq!(
            recorded[0].metadata["coordinator_task_kind"],
            "implementation"
        );
    }

    #[tokio::test]
    async fn default_agent_tool_does_not_apply_builtin_worktree_isolation() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "run batch unit",
                "subagent_type": "batch-worker"
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert!(recorded[0].metadata.get("isolation").is_none());
        assert_eq!(recorded[0].metadata["agent_type"], DEFAULT_SUB_AGENT_TYPE);
    }

    #[tokio::test]
    async fn worktree_enabled_agent_tool_applies_builtin_worktree_isolation() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::with_coordinator_use_worktree(true);
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "run batch unit",
                "subagent_type": "batch-worker"
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].metadata["isolation"], "worktree");
    }

    #[tokio::test]
    async fn explicit_worktree_isolation_is_preserved_for_spawner_policy() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "run isolated task",
                "isolation": "worktree"
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].metadata["isolation"], "worktree");
    }

    #[tokio::test]
    async fn agent_tool_forwards_effective_agent_type_in_metadata() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "inspect the code",
                "subagent_type": "Explore"
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].metadata["agent_type"], "Explore");
        assert!(recorded[0].model.is_none());
    }

    #[tokio::test]
    async fn agent_tool_rejects_disabled_agent_override() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let registry = Arc::new(AgentRegistry::from_groups(AgentGroups {
            built_in: rebon_tool::builtin_agents::all_builtin_agents()
                .into_iter()
                .map(ResolvedAgentDef::from_builtin)
                .collect(),
            user: vec![ResolvedAgentDef {
                agent_type: "rebon-code-guide".into(),
                when_to_use: "DISABLED. Do not invoke.".into(),
                system_prompt: "This agent is intentionally disabled.".into(),
                tool_filter: ToolFilter::allow_only(["Read"]),
                model: None,
                model_profile: None,
                provider: None,
                effort: None,
                background: false,
                isolation: None,
                memory: None,
                permission_mode: None,
                runtime: AgentRuntime::Local,
                source: AgentSource::Settings(
                    rebon_tool::agent_registry::SettingSource::UserSettings,
                ),
                file_stem: None,
            }],
            ..Default::default()
        }));
        let tool = AgentTool::with_registry(registry);
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        let err = tool
            .call(
                json!({
                    "prompt": "explain sdk usage",
                    "subagent_type": "rebon-code-guide"
                }),
                &context,
            )
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidInput { reason, .. } => {
                assert!(reason.contains("rebon-code-guide"));
                assert!(reason.contains("disabled"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(spawner.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn agent_tool_normalizes_explore_alias_in_metadata_and_output() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        let out = tool
            .call(
                json!({
                    "prompt": "find where this behavior lives",
                    "subagent_type": "explorer"
                }),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(out["agent_type"], "Explore");
        assert_eq!(out["agentType"], "Explore");
        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].metadata["agent_type"], "Explore");
        assert!(recorded[0]
            .system
            .as_deref()
            .unwrap_or_default()
            .contains("file search specialist"));
    }

    #[tokio::test]
    async fn agent_tool_preserves_exact_custom_explore_agent_type() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let registry = Arc::new(AgentRegistry::from_groups(AgentGroups {
            built_in: rebon_tool::builtin_agents::all_builtin_agents()
                .into_iter()
                .map(ResolvedAgentDef::from_builtin)
                .collect(),
            user: vec![ResolvedAgentDef {
                agent_type: "explore".into(),
                when_to_use: "custom lowercase explore".into(),
                system_prompt: "custom explore system".into(),
                tool_filter: ToolFilter::unrestricted(),
                model: None,
                model_profile: None,
                provider: None,
                effort: None,
                background: false,
                isolation: None,
                memory: None,
                permission_mode: None,
                runtime: AgentRuntime::Local,
                source: AgentSource::Settings(
                    rebon_tool::agent_registry::SettingSource::UserSettings,
                ),
                file_stem: None,
            }],
            ..Default::default()
        }));
        let tool = AgentTool::with_registry(registry);
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        let out = tool
            .call(
                json!({
                    "prompt": "use my custom agent",
                    "subagent_type": "explore"
                }),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(out["agent_type"], "explore");
        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].metadata["agent_type"], "explore");
        assert_eq!(recorded[0].system.as_deref(), Some("custom explore system"));
    }

    #[tokio::test]
    async fn agent_tool_applies_general_purpose_config_when_type_is_omitted() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let registry = Arc::new(AgentRegistry::from_groups(AgentGroups {
            project: vec![ResolvedAgentDef {
                agent_type: DEFAULT_SUB_AGENT_TYPE.into(),
                when_to_use: "default work".into(),
                system_prompt: "general system".into(),
                tool_filter: ToolFilter::unrestricted(),
                model: Some("inherit".into()),
                model_profile: None,
                provider: None,
                effort: Some("medium".into()),
                background: false,
                isolation: None,
                memory: None,
                permission_mode: None,
                runtime: AgentRuntime::Local,
                source: AgentSource::BuiltIn,
                file_stem: None,
            }],
            ..Default::default()
        }));
        let tool = AgentTool::with_registry(registry);
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(json!({ "prompt": "inspect the code" }), &context)
            .await
            .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].metadata["agent_type"], DEFAULT_SUB_AGENT_TYPE);
        assert_eq!(recorded[0].metadata["reasoning_effort"], "medium");
        assert_eq!(recorded[0].model.as_deref(), Some("inherit"));
        assert_eq!(recorded[0].system.as_deref(), Some("general system"));
        assert!(!recorded[0]
            .system
            .as_deref()
            .unwrap_or_default()
            .contains("SHARED WORKTREE SAFETY:"));
    }

    #[tokio::test]
    async fn agent_tool_forwards_configured_effort_in_metadata() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let registry = Arc::new(AgentRegistry::from_groups(AgentGroups {
            project: vec![ResolvedAgentDef {
                agent_type: "Deep".into(),
                when_to_use: "deep work".into(),
                system_prompt: "system".into(),
                tool_filter: ToolFilter::unrestricted(),
                model: None,
                model_profile: Some("reasoning".into()),
                provider: None,
                effort: Some("high".into()),
                background: false,
                isolation: None,
                memory: None,
                permission_mode: None,
                runtime: AgentRuntime::Local,
                source: AgentSource::BuiltIn,
                file_stem: None,
            }],
            ..Default::default()
        }));
        let tool = AgentTool::with_registry(registry);
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "inspect the code",
                "subagent_type": "Deep"
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].metadata["reasoning_effort"], "high");
        assert_eq!(recorded[0].model_profile.as_deref(), Some("reasoning"));
    }

    #[tokio::test]
    async fn agent_tool_forwards_top_level_reasoning_effort_in_metadata() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "inspect the code",
                "reasoningEffort": "low"
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].metadata["reasoning_effort"], "low");
    }

    #[tokio::test]
    async fn agent_tool_passes_parent_permission_broker_to_sub_agent_spec() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let broker =
            Arc::new(rebon_tool::DenyAskPermissionBroker) as Arc<dyn rebon_tool::PermissionBroker>;
        let context = ToolContext::new()
            .with_permission_broker(broker.clone())
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(json!({ "prompt": "do the thing" }), &context)
            .await
            .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        let inherited = recorded[0]
            .permission_broker
            .as_ref()
            .expect("parent broker should be inherited");
        assert!(Arc::ptr_eq(inherited, &broker));
    }

    #[tokio::test]
    async fn agent_tool_inherits_context_cwd_when_input_cwd_is_omitted() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let context = ToolContext::new()
            .with_cwd("C:/projects/example")
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(json!({ "prompt": "do the thing" }), &context)
            .await
            .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].cwd.as_deref(), Some("C:/projects/example"));
    }

    #[tokio::test]
    async fn agent_tool_preserves_explicit_input_cwd() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let context = ToolContext::new()
            .with_cwd("C:/projects/example")
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "do the thing",
                "cwd": "C:/projects/example/nested"
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].cwd.as_deref(),
            Some("C:/projects/example/nested")
        );
    }

    #[tokio::test]
    async fn agent_tool_rejects_explicit_cwd_outside_parent_scope() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let dir_guard = tempfile::Builder::new()
            .prefix("rebon-agent-parent-scope-")
            .tempdir()
            .unwrap();
        let dir = dir_guard.path().to_path_buf();
        let outside_guard = tempfile::Builder::new()
            .prefix("rebon-agent-outside-scope-")
            .tempdir()
            .unwrap();
        let outside = outside_guard.path().to_path_buf();
        let context = ToolContext::new()
            .with_agent_id("parent-agent")
            .with_cwd(dir.to_string_lossy())
            .with_sub_agent_spawner(spawner as Arc<dyn SubAgentSpawner>);

        let err = tool
            .call(
                json!({
                    "prompt": "do the thing",
                    "cwd": outside.to_string_lossy()
                }),
                &context,
            )
            .await
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("must stay within parent authorized roots"));
    }

    #[tokio::test]
    async fn agent_tool_allows_read_only_explore_cwd_outside_parent_scope() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let dir_guard = tempfile::Builder::new()
            .prefix("rebon-agent-explore-parent-scope-")
            .tempdir()
            .unwrap();
        let dir = dir_guard.path().to_path_buf();
        let outside_guard = tempfile::Builder::new()
            .prefix("rebon-agent-explore-outside-scope-")
            .tempdir()
            .unwrap();
        let outside = outside_guard.path().to_path_buf();
        let context = ToolContext::new()
            .with_agent_id("parent-agent")
            .with_cwd(dir.to_string_lossy())
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "inspect the other repository",
                "subagent_type": "Explore",
                "cwd": outside.to_string_lossy()
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].cwd.as_deref(), outside.to_str());
        assert_eq!(recorded[0].allowed_roots, vec![outside.clone()]);
        drop(recorded);
    }

    #[tokio::test]
    async fn agent_tool_rejects_external_read_only_explore_worktree_without_authorization() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let dir_guard = tempfile::Builder::new()
            .prefix("rebon-agent-explore-worktree-parent-scope-")
            .tempdir()
            .unwrap();
        let dir = dir_guard.path().to_path_buf();
        let outside_guard = tempfile::Builder::new()
            .prefix("rebon-agent-explore-worktree-outside-scope-")
            .tempdir()
            .unwrap();
        let outside = outside_guard.path().to_path_buf();
        let context = ToolContext::new()
            .with_agent_id("parent-agent")
            .with_cwd(dir.to_string_lossy())
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        let err = tool
            .call(
                json!({
                    "prompt": "inspect the other repository",
                    "subagent_type": "Explore",
                    "cwd": outside.to_string_lossy(),
                    "isolation": "worktree",
                    "run_in_background": true
                }),
                &context,
            )
            .await
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("must stay within parent authorized roots"));
        assert!(spawner.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn agent_tool_allows_cwd_inside_additional_working_directory() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let dir_guard = tempfile::Builder::new()
            .prefix("rebon-agent-add-dir-parent-")
            .tempdir()
            .unwrap();
        let dir = dir_guard.path().to_path_buf();
        let additional_guard = tempfile::Builder::new()
            .prefix("rebon-agent-add-dir-authorized-")
            .tempdir()
            .unwrap();
        let additional = additional_guard.path().to_path_buf();
        let context = ToolContext::new()
            .with_agent_id("parent-agent")
            .with_cwd(dir.to_string_lossy())
            .with_additional_working_directories([additional.to_string_lossy().to_string()])
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "work in the authorized repository",
                "subagent_type": "general-purpose",
                "cwd": additional.to_string_lossy()
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].cwd.as_deref(), additional.to_str());
        assert_eq!(recorded[0].allowed_roots, vec![additional.clone()]);
        drop(recorded);
    }

    #[tokio::test]
    async fn agent_tool_allows_cwd_inside_parent_scope_without_context_cwd() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let root_guard = tempfile::Builder::new()
            .prefix("rebon-agent-no-context-cwd-root-")
            .tempdir()
            .unwrap();
        let root = root_guard.path().to_path_buf();
        let child = root.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let context = ToolContext::new()
            .with_path_scope_roots([root.clone()])
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "work in the authorized child",
                "subagent_type": "general-purpose",
                "cwd": child.to_string_lossy()
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].cwd.as_deref(), child.to_str());
        assert_eq!(recorded[0].allowed_roots, vec![child]);
        drop(recorded);
    }

    /// Workflow lineage reaches grandchildren: an agent spawned *by* a
    /// workflow worker keeps the worker's depth, so the shared-worktree Git
    /// gate covers the whole workflow subtree rather than stopping at the
    /// first generation. The depth counts workflow nesting levels, not agent
    /// generations, so it is inherited verbatim rather than incremented —
    /// incrementing per spawn would also trip `MAX_NESTED_WORKFLOW_DEPTH`.
    #[tokio::test]
    async fn workflow_lineage_depth_reaches_grandchild_agents() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let context = ToolContext::new()
            .with_workflow_nesting_depth(1)
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "clean up the tree",
                "subagent_type": "general-purpose"
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].workflow_nesting_depth, 1);
        drop(recorded);
    }

    #[tokio::test]
    async fn background_spawn_also_carries_workflow_lineage_depth() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let context = ToolContext::new()
            .with_workflow_nesting_depth(2)
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "clean up the tree",
                "subagent_type": "general-purpose",
                "run_in_background": true
            }),
            &context,
        )
        .await
        .unwrap();

        let background = spawner.background_calls.lock().unwrap();
        assert_eq!(background[0].workflow_nesting_depth, 2);
        drop(background);
    }

    /// Depth is runtime-derived, exactly like `runtime_isolated_worktree`:
    /// it is overwritten from the parent context after parsing, so tool
    /// input claiming a depth cannot buy its way out of (or into) the gate.
    #[tokio::test]
    async fn agent_tool_input_cannot_forge_workflow_nesting_depth() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let context =
            ToolContext::new().with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "ordinary sub-agent",
                "subagent_type": "general-purpose",
                "workflow_nesting_depth": 7
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(
            recorded[0].workflow_nesting_depth, 0,
            "a non-workflow spawn must stay at depth 0 regardless of input"
        );
        drop(recorded);
    }

    /// A parent picks the `cwd` its children run in, so a directory named
    /// like a runtime worktree must not confer worktree isolation — that
    /// flag lifts the shared-worktree Git approval gate.
    #[tokio::test]
    async fn spoofed_worktree_cwd_does_not_claim_runtime_isolation() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let root_guard = tempfile::Builder::new()
            .prefix("rebon-agent-spoofed-worktree-")
            .tempdir()
            .unwrap();
        let root = root_guard.path().to_path_buf();
        let decoy = root.join(".rebon").join("worktrees").join("agent-x");
        std::fs::create_dir_all(&decoy).unwrap();
        let context = ToolContext::new()
            .with_path_scope_roots([root.clone()])
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "clean up",
                "subagent_type": "general-purpose",
                "cwd": decoy.to_string_lossy()
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].cwd.as_deref(), decoy.to_str());
        assert!(
            !recorded[0].runtime_isolated_worktree,
            "a caller-supplied worktree-shaped cwd must not claim isolation"
        );
        drop(recorded);
    }

    /// Isolation travels down the creation chain: a child that keeps its
    /// parent's tree untouched is as isolated as the parent was.
    #[tokio::test]
    async fn inherited_cwd_carries_parent_worktree_isolation() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let root_guard = tempfile::Builder::new()
            .prefix("rebon-agent-inherit-isolation-")
            .tempdir()
            .unwrap();
        let root = root_guard.path().to_path_buf();
        let context = ToolContext::new()
            .with_cwd(root.to_string_lossy().to_string())
            .with_isolated_worktree(true)
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "continue in the same tree",
                "subagent_type": "general-purpose"
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert!(recorded[0].runtime_isolated_worktree);
        drop(recorded);
    }

    /// …but only when the tree is untouched. A child pointed somewhere else
    /// is back in shared territory, so it must not keep the exemption.
    #[tokio::test]
    async fn redirected_child_cwd_drops_parent_worktree_isolation() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let root_guard = tempfile::Builder::new()
            .prefix("rebon-agent-redirect-isolation-")
            .tempdir()
            .unwrap();
        let root = root_guard.path().to_path_buf();
        let child = root.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let context = ToolContext::new()
            .with_cwd(root.to_string_lossy().to_string())
            .with_isolated_worktree(true)
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "work over there instead",
                "subagent_type": "general-purpose",
                "cwd": child.to_string_lossy()
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].cwd.as_deref(), child.to_str());
        assert!(!recorded[0].runtime_isolated_worktree);
        drop(recorded);
    }

    #[tokio::test]
    async fn agent_tool_rejects_external_cwd_without_context_cwd() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let root_guard = tempfile::Builder::new()
            .prefix("rebon-agent-no-context-cwd-reject-root-")
            .tempdir()
            .unwrap();
        let root = root_guard.path().to_path_buf();
        let outside_guard = tempfile::Builder::new()
            .prefix("rebon-agent-no-context-cwd-reject-outside-")
            .tempdir()
            .unwrap();
        let outside = outside_guard.path().to_path_buf();
        let context = ToolContext::new()
            .with_path_scope_roots([root.clone()])
            .with_sub_agent_spawner(spawner as Arc<dyn SubAgentSpawner>);

        let err = tool
            .call(
                json!({
                    "prompt": "modify the external directory",
                    "subagent_type": "general-purpose",
                    "cwd": outside.to_string_lossy()
                }),
                &context,
            )
            .await
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("must stay within parent authorized roots"));
    }

    #[tokio::test]
    async fn agent_tool_inherits_explicit_parent_roots_without_context_cwd() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let root_guard = tempfile::Builder::new()
            .prefix("rebon-agent-no-context-cwd-inherit-root-")
            .tempdir()
            .unwrap();
        let root = root_guard.path().to_path_buf();
        let context = ToolContext::new()
            .with_path_scope_roots([root.clone()])
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "work within the inherited scope",
                "subagent_type": "general-purpose"
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].cwd, None);
        assert_eq!(recorded[0].allowed_roots, vec![root.clone()]);
        drop(recorded);
    }

    #[tokio::test]
    async fn agent_tool_worktree_uses_explicit_parent_root_without_context_cwd() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let root_guard = tempfile::Builder::new()
            .prefix("rebon-agent-worktree-explicit-root-")
            .tempdir()
            .unwrap();
        let root = root_guard.path().to_path_buf();
        let context = ToolContext::new()
            .with_path_scope_roots([root.clone()])
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "work within the authorized root",
                "subagent_type": "general-purpose",
                "isolation": "worktree"
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].cwd.as_deref(), root.to_str());
        assert_eq!(recorded[0].allowed_roots, vec![root.clone()]);
        drop(recorded);
    }

    #[tokio::test]
    async fn agent_tool_allows_explicit_allowed_roots_within_parent_scope() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let dir_guard = tempfile::Builder::new()
            .prefix("rebon-agent-allowed-roots-")
            .tempdir()
            .unwrap();
        let dir = dir_guard.path().to_path_buf();
        let child = dir.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let context = ToolContext::new()
            .with_cwd(dir.to_string_lossy())
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "do the thing",
                "cwd": child.to_string_lossy(),
                "allowed_roots": [child.to_string_lossy()]
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].allowed_roots, vec![child.clone()]);
        drop(recorded);
    }

    #[tokio::test]
    async fn agent_tool_narrows_broad_allowed_root_to_parent_scope() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let dir_guard = tempfile::Builder::new()
            .prefix("rebon-agent-broad-allowed-root-")
            .tempdir()
            .unwrap();
        let dir = dir_guard.path().to_path_buf();
        let authorized = dir.join("app");
        std::fs::create_dir_all(&authorized).unwrap();
        let context = ToolContext::new()
            .with_agent_id("parent-agent")
            .with_cwd(authorized.to_string_lossy())
            .with_path_scope_roots([authorized.clone()])
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "work with the code",
                "subagent_type": "general-purpose",
                "allowed_roots": [dir.to_string_lossy()]
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].allowed_roots, vec![authorized.clone()]);
        drop(recorded);
    }

    #[tokio::test]
    async fn agent_tool_narrows_broad_allowed_root_to_all_overlapping_parent_roots() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let dir_guard = tempfile::Builder::new()
            .prefix("rebon-agent-multiple-allowed-roots-")
            .tempdir()
            .unwrap();
        let dir = dir_guard.path().to_path_buf();
        let app = dir.join("app");
        let shared = dir.join("shared");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        let context = ToolContext::new()
            .with_agent_id("parent-agent")
            .with_cwd(app.to_string_lossy())
            .with_path_scope_roots([app.clone(), shared.clone()])
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(
            json!({
                "prompt": "work with the code",
                "subagent_type": "general-purpose",
                "allowed_roots": [dir.to_string_lossy()]
            }),
            &context,
        )
        .await
        .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].allowed_roots, vec![app.clone(), shared]);
        drop(recorded);
    }

    #[tokio::test]
    async fn agent_tool_rejects_disjoint_allowed_root() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let dir_guard = tempfile::Builder::new()
            .prefix("rebon-agent-disjoint-parent-root-")
            .tempdir()
            .unwrap();
        let dir = dir_guard.path().to_path_buf();
        let outside_guard = tempfile::Builder::new()
            .prefix("rebon-agent-disjoint-outside-root-")
            .tempdir()
            .unwrap();
        let outside = outside_guard.path().to_path_buf();
        let context = ToolContext::new()
            .with_agent_id("parent-agent")
            .with_cwd(dir.to_string_lossy())
            .with_path_scope_roots([dir.clone()])
            .with_sub_agent_spawner(spawner as Arc<dyn SubAgentSpawner>);

        let err = tool
            .call(
                json!({
                    "prompt": "work with the code",
                    "subagent_type": "general-purpose",
                    "allowed_roots": [outside.to_string_lossy()]
                }),
                &context,
            )
            .await
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("must stay within parent authorized roots"));
    }

    #[tokio::test]
    async fn agent_tool_background_spawn_inherits_ultraplan_policy_and_metadata() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let policy = ExecutionPolicy::ultraplan(UltraplanContext::planning_turn(
            "run-bg",
            "plan_mode_active",
            PolicyMode::Observe,
        ));
        let context = ToolContext::new()
            .with_cwd("C:/projects/example")
            .with_execution_policy(policy.clone())
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        let out = tool
            .call(
                json!({
                    "prompt": "delegate this",
                    "run_in_background": true,
                    "metadata": { "ultraplan_id": "stale" }
                }),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(out["status"], "async_launched");
        let background = spawner.background_calls.lock().unwrap();
        assert_eq!(background.len(), 1);
        let child_policy = background[0]
            .execution_policy
            .as_ref()
            .and_then(|policy| policy.ultraplan.as_ref())
            .expect("child ultraplan policy");
        assert_eq!(child_policy.run_id, "run-bg");
        assert!(!child_policy
            .allowed_tools
            .iter()
            .any(|tool| tool == "Agent"));
        assert!(!child_policy
            .allowed_tools
            .iter()
            .any(|tool| tool == "PlanLedger"));
        assert!(child_policy.denied_tools.iter().any(|tool| tool == "Agent"));
        assert!(child_policy
            .denied_tools
            .iter()
            .any(|tool| tool == "PlanLedger"));
        assert_eq!(background[0].metadata["ultraplan_id"], "run-bg");
        assert_eq!(background[0].metadata["ultraplan_role"], "researcher");
        assert_eq!(background[0].cwd.as_deref(), Some("C:/projects/example"));
    }

    #[tokio::test]
    async fn agent_tool_refreshes_worker_policy_from_current_run_head() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let state = UltraplanRunState::new(
            "run-current".into(),
            "session".into(),
            "task".into(),
            None,
            1,
        );
        let stale_policy = ExecutionPolicy::ultraplan(UltraplanContext::planning_turn(
            "run-current",
            "plan_mode_active",
            PolicyMode::Enforce,
        ));
        let context = ToolContext::new()
            .with_execution_policy(stale_policy)
            .with_ultraplan_run_handle(Arc::new(Mutex::new(state.clone())))
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(json!({"prompt": "inspect the code"}), &context)
            .await
            .unwrap();

        let recorded = spawner.calls.lock().unwrap();
        let child = recorded[0]
            .execution_policy
            .as_ref()
            .and_then(|policy| policy.ultraplan.as_ref())
            .expect("child ultraplan policy");
        assert_eq!(child.ledger_revision, state.ledger_revision);
        assert_eq!(child.requirements_hash, state.requirements_hash);
    }

    #[tokio::test]
    async fn agent_tool_persists_research_budget_before_spawn() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let mut state = UltraplanRunState::new(
            "run-budget".into(),
            "session".into(),
            "task".into(),
            None,
            1,
        );
        let mut capability = CapabilityContext {
            run_id: state.run_id.clone(),
            ledger_revision: state.ledger_revision,
            requirements_hash: state.requirements_hash.clone(),
            session_id: state.session_id.clone(),
            read_allowed: true,
            tool_ids: vec!["Read".into(), "Glob".into(), "Grep".into()],
            sub_agent_available: true,
            max_research_agents: state.budget.max_research_agents,
            max_adversarial_reviews: state.budget.max_adversarial_reviews,
            max_tool_error_retries: state.budget.max_tool_error_retries,
            ..CapabilityContext::default()
        };
        capability.refresh_hash();
        state.set_capability_context(capability);
        let repository = TestRunRepository {
            state: Arc::new(Mutex::new(state.clone())),
        };
        let policy = ExecutionPolicy::ultraplan(
            UltraplanContext::planning_turn(
                state.run_id.clone(),
                "evidence_verify",
                PolicyMode::Enforce,
            )
            .with_run_head(&state.head()),
        );
        let context = ToolContext::new()
            .with_execution_policy(policy)
            .with_ultraplan_run_repository(Arc::new(repository.clone()))
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        tool.call(json!({"prompt": "inspect the code"}), &context)
            .await
            .unwrap();

        let persisted = repository.load_current().unwrap();
        assert_eq!(persisted.budget.research_agents_used, 1);
        assert_eq!(persisted.ledger_revision, state.ledger_revision);
        assert!(persisted.state_revision > state.state_revision);
        let recorded = spawner.calls.lock().unwrap();
        assert_eq!(recorded[0].metadata["ultraplan_budget_pre_reserved"], true);
        assert_eq!(
            recorded[0]
                .capability_context
                .as_ref()
                .unwrap()
                .research_agents_used,
            1
        );
    }

    #[tokio::test]
    async fn agent_tool_coordinator_mode_forces_background_spawn() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let context = ToolContext::new()
            .with_coordinator_mode(true)
            .with_cwd("C:/projects/example")
            .with_workflow_nesting_depth(1)
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        let out = tool
            .call(
                json!({
                    "prompt": "delegate this",
                    "metadata": { "display_name": "explore-engine" }
                }),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(out["status"], "async_launched");
        assert_eq!(out["agent_id"], "background-agent-id");
        assert_eq!(out["display_name"], "explore-engine");
        assert_eq!(out["displayName"], "explore-engine");
        assert!(spawner.calls.lock().unwrap().is_empty());
        let background = spawner.background_calls.lock().unwrap();
        assert_eq!(background.len(), 1);
        assert!(background[0].run_in_background);
        assert_eq!(background[0].workflow_nesting_depth, 1);
        assert!(background[0]
            .system
            .as_deref()
            .unwrap_or_default()
            .contains(WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT));
        assert_eq!(background[0].metadata["display_name"], "explore-engine");
        assert_eq!(background[0].cwd.as_deref(), Some("C:/projects/example"));
    }

    #[tokio::test]
    async fn agent_tool_errors_when_no_spawner_is_injected() {
        let tool = AgentTool::new();
        let context = ToolContext::new();
        let err = tool
            .call(json!({ "prompt": "anything" }), &context)
            .await
            .unwrap_err();
        match err {
            ToolError::Execution { source, .. } => {
                assert!(source.to_string().contains("SubAgentSpawner"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn agent_tool_rejects_missing_prompt() {
        let tool = AgentTool::new();
        let context = ToolContext::new();
        let validation = tool.validate_input(&json!({}), &context).await.unwrap();
        assert!(!validation.is_valid());
        assert!(validation.message.as_deref().unwrap().contains("required"));
    }

    #[tokio::test]
    async fn agent_tool_rejects_empty_prompt() {
        let tool = AgentTool::new();
        let context = ToolContext::new();
        let validation = tool
            .validate_input(&json!({ "prompt": "   " }), &context)
            .await
            .unwrap();
        assert!(!validation.is_valid());
    }

    #[tokio::test]
    async fn agent_tool_rejects_non_array_allowed_tools() {
        let tool = AgentTool::new();
        let context = ToolContext::new();
        let validation = tool
            .validate_input(&json!({ "prompt": "x", "allowed_tools": "Read" }), &context)
            .await
            .unwrap();
        assert!(!validation.is_valid());
        assert!(validation
            .message
            .as_deref()
            .unwrap()
            .contains("allowed_tools"));
    }

    #[tokio::test]
    async fn agent_tool_rejects_plan_agent_when_parent_is_in_plan_mode() {
        let tool = AgentTool::new();
        let context = ToolContext::new().with_permission_mode("plan");
        let validation = tool
            .validate_input(
                &json!({
                    "prompt": "design the implementation",
                    "subagent_type": "Plan"
                }),
                &context,
            )
            .await
            .unwrap();

        assert!(!validation.is_valid());
        assert_eq!(validation.error_code, Some(INVALID_INPUT_CODE));
        assert!(validation
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("parent session is already in Plan Mode"));
    }

    #[tokio::test]
    async fn agent_tool_call_defensively_rejects_plan_agent_in_parent_plan_mode() {
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let tool = AgentTool::new();
        let context = ToolContext::new()
            .with_permission_mode("plan")
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);
        let err = tool
            .call(
                json!({
                    "prompt": "design the implementation",
                    "subagent_type": "plan"
                }),
                &context,
            )
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidInput {
                reason, error_code, ..
            } => {
                assert_eq!(error_code, Some(INVALID_INPUT_CODE));
                assert!(reason.contains("parent session is already in Plan Mode"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(spawner.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn agent_tool_allows_explore_when_parent_is_in_plan_mode() {
        let tool = AgentTool::new();
        let context = ToolContext::new().with_permission_mode("plan");
        let validation = tool
            .validate_input(
                &json!({
                    "prompt": "locate the relevant code",
                    "subagent_type": "Explore"
                }),
                &context,
            )
            .await
            .unwrap();

        assert!(validation.is_valid());
    }

    #[tokio::test]
    async fn agent_tool_allows_plan_agent_outside_parent_plan_mode() {
        let tool = AgentTool::new();
        let context = ToolContext::new().with_permission_mode("default");
        let validation = tool
            .validate_input(
                &json!({
                    "prompt": "provide an independent architecture review",
                    "subagent_type": "Plan"
                }),
                &context,
            )
            .await
            .unwrap();

        assert!(validation.is_valid());
    }

    #[tokio::test]
    async fn agent_tool_observes_live_parent_permission_mode() {
        let mode = Arc::new(Mutex::new("default".to_string()));
        let mode_for_provider = mode.clone();
        let context = ToolContext::new()
            .with_permission_mode_provider(move || Some(mode_for_provider.lock().unwrap().clone()));
        let tool = AgentTool::new();
        let input = json!({
            "prompt": "design the implementation",
            "subagent_type": "Plan"
        });

        assert!(tool
            .validate_input(&input, &context)
            .await
            .unwrap()
            .is_valid());
        *mode.lock().unwrap() = "plan".into();
        assert!(!tool
            .validate_input(&input, &context)
            .await
            .unwrap()
            .is_valid());
    }

    #[tokio::test]
    async fn agent_tool_check_permissions_allows_inherited_cwd() {
        let tool = AgentTool::new();
        let context = ToolContext::new().with_cwd("F:/repo");
        let decision = tool
            .check_permissions(&json!({ "prompt": "spawn me" }), &context)
            .await
            .unwrap();
        assert!(matches!(
            decision.behavior,
            rebon_tools_core::PermissionBehavior::Allow
        ));
    }

    #[tokio::test]
    async fn agent_tool_check_permissions_allows_external_read_only_explore() {
        let tool = AgentTool::new();
        let dir_guard = tempfile::Builder::new()
            .prefix("rebon-agent-permission-explore-parent-")
            .tempdir()
            .unwrap();
        let dir = dir_guard.path().to_path_buf();
        let outside_guard = tempfile::Builder::new()
            .prefix("rebon-agent-permission-explore-outside-")
            .tempdir()
            .unwrap();
        let outside = outside_guard.path().to_path_buf();
        let context = ToolContext::new().with_cwd(dir.to_string_lossy());
        let decision = tool
            .check_permissions(
                &json!({
                    "prompt": "inspect the sibling repository",
                    "subagent_type": "Explore",
                    "cwd": outside.to_string_lossy()
                }),
                &context,
            )
            .await
            .unwrap();
        assert!(matches!(
            decision.behavior,
            rebon_tools_core::PermissionBehavior::Allow
        ));
    }

    #[tokio::test]
    async fn agent_tool_check_permissions_requests_external_read_only_explore_worktree() {
        let tool = AgentTool::new();
        let dir_guard = tempfile::Builder::new()
            .prefix("rebon-agent-permission-explore-worktree-parent-")
            .tempdir()
            .unwrap();
        let dir = dir_guard.path().to_path_buf();
        let outside_guard = tempfile::Builder::new()
            .prefix("rebon-agent-permission-explore-worktree-outside-")
            .tempdir()
            .unwrap();
        let outside = outside_guard.path().to_path_buf();
        let context = ToolContext::new().with_cwd(dir.to_string_lossy());
        let decision = tool
            .check_permissions(
                &json!({
                    "prompt": "inspect the sibling repository",
                    "subagent_type": "Explore",
                    "cwd": outside.to_string_lossy(),
                    "isolation": "worktree",
                    "run_in_background": true
                }),
                &context,
            )
            .await
            .unwrap();

        assert!(matches!(
            decision.behavior,
            rebon_tools_core::PermissionBehavior::Ask
        ));
        let request = decision.request.unwrap();
        assert!(request.message.contains("may create a local Git branch"));
        assert!(request.message.contains("merge them back"));
        assert_eq!(
            request.metadata.unwrap()["authorized_path_roots"],
            json!([outside.to_string_lossy()])
        );
    }

    #[tokio::test]
    async fn agent_tool_check_permissions_requests_external_implementation_explore() {
        let tool = AgentTool::new();
        let dir_guard = tempfile::Builder::new()
            .prefix("rebon-agent-permission-explore-implementation-parent-")
            .tempdir()
            .unwrap();
        let dir = dir_guard.path().to_path_buf();
        let outside_guard = tempfile::Builder::new()
            .prefix("rebon-agent-permission-explore-implementation-outside-")
            .tempdir()
            .unwrap();
        let outside = outside_guard.path().to_path_buf();
        let context = ToolContext::new().with_cwd(dir.to_string_lossy());
        let decision = tool
            .check_permissions(
                &json!({
                    "prompt": "inspect the sibling repository",
                    "subagent_type": "Explore",
                    "cwd": outside.to_string_lossy(),
                    "task_kind": "implementation"
                }),
                &context,
            )
            .await
            .unwrap();

        assert!(matches!(
            decision.behavior,
            rebon_tools_core::PermissionBehavior::Ask
        ));
        assert_eq!(
            decision.request.unwrap().metadata.unwrap()["authorized_path_roots"],
            json!([outside.to_string_lossy()])
        );
    }

    /// A coordinator asks for the worker report directory on nearly every
    /// spawn, because its workers are ordered to write a report there.
    /// Prompting for it put an approval in front of every delegation and
    /// settled nothing: the runtime grants each worker its own report file
    /// either way, and refusing did not make the report optional.
    #[tokio::test]
    async fn agent_tool_does_not_prompt_for_the_worker_report_directory() {
        // `worker_report_dir()` is read twice here — once by this test and
        // once inside `check_permissions` — and it follows the config home
        // in the environment, which other tests in this crate swap under
        // the same lock. Hold it, or the two reads can disagree.
        let _env = rebon_tool::env_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tool = AgentTool::new();
        let dir_guard = tempfile::Builder::new()
            .prefix("rebon-agent-report-root-")
            .tempdir()
            .unwrap();
        let dir = dir_guard.path().to_path_buf();
        let reports = rebon_tool::path_scope::worker_report_dir();
        let context = ToolContext::new().with_cwd(dir.to_string_lossy());

        let decision = tool
            .check_permissions(
                &json!({
                    "prompt": "do the work",
                    "subagent_type": "general-purpose",
                    "cwd": dir.to_string_lossy(),
                    "allowed_roots": [dir.to_string_lossy(), reports.to_string_lossy()],
                }),
                &context,
            )
            .await
            .unwrap();
        assert!(
            matches!(
                decision.behavior,
                rebon_tools_core::PermissionBehavior::Allow
            ),
            "{decision:?}"
        );

        // A genuinely external root in the same call still prompts, and the
        // report directory is not smuggled into the request alongside it.
        let outside_guard = tempfile::Builder::new()
            .prefix("rebon-agent-report-root-outside-")
            .tempdir()
            .unwrap();
        let outside = outside_guard.path().to_path_buf();
        let decision = tool
            .check_permissions(
                &json!({
                    "prompt": "do the work",
                    "subagent_type": "general-purpose",
                    "cwd": dir.to_string_lossy(),
                    "allowed_roots": [
                        reports.to_string_lossy(),
                        outside.to_string_lossy(),
                    ],
                }),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(
            decision
                .request
                .expect("permission request")
                .metadata
                .unwrap()["authorized_path_roots"],
            json!([outside.to_string_lossy()])
        );
    }

    #[tokio::test]
    async fn agent_tool_check_permissions_requests_external_writable_agent_root() {
        let tool = AgentTool::new();
        let dir_guard = tempfile::Builder::new()
            .prefix("rebon-agent-permission-write-parent-")
            .tempdir()
            .unwrap();
        let dir = dir_guard.path().to_path_buf();
        let outside_guard = tempfile::Builder::new()
            .prefix("rebon-agent-permission-write-outside-")
            .tempdir()
            .unwrap();
        let outside = outside_guard.path().to_path_buf();
        let context = ToolContext::new().with_cwd(dir.to_string_lossy());
        let decision = tool
            .check_permissions(
                &json!({
                    "prompt": "modify the sibling repository",
                    "subagent_type": "general-purpose",
                    "cwd": outside.to_string_lossy()
                }),
                &context,
            )
            .await
            .unwrap();
        assert!(matches!(
            decision.behavior,
            rebon_tools_core::PermissionBehavior::Ask
        ));
        let request = decision.request.expect("permission request");
        // `allow_always` is offered here and scoped by the frontend to
        // the roots in `authorized_path_roots` — a coordinator otherwise
        // needs the same directory re-approved for every worker it
        // delegates.
        assert_eq!(
            request.options,
            vec![
                "allow_once".to_string(),
                "allow_always".to_string(),
                "reject_once".to_string()
            ]
        );
        assert_eq!(
            request.metadata.unwrap()["authorized_path_roots"],
            json!([outside.to_string_lossy()])
        );
    }

    #[tokio::test]
    async fn agent_tool_check_permissions_requests_external_cwd_without_context_cwd() {
        let tool = AgentTool::new();
        let root_guard = tempfile::Builder::new()
            .prefix("rebon-agent-permission-no-context-cwd-root-")
            .tempdir()
            .unwrap();
        let root = root_guard.path().to_path_buf();
        let outside_guard = tempfile::Builder::new()
            .prefix("rebon-agent-permission-no-context-cwd-outside-")
            .tempdir()
            .unwrap();
        let outside = outside_guard.path().to_path_buf();
        let context = ToolContext::new().with_path_scope_roots([root.clone()]);

        let decision = tool
            .check_permissions(
                &json!({
                    "prompt": "modify the external directory",
                    "subagent_type": "general-purpose",
                    "cwd": outside.to_string_lossy()
                }),
                &context,
            )
            .await
            .unwrap();

        assert!(matches!(
            decision.behavior,
            rebon_tools_core::PermissionBehavior::Ask
        ));
        assert_eq!(
            decision.request.unwrap().metadata.unwrap()["authorized_path_roots"],
            json!([outside.to_string_lossy()])
        );
    }

    #[tokio::test]
    async fn agent_tool_check_permissions_requests_external_default_worktree_explore() {
        let registry = Arc::new(AgentRegistry::from_groups(AgentGroups {
            built_in: rebon_tool::builtin_agents::all_builtin_agents()
                .into_iter()
                .map(ResolvedAgentDef::from_builtin)
                .collect(),
            user: vec![ResolvedAgentDef {
                agent_type: "Explore".into(),
                when_to_use: "custom isolated explore".into(),
                system_prompt: "custom isolated explore".into(),
                tool_filter: ToolFilter::allow_only(["Read", "Glob", "Grep"]),
                model: None,
                model_profile: None,
                provider: None,
                effort: None,
                background: false,
                isolation: Some("worktree".into()),
                memory: None,
                permission_mode: None,
                runtime: AgentRuntime::Local,
                source: AgentSource::Settings(
                    rebon_tool::agent_registry::SettingSource::UserSettings,
                ),
                file_stem: None,
            }],
            ..Default::default()
        }));
        let tool = AgentTool::with_registry_and_options(registry, true);
        let dir_guard = tempfile::Builder::new()
            .prefix("rebon-agent-permission-default-worktree-parent-")
            .tempdir()
            .unwrap();
        let dir = dir_guard.path().to_path_buf();
        let outside_guard = tempfile::Builder::new()
            .prefix("rebon-agent-permission-default-worktree-outside-")
            .tempdir()
            .unwrap();
        let outside = outside_guard.path().to_path_buf();
        let context = ToolContext::new().with_cwd(dir.to_string_lossy());
        let decision = tool
            .check_permissions(
                &json!({
                    "prompt": "inspect the sibling repository",
                    "subagent_type": "Explore",
                    "cwd": outside.to_string_lossy()
                }),
                &context,
            )
            .await
            .unwrap();

        assert!(matches!(
            decision.behavior,
            rebon_tools_core::PermissionBehavior::Ask
        ));

        let no_isolation = tool
            .check_permissions(
                &json!({
                    "prompt": "inspect the sibling repository without isolation",
                    "subagent_type": "Explore",
                    "cwd": outside.to_string_lossy(),
                    "isolation": "none"
                }),
                &context,
            )
            .await
            .unwrap();
        assert!(matches!(
            no_isolation.behavior,
            rebon_tools_core::PermissionBehavior::Allow
        ));
    }

    #[tokio::test]
    async fn agent_tool_check_permissions_does_not_trust_writable_explore_override() {
        let registry = Arc::new(AgentRegistry::from_groups(AgentGroups {
            built_in: rebon_tool::builtin_agents::all_builtin_agents()
                .into_iter()
                .map(ResolvedAgentDef::from_builtin)
                .collect(),
            user: vec![ResolvedAgentDef {
                agent_type: "Explore".into(),
                when_to_use: "custom writable explore".into(),
                system_prompt: "custom writable explore".into(),
                tool_filter: ToolFilter::unrestricted(),
                model: None,
                model_profile: None,
                provider: None,
                effort: None,
                background: false,
                isolation: None,
                memory: None,
                permission_mode: None,
                runtime: AgentRuntime::Local,
                source: AgentSource::Settings(
                    rebon_tool::agent_registry::SettingSource::UserSettings,
                ),
                file_stem: None,
            }],
            ..Default::default()
        }));
        let tool = AgentTool::with_registry(registry);
        let dir_guard = tempfile::Builder::new()
            .prefix("rebon-agent-permission-custom-parent-")
            .tempdir()
            .unwrap();
        let dir = dir_guard.path().to_path_buf();
        let outside_guard = tempfile::Builder::new()
            .prefix("rebon-agent-permission-custom-outside-")
            .tempdir()
            .unwrap();
        let outside = outside_guard.path().to_path_buf();
        let context = ToolContext::new().with_cwd(dir.to_string_lossy());
        let decision = tool
            .check_permissions(
                &json!({
                    "prompt": "modify the sibling repository",
                    "subagent_type": "Explore",
                    "cwd": outside.to_string_lossy()
                }),
                &context,
            )
            .await
            .unwrap();
        assert!(matches!(
            decision.behavior,
            rebon_tools_core::PermissionBehavior::Ask
        ));
    }

    #[tokio::test]
    async fn agent_tool_spawns_teammate_when_name_and_team_are_present() {
        let session_id = "session-alpha";
        let manager = Arc::new(ScriptedTeamManager {
            result: TeammateSpawnResult {
                team_name: "alpha".into(),
                agent_id: "alice@alpha".into(),
                task_id: "tabc1234".into(),
                reused: false,
                handoff: None,
            },
            calls: Mutex::new(Vec::new()),
        });
        let tool = AgentTool::new();
        let parent_broker: Arc<dyn rebon_tool::PermissionBroker> =
            Arc::new(rebon_tool::DenyAskPermissionBroker);
        let context = ToolContext::new()
            .with_session_id(session_id)
            .with_team_manager(manager.clone() as Arc<dyn rebon_tool::TeamManager>)
            .with_permission_broker(parent_broker)
            .with_workflow_nesting_depth(1);

        let out = tool
            .call(
                json!({
                    "prompt": "implement the parser",
                    "name": "alice",
                    "team_name": "alpha",
                    "mode": "plan"
                }),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(out["status"], "teammate_spawned");
        assert_eq!(out["agent_id"], "alice@alpha");
        assert!(out.get("handoff").is_none());
        let calls = manager.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "alice");
        assert_eq!(calls[0].team_name, "alpha");
        assert_eq!(calls[0].parent_session_id, session_id);
        assert_eq!(calls[0].mode.as_deref(), Some("plan"));
        assert_eq!(calls[0].workflow_nesting_depth, 1);
        assert!(calls[0].permission_broker.is_some());
        assert!(calls[0].permission_prompts_unavailable);
        assert!(!calls[0].wait_for_completion);
        assert!(calls[0]
            .system
            .as_deref()
            .is_some_and(|system| system.contains(WORKFLOW_AGENT_SHARED_WORKTREE_PROMPT)));
    }

    #[tokio::test]
    async fn named_agent_rejects_ignored_execution_boundary_params() {
        let manager = Arc::new(ScriptedTeamManager {
            result: TeammateSpawnResult {
                team_name: "alpha".into(),
                agent_id: "alice@alpha".into(),
                task_id: "tabc1234".into(),
                reused: false,
                handoff: None,
            },
            calls: Mutex::new(Vec::new()),
        });
        let context = ToolContext::new()
            .with_session_id("session-alpha")
            .with_team_manager(manager.clone() as Arc<dyn rebon_tool::TeamManager>);

        let err = AgentTool::new()
            .call(
                json!({
                    "prompt": "implement the parser",
                    "name": "alice",
                    "team_name": "alpha",
                    "cwd": "F:/somewhere",
                    "isolation": "worktree",
                    "allowed_tools": ["Read"]
                }),
                &context,
            )
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidInput { reason, .. } => {
                for setting in ["cwd", "allowed_roots", "isolation", "allowed_tools"] {
                    assert!(reason.contains(setting), "{reason}");
                }
                assert!(
                    reason.contains("do not replace one with another"),
                    "{reason}"
                );
                assert!(reason.contains("exactly one form"), "{reason}");
                assert!(reason.contains("one-shot sub-agent"), "{reason}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }

        let err = AgentTool::new()
            .call(
                json!({
                    "prompt": "inspect the parser",
                    "name": "alice",
                    "team_name": "alpha",
                    "isolation": "worktree"
                }),
                &context,
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput { reason, .. } => {
                for setting in ["cwd", "allowed_roots", "isolation", "allowed_tools"] {
                    assert!(reason.contains(setting), "{reason}");
                }
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        assert!(
            manager.calls.lock().unwrap().is_empty(),
            "the teammate must not have been spawned"
        );
    }

    #[tokio::test]
    async fn named_agent_teammate_inherits_parent_cwd() {
        let manager = Arc::new(ScriptedTeamManager {
            result: TeammateSpawnResult {
                team_name: "alpha".into(),
                agent_id: "alice@alpha".into(),
                task_id: "tabc1234".into(),
                reused: false,
                handoff: None,
            },
            calls: Mutex::new(Vec::new()),
        });
        let context = ToolContext::new()
            .with_session_id("session-alpha")
            .with_team_manager(manager.clone() as Arc<dyn rebon_tool::TeamManager>)
            .with_cwd("F:/proj")
            .with_additional_working_directories(["F:/extra"]);

        AgentTool::new()
            .call(
                json!({
                    "prompt": "implement the parser",
                    "name": "alice",
                    "team_name": "alpha"
                }),
                &context,
            )
            .await
            .unwrap();

        let calls = manager.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].cwd.as_deref(), Some("F:/proj"));
        assert_eq!(
            calls[0].additional_working_directories,
            vec!["F:/extra".to_string()]
        );
    }

    #[tokio::test]
    async fn named_agent_without_team_uses_session_default_team() {
        let _home = TestConfigHome::new("agent-default-team");
        let session_id = "session-default-team";
        let team_name = rebon_tool::default_team_name(session_id);
        let manager = Arc::new(ScriptedTeamManager {
            result: TeammateSpawnResult {
                team_name: team_name.clone(),
                agent_id: rebon_tool::format_agent_id("researcher", &team_name),
                task_id: "tdefault01".into(),
                reused: false,
                handoff: None,
            },
            calls: Mutex::new(Vec::new()),
        });
        let context = ToolContext::new()
            .with_session_id(session_id)
            .with_team_manager(manager.clone() as Arc<dyn rebon_tool::TeamManager>);

        let out = AgentTool::new()
            .call(
                json!({
                    "prompt": "inspect the parser",
                    "name": "researcher",
                    "subagent_type": "Explore"
                }),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(out["status"], "teammate_spawned");
        assert_eq!(out["team_name"], "default");
        assert!(out.get("handoff").is_none());
        let team = rebon_tool::read_team_file(&team_name).unwrap().unwrap();
        assert_eq!(team.lead_session_id.as_deref(), Some(session_id));
        assert_eq!(team.description.as_deref(), Some("Session default team"));

        let calls = manager.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].team_name, team_name);
        assert_eq!(calls[0].name, "researcher");
        assert_eq!(calls[0].agent_type.as_deref(), Some("Explore"));
        assert_eq!(calls[0].parent_session_id, session_id);
        assert!(calls[0].agent_type_explicit);
        assert!(!calls[0].wait_for_completion);
    }

    #[tokio::test]
    async fn named_agent_can_explicitly_wait_for_handoff() {
        let _home = TestConfigHome::new("agent-explicit-foreground");
        let manager = Arc::new(ScriptedTeamManager {
            result: TeammateSpawnResult {
                team_name: "alpha".into(),
                agent_id: "alice@alpha".into(),
                task_id: "tforeground".into(),
                reused: false,
                handoff: Some(TeammateHandoff {
                    status: "completed".into(),
                    summary: "done".into(),
                    error: None,
                }),
            },
            calls: Mutex::new(Vec::new()),
        });
        let context = ToolContext::new()
            .with_session_id("session-explicit-foreground")
            .with_team_manager(manager.clone() as Arc<dyn rebon_tool::TeamManager>);

        let out = AgentTool::new()
            .call(
                json!({
                    "prompt": "inspect the parser",
                    "name": "alice",
                    "team_name": "alpha",
                    "run_in_background": false
                }),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(out["handoff"]["status"], "completed");
        assert_eq!(out["handoff"]["summary"], "done");
        let calls = manager.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].wait_for_completion);
    }

    #[tokio::test]
    async fn named_agent_keeps_current_explicit_team_for_same_session() {
        let _home = TestConfigHome::new("agent-current-team");
        let session_id = "session-current-team";
        let team_name = "explicit-team";
        rebon_tool::write_team_file(
            team_name,
            &rebon_tool::TeamFile {
                name: team_name.into(),
                description: Some("Explicit team".into()),
                created_at: rebon_tool::team_files::now_wall_ms(),
                lead_agent_id: rebon_tool::format_agent_id("team-lead", team_name),
                lead_session_id: Some(session_id.into()),
                hidden_pane_ids: Vec::new(),
                members: Vec::new(),
            },
        )
        .unwrap();
        rebon_tool::set_current_team_name("foreign-team");

        let manager = Arc::new(ScriptedTeamManager {
            result: TeammateSpawnResult {
                team_name: team_name.into(),
                agent_id: rebon_tool::format_agent_id("researcher", team_name),
                task_id: "texplicit01".into(),
                reused: false,
                handoff: None,
            },
            calls: Mutex::new(Vec::new()),
        });
        let context = ToolContext::new()
            .with_session_id(session_id)
            .with_team_manager(manager.clone() as Arc<dyn rebon_tool::TeamManager>);

        AgentTool::new()
            .call(
                json!({
                    "prompt": "inspect the parser",
                    "name": "researcher"
                }),
                &context,
            )
            .await
            .unwrap();

        let calls = manager.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].team_name, team_name);
        assert!(
            rebon_tool::read_team_file(&rebon_tool::default_team_name(session_id))
                .unwrap()
                .is_none()
        );
        drop(calls);
    }

    #[tokio::test]
    async fn ordinary_named_teammate_keeps_legacy_system_and_permission_context() {
        let _home = TestConfigHome::new("agent-legacy-no-session");
        let manager = Arc::new(ScriptedTeamManager {
            result: TeammateSpawnResult {
                team_name: "beta".into(),
                agent_id: "bob@beta".into(),
                task_id: "tlegacy01".into(),
                reused: false,
                handoff: None,
            },
            calls: Mutex::new(Vec::new()),
        });
        let context = ToolContext::new()
            .with_team_manager(manager.clone() as Arc<dyn rebon_tool::TeamManager>);

        AgentTool::new()
            .call(
                json!({
                    "prompt": "inspect the parser",
                    "name": "bob",
                    "team_name": "beta"
                }),
                &context,
            )
            .await
            .unwrap();

        let calls = manager.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].workflow_nesting_depth, 0);
        assert!(calls[0].system.is_none());
        assert!(calls[0].permission_broker.is_none());
        assert!(!calls[0].permission_prompts_unavailable);
    }

    #[tokio::test]
    async fn stale_current_team_binding_falls_back_to_standalone_spawn() {
        let _home = TestConfigHome::new("agent-stale-no-session");
        set_current_team_name(&format!("stale-ghost-team-{}", std::process::id()));
        let manager = Arc::new(ScriptedTeamManager {
            result: TeammateSpawnResult {
                team_name: "unused".into(),
                agent_id: "unused".into(),
                task_id: "unused".into(),
                reused: false,
                handoff: None,
            },
            calls: Mutex::new(Vec::new()),
        });
        let spawner = Arc::new(ScriptedSpawner::new(success_result()));
        let context = ToolContext::new()
            .with_team_manager(manager.clone() as Arc<dyn rebon_tool::TeamManager>)
            .with_sub_agent_spawner(spawner.clone() as Arc<dyn SubAgentSpawner>);

        AgentTool::new()
            .call(
                json!({
                    "prompt": "inspect the parser",
                    "name": "bob"
                }),
                &context,
            )
            .await
            .unwrap();

        assert!(
            manager.calls.lock().unwrap().is_empty(),
            "stale team binding must not route a named spawn to the team manager"
        );
        assert!(spawner.calls.lock().unwrap().is_empty());
        assert_eq!(spawner.background_calls.lock().unwrap().len(), 1);
    }

    // --- sub-agent enable toggle ---------------------------------
    //
    // These tests touch the `SUB_AGENTS_ENABLED` atomic which is
    // process-global. Rust test binaries run tests in parallel by
    // default, so if another test flipped the flag we'd see flakes.
    // Guard them with a shared Mutex and always restore the previous
    // value on drop so test ordering does not matter.
    use std::sync::OnceLock;
    static TOGGLE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            set_sub_agents_enabled(self.0);
        }
    }

    #[test]
    fn sub_agents_enabled_defaults_to_true() {
        let _guard = TOGGLE_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _restore = Restore(sub_agents_enabled());
        set_sub_agents_enabled(true);
        assert!(sub_agents_enabled());
    }

    #[test]
    fn sub_agents_disable_hides_agent_tool() {
        let _guard = TOGGLE_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _restore = Restore(sub_agents_enabled());
        let tool = AgentTool::new();

        set_sub_agents_enabled(true);
        assert!(tool.is_enabled(), "AgentTool should be enabled by default");

        set_sub_agents_enabled(false);
        assert!(
            !tool.is_enabled(),
            "AgentTool should become disabled after set_sub_agents_enabled(false)"
        );

        set_sub_agents_enabled(true);
        assert!(tool.is_enabled(), "AgentTool should re-enable after flip");
    }

    /// Stands in for the `memory` plugin behind the `agent-memory-prompt`
    /// seam. What the memory prompt says is that plugin's business and is
    /// tested there; what this crate owns is the concatenation rule and the
    /// three ways an injection is skipped.
    struct FakeAgentMemory;

    impl rebon_instructions::agent_documents::AgentMemoryPrompt for FakeAgentMemory {
        fn agent_memory_prompt(
            &self,
            agent_type: &str,
            scope: &str,
            _cwd: &std::path::Path,
        ) -> Option<String> {
            ["user", "project", "local"]
                .contains(&scope)
                .then(|| format!("MEMORY({agent_type}, {scope})"))
        }
    }

    /// A kernel with the seam provided, and the scope to resolve through.
    /// The kernel is returned so it outlives the scope.
    fn kernel_with_memory() -> (Arc<rebon_kernel::Kernel>, rebon_kernel::Context) {
        let kernel = rebon_kernel::Kernel::new();
        let ctx = kernel.context().fork("memory");
        ctx.provide::<rebon_instructions::agent_documents::AgentMemoryPromptService>(Arc::new(
            FakeAgentMemory,
        ))
        .expect("a fresh scope accepts the provider");
        (kernel, ctx)
    }

    #[test]
    fn inject_memory_noop_when_scope_is_none() {
        let tmp = std::env::temp_dir().join(format!("rebon-inject-noop-{}", std::process::id()));
        let (_kernel, ctx) = kernel_with_memory();
        let out =
            super::inject_agent_memory_raw(Some(&ctx), "coder", None, Some("BASE".into()), &tmp);
        assert_eq!(out, Some("BASE".into()));
    }

    #[test]
    fn inject_memory_appends_after_existing_system() {
        let tmp = std::env::temp_dir().join(format!("rebon-inject-append-{}", std::process::id()));
        let (_kernel, ctx) = kernel_with_memory();
        let out = super::inject_agent_memory_raw(
            Some(&ctx),
            "coder",
            Some("project"),
            Some("BASE".into()),
            &tmp,
        )
        .expect("memory scope should produce a prompt");
        assert_eq!(out, "BASE\n\nMEMORY(coder, project)");
    }

    #[test]
    fn inject_memory_returns_prompt_alone_when_no_existing_system() {
        let tmp = std::env::temp_dir().join(format!("rebon-inject-solo-{}", std::process::id()));
        let (_kernel, ctx) = kernel_with_memory();
        let out = super::inject_agent_memory_raw(Some(&ctx), "coder", Some("local"), None, &tmp)
            .expect("memory scope should produce a prompt");
        assert_eq!(out, "MEMORY(coder, local)");
    }

    #[test]
    fn inject_memory_ignores_unknown_scope() {
        let tmp = std::env::temp_dir().join(format!("rebon-inject-bad-{}", std::process::id()));
        let (_kernel, ctx) = kernel_with_memory();
        let out = super::inject_agent_memory_raw(
            Some(&ctx),
            "coder",
            Some("global"),
            Some("BASE".into()),
            &tmp,
        );
        assert_eq!(out, Some("BASE".into()));
    }

    /// With no provider behind the seam — the `memory` plugin switched off,
    /// or a kernel-less host — a declared scope is skipped rather than
    /// failing the spawn.
    #[test]
    fn inject_memory_is_skipped_without_a_provider() {
        let tmp = std::env::temp_dir().join(format!("rebon-inject-none-{}", std::process::id()));
        assert_eq!(
            super::inject_agent_memory_raw(
                None,
                "coder",
                Some("project"),
                Some("BASE".into()),
                &tmp
            ),
            Some("BASE".into())
        );
        let kernel = rebon_kernel::Kernel::new();
        assert_eq!(
            super::inject_agent_memory_raw(
                Some(kernel.context()),
                "coder",
                Some("project"),
                Some("BASE".into()),
                &tmp
            ),
            Some("BASE".into())
        );
    }

    #[test]
    fn sub_agents_toggle_is_process_wide() {
        let _guard = TOGGLE_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _restore = Restore(sub_agents_enabled());

        // Two independently-constructed AgentTool instances share the
        // same atomic — this is deliberate: the user toggles one flag,
        // every registered AgentTool observes it.
        let a = AgentTool::new();
        let b = AgentTool::new();

        set_sub_agents_enabled(false);
        assert!(!a.is_enabled());
        assert!(!b.is_enabled());

        set_sub_agents_enabled(true);
        assert!(a.is_enabled());
        assert!(b.is_enabled());
    }
}
