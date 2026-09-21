//! `ModelClient` trait + default accumulator path.
//!
//! Every concrete provider (Anthropic, OpenAI-compatible, mock)
//! implements [`ModelClient`]. The trait ships a default
//! implementation of [`ModelClient::create_message`] that consumes
//! the streaming path via [`crate::MessageAccumulator`] so
//! implementers only need to provide
//! [`ModelClient::create_message_stream`].

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;

use crate::error::ModelResult;
use crate::events::{MessageAccumulator, StreamEventStream};
use crate::request::CreateMessageRequest;
use crate::types::AssistantMessage;

/// A model backend's runtime capabilities, captured in one snapshot.
///
/// Every field defaults to `false`; negative names are used where the safe
/// historical default was `true`, so adding a field never also requires a
/// second default table. Middleware forwards this value as a whole.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub codex_oauth_web_search: bool,
    /// Transient context must be materialized into durable messages.
    pub requires_inline_transient_context: bool,
    pub forced_tool_choice: bool,
    pub anchored_minimal: bool,
    pub output_budget_includes_reasoning: bool,
    /// Unsigned thinking can be dropped instead of making replay invalid.
    pub accepts_unsigned_thinking_replay: bool,
    pub prefix_cache_is_byte_exact: bool,
    pub remote_compaction_v2: bool,
}

impl ModelCapabilities {
    /// Capture capabilities from clients that still override the original
    /// query methods. Starting from `capabilities()` preserves any newer fields
    /// that have no legacy accessor.
    pub(crate) fn forwarded(client: &dyn ModelClient) -> Self {
        let mut capabilities = client.capabilities();
        capabilities.codex_oauth_web_search = client.supports_codex_oauth_web_search();
        capabilities.requires_inline_transient_context =
            !client.supports_request_scoped_transient_context();
        capabilities.forced_tool_choice = client.supports_forced_tool_choice();
        capabilities.anchored_minimal = client.supports_anchored_minimal();
        capabilities.output_budget_includes_reasoning = client.output_budget_includes_reasoning();
        capabilities.accepts_unsigned_thinking_replay =
            !client.thinking_replay_requires_signature();
        capabilities.prefix_cache_is_byte_exact = client.prefix_cache_is_byte_exact();
        capabilities.remote_compaction_v2 = client.supports_remote_compaction_v2();
        capabilities
    }
}

/// Unified async trait every model client implements.
#[async_trait]
pub trait ModelClient: Send + Sync {
    /// Stable identifier for the underlying provider. Used in
    /// diagnostic logs; no caller depends on the exact strings, but
    /// they should stay stable over time.
    fn provider_name(&self) -> &'static str;

