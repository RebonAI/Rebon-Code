//! `QueryEngine` — the agentic tool-use loop.
//!
//! The subset of an agentic query loop the harness actually needs to
//! drive a real prompt turn:
//!
//! 1. Call the injected [`ModelClient`] with the current history.
//! 2. Walk the returned stream, forwarding every raw [`StreamEvent`]
//!    so an observer (the ACP publisher) can emit partial updates.
//! 3. Accumulate the stream into a final
//!    [`AssistantMessage`](rebon_api::AssistantMessage).
//! 4. If the stop reason is [`StopReason::ToolUse`], dispatch each
//!    tool_use block through [`Engine::invoke_tool`], collect the
//!    results, append a user-role message carrying the
//!    `tool_result` blocks, and loop.
//! 5. Otherwise emit [`QueryEvent::Done`] and finish.
//!
//! The loop terminates after [`QueryParams::max_iterations`]
//! iterations, on a cancel signal, or on the first non-tool_use
//! stop reason. Errors from the model client or from tool execution
//! are surfaced as [`QueryEvent::Error`] and end the loop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use async_trait::async_trait;
use futures_util::{future::BoxFuture, FutureExt, StreamExt};
use rebon_agent_core::file_history::FileHistoryTracker;
use rebon_agent_core::{
    DenialReplayRequest, PromptExecutor, PromptExecutorError, PromptOutcome, PromptRequest,
    SessionUpdatePublisher, SkillInvocationRequest,
};
use rebon_api::{
    AssistantMessage, CacheMissReason, CacheTraceContext, CompactProvider,
    ContentBlock as ApiContentBlock, CreateMessageRequest, DocumentBlock, ImageBlock,
    Message as ApiMessage, MessageAccumulator, ModelClient, PruneLevelHandle, Role, SessionHandle,
    StopReason, StreamEvent, TextBlock, ThinkingConfig, Tool as ApiTool, ToolChoice,
    ToolResultBlock, ToolResultContent, ToolResultContentBlock, ToolUseBlock, Usage,
    TOOL_RESULT_CLEARED,
};
use rebon_proto::McpServerConfig;
use rebon_session::{TranscriptEntry, TranscriptWriteEntry};
use rebon_session_state::{
    extract_locations, tool_result_update_content, trim_raw_output_for_transcript, ServerState,
};
use rebon_tool::{
    ContextShareMode, FileContextMode, FrozenParentContextCapsule, McpClient, McpToolDefinition,
    PermissionBroker, SharedToolFilter, SubAgentSpawner, TaskRuntimeController, TeamManager,
    ToolContext, ToolFilter, ToolResultMode, WebSearchDelegate, WEB_SEARCH_TOOL_NAME,
};
use rebon_tools_core::{
    FileStateCache, ToolError, ToolErrorPresentation, ToolId, ToolProgressUpdate, ToolResult,
};
use rebon_types::{
    AgentCapabilityMode, ContentBlock as AcpContentBlock, ExecutionPolicy, SessionUpdate,
    StopReason as AcpStopReason, TextContent, ToolCallContent, ToolCallLocation, ToolCallStatus,
    ToolKind, UltraplanRunState,
};

use crate::context_accounting::{
    estimate_messages_input_tokens, prompt_messages_report, prompt_text_section_report,
    prompt_tool_metadata_report, PromptCostReport,
};
use crate::context_manager::{truncate_messages_for_token_budget, ContextManager};
use crate::hooks::{HookedPermissionBroker, PreToolUseDecision};
use crate::policy_seat::{PolicySources, PolicySourcesResolver};
use crate::system_prompt::{DynamicPromptContext, SystemPromptConfig};
use crate::turn_hook::TurnHooks;

use serde_json::Value;
use std::path::PathBuf;
use tokio::sync::{mpsc, Notify};

use crate::Engine;

mod compact;
mod executor;
mod file_history;
mod formatting;
mod model_routing;
mod parent_context;
mod prompt;
mod replay_window;
mod runtime_model;
mod session_prompt;
mod stream;
#[cfg(test)]
mod tests;
mod tool_projection;
mod traits;
mod transcript;
mod turn_control;

