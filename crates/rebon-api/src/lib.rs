//! # rebon-api - unified model client layer
//!
//! Provider API surface. Every backend is funnelled through the same
//! streaming and non-streaming entry points, and both outbound requests
//! and inbound SSE streams are normalised into provider-agnostic types.
//! The layering is expressed through traits:
//!
//! ```text
//!              +-----------------------------+
//!              |        ModelClient          |  <- stable consumer-facing trait
//!              +--------------+--------------+
//!                             |
//!                  +----------+-----------+
//!                  |                      |
//!      +-----------v----------+   +-------v----------+
//!      |   RetryMiddleware    |   | LoggingMiddleware |  <- cross-cutting concerns
//!      |   LoggingMiddleware  |   |   (stack freely)  |
//!      +-----------+----------+   +-------------------+
//!                  |
//!      +-----------v----------+
//!      | UniversalModelClient |  <- provider-agnostic glue
//!      |   (+ reqwest::Client)|
//!      +-----------+----------+
//!                  |
//!                  v
//!      +----------------------+
//!      |    ChatProvider      |  <- provider-specific, one impl per backend
//!      |  (Anthropic, OpenAI, |
//!      |   Bedrock, Vertex,   |
//!      |    Mock, Echo...)    |
//!      +----------------------+
//! ```
//!
//! ## Why this layering
//!
//! - Adding a new backend (Bedrock, Vertex, Azure, Gemini, Ollama,
//!   local llama.cpp, an internal proxy) is **one file + one
//!   `impl ChatProvider`**. No HTTP plumbing duplication, and no change
//!   to anything that consumes this crate.
//! - Cross-cutting concerns (retry, logging, metrics, rate-limit,
//!   caching, prompt-cache-break detection) are [`middleware`] that
//!   wrap any `Arc<dyn ModelClient>`. They never touch provider code.
//! - Tests stay simple: [`mock::MockModelClient`] implements
//!   [`ModelClient`] directly, and the `EchoProvider` pattern in
//!   [`provider::tests`] shows the lowest-friction injection point.
//!
//! ## Scope
//!
//! Implemented here: the streaming `create_message` path with tool-use
//! round-trip support for the [`anthropic`], [`openai`]
//! (chat-completions) and [`openai_responses`] backends; the
//! [`RetryMiddleware`] / [`LoggingMiddleware`] wrappers; conversation
//! compaction and pruning ([`compact`], [`context_prune`]); and the
//! embedded model catalogue ([`model_table`]).
//!
//! Deliberately not implemented here (each would be added on top of the
//! same abstraction): OAuth flows, the files API, admin endpoints, and
//! structured observability.
//!
//! ## Quick start
//!
//! ```no_run
//! use std::sync::Arc;
//! use rebon_api::{
//!     anthropic_client, AnthropicClientConfig, LoggingMiddleware, ModelClient,
//!     RetryConfig, RetryMiddleware,
//! };
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let base: Arc<dyn ModelClient> =
//!     Arc::new(anthropic_client(AnthropicClientConfig::with_api_key(
//!         std::env::var("ANTHROPIC_API_KEY")?,
//!     )));
//! let with_retry: Arc<dyn ModelClient> =
//!     Arc::new(RetryMiddleware::wrap(base, RetryConfig::default()));
//! let client: Arc<dyn ModelClient> = Arc::new(LoggingMiddleware::wrap(with_retry));
//! # let _ = client;
//! # Ok(()) }
//! ```
//!
//! Swap `anthropic_client` for `openai_compatible_client` to talk
//! to OpenAI, OpenRouter, Together, or a local llama.cpp server -
//! no changes to anything above.

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub mod agent_view_summary;
pub mod anthropic;
pub mod cache_trace;
pub mod client;
pub mod compact;
pub mod context_prune;
pub mod effort;
pub mod error;
pub mod events;
pub mod middleware;
pub mod mock;
pub mod model_catalog;
pub mod model_provider_protocol;
pub mod model_table;
pub mod openai;
pub mod openai_responses;
pub mod provider;
pub mod request;
pub mod session_handle;
pub mod session_title;
pub mod sse;
pub mod types;
pub mod typesafe;
pub mod vendor;

/// Shared runtime switch for OpenAI `service_tier: "priority"`.
#[derive(Debug, Clone)]
pub struct ServiceTierHandle {
    fast: Arc<AtomicBool>,
}

impl ServiceTierHandle {
    pub fn new(fast: bool) -> Self {
        Self {
            fast: Arc::new(AtomicBool::new(fast)),
        }
    }

    pub fn is_fast(&self) -> bool {
        self.fast.load(Ordering::Relaxed)
    }

    pub fn set_fast(&self, fast: bool) {
        self.fast.store(fast, Ordering::Relaxed);
    }
}

impl Default for ServiceTierHandle {
    fn default() -> Self {
        Self::new(false)
    }
}

/// The tier fast mode asks for. OpenAI renamed priority processing to
/// "Fast" in July 2026 but kept `priority` as the wire value, which is what
/// the Codex CLI sends too.
pub const FAST_SERVICE_TIER: &str = "priority";

