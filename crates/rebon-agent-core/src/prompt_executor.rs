//! `PromptExecutor` trait — the injection point that turns a
//! `session/prompt` request into a real model turn.
//!
//! The ACP server and the actual agentic loop (model client + tool
//! dispatch + transcript persistence) do not live in this crate. Instead
//! of folding that downstream work back into the transport layer, this
//! crate exposes a trait that any caller can implement — in production
//! the local engine, in tests a simple scripted stub.
//!
//! The trait is invoked by the front end once an active-prompt slot has
//! been acquired. The executor owns the turn from that point until it
//! returns a [`PromptOutcome`], at which point the slot is released and
//! the `session/prompt` response is sent back to the client.

use std::sync::Arc;

use async_trait::async_trait;

use crate::publisher::{ChannelPermissionRequestPublisher, SessionUpdatePublisher};
use rebon_proto::types::McpServerConfig;
use rebon_types::{ContentBlock, ExecutionPolicy, StopReason, Usage};
use serde::{Deserialize, Serialize};

pub use rebon_types::PromptCancel;

/// Replay payload emitted by `/permissions retry <id>`.
///
/// This type intentionally lives at the prompt-executor boundary: slash command
/// parsing can carry replay intent without knowing how tools are executed, and
/// engine implementations can replay through their normal tool dispatch,
/// transcript, and update-publishing paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DenialReplayRequest {
    pub denial_id: String,
    pub tool_use_id: String,
    pub tool_name: String,
    pub tool_input: String,
    pub reason: String,
    pub task_id: Option<String>,
    pub conversation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInvocationRequest {
    pub skill: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
}

impl DenialReplayRequest {
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }
}

/// Input the ACP server hands to a [`PromptExecutor`] when a
/// `session/prompt` request arrives.
///
/// `Clone` so a decorating executor can re-issue the same turn with a
/// different prompt — see [`crate::turn_gate`]. The publishers and the cancel
/// handle are shared handles, so a clone drives the same session, not a copy
/// of it.
#[derive(Clone)]
pub struct PromptRequest {
    /// ACP session id (for logging, transcript persistence, and
    /// update emission).
    pub session_id: String,
    /// Resolved cwd — the one the ACP handler chose via the
    /// `params.cwd || options.cwd || the current process directory` chain. Mostly
    /// useful for transcript path resolution and as $PWD equivalent
    /// for tool execution.
    pub cwd: String,
    /// Content blocks the client sent with the new user prompt.
    pub prompt: Vec<ContentBlock>,
    // 只由真实用户入口填写原文，避免附件、恢复和后台合成回合冒充首轮请求。
    pub user_prompt: Option<String>,
    // 入口计算的默认值可被首次选型替换；显式逐回合设置始终优先。
    pub effort_is_session_default: bool,
    /// MCP servers supplied for this ACP session activation.
    pub mcp_servers: Vec<McpServerConfig>,
    /// Optional update publisher. The executor is encouraged to
    /// forward streaming updates (`agent_message_chunk`, `tool_call`,
    /// `tool_call_update`) through this sink so the ACP client can
    /// render progress.
    pub update_publisher: Option<Arc<dyn SessionUpdatePublisher>>,
    /// Optional outbound permission request publisher. The executor
    /// forwards tool permission prompts through this publisher so
    /// the ACP client can approve or deny them.
    pub permission_publisher: Option<ChannelPermissionRequestPublisher>,
    /// Cancel handle triggered by `session/cancel`.
    pub cancel: PromptCancel,
    /// Per-turn thinking budget (Anthropic `budget_tokens`). When
    /// `Some`, the executor builds a `ThinkingConfig::Enabled` with
    /// this budget. `Some(0)` means disabled.
    pub thinking_budget: Option<u32>,
    /// Per-turn `max_tokens` override. When `Some`, the executor
    /// uses this instead of its default. Needed because thinking
    /// budgets require a matching `max_tokens`.
    pub max_tokens: Option<u32>,
    /// Per-turn reasoning effort ordinal for OpenAI Responses API.
    /// `0` = low, `1` = medium, `2` = high, `3` = xhigh.
    /// `None` means server default.
    pub reasoning_effort_ordinal: Option<u8>,
    /// Additional working directories granted by `--add-dir`.
    pub additional_working_directories: Vec<String>,
    /// Optional request-scoped coordinator mode override. `None` means use the
    /// executor/session default; ACP/TUI sessions populate this from their
    /// explicit session handle rather than reading process env per prompt.
    pub coordinator_mode: Option<bool>,
    /// Optional request-scoped report paths that coordinator-mode Read calls may access.
    pub coordinator_report_paths: Vec<String>,
    /// UUID of the local transcript user row that represents this
    /// prompt, when the caller already committed one. The executor
    /// reuses it for transcript persistence and file-history snapshots.
    pub user_message_uuid: Option<String>,
    /// Optional system prompt override for a background job launched with
    /// `--agent`. Ordinary interactive turns leave this unset.
    pub background_agent_system: Option<String>,
    /// Optional tool filter for a background job launched with `--agent`.
    pub background_agent_tool_filter: Option<rebon_types::ToolFilterSpec>,
    /// Optional request-scoped execution policy. Normal turns leave
    /// this unset; local workflows such as `/ultraplan` can attach a
    /// per-turn policy without mutating the session/global tool filter.
    pub execution_policy: Option<ExecutionPolicy>,
    /// Optional explicit tool replays requested locally by `/permissions retry`.
    /// Normal prompt turns leave this empty. Executors must process these
    /// through the same tool dispatch/update/transcript machinery used for
    /// model-emitted tool calls, not by running ad hoc tool executors in the
    /// slash-command layer.
    pub replay_requests: Vec<DenialReplayRequest>,
    /// Optional explicit skill invocations requested locally by slash-command
    /// submission. Executors should load these through the Skill tool before
    /// the first model request so `/skill` behaves like a deterministic skill
    /// load instead of relying on the model to decide to call Skill.
    pub skill_invocations: Vec<SkillInvocationRequest>,
}