use compact::*;
use file_history::*;
use formatting::*;
use parent_context::*;
pub(crate) use prompt::stable_base_system_enabled;
use prompt::*;
// The turn-control plugin owns cross-cutting policy and the public entry point.
use runtime_model::*;
use session_prompt::*;
use stream::*;
use tool_projection::*;
pub(crate) use transcript::model_message_for_attachment;
use transcript::*;
pub(crate) use turn_control::attachment_repeats_history;
use turn_control::dispatch_tool_use;
#[cfg(test)]
use turn_control::MAX_TRANSIENT_REPLAYS;

pub use compact::{ManualCompactReport, MANUAL_COMPACT_PROTECTED_TURNS};
pub use executor::{
    EngineQueryExecutor, FileUltraplanRunRepository, KernelSessionContextResolver,
    ResumeReplayHandle, SessionExecutionRuntime,
};
pub use prompt::build_request_prompt_cost_report;
pub use replay_window::PreparedResumeSummary;
pub use runtime_model::{RuntimeModelConfig, SharedRuntimeModel, SystemPromptSnapshot};
pub use tool_projection::{
    eager_tools_from_engine, eager_tools_from_engine_for_policy, filtered_eager_tools_from_engine,
    filtered_eager_tools_from_engine_for_policy, filtered_tools_from_engine, tools_from_engine,
    ToolNameSnapshot, ToolSnapshot,
};
pub use traits::{AttachmentPollPhase, AttachmentPollRequest, AttachmentPoller};
pub use transcript::visible_runtime_attachment_message;
pub use transcript::{estimate_transcript_input_tokens, transcript_to_api_messages};
pub use turn_control::run_query;

/// A query's poller paired with the exact session and turn it is serving.
#[derive(Clone)]
pub struct AttachmentPollerBinding {
    pub(crate) poller: Arc<dyn AttachmentPoller>,
    pub(crate) session_id: String,
    pub(crate) turn_id: String,
}

impl AttachmentPollerBinding {
    pub fn new(
        poller: Arc<dyn AttachmentPoller>,
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
    ) -> Self {
        Self {
            poller,
            session_id: session_id.into(),
            turn_id: turn_id.into(),
        }
    }

    pub(crate) fn request(
        &self,
        next_iteration: u64,
        phase: AttachmentPollPhase,
    ) -> AttachmentPollRequest<'_> {
        AttachmentPollRequest {
            session_id: &self.session_id,
            turn_id: &self.turn_id,
            next_iteration,
            phase,
        }
    }
}

impl std::fmt::Debug for AttachmentPollerBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentPollerBinding")
            .field("session_id", &self.session_id)
            .field("turn_id", &self.turn_id)
            .finish_non_exhaustive()
    }
}

/// Request state used to promote an Anchored Minimal bootstrap without
/// storing provider-specific session state outside durable history.
#[derive(Clone)]
pub struct AnchoredMinimalPromotion {
    pub tools: Vec<ApiTool>,
    pub tool_search_index: Arc<rebon_tool::ToolSearchIndex>,
    pub runtime_context_message: Option<String>,
    pub transient_context_message: Option<String>,
    pub attachment_poller: Option<AttachmentPollerBinding>,
    pub max_tokens: u32,
}