    /// Start a streaming `create_message` call. Returns a boxed
    /// stream of [`crate::StreamEvent`]s.
    ///
    /// Implementations are expected to force `request.stream = true`
    /// internally, even if the caller cleared the flag.
    async fn create_message_stream(
        &self,
        request: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream>;

    /// Non-streaming convenience wrapper. Default implementation
    /// drives the stream through a [`MessageAccumulator`] and
    /// returns the synthesised final message.
    ///
    /// Requested `stop_sequences` are applied to the accumulated
    /// result, because only the Anthropic wire format carries them —
    /// see [`AssistantMessage::apply_stop_sequences`]. On a provider
    /// that honoured them this is a no-op.
    async fn create_message(&self, request: CreateMessageRequest) -> ModelResult<AssistantMessage> {
        let stop_sequences = request.stop_sequences.clone();
        let mut stream = self.create_message_stream(request).await?;
        let mut acc = MessageAccumulator::new();
        while let Some(event) = stream.next().await {
            let event = event?;
            acc.apply(&event)?;
        }
        let mut message = acc.finish();
        if message.apply_stop_sequences(&stop_sequences) {
            tracing::debug!(
                provider = self.provider_name(),
                "applied stop sequence client-side: the provider returned text past it"
            );
        }
        Ok(message)
    }

    /// Create an isolated client suitable for a sub-agent session.
    ///
    /// The returned client shares immutable configuration (auth,
    /// retry policy, logging) but has **fresh** session-scoped
    /// mutable state so `previous_response_id` chains and WebSocket
    /// connections do not leak between parent and child. Clients
    /// without session state return `None` (the default), which
    /// tells the caller that a plain `Arc::clone` is sufficient.
    fn fork_for_sub_agent(&self) -> Option<Arc<dyn ModelClient>> {
        None
    }

    fn fork_for_sub_agent_with_cache_key(
        &self,
        _prompt_cache_key: Option<String>,
    ) -> Option<Arc<dyn ModelClient>> {
        self.fork_for_sub_agent()
    }

    /// Context-prune handle associated with this client stack, if present.
    ///
    /// Engine callers use this to apply model-specific context windows
    /// before pre-request budget checks run. Middleware wrappers should
    /// delegate to their inner client.
    fn context_prune_handle(&self) -> Option<crate::context_prune::PruneLevelHandle> {
        None
    }

    /// Return all feature support in one snapshot. Implementations override
    /// this once; middleware forwards the value without a method per feature.
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    /// Whether this client is backed by the ChatGPT Codex OAuth route
    /// that can execute Codex-entitled native web search requests.
    fn supports_codex_oauth_web_search(&self) -> bool {
        self.capabilities().codex_oauth_web_search
    }

    /// Whether request-scoped transient context can be sent outside
    /// durable message history without breaking provider-side
    /// continuation state.
    fn supports_request_scoped_transient_context(&self) -> bool {
        !self.capabilities().requires_inline_transient_context
    }

    /// Whether this provider supports request-level forced tool choice.
    fn supports_forced_tool_choice(&self) -> bool {
        self.capabilities().forced_tool_choice
    }

    /// Whether Minimal capability mode should bootstrap the first request and
    /// promote to the normal tool/context projection after durable assistant output.
    fn supports_anchored_minimal(&self) -> bool {
        self.capabilities().anchored_minimal
    }

    /// Whether hidden reasoning output is billed against
    /// `max_tokens` (OpenAI Responses-style `max_output_tokens`
    /// semantics, including plugin providers that stream
    /// `reasoning_text`). Anthropic-style providers budget thinking
    /// separately via `ThinkingConfig`, so the default is `false`.
    /// Callers sizing per-turn output caps (e.g. worker budgets)
    /// should scale up when this is true, or reasoning can starve
    /// the visible tool-call output mid-JSON.
    fn output_budget_includes_reasoning(&self) -> bool {
        self.capabilities().output_budget_includes_reasoning
    }

    /// Whether replaying an assistant thinking block back to this
    /// provider requires it to carry a `signature` (Anthropic) or
    /// `data` (Responses-style `encrypted_content`).
    ///
    /// Anthropic rejects an unsigned thinking block outright, so
    /// anything that replays assistant turns must treat one as
    /// unsendable. Providers that simply drop what they cannot replay
    /// — OpenAI-style dialects and plugin providers, which never emit
    /// a signature at all — should return `false`, or a truncated turn
    /// can never be continued. Defaults to `true` so an unknown
    /// provider keeps the conservative behavior.
    fn thinking_replay_requires_signature(&self) -> bool {
        !self.capabilities().accepts_unsigned_thinking_replay
    }

    /// Whether this provider's prompt cache is keyed on a byte-exact
    /// request prefix, so that rewriting bytes in the middle of the
    /// history (clearing an old tool result, dropping an old thinking
    /// block) costs a cache miss from that point on every later turn.
    ///
    /// The prune middleware skips its in-place history edits when this is
    /// true. The vendor table in [`crate::vendor`] answers for
    /// OpenAI-compatible endpoints; Anthropic and OpenAI Responses answer
    /// `true` by protocol. Defaults to `false`: a provider that advertises
    /// no prefix contract gets the smaller request.
    fn prefix_cache_is_byte_exact(&self) -> bool {
        self.capabilities().prefix_cache_is_byte_exact
    }

    /// Whether this endpoint implements OpenAI's remote compaction v2:
    /// a normal streaming `/responses` turn whose input ends with a
    /// `{"type":"compaction_trigger"}` control item, answered with a
    /// single `compaction` output item that replaces the history.
    ///
    /// Host-scoped, not protocol-scoped — plenty of endpoints speak the
    /// Responses format without implementing this. Defaults to `false`
    /// so an unknown backend never sees a control item it would reject;
    /// the compaction ladder just falls through to its next rung.
    fn supports_remote_compaction_v2(&self) -> bool {
        self.capabilities().remote_compaction_v2
    }

    /// Reset session-scoped mutable state without creating a new
    /// client. Called when starting a fresh conversation (e.g.
    /// `/new`) so stale response IDs don't leak into the new
    /// session. Default is a no-op.
    fn reset_session_state(&self) {}

    /// Called by the engine when a turn finishes (success, error, or
    /// cancel). Providers may use this to clear strictly turn-local
    /// scratch state; long-lived session transports should only be
    /// reset here if the provider cannot safely validate them before
    /// the next request.
    ///
    /// This is the per-turn counterpart to [`reset_session_state`],
    /// which is reserved for explicit "new session" events. Default
    /// is a no-op — stateless providers ignore it.
    fn end_turn(&self) {}

    /// Invalidate only the `previous_response_id` chain without
    /// tearing down the transport. Providers that use server-side
    /// continuation must validate each request against the stored
    /// baseline before reusing that continuation, and fall back to a
    /// full replay if the baseline diverged. Default is a no-op.
    fn invalidate_previous_response_id(&self) {}
}