/// Return value from [`PromptExecutor::execute`].
///
/// At minimum carries a [`StopReason`] that the ACP server reports
/// back in the `session/prompt` response. Richer fields (token counts,
/// recorded transcript ids, trailing permission-request ids) can be
/// added later without breaking the trait.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptOutcome {
    /// Final stop reason, mapped into the ACP `SessionPromptResult`.
    pub stop_reason: StopReason,
    /// Aggregated model usage for the completed prompt turn.
    pub usage: Usage,
}

impl PromptOutcome {
    /// Convenience constructor for the happy path.
    pub fn end_turn() -> Self {
        Self {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }
}

/// Error surface returned by a [`PromptExecutor`] — kept narrow so
/// the ACP handler can translate it back into a JSON-RPC error
/// without leaking implementation details from the downstream crate.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PromptExecutorError {
    /// The caller issued a cancel while the turn was mid-flight.
    #[error("prompt turn was cancelled")]
    Cancelled,
    /// Underlying model client or tool execution failed.
    #[error("prompt executor failed: {0}")]
    Execution(String),
    /// The executor needs more configuration than the caller
    /// supplied (e.g. a model client was not set).
    #[error("prompt executor misconfigured: {0}")]
    Misconfigured(String),
}

/// Trait every prompt executor implements.
///
/// Implementations own the whole prompt turn: model invocation,
/// tool dispatch, transcript persistence, update emission. The ACP
/// server calls [`Self::execute`] exactly once per `session/prompt`
/// request and awaits the returned [`PromptOutcome`] before
/// releasing the active-prompt slot.
#[async_trait]
pub trait PromptExecutor: Send + Sync {
    /// Run the turn. Implementations are expected to:
    ///
    /// 1. Persist the inbound prompt blocks on the transcript.
    /// 2. Call the injected model client.
    /// 3. Forward every stream event through
    ///    [`PromptRequest::update_publisher`] when present.
    /// 4. Dispatch every tool_use block, using
    ///    [`PromptRequest::permission_publisher`] to gate tool calls
    ///    when the tool requests it.
    /// 5. Persist the assistant response + any tool results on the
    ///    transcript.
    /// 6. Return a [`PromptOutcome`] carrying the final stop reason.
    async fn execute(&self, request: PromptRequest) -> Result<PromptOutcome, PromptExecutorError>;
}

/// Trivial executor that records the prompt on the session but does
/// not call any model. Returns `StopReason::EndTurn` immediately.
///
/// Used as the default in tests that want the legacy stub behaviour
/// and as a smoke-test for the executor wiring before a real
/// model-backed implementation is plugged in.
#[derive(Debug, Default, Clone)]
pub struct StubPromptExecutor;

#[async_trait]
impl PromptExecutor for StubPromptExecutor {
    async fn execute(&self, _request: PromptRequest) -> Result<PromptOutcome, PromptExecutorError> {
        Ok(PromptOutcome::end_turn())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn prompt_cancel_notifies_waiters() {
        let cancel = PromptCancel::new();
        let clone = cancel.clone();
        let task = tokio::spawn(async move {
            clone.notified().await;
        });
        // Give the task a moment to enter the await point.
        tokio::task::yield_now().await;
        cancel.cancel();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(cancel.is_cancelled());
    }

    #[tokio::test]
    async fn notified_resolves_immediately_if_already_cancelled() {
        let cancel = PromptCancel::new();
        cancel.cancel();
        timeout(Duration::from_secs(1), cancel.notified())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stub_executor_returns_end_turn() {
        let stub = StubPromptExecutor;
        let request = PromptRequest {
            user_prompt: None,
            effort_is_session_default: false,
            session_id: "sess-1".into(),
            cwd: "/tmp".into(),
            prompt: Vec::new(),
            mcp_servers: Vec::new(),
            update_publisher: None,
            permission_publisher: None,
            cancel: PromptCancel::new(),
            thinking_budget: None,
            max_tokens: None,
            reasoning_effort_ordinal: None,
            additional_working_directories: Vec::new(),
            coordinator_mode: None,
            coordinator_report_paths: Vec::new(),
            user_message_uuid: None,
            background_agent_system: None,
            background_agent_tool_filter: None,
            execution_policy: None,
            replay_requests: Vec::new(),
            skill_invocations: Vec::new(),
        };
        let outcome = stub.execute(request).await.unwrap();
        assert_eq!(outcome.stop_reason, StopReason::EndTurn);
    }
}