/// Input for [`run_query`].
#[derive(Clone)]
pub struct QueryParams {
    /// Target model identifier.
    pub model: String,
    /// System prompt to send as top-level provider system/instructions.
    pub system: Option<String>,
    /// Stable runtime context to virtually inject near the start of
    /// the provider request.
    pub runtime_context_message: Option<String>,
    /// Volatile runtime context to append to the current request tail,
    /// without persisting it into durable history.
    pub transient_context_message: Option<String>,
    /// Initial history. The first call adds any new user prompt on
    /// top of this list, so callers that want to *extend* an
    /// existing transcript should push their new user message on
    /// before invoking `run_query`.
    pub messages: Vec<ApiMessage>,
    /// Tool definitions to expose to the model. Typically
    /// constructed from [`Engine::tool_names`] + `Tool::description`
    /// + `Tool::input_schema`; see [`tools_from_engine`].
    pub tools: Vec<ApiTool>,
    /// `max_tokens` forwarded to [`CreateMessageRequest::max_tokens`].
    pub max_tokens: u32,
    /// Cap on agentic iterations. When reached, the loop emits
    /// [`QueryEvent::IterationLimitReached`] and exits.
    pub max_iterations: usize,
    /// Session capability mode controlling the initial prompt and tool exposure.
    pub capability_mode: AgentCapabilityMode,
    /// Follow-up request shape for providers that opt into Anchored Minimal.
    pub anchored_minimal_promotion: Option<AnchoredMinimalPromotion>,
    /// Optional per-iteration attachment injector.
    ///
    /// When set, the turn controller calls [`AttachmentPoller::poll`] eagerly before
    /// iteration zero and before an otherwise terminal response, and regularly
    /// after each completed tool round. Returned messages are appended to
    /// [`Self::messages`] before the next `create_message_stream` call.
    /// Idiomatic implementations close over a [`ServerState`] and use the
    /// request's exact session and turn identity to decide what to emit.
    pub attachment_poller: Option<AttachmentPollerBinding>,
    /// Ordered, disposable subscribers for raw query events and turn writeback.
    pub turn_hooks: TurnHooks,
    /// One-shot forced tool choice for the next provider request.
    pub next_tool_choice: Option<ToolChoice>,
    /// Per-session feature state, keyed by type.
    ///
    /// The same bag this turn's [`ToolContext`] carries. A feature whose code
    /// lives in a plugin puts one value in it and both readers find it there:
    /// the tool that reads it during a call, and the turn subscriber that
    /// reads it after a completed tool round. Empty on a host that wires no
    /// such feature, which is what makes those subscribers no-ops rather than
    /// failures.
    pub extensions: rebon_tool::Extensions,
    /// Extended thinking / reasoning configuration forwarded to the
    /// model provider. `None` means disabled.
    pub thinking: Option<ThinkingConfig>,
    /// Reasoning effort for OpenAI. `None` means server default.
    pub reasoning_effort: Option<rebon_api::ReasoningEffort>,
    /// Reasoning mode for OpenAI (gpt-5.6+ pro mode). `None` means
    /// standard mode.
    pub reasoning_mode: Option<rebon_api::ReasoningMode>,
    /// Web search tool configuration. When `Some`, the model provider
    /// injects its native web search tool and handles results
    /// server-side (no client tool dispatch needed).
    pub web_search: Option<rebon_api::WebSearchToolConfig>,
    /// Anthropic server-side context management configuration.
    pub context_management: Option<rebon_api::ContextManagementConfig>,
    /// Shared context-prune handle. When set, `TurnControlPlugin` reports
    /// token usage after each iteration and triggers active
    /// compaction (truncation + session reset) when the context
    /// budget threshold is exceeded — mirroring codex-ref's
    /// mid-turn auto-compact.
    pub prune_level: Option<PruneLevelHandle>,
    /// Optional model-based compact provider. When set AND the
    /// context budget threshold is exceeded, `TurnControlPlugin` calls
    /// this provider to generate a summarised replacement history
    /// instead of falling back to simple truncation.
    ///
    /// For OpenAI Responses: [`rebon_api::RemoteCompactProvider`]
    /// calls `/responses/compact`.
    /// For Anthropic / others: [`rebon_api::ModelCompactProvider`]
    /// calls the caller-resolved small-profile model.
    pub compact_provider: Option<Arc<dyn CompactProvider>>,
    /// Optional fallback compact provider tried when [`Self::compact_provider`] fails.
    pub compact_fallback_provider: Option<Arc<dyn CompactProvider>>,
    /// Optional extra instructions appended to the compaction prompt.
    pub compact_custom_instructions: Option<String>,
    /// Controls how the compacted summary should be shaped.
    pub compact_summary_options: rebon_api::CompactSummaryOptions,
    /// Shared per-session file-state cache. Read registers observed files;
    /// Edit / Write consult it to enforce "must read first".
    pub file_state_cache: Option<FileStateCache>,
    /// Optional request-scoped execution policy for this query turn.
    pub execution_policy: Option<ExecutionPolicy>,
    /// Run-scoped invariant execution policy. Unlike request-scoped
    /// `execution_policy`, context reset/microcompact must preserve this (for
    /// example workflow schema agents' eager StructuredOutput promotion).
    pub invariant_execution_policy: Option<ExecutionPolicy>,
    /// Session/base tool filter before request-scoped policy is applied.
    ///
    /// Context reset exits the request-scoped plan turn and should restore this
    /// base visibility instead of carrying the plan-only tool filter forward.
    pub base_tool_filter: Option<ToolFilter>,
    /// Effective model-visible tool filter after session and request
    /// policy filters have been intersected.
    pub effective_tool_filter: Option<ToolFilter>,
    /// System prompt to restore after a context reset exits the current
    /// request-scoped policy.
    pub post_context_reset_system: Option<String>,
    /// Stable runtime context to restore after a context reset exits the
    /// current request-scoped policy.
    pub post_context_reset_runtime_context_message: Option<String>,
    /// Volatile runtime context to restore after a context reset exits the
    /// current request-scoped policy.
    pub post_context_reset_transient_context_message: Option<String>,
    /// The policy-event subscribers this turn asks: the process seat plus
    /// this session's own. Empty means nobody is asked and every event
    /// resolves to `Allow`.
    pub policy: PolicySources,
    /// Request-shape diagnostics and provider cache-key hints for cache tracing.
    pub cache_trace_context: Option<CacheTraceContext>,
    pub mcp_tool_definitions: Vec<(String, McpToolDefinition)>,
}