/// Put `service_tier` on a request, but only for a model that takes it.
///
/// The endpoint is not what decides this: `api.openai.com` and the ChatGPT
/// Codex backend both serve models that accept the field and models that
/// reject it, so the gate reads the body's own `model` against
/// [`model_table`]. A model the table does not list keeps whatever fast
/// mode was doing — see [`model_table::TierSupport`] on why absence is not
/// a "no".
pub(crate) fn apply_openai_service_tier(
    body: &mut serde_json::Value,
    handle: Option<&ServiceTierHandle>,
) {
    let Some(handle) = handle else {
        return;
    };
    let model = body
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let supported =
        model_table::service_tier_support(None, &model, FAST_SERVICE_TIER).should_send();
    if handle.is_fast() && supported {
        obj.insert(
            "service_tier".to_string(),
            serde_json::json!(FAST_SERVICE_TIER),
        );
    } else {
        if handle.is_fast() {
            tracing::debug!(
                "rebon-api: fast mode is on but model `{model}` does not accept \
                 service_tier: \"{FAST_SERVICE_TIER}\"; omitting it"
            );
        }
        obj.remove("service_tier");
        obj.remove("serviceTier");
    }
}

pub use agent_view_summary::{
    extract_agent_view_summary_text, generate_agent_view_summary, AGENT_VIEW_SUMMARY_PROMPT,
    MAX_AGENT_VIEW_SUMMARY_CHARS, MAX_AGENT_VIEW_SUMMARY_TEXT,
};
pub use anthropic::{
    anthropic_client, anthropic_client_with_http,
    build_request_body as build_anthropic_request_body, parse_anthropic_event,
    AnthropicClientConfig, AnthropicProvider,
};
pub use cache_trace::{
    cache_trace_enabled, schema_hash, stable_hash_str, stable_hash_value, tools_hash,
    CacheMissReason, RequestShapeTrace,
};
pub use client::{ModelCapabilities, ModelClient};
pub use compact::{
    compact_with_retry, CompactProvider, CompactResult, CompactSummaryOptions,
    FallbackCompactProvider, ModelCompactProvider, PrefixAlignedCompactProvider,
    RemoteCompactProvider, RemoteCompactV2Provider, COMPACT_RETRY_DELAYS_MS,
};
pub use context_prune::{
    auto_compact_truncate, auto_compact_truncate_cache_stable, auto_compact_truncate_from,
    ensure_tool_result_pairing, ensure_tool_result_pairing_with_report, microcompact_tool_results,
    should_preserve_prefix_cache, sweep_recent_tool_results, ContextBudget, ContextPruneConfig,
    ContextPruneMiddleware, ContextUsageSnapshot, ContextUsageSource, PairingRepairReport,
    PruneLevel, PruneLevelHandle, PruneStats, PruneStatsSnapshot, DEFAULT_AUTO_COMPACT_TOKEN_LIMIT,
    SYNTHETIC_TOOL_RESULT_PLACEHOLDER, SYNTHETIC_TOOL_RESULT_PREFIX, TOOL_RESULT_CLEARED,
};
pub use error::{
    parse_context_overflow, parse_retry_after, redact_secrets, ContextOverflow, ModelError,
    ModelResult, RetryHint, FLOOR_OUTPUT_TOKENS,
};
pub use events::{
    ContentBlockDelta, ContentBlockStart, MessageAccumulator, MessageDeltaFields, StreamEvent,
    StreamEventStream,
};
pub use middleware::{
    LoggingMiddleware, RetryConfig, RetryMiddleware, RetryNotifier, RetryProgress,
    MAX_HONOURED_RETRY_AFTER,
};
pub use mock::MockModelClient;
pub use model_catalog::{
    catalogue_as_discovery, discover_models, discover_models_blocking, DiscoveredModel,
    LimitsSource, ModelDiscovery, ModelDiscoveryError, ModelDiscoveryRequest,
};
pub use model_provider_protocol::*;
pub use openai::{
    build_openai_request_body, is_deepseek_model, openai_compatible_client,
    openai_compatible_client_with_http, ChatCompletionsCompat, OpenAiCompatibleClientConfig,
    OpenAiCompatibleProvider, OpenAiRequestOptions, OpenAiTranslator, ReasoningReplayMode,
    ToolCallContentMode,
};
pub use openai_responses::{
    build_responses_input, build_responses_request_body,
    build_responses_request_body_with_service_tier, build_responses_tools,
    is_chatgpt_codex_backend, openai_responses_client, openai_responses_client_with_http,
    OpenAiResponsesClientConfig, OpenAiResponsesProvider, OpenAiResponsesTranslator,
    TokenRefresher,
};
pub use provider::{classify_http_error, ChatProvider, UniversalModelClient};
pub use request::{
    effort_to_anthropic_thinking, effort_to_openai_max_tokens, effort_to_openai_reasoning,
    is_runtime_context_message, runtime_context_body_from_message, runtime_context_message,
    split_pro_model_alias, wrap_runtime_context, CacheTraceContext, ContextEditStrategy,
    ContextManagementConfig, CreateMessageRequest, ReasoningEffort, ReasoningMode,
    ReasoningSummary, ThinkingConfig, TokenThreshold, WebSearchToolConfig, WebSearchUserLocation,
};
pub use session_handle::{SessionHandle, SessionHandleId, SessionRegistry};
pub use session_title::{
    extract_conversation_text, generate_session_title, MAX_CONVERSATION_TEXT, SESSION_TITLE_PROMPT,
};
pub use types::{
    make_meta_user_message, AssistantMessage, CompactionBlock, ContentBlock, DocumentBlock,
    DocumentSource, GeneratedImageBlock, ImageBlock, ImageSource, Message, Role, SearchResultEntry,
    ServerToolUseBlock, StopReason, TextBlock, ThinkingBlock, Tool, ToolChoice, ToolResultBlock,
    ToolResultContent, ToolResultContentBlock, ToolUseBlock, Usage, WebSearchResultBlock,
};
pub use vendor::{
    host_of as endpoint_host, ChatWireRules, EffortVocabulary, KnownModel, MaxTokensField,
    ModelListing, PromptCacheKind, PromptCacheProfile, ProviderVendor, ThinkingDialect, WireFamily,
};