impl std::fmt::Debug for QueryParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryParams")
            .field("model", &self.model)
            .field("system", &self.system.is_some())
            .field(
                "runtime_context_message",
                &self.runtime_context_message.is_some(),
            )
            .field(
                "transient_context_message",
                &self.transient_context_message.is_some(),
            )
            .field("messages", &self.messages.len())
            .field("tools", &self.tools.len())
            .field("max_tokens", &self.max_tokens)
            .field("max_iterations", &self.max_iterations)
            .field("capability_mode", &self.capability_mode)
            .field(
                "has_anchored_minimal_promotion",
                &self.anchored_minimal_promotion.is_some(),
            )
            .field("has_execution_policy", &self.execution_policy.is_some())
            .field(
                "has_invariant_execution_policy",
                &self.invariant_execution_policy.is_some(),
            )
            .field("has_base_tool_filter", &self.base_tool_filter.is_some())
            .field(
                "has_effective_tool_filter",
                &self.effective_tool_filter.is_some(),
            )
            .field(
                "has_post_context_reset_system",
                &self.post_context_reset_system.is_some(),
            )
            .field(
                "has_post_context_reset_runtime_context_message",
                &self.post_context_reset_runtime_context_message.is_some(),
            )
            .field(
                "has_post_context_reset_transient_context_message",
                &self.post_context_reset_transient_context_message.is_some(),
            )
            .field("has_turn_hook_seat", &self.turn_hooks.has_seat())
            .field("has_attachment_poller", &self.attachment_poller.is_some())
            .finish()
    }
}

impl QueryParams {
    /// Sensible defaults: no system prompt, 4096 max_tokens, 600
    pub fn new(model: impl Into<String>, messages: Vec<ApiMessage>) -> Self {
        Self {
            model: model.into(),
            system: None,
            runtime_context_message: None,
            transient_context_message: None,
            messages,
            tools: Vec::new(),
            max_tokens: 4096,
            max_iterations: 600,
            capability_mode: AgentCapabilityMode::Normal,
            anchored_minimal_promotion: None,
            attachment_poller: None,
            turn_hooks: TurnHooks::default(),
            next_tool_choice: None,
            extensions: rebon_tool::Extensions::default(),
            thinking: None,
            reasoning_effort: None,
            reasoning_mode: None,
            web_search: None,
            context_management: None,
            prune_level: None,
            compact_provider: None,
            compact_fallback_provider: None,
            compact_custom_instructions: None,
            compact_summary_options: rebon_api::CompactSummaryOptions::default(),
            file_state_cache: None,
            execution_policy: None,
            invariant_execution_policy: None,
            base_tool_filter: None,
            effective_tool_filter: None,
            post_context_reset_system: None,
            post_context_reset_runtime_context_message: None,
            post_context_reset_transient_context_message: None,
            policy: PolicySources::default(),
            cache_trace_context: None,
            mcp_tool_definitions: Vec::new(),
        }
    }

    pub fn with_capability_mode(mut self, capability_mode: AgentCapabilityMode) -> Self {
        self.capability_mode = capability_mode;
        self
    }

    /// Attach the policy-event subscribers for query/tool lifecycle events.
    pub fn with_policy(mut self, policy: PolicySources) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_mcp_tool_definitions(
        mut self,
        definitions: Vec<(String, McpToolDefinition)>,
    ) -> Self {
        self.mcp_tool_definitions = definitions;
        self
    }

    /// Attach a shared [`FileStateCache`] (builder-style).
    pub fn with_file_state_cache(mut self, cache: FileStateCache) -> Self {
        self.file_state_cache = Some(cache);
        self
    }

    /// Attach a request-scoped execution policy (builder-style).
    pub fn with_execution_policy(mut self, policy: ExecutionPolicy) -> Self {
        self.execution_policy = Some(policy);
        self
    }

    /// Attach a per-iteration attachment poller with its query identity.
    pub fn with_attachment_poller(
        mut self,
        poller: Arc<dyn AttachmentPoller>,
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
    ) -> Self {
        self.attachment_poller = Some(AttachmentPollerBinding::new(poller, session_id, turn_id));
        self
    }

    /// Attach a system prompt (builder-style).
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Attach a tool list (builder-style).
    pub fn with_tools(mut self, tools: Vec<ApiTool>) -> Self {
        self.tools = tools;
        self
    }

    /// Override `max_iterations` (builder-style).
    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    /// Attach ordered query-event subscribers for this turn.
    pub fn with_turn_hook_seat(mut self, seat: Arc<crate::turn_hook::TurnHookSeat>) -> Self {
        self.turn_hooks = self.turn_hooks.with_seat(seat);
        self
    }

    /// Attach one query-local subscriber to the turn hook pipeline.
    pub fn with_turn_hook(
        mut self,
        id: impl Into<String>,
        order: crate::turn_hook::Order,
        hook: Arc<dyn crate::turn_hook::TurnHook>,
    ) -> Self {
        self.turn_hooks = self.turn_hooks.with_hook(id, order, hook);
        self
    }
}

/// Event emitted by [`run_query`] as the turn progresses.
#[derive(Debug)]
pub enum QueryEvent {
    /// Raw forwarded stream event from the model. ACP publishers
    /// consume these to emit partial `session/update` notifications.
    Stream(StreamEvent),
    /// The current iteration finished with a final assistant message.
    IterationComplete {
        /// Iteration number (0-indexed).
        iteration: usize,
        /// Fully accumulated message.
        message: AssistantMessage,
    },
    /// A tool_use block is about to be dispatched.
    ToolDispatchStart {
        /// Tool use id (`toolu_…`).
        tool_use_id: String,
        /// Registered tool name the model requested.
        name: String,
        /// Input the model sent.
        input: Value,
    },
    /// A tool call cleared the auto-mode gate without a permission
    /// dialog. Display-only: clients annotate the tool row with it, and
    /// the note never becomes model-visible transcript content.
    ToolAutoModeAllowed {
        /// Tool use id (`toolu_…`) the auto-mode gate let through.
        tool_use_id: String,
        /// Which part of the gate decided, so the row can attribute it.
        source: rebon_types::AutoModeAllowSource,
    },
    /// A tool emitted a progress update while executing.
    ToolDispatchProgress {
        /// Tool use id.
        tool_use_id: String,
        /// Registered tool name.
        name: String,
        /// Progress payload emitted by the tool runtime.
        progress: ToolProgressUpdate,
    },
    /// A tool finished executing.
    ToolDispatchResult {
        /// Tool use id.
        tool_use_id: String,
        /// Registered tool name.
        name: String,
        /// Outcome — either the tool's JSON output or an error
        /// string surfaced back to the model as `is_error: true`.
        outcome: Result<Value, String>,
        /// Structured presentation for failed calls. The model receives
        /// `model_message`; clients should render `display_message`.
        error_presentation: Option<ToolErrorPresentation>,
    },
    /// The loop reached [`QueryParams::max_iterations`] without the
    /// model terminating the turn.
    IterationLimitReached {
        /// Iteration count when the limit was hit.
        iterations: usize,
    },
    /// The attachment poller injected a message between iterations.
    /// Emitted once per message the poller returned, right before
    /// the next model request is built. Consumers (TUI, ACP update
    /// publishers) that want to show the model-visible injection
    /// stream can forward these; consumers that only care about the
    /// final assistant output can ignore them.
    AttachmentInjected {
        /// Iteration the message is being injected ahead of (1-based).
        iteration: usize,
        /// The full user-role message that was appended to history.
        message: ApiMessage,
    },
    /// The attachment poller requested a full context reset (e.g.
    /// ExitPlanMode with clear context). `TurnControlPlugin::run` ends
    /// the current controller drive, resets the provider session, and starts
    /// another drive with the provided messages as the initial conversation.
    /// The outer query task remains alive throughout the reset.
    ContextReset {
        /// Fresh messages to use for the new controller drive (typically a
        /// single "Implement the following plan" user message).
        messages: Vec<ApiMessage>,
        /// Raw plan text extracted from the leading user message,
        /// used by clients to render visible plan content in the
        /// cleared transcript so the user sees
        /// what's being executed instead of a blank screen.
        plan: Option<String>,
    },
    /// Active compaction started. The TUI should show a "Compacting"
    /// spinner verb while compaction is in progress.
    CompactingStarted {
        /// Number of messages before compaction.
        messages_before: usize,
    },
    /// Active compaction finished. The TUI should inject a system
    /// message and resume the normal spinner verb.
    CompactingFinished {
        /// Number of messages after compaction.
        messages_after: usize,
        /// Whether model-based summarisation was used (vs truncation).
        used_model: bool,
    },
    /// The loop was cancelled via the caller-supplied cancel handle.
    Cancelled,
    /// A tool requires permission from the user. Routed through the
    /// query-event channel so it arrives at the executor AFTER any
    /// preceding `ToolDispatchStart` for the same tool, preventing
    /// the race where the permission dialog appears in the TUI before
    /// the tool-call event.
    PermissionQuery(crate::permission::OutboundPermissionQuery),
    /// Hard error from the model or a tool.
    Error(String),
    /// Final successful completion.
    Done {
        /// Final assistant message.
        final_message: AssistantMessage,
        /// Final stop reason.
        stop_reason: StopReason,
        /// Cumulative usage across every iteration.
        total_usage: Usage,
    },
}

impl QueryEvent {
    /// Whether this event marks a terminal state (the caller can
    /// stop consuming the receiver after observing one).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Done { .. }
                | Self::Cancelled
                | Self::Error(_)
                | Self::IterationLimitReached { .. }
                | Self::ContextReset { .. }
        )
    }
}

/// Observer invoked with every [`QueryEvent`] as it flows through
/// [`EngineQueryExecutor::execute`], right before the executor consumes it.
///
/// Unlike the ACP `SessionUpdatePublisher` (which only sees a lossy UI
/// projection — tool *titles* and *kinds*, not the registered tool name),
/// the observer sees the raw events with the exact tool name, input, and
/// outcome. `rebon exec --json` uses it to emit a machine-readable JSONL
/// event stream for eval harnesses (e.g. NiceEval).
///
/// The callback runs synchronously in the ordered turn-hook event pipeline.
/// Dispatch and channel insertion share one serialization lock, so invocations
/// never interleave and observe the same event order as the public receiver.
#[derive(Clone)]
pub struct QueryEventObserver(Arc<dyn Fn(&QueryEvent) + Send + Sync>);

impl QueryEventObserver {
    /// Wrap a callback into an observer.
    pub fn new(f: impl Fn(&QueryEvent) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    /// Invoke the observer with one event.
    pub(crate) fn call(&self, event: &QueryEvent) {
        (self.0)(event)
    }
}

impl std::fmt::Debug for QueryEventObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("QueryEventObserver(..)")
    }
}

/// Caller-controlled cancel handle.
///
/// Re-export of [`rebon_agent_core::PromptCancel`] so the engine's query
/// loop and the ACP prompt executor share exactly the same cancel
/// primitive. The ACP server trips the handle on `session/cancel`;
/// the query loop observes it between stream chunks.
pub use rebon_types::PromptCancel as CancelToken;

// Keep `Notify` in scope for any future plumbing that wants its own
// handle; the cancel token itself no longer uses it directly.
#[allow(dead_code)]
type _NotifyHandle = Arc<Notify>;
