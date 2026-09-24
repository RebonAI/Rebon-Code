//! `OpenAiCompatibleProvider` — [`crate::ChatProvider`]
//! implementation against any endpoint that speaks the OpenAI chat
//! completions streaming API (OpenAI itself, Azure OpenAI, Together,
//! OpenRouter, local llama.cpp, etc.).
//!
//! The provider translates both directions:
//!
//! 1. **Outbound** — [`build_openai_request_body`] rewrites the
//!    Anthropic-shaped [`CreateMessageRequest`] into an OpenAI chat
//!    completions request (`messages[]` with `role` + `content`
//!    strings, `tools[]` with `function.name/description/parameters`,
//!    etc.).
//! 2. **Inbound** — [`OpenAiTranslator`] consumes the OpenAI SSE
//!    stream (`chat.completion.chunk` events with
//!    `choices[0].delta.content` / `delta.tool_calls[]`) and emits
//!    provider-agnostic [`StreamEvent`]s so downstream consumers
//!    never see the OpenAI wire shape.
//!
//! ## Scope
//!
//! This module handles:
//!
//! - text deltas on `choices[0].delta.content`
//! - tool-call deltas (id + name + arguments streamed via
//!   `choices[0].delta.tool_calls[]`)
//! - `finish_reason` → [`crate::StopReason`] translation
//! - usage totals on the final chunk
//! - image and file content blocks (`image_url` / `file` parts),
//!   including inside tool results
//!
//! Deferred:
//!
//! - `logprobs`, `n`, `parallel_tool_calls`
//! - Function-call legacy shape (pre-`tools[]`)
//!
//! The OpenAI Responses API (with `previous_response_id` continuity)
//! is not missing — it has its own provider in
//! [`crate::openai_responses`].

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
#[cfg(test)]
use futures_util::StreamExt;
use futures_util::TryStreamExt;
use serde::Serialize;
use serde_json::Value;

use crate::cache_trace::{
    cache_trace_enabled, stable_hash_value, CacheMissReason, RequestShapeTrace,
};
use crate::client::ModelCapabilities;
use crate::error::{ModelError, ModelResult};
use crate::events::{
    ContentBlockDelta, ContentBlockStart, MessageDeltaFields, StreamEvent, StreamEventStream,
};
use crate::openai_responses::TokenRefresher;
use crate::provider::{classify_http_error, ChatProvider, UniversalModelClient};
use crate::request::CreateMessageRequest;
use crate::sse::{decode_sse_stream, SseDecoder, SseFrame};
use crate::types::{
    ContentBlock, ImageBlock, Message, Role, StopReason, Tool, ToolChoice, ToolResultContent,
    ToolResultContentBlock, Usage,
};
use crate::vendor::{ChatWireRules, MaxTokensField, ProviderVendor, ThinkingDialect};
use crate::ServiceTierHandle;

/// Generic request body customisation for OpenAI-compatible chat/completions endpoints.
///
/// `body` entries are normalized from common camelCase config names to OpenAI wire names
/// before being merged into the generated request. `extra_body` entries are merged after
/// `body` and can be used for provider-specific extension fields. `omit_body_fields` removes
/// generated fields before the merges are applied.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OpenAiRequestOptions {
    pub body: serde_json::Map<String, Value>,
    pub extra_body: serde_json::Map<String, Value>,
    pub omit_body_fields: Vec<String>,
}

impl OpenAiRequestOptions {
    pub fn is_empty(&self) -> bool {
        self.body.is_empty() && self.extra_body.is_empty() && self.omit_body_fields.is_empty()
    }

    fn merge_from(&mut self, other: &OpenAiRequestOptions) {
        for field in &other.omit_body_fields {
            if !self
                .omit_body_fields
                .iter()
                .any(|existing| existing == field)
            {
                self.omit_body_fields.push(field.clone());
            }
        }
        for (key, value) in &other.body {
            self.body.insert(key.clone(), value.clone());
        }
        for (key, value) in &other.extra_body {
            self.extra_body.insert(key.clone(), value.clone());
        }
    }
}

/// How historical assistant `reasoning_content` (Anthropic Thinking blocks)
/// should be replayed when serializing chat-completions input.
///
/// Different providers have *opposite* expectations:
///
/// - OpenAI Chat Completions does not define `reasoning_content` as a valid
///   input field; echoing it diverges request bytes from any cached prefix
///   the model previously stored. → [`Self::Never`].
/// - DeepSeek thinking mode requires `reasoning_content` to be replayed on
///   **every** assistant turn that originally carried it — not just
///   tool-call turns. Empirically verified: stripping it on a text-only
///   assistant turn returns `400 invalid_request_error: "The reasoning_content
///   in the thinking mode must be passed back to the API."` Doc paragraphs
///   that suggest otherwise describe context-efficiency hygiene, not
///   protocol enforcement. → [`Self::Always`].
///   ([`Self::OnlyOnToolCallTurns`] is kept as an enum variant for
///   completeness but matches no known provider's actual requirements.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReasoningReplayMode {
    /// Strip Thinking from every historical assistant message.
    /// OpenAI Chat Completions semantics. This is the default.
    #[default]
    Never,
    /// Replay `reasoning_content` only on assistant turns that also carry
    /// `tool_calls`. No known provider asks for this — DeepSeek thinking
    /// mode needs [`Self::Always`] — so the variant is kept for
    /// completeness only.
    OnlyOnToolCallTurns,
    /// Always replay `reasoning_content`. Reserved for providers that may
    /// require it on every assistant turn.
    Always,
}

/// How the assistant `content` field should be shaped when the historical
/// assistant turn is tool-calls-only (no text).
///
/// - OpenAI Chat Completions: the schema lets `content` be omitted when
///   `tool_calls` is present. Both omit and `null` are accepted; we omit
///   to keep the canonical wire small. → [`Self::OmitIfAllowed`].
/// - DeepSeek thinking + tool calls: the compat list (`requiresAssistantContentForToolCalls`)
///   says tool-call assistant turns must carry non-null content; we emit
///   an empty string to match what the provider itself streams back.
///   → [`Self::EmptyStringRequired`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolCallContentMode {
    /// Omit the `content` field entirely when only tool_calls are present.
    /// OpenAI Chat Completions semantics. This is the default.
    #[default]
    OmitIfAllowed,
    /// Emit `content: null` when only tool_calls are present.
    NullIfAllowed,
    /// Emit `content: ""` (non-null empty string) when only tool_calls are
    /// present. DeepSeek thinking tool-call compatibility.
    EmptyStringRequired,
}

/// Per-provider wire-shape capability flags applied while building
/// chat-completions request bodies.
///
/// Default = OpenAI Chat Completions (Never replay reasoning_content;
/// omit content on tool-only assistant turns).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChatCompletionsCompat {
    pub reasoning_replay: ReasoningReplayMode,
    pub tool_call_content: ToolCallContentMode,
    /// Also replay the vendor's encrypted copy of the reasoning
    /// (`encrypted_content`, kept as the thinking block's signature).
    /// Volcengine Ark asks for it on every tool turn.
    pub replay_encrypted_reasoning: bool,
}

impl ChatCompletionsCompat {
    /// OpenAI Chat Completions semantics. Same as [`Default::default`].
    pub const OPENAI: Self = Self {
        reasoning_replay: ReasoningReplayMode::Never,
        tool_call_content: ToolCallContentMode::OmitIfAllowed,
        replay_encrypted_reasoning: false,
    };

    /// The compat a vendor's documentation calls for: the DeepSeek-style
    /// reasoning replay for every vendor that requires `reasoning_content`
    /// back on tool turns, plain OpenAI otherwise.
    pub fn for_vendor(vendor: ProviderVendor, model: &str) -> Self {
        let rules = vendor.chat_wire_rules(model);
        if rules.replay_reasoning_content {
            Self {
                replay_encrypted_reasoning: rules.replay_encrypted_reasoning,
                ..Self::DEEPSEEK_THINKING
            }
        } else {
            Self::OPENAI
        }
    }

    /// DeepSeek thinking mode semantics (V4-pro).
    ///
    /// Empirically (observed via 400 `invalid_request_error`: "The
    /// `reasoning_content` in the thinking mode must be passed back to the
    /// API."), DeepSeek's API requires `reasoning_content` to be replayed on
    /// **every** assistant turn that originally carried it — not just
    /// tool-call turns. Doc paragraphs that read otherwise describe context-
    /// efficiency hygiene, not protocol enforcement.
    ///
    /// `tool_call_content = EmptyStringRequired` keeps tool-only assistant
    /// turns at `content: ""` to match the agent-integration compat flag
    /// `requiresAssistantContentForToolCalls`.
    pub const DEEPSEEK_THINKING: Self = Self {
        reasoning_replay: ReasoningReplayMode::Always,
        tool_call_content: ToolCallContentMode::EmptyStringRequired,
        replay_encrypted_reasoning: false,
    };
}

/// Configuration for an [`OpenAiCompatibleProvider`].
#[derive(Debug, Clone)]
pub struct OpenAiCompatibleClientConfig {
    /// Base URL. The endpoint path (`/v1/chat/completions`) is
    /// appended internally. Defaults to `https://api.openai.com`.
    pub base_url: String,
    /// API key. Sent as `Authorization: Bearer {api_key}`.
    pub api_key: String,
    /// Optional organisation id (`OpenAI-Organization` header).
    pub organization: Option<String>,
    /// Extra HTTP headers to inject on every request. Useful for
    /// provider-specific keys (e.g. `HTTP-Referer` for OpenRouter).
    pub extra_headers: Vec<(String, String)>,
    /// HTTP client connect timeout. Only honoured when the provider
    /// constructs its own [`UniversalModelClient`] via the
    /// convenience constructor — when the caller injects an
    /// existing `reqwest::Client`, this field is ignored.
    pub request_timeout: Option<Duration>,
    /// Provider-level request body defaults/overrides for OpenAI-compatible
    /// chat/completions endpoints.
    pub request_options: OpenAiRequestOptions,
    /// Per-model request body defaults/overrides. The entry keyed by
    /// [`CreateMessageRequest::model`] is applied after provider-level options.
    pub model_request_options: BTreeMap<String, OpenAiRequestOptions>,
    /// Optional prompt-cache routing key for OpenAI-compatible endpoints that support it.
    pub prompt_cache_key: Option<String>,
    /// Optional prompt-cache retention hint. Mirrored into diagnostics, and
    /// emitted on the wire when it is a wire-legal value (`in-memory` / `24h`);
    /// Rebon-internal hints stay diagnostics-only.
    pub prompt_cache_retention: Option<String>,
    /// Optional runtime switch for OpenAI `service_tier: "priority"`.
    pub service_tier: Option<ServiceTierHandle>,
    /// Whether volatile runtime context may stay request-scoped instead of
    /// being materialized into durable history.
    pub request_scoped_transient_context: bool,
    /// Per-provider wire-shape capability flags. Defaults to OpenAI Chat
    /// Completions semantics. Set to [`ChatCompletionsCompat::DEEPSEEK_THINKING`]
    /// when the endpoint is DeepSeek's thinking mode so historical
    /// `reasoning_content` is replayed on tool-call turns and tool-only
    /// content stays non-null.
    pub compat: ChatCompletionsCompat,
    /// Who is behind the endpoint. [`ProviderVendor::Unknown`] (the
    /// default) means "recognise it from `base_url`'s host at send time";
    /// a caller that knows better — a gateway entry with a `vendor` pin —
    /// sets it. Decides the thinking switch, the output-cap field, the
    /// fields the vendor rejects, reasoning replay and cache protection
    /// (see [`crate::vendor`]).
    pub vendor: ProviderVendor,
    /// Mints a fresh bearer when the endpoint answers `401`: the request is
    /// retried once with it. Set for account logins whose request token is
    /// short-lived (a Copilot session token lasts about half an hour), so a
    /// long session recovers instead of failing every turn after expiry.
    pub refresher: Option<Arc<dyn TokenRefresher>>,
}

impl Default for OpenAiCompatibleClientConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com".to_string(),
            api_key: String::new(),
            organization: None,
            extra_headers: Vec::new(),
            request_timeout: Some(Duration::from_secs(60)),
            request_options: OpenAiRequestOptions::default(),
            model_request_options: BTreeMap::new(),
            prompt_cache_key: None,
            prompt_cache_retention: None,
            service_tier: None,
            request_scoped_transient_context: true,
            compat: ChatCompletionsCompat::OPENAI,
            vendor: ProviderVendor::Unknown,
            refresher: None,
        }
    }
}

impl OpenAiCompatibleClientConfig {
    /// Convenience constructor that sets the API key and leaves the
    /// rest at the default.
    pub fn with_api_key(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            ..Self::default()
        }
    }

    /// Build a config pointing at an arbitrary base URL (for
    /// OpenRouter, local llama.cpp, Together, etc.).
    pub fn with_base_url(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            ..Self::default()
        }
    }

    /// The vendor requests are shaped for: the explicit pin, or the one
    /// the host names.
    pub fn effective_vendor(&self) -> ProviderVendor {
        match self.vendor {
            ProviderVendor::Unknown => ProviderVendor::detect(&self.base_url),
            pinned => pinned,
        }
    }
}

/// [`ChatProvider`] implementation that translates both directions
/// between Anthropic shapes and OpenAI chat completions.
#[derive(Debug, Clone)]
pub struct OpenAiCompatibleProvider {
    config: Arc<OpenAiCompatibleClientConfig>,
    /// The bearer every request carries. Starts as `config.api_key` and is
    /// replaced when [`OpenAiCompatibleClientConfig::refresher`] mints a new
    /// one; shared with sub-agent forks so one refresh serves them all.
    bearer: Arc<std::sync::Mutex<String>>,
}

impl OpenAiCompatibleProvider {
    /// Construct from a config.
    pub fn new(config: OpenAiCompatibleClientConfig) -> Self {
        Self {
            bearer: Arc::new(std::sync::Mutex::new(config.api_key.clone())),
            config: Arc::new(config),
        }
    }

    /// Clone of the config — useful for diagnostics.
    pub fn config(&self) -> Arc<OpenAiCompatibleClientConfig> {
        self.config.clone()
    }

    fn endpoint(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        let last_segment = base.rsplit('/').next().unwrap_or_default();
        if self.config.effective_vendor().api_root_is_unversioned() {
            format!("{base}/chat/completions")
        } else if last_segment
            .strip_prefix('v')
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit()))
        {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
        }
    }
}

#[async_trait]
impl ChatProvider for OpenAiCompatibleProvider {
    fn provider_name(&self) -> &'static str {
        "openai-compatible"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            requires_inline_transient_context: !self.config.request_scoped_transient_context,
            accepts_unsigned_thinking_replay: true,
            prefix_cache_is_byte_exact: self
                .config
                .effective_vendor()
                .prompt_cache()
                .is_prefix_based(),
            ..ModelCapabilities::default()
        }
    }

    async fn send_message_stream(
        &self,
        http: &reqwest::Client,
        mut request: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream> {
        request.stream = true;
        let mut config = self.config.as_ref().clone();
        if let Some(cache_key) = effective_prompt_cache_key(&request, &config) {
            config.prompt_cache_key = Some(cache_key);
        }
        if let Some(retention) = effective_prompt_cache_retention(&request, &config) {
            config.prompt_cache_retention = Some(retention);
        }
        let body = build_openai_request_body(&request, &config);
        emit_openai_chat_request_shape_trace(&request, &config, &body);
        let response = self.post_chat(http, &config, &body).await?;

        let byte_stream = response
            .bytes_stream()
            .map_err(|e| ModelError::Http(format!("body stream: {e}")));

        Ok(decode_sse_stream(
            Box::pin(byte_stream),
            OpenAiSseDecoder::default(),
        ))
    }

    fn fork_for_sub_agent_with_cache_key(
        &self,
        prompt_cache_key: Option<String>,
    ) -> Option<Arc<dyn ChatProvider>> {
        let prompt_cache_key = prompt_cache_key.or_else(|| self.config.prompt_cache_key.clone());
        Some(Arc::new(Self {
            config: Arc::new(OpenAiCompatibleClientConfig {
                prompt_cache_key,
                ..self.config.as_ref().clone()
            }),
            bearer: Arc::clone(&self.bearer),
        }))
    }
}

impl OpenAiCompatibleProvider {
    /// POST one chat-completions request and hand back the successful
    /// response.
    ///
    /// A `401` with a refresher attached mints a new bearer and retries once;
    /// a second `401`, a refresher that fails, and every other status end
    /// the call classified as usual. Only `401` refreshes: a `403` says the
    /// credential is valid but not allowed, which a new token does not fix.
    async fn post_chat(
        &self,
        http: &reqwest::Client,
        config: &OpenAiCompatibleClientConfig,
        body: &Value,
    ) -> ModelResult<reqwest::Response> {
        let mut refreshed = false;
        loop {
            let bearer = self
                .bearer
                .lock()
                .expect("openai-compatible bearer mutex poisoned")
                .clone();
            let mut http_req = http
                .post(self.endpoint())
                .header("Authorization", format!("Bearer {bearer}"))
                .header("Content-Type", "application/json")
                .header("Accept", "text/event-stream")
                .json(body);
            if let Some(org) = &config.organization {
                http_req = http_req.header("OpenAI-Organization", org);
            }
            for (name, value) in &config.extra_headers {
                http_req = http_req.header(name, value);
            }
            let response = http_req.send().await?;
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                if let Some(refresher) = config.refresher.as_ref() {
                    let fresh = refresher.refresh().await.map_err(|msg| {
                        ModelError::Unauthorized(format!("token refresh failed: {msg}"))
                    })?;
                    *self
                        .bearer
                        .lock()
                        .expect("openai-compatible bearer mutex poisoned") = fresh;
                    refreshed = true;
                    tracing::info!("openai-compatible: got 401, refreshed the bearer and retrying");
                    continue;
                }
            }
            let retry_after = crate::error::parse_retry_after(response.headers());
            let text = response.text().await.unwrap_or_default();
            return Err(classify_http_error(status.as_u16(), text, retry_after));
        }
    }
}

fn emit_openai_chat_request_shape_trace(
    request: &CreateMessageRequest,
    config: &OpenAiCompatibleClientConfig,
    body: &Value,
) {
    if !cache_trace_enabled() {
        return;
    }
    let mut trace_request = request.clone();
    let prompt_cache_key = effective_prompt_cache_key(&trace_request, config);
    let prompt_cache_retention = effective_prompt_cache_retention(&trace_request, config);
    let trace_context = trace_request
        .cache_trace_context
        .get_or_insert_with(Default::default);
    trace_context.prompt_cache_key = prompt_cache_key;
    trace_context.prompt_cache_retention = prompt_cache_retention;
    trace_context.api_path = Some("openai_chat_completions".to_string());
    trace_context.previous_response_id_present = Some(false);
    let shape = RequestShapeTrace::from_request(&trace_request);
    let trace = trace_request.cache_trace_context.as_ref();
    let provider_fingerprint_hash = Some(stable_hash_value(body));
    tracing::info!(
        target: "rebon_cache_trace",
        provider = "openai_chat_completions",
        model = %trace_request.model,
        model_hash = %shape.model_hash,
        system_hash = %shape.system_hash,
        tools_hash = %trace
            .and_then(|trace| trace.tools_hash.as_deref())
            .unwrap_or(&shape.tools_hash),
        schema_hash = trace.and_then(|trace| trace.schema_hash.as_deref()),
        shared_preamble_hash = trace.and_then(|trace| trace.shared_preamble_hash.as_deref()),
        profile_preamble_hash = trace.and_then(|trace| trace.profile_preamble_hash.as_deref()),
        capsule_hash = trace.and_then(|trace| trace.capsule_hash.as_deref()),
        task_hash = trace.and_then(|trace| trace.task_hash.as_deref()),
        messages_prefix_hash = %shape.messages_prefix_hash,
        reasoning_hash = %shape.reasoning_hash,
        dynamic_context_hash = shape.dynamic_context_hash.as_deref(),
        context_policy = trace.and_then(|trace| trace.context_policy.as_deref()),
        tokens_before_capsule = trace.and_then(|trace| trace.tokens_before_capsule),
        tokens_before_task = trace.and_then(|trace| trace.tokens_before_task),
        cross_run_cache_eligible = trace.and_then(|trace| trace.cross_run_cache_eligible),
        same_dispatch_cache_eligible = trace.and_then(|trace| trace.same_dispatch_cache_eligible),
        previous_response_id_present = false,
        prompt_cache_key = trace.and_then(|trace| trace.prompt_cache_key.as_deref()),
        prompt_cache_retention = trace.and_then(|trace| trace.prompt_cache_retention.as_deref()),
        api_path = trace.and_then(|trace| trace.api_path.as_deref()),
        provider_fingerprint_hash = provider_fingerprint_hash.as_deref(),
        cache_miss_reason = CacheMissReason::PreviousResponseIdMissing.as_str(),
        "request_shape_trace"
    );
}

fn effective_prompt_cache_key(
    request: &CreateMessageRequest,
    config: &OpenAiCompatibleClientConfig,
) -> Option<String> {
    request
        .cache_trace_context
        .as_ref()
        .and_then(|trace| trace.prompt_cache_key.clone())
        .or_else(|| config.prompt_cache_key.clone())
}

fn effective_prompt_cache_retention(
    request: &CreateMessageRequest,
    config: &OpenAiCompatibleClientConfig,
) -> Option<String> {
    request
        .cache_trace_context
        .as_ref()
        .and_then(|trace| trace.prompt_cache_retention.clone())
        .or_else(|| config.prompt_cache_retention.clone())
}

/// Convenience constructor that wraps an
/// [`OpenAiCompatibleProvider`] in a [`UniversalModelClient`] using
/// a fresh [`reqwest::Client`].
pub fn openai_compatible_client(config: OpenAiCompatibleClientConfig) -> UniversalModelClient {
    let provider = Arc::new(OpenAiCompatibleProvider::new(config.clone()));
    UniversalModelClient::with_http_client(
        provider,
        crate::provider::build_http_client(config.request_timeout),
    )
}

/// Convenience constructor that reuses an existing
/// [`reqwest::Client`] — matches [`crate::anthropic_client_with_http`].
pub fn openai_compatible_client_with_http(
    config: OpenAiCompatibleClientConfig,
    http: reqwest::Client,
) -> UniversalModelClient {
    UniversalModelClient::with_http_client(Arc::new(OpenAiCompatibleProvider::new(config)), http)
}

#[derive(Default)]
struct OpenAiSseDecoder {
    translator: OpenAiTranslator,
    data_frames_seen: usize,
}

/// End-of-stream truncation error. The message carries enough
/// forensics to tell a gateway that cut the connection mid-reply
/// (frames > 0, possibly a truncated trailing chunk) from one that
/// never streamed a single event (frames == 0).
fn eof_truncation_error(data_frames_seen: usize, residue: Option<&str>) -> ModelError {
    const PREVIEW_CHARS: usize = 200;
    let base = format!(
        "openai stream ended before a finish_reason or [DONE] marker \
         (data frames seen: {data_frames_seen}"
    );
    ModelError::Http(match residue {
        Some(data) => {
            let preview: String = data.chars().take(PREVIEW_CHARS).collect();
            format!("{base}; truncated trailing chunk: {preview:?})")
        }
        None => format!("{base})"),
    })
}

impl SseDecoder for OpenAiSseDecoder {
    fn next_event(&mut self) -> Option<StreamEvent> {
        self.translator.next_pending()
    }

    fn push_frame(&mut self, frame: SseFrame, at_eof: bool) -> ModelResult<()> {
        if !frame.has_data() {
            return Ok(());
        }
        self.data_frames_seen += 1;
        if frame.data == "[DONE]" {
            self.translator.finalize();
            return Ok(());
        }
        if let Err(error) = self.translator.push_chunk(&frame.data) {
            // A frame recovered by the EOF flush can be a chunk the gateway
            // cut mid-JSON. After a terminal chunk it is trailing junk (for
            // example a cut-off `[DONE]`) and the reply is complete; before
            // one it is genuine truncation and must retain the residue.
            if at_eof {
                if self.translator.has_final_stop_reason() {
                    return Ok(());
                }
                return Err(eof_truncation_error(
                    self.data_frames_seen,
                    Some(&frame.data),
                ));
            }
            return Err(error);
        }
        Ok(())
    }

    fn is_terminal(&self) -> bool {
        self.translator.is_drained()
    }

    fn finish(&mut self) -> ModelResult<()> {
        if !self.translator.has_final_stop_reason() {
            return Err(eof_truncation_error(self.data_frames_seen, None));
        }
        self.translator.finalize();
        Ok(())
    }
}

/// Stateful translator that walks OpenAI chat-completions stream
/// chunks and emits a queue of Anthropic-style [`StreamEvent`]s.
///
/// Exposed publicly so alternate OpenAI-compatible transports
/// (custom auth, mock servers, SDK wrappers) can reuse the
/// translation logic without reimplementing it.
#[derive(Debug, Default)]
pub struct OpenAiTranslator {
    started: bool,
    text_block_started: bool,
    text_block_index: usize,
    thinking_block_started: bool,
    thinking_block_index: usize,
    tool_calls: std::collections::HashMap<usize, ToolCallState>,
    block_order: Vec<BlockOrder>,
    pending: Vec<StreamEvent>,
    finished: bool,
    model: String,
    message_id: String,
    usage: Usage,
    final_stop_reason: Option<StopReason>,
    stop_emitted: bool,
}

#[derive(Debug, Clone, Copy)]
enum BlockOrder {
    Text,
    Thinking,
    ToolCall(usize),
}

#[derive(Debug, Clone, Default)]
struct ToolCallState {
    block_index: usize,
    id: String,
    name: String,
    started: bool,
    arguments: String,
}

impl OpenAiTranslator {
    /// Feed one OpenAI stream chunk (the JSON payload from a single
    /// `data:` SSE frame) into the translator. Events become
    /// available via [`Self::next_pending`].
    pub fn push_chunk(&mut self, raw: &str) -> ModelResult<()> {
        let value: Value = serde_json::from_str(raw)
            .map_err(|e| ModelError::Protocol(format!("openai chunk parse: {e} for {raw}")))?;

        // Gateways (opencode Go, new-api, …) report upstream failures
        // as an in-stream `{"error": {...}}` payload and then close
        // the connection. Without this check the frame parses as a
        // chunk with no choices and is silently dropped — the turn
        // then dies with the opaque end-of-stream error instead of
        // the actual cause.
        if let Some(error) = value.get("error") {
            return Err(stream_error_frame_to_model_error(error));
        }

        if !self.started {
            self.started = true;
            self.message_id = value
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("openai_msg")
                .to_string();
            self.model = value
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            self.pending.push(StreamEvent::MessageStart {
                message_id: self.message_id.clone(),
                model: self.model.clone(),
                usage: Usage::default(),
            });
        }

        if let Some(usage) = value.get("usage") {
            let u: Usage = serde_json::from_value(usage.clone()).unwrap_or_default();
            self.usage.merge(&u);
        }

        let choices = value
            .get("choices")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for choice in choices {
            let finish_reason = choice
                .get("finish_reason")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(delta) = choice.get("delta") {
                // `reasoning_content` is the DeepSeek spelling most vendors
                // adopted; Ollama's OpenAI surface (and OpenRouter) stream
                // the same thing as `reasoning`.
                let thinking = delta
                    .get("reasoning_content")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .or_else(|| delta.get("reasoning").and_then(|v| v.as_str()));
                if let Some(thinking) = thinking {
                    if !thinking.is_empty() {
                        self.ensure_thinking_block_started();
                        self.pending.push(StreamEvent::ContentBlockDelta {
                            index: self.thinking_block_index,
                            delta: ContentBlockDelta::ThinkingDelta {
                                thinking: thinking.to_string(),
                            },
                        });
                    }
                }
                // Volcengine Ark returns the raw chain of thought encrypted
                // and wants it back on tool turns; it rides along as the
                // thinking block's signature.
                if let Some(encrypted) = delta.get("encrypted_content").and_then(|v| v.as_str()) {
                    if !encrypted.is_empty() {
                        self.ensure_thinking_block_started();
                        self.pending.push(StreamEvent::ContentBlockDelta {
                            index: self.thinking_block_index,
                            delta: ContentBlockDelta::SignatureDelta {
                                signature: encrypted.to_string(),
                            },
                        });
                    }
                }
                if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        self.ensure_text_block_started();
                        self.pending.push(StreamEvent::ContentBlockDelta {
                            index: self.text_block_index,
                            delta: ContentBlockDelta::TextDelta {
                                text: text.to_string(),
                            },
                        });
                    }
                }
                if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tool_calls {
                        self.apply_tool_call_delta(tc);
                    }
                }
            }
            if let Some(reason) = finish_reason {
                self.final_stop_reason = Some(finish_reason_to_stop(&reason));
            }
        }
        Ok(())
    }

    fn ensure_thinking_block_started(&mut self) {
        if !self.thinking_block_started {
            self.thinking_block_index = self.block_order.len();
            self.block_order.push(BlockOrder::Thinking);
            self.pending.push(StreamEvent::ContentBlockStart {
                index: self.thinking_block_index,
                content_block: ContentBlockStart::Thinking {
                    thinking: String::new(),
                    data: None,
                },
            });
            self.thinking_block_started = true;
        }
    }

    fn ensure_text_block_started(&mut self) {
        if !self.text_block_started {
            self.text_block_index = self.block_order.len();
            self.block_order.push(BlockOrder::Text);
            self.pending.push(StreamEvent::ContentBlockStart {
                index: self.text_block_index,
                content_block: ContentBlockStart::Text {
                    text: String::new(),
                },
            });
            self.text_block_started = true;
        }
    }

    fn apply_tool_call_delta(&mut self, delta: &Value) {
        let tc_index = delta
            .get("index")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(0);
        let entry = self.tool_calls.entry(tc_index).or_insert_with(|| {
            let block_index = self.block_order.len();
            self.block_order.push(BlockOrder::ToolCall(tc_index));
            ToolCallState {
                block_index,
                ..Default::default()
            }
        });
        let block_index = entry.block_index;
        if let Some(id) = delta.get("id").and_then(|v| v.as_str()) {
            if !id.is_empty() {
                entry.id = id.to_string();
            }
        }
        if let Some(func) = delta.get("function") {
            if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                if !name.is_empty() {
                    entry.name = name.to_string();
                }
            }
            if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                entry.arguments.push_str(args);
            }
        }

        let can_start = !entry.started && !entry.id.is_empty() && !entry.name.is_empty();
        let (started_id, started_name) = if can_start {
            entry.started = true;
            (entry.id.clone(), entry.name.clone())
        } else {
            (String::new(), String::new())
        };
        let pending_arg_fragment = if entry.started {
            let args_owned = entry.arguments.clone();
            entry.arguments.clear();
            args_owned
        } else {
            String::new()
        };

        if can_start {
            self.pending.push(StreamEvent::ContentBlockStart {
                index: block_index,
                content_block: ContentBlockStart::ToolUse {
                    id: started_id,
                    name: started_name,
                },
            });
        }
        if !pending_arg_fragment.is_empty() {
            self.pending.push(StreamEvent::ContentBlockDelta {
                index: block_index,
                delta: ContentBlockDelta::InputJsonDelta {
                    partial_json: pending_arg_fragment,
                },
            });
        }
    }

    /// Emit the trailing `content_block_stop` / `message_delta` /
    /// `message_stop` events. Idempotent.
    pub fn finalize(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        for order in &self.block_order {
            let index = match order {
                BlockOrder::Text => self.text_block_index,
                BlockOrder::Thinking => self.thinking_block_index,
                BlockOrder::ToolCall(i) => {
                    self.tool_calls.get(i).map(|t| t.block_index).unwrap_or(*i)
                }
            };
            self.pending.push(StreamEvent::ContentBlockStop { index });
        }
        self.pending.push(StreamEvent::MessageDelta {
            delta: MessageDeltaFields {
                stop_reason: self.final_stop_reason.clone(),
                usage: self.usage,
            },
        });
        self.pending.push(StreamEvent::MessageStop);
        self.stop_emitted = true;
    }

    /// Pop the next translated event, if any.
    pub fn next_pending(&mut self) -> Option<StreamEvent> {
        if self.pending.is_empty() {
            None
        } else {
            Some(self.pending.remove(0))
        }
    }

    /// Whether the translator has emitted its `message_stop` and
    /// the pending queue is drained.
    pub fn is_drained(&self) -> bool {
        self.pending.is_empty() && self.stop_emitted
    }

    fn has_final_stop_reason(&self) -> bool {
        self.final_stop_reason.is_some()
    }
}

/// Convert an in-stream `{"error": ...}` payload into a
/// [`ModelError`]. Gateways vary between a bare string and the
/// OpenAI-style `{message, type, code}` object; surface whatever is
/// there. Classified as [`ModelError::Http`] (transient) — the retry
/// middleware treats it like any other stream interruption.
fn stream_error_frame_to_model_error(error: &Value) -> ModelError {
    let message = match error {
        Value::String(message) => message.clone(),
        _ => error
            .get("message")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| error.to_string()),
    };
    let mut detail = format!("openai stream reported an error: {message}");
    for key in ["type", "code"] {
        if let Some(value) = error.get(key).filter(|v| !v.is_null()) {
            let value = value
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| value.to_string());
            detail.push_str(&format!(" ({key}: {value})"));
        }
    }
    ModelError::Http(detail)
}

fn finish_reason_to_stop(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => StopReason::Refusal,
        other => StopReason::Other(other.to_string()),
    }
}

/// Return true if `model` is a DeepSeek model name (any variant — including
/// `-pro`/`-thinking`/`-chat`/`-reasoner`/etc).
///
/// Single source of truth for "is this DeepSeek?" judgments across the
/// crate. Used by:
/// - [`effective_chat_completions_compat`] — to escalate to
///   [`ChatCompletionsCompat::DEEPSEEK_THINKING`] when a DeepSeek model is
///   routed through a relay whose `base_url` does NOT match the dedicated
///   DeepSeek host detector in wiring (`is_deepseek_chat_endpoint`).
/// - `ContextPruneMiddleware` — to enable cache-stable mode and skip the
///   in-place sliding-window mutations (`clear_old_tool_results`,
///   `deduplicate_tool_calls`, `purge_error_inputs`, `strip_old_thinking_blocks`)
///   that collapse DeepSeek's prefix cache on every outbound request.
///
/// We deliberately use a broad prefix match. Non-thinking DeepSeek variants
/// won't have produced Thinking blocks so reasoning_content replay degrades
/// to a no-op, and `content: ""` on tool-only turns is accepted by the whole
/// DeepSeek API family — so the fallback is safe even if the model isn't
/// actually thinking-mode.
pub fn is_deepseek_model(model: &str) -> bool {
    model.trim().to_ascii_lowercase().starts_with("deepseek")
}

/// Resolve the effective per-request compat from the provider config and the
/// model being requested. Honors explicit non-default compat on the config;
/// then what the endpoint's vendor documents; otherwise applies a model-name
/// fallback for DeepSeek-via-relay setups (axonhub, openrouter, custom
/// proxies) where neither the pin nor the host says who is upstream.
fn effective_chat_completions_compat(
    config: &OpenAiCompatibleClientConfig,
    model: &str,
) -> ChatCompletionsCompat {
    if config.compat != ChatCompletionsCompat::OPENAI {
        return config.compat;
    }
    ChatCompletionsCompat::for_vendor(effective_vendor_for_model(config, model), model)
}

/// The vendor whose dialect this request is written in.
///
/// The pin or the host decides, with one fallback kept from before the
/// vendor table existed: a DeepSeek-named model reached through an
/// unrecognised relay (or through a config still on the default OpenAI
/// base URL, which is what a relay setup looks like from here) speaks
/// DeepSeek's dialect — the model, not the relay, is what rejects a
/// request that omits `reasoning_content`.
fn effective_vendor_for_model(
    config: &OpenAiCompatibleClientConfig,
    model: &str,
) -> ProviderVendor {
    let vendor = config.effective_vendor();
    if matches!(vendor, ProviderVendor::Unknown | ProviderVendor::OpenAi)
        && is_deepseek_model(model)
    {
        return ProviderVendor::DeepSeek;
    }
    vendor
}

/// The wire rules for this request.
fn effective_wire_rules(config: &OpenAiCompatibleClientConfig, model: &str) -> ChatWireRules {
    effective_vendor_for_model(config, model).chat_wire_rules(model)
}

/// Build the OpenAI-shaped request body from the shared
/// [`CreateMessageRequest`].
pub fn build_openai_request_body(
    request: &CreateMessageRequest,
    config: &OpenAiCompatibleClientConfig,
) -> Value {
    #[derive(Serialize)]
    struct Wire<'a> {
        model: &'a str,
        messages: Vec<Value>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tools: Vec<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_choice: Option<Value>,
        max_tokens: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        temperature: Option<f32>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        stop: Vec<String>,
        stream: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        stream_options: Option<Value>,
    }

    let mut messages = Vec::new();
    if let Some(system) = &request.system {
        messages.push(serde_json::json!({
            "role": "system",
            "content": system,
        }));
    }
    let compat = effective_chat_completions_compat(config, &request.model);
    for msg in request.messages_with_transient_context() {
        messages.extend(convert_message_to_openai(&msg, compat));
    }

    let tools = request
        .tools
        .iter()
        .map(|t: &Tool| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }
            })
        })
        .collect::<Vec<_>>();

    let tool_choice = request.tool_choice.as_ref().map(|c| match c {
        ToolChoice::Auto => Value::String("auto".into()),
        ToolChoice::Any => Value::String("required".into()),
        ToolChoice::Tool { name } => serde_json::json!({
            "type": "function",
            "function": { "name": name }
        }),
        ToolChoice::None => Value::String("none".into()),
    });

    let wire = Wire {
        model: &request.model,
        messages,
        tools,
        tool_choice,
        max_tokens: request.max_tokens,
        temperature: request.temperature,
        stop: request.stop_sequences.clone(),
        stream: true,
        stream_options: Some(serde_json::json!({ "include_usage": true })),
    };
    let mut body = serde_json::to_value(&wire).expect("openai wire request serializes");
    let rules = effective_wire_rules(config, &request.model);
    apply_openai_request_options(&mut body, request, config);
    apply_per_turn_thinking_overrides(&mut body, request, rules);
    apply_vendor_wire_rules(&mut body, rules);
    if let Some(cache_key) = config.prompt_cache_key.as_deref() {
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "prompt_cache_key".into(),
                Value::String(cache_key.to_string()),
            );
        }
    }
    // Only the wire-legal retention values are emitted; Rebon-internal hints
    // ("session", "provider_native", "same_dispatch") stay diagnostics-only so
    // endpoints that validate unknown values never see them. Gateways with a
    // short default TTL (e.g. opencode Go's ~5 minutes) honour "24h" and keep
    // the session prefix cached across idle gaps.
    if let Some(retention) = config
        .prompt_cache_retention
        .as_deref()
        .filter(|value| matches!(*value, "in-memory" | "24h"))
    {
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "prompt_cache_retention".into(),
                Value::String(retention.to_string()),
            );
        }
    }
    crate::apply_openai_service_tier(&mut body, config.service_tier.as_ref());
    body
}

/// Apply per-turn `reasoning_effort` and `thinking` from the request onto the
/// wire body, taking precedence over any static `request_options` defaults.
///
/// Why this layers AFTER [`apply_openai_request_options`]:
/// `request_options` is the provider/model-level default a user pins in their
/// config (e.g. DeepSeek `thinkingEnabled: true, thinkingEffort: "high"`). The
/// TUI `/effort` selector is an explicit *per-turn* override — when the user
/// picks `xhigh` on this turn they expect it to beat the static `high`.
///
/// Provider gating, by the vendor's [`ChatWireRules`]:
/// - `reasoning_effort` is emitted in the vendor's vocabulary (folded by
///   [`crate::vendor::EffortVocabulary`]) and not at all where the vendor
///   rejects it (MiniMax, Kimi K2.x, DashScope).
/// - The thinking switch is spelled the way the vendor spells it —
///   `thinking.type` (DeepSeek, Zhipu, Ark, Kimi K2.x), `thinking.type:
///   adaptive` (MiniMax) or `enable_thinking` (Qwen, SiliconFlow) — and is
///   never sent where there is no switch (OpenAI, Kimi K3).
/// - Where the vendor's thinking endpoint rejects `temperature` (DeepSeek),
///   enabling thinking drops it. Matches the `omit_body_fields =
///   ["temperature"]` that the static-config path sets up.
fn apply_per_turn_thinking_overrides(
    body: &mut Value,
    request: &CreateMessageRequest,
    rules: ChatWireRules,
) {
    use crate::request::ThinkingConfig;
    let Some(obj) = body.as_object_mut() else {
        return;
    };

    if let Some(effort) = request.reasoning_effort {
        let wire = serde_json::to_value(effort)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .and_then(|raw| rules.effort.fold(&raw));
        if let Some(wire) = wire {
            obj.insert("reasoning_effort".into(), Value::String(wire.to_string()));
        }
    }

    let enabled = match &request.thinking {
        Some(ThinkingConfig::Enabled { .. }) => true,
        Some(ThinkingConfig::Disabled) => false,
        None => return,
    };
    if !insert_thinking_switch(obj, rules.thinking, enabled) {
        return;
    }
    if enabled && rules.drop_temperature_when_thinking {
        obj.remove("temperature");
    }
}

/// Write the thinking switch in `dialect`'s spelling. Returns whether the
/// dialect has a switch at all.
fn insert_thinking_switch(
    obj: &mut serde_json::Map<String, Value>,
    dialect: ThinkingDialect,
    enabled: bool,
) -> bool {
    match dialect {
        ThinkingDialect::ThinkingType => {
            obj.insert(
                "thinking".into(),
                serde_json::json!({ "type": if enabled { "enabled" } else { "disabled" } }),
            );
        }
        ThinkingDialect::AdaptiveThinkingType => {
            obj.insert(
                "thinking".into(),
                serde_json::json!({ "type": if enabled { "adaptive" } else { "disabled" } }),
            );
        }
        ThinkingDialect::EnableThinking => {
            obj.insert("enable_thinking".into(), Value::Bool(enabled));
        }
        ThinkingDialect::None | ThinkingDialect::Verbatim => return false,
    }
    true
}

/// Reshape the finished body to the vendor's dialect.
///
/// Runs last so it also covers what the static `options.body` /
/// `thinkingEnabled` path put in: a `thinking: {type}` object written for
/// DeepSeek is re-spelled as `enable_thinking` for Qwen, as `adaptive` for
/// MiniMax, and removed for a vendor with no switch (Kimi K3 errors on it);
/// a `reasoning_effort` outside the vendor's vocabulary is folded into it
/// or dropped; the output cap moves to `max_completion_tokens` where
/// `max_tokens` is deprecated or capped; fields the vendor documents as
/// rejected are stripped; MiniMax gets `reasoning_split`.
fn apply_vendor_wire_rules(body: &mut Value, rules: ChatWireRules) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };

    // Thinking switch, whatever spelling it arrived in. An unrecognised
    // listener gets the config's spelling untouched.
    let requested = if rules.thinking == ThinkingDialect::Verbatim {
        None
    } else {
        match obj.get("thinking") {
            Some(Value::Object(thinking)) => thinking
                .get("type")
                .and_then(Value::as_str)
                .map(|kind| !matches!(kind, "disabled")),
            Some(Value::Bool(flag)) => Some(*flag),
            _ => None,
        }
        .or_else(|| obj.get("enable_thinking").and_then(Value::as_bool))
    };
    if let Some(enabled) = requested {
        let already_native = match rules.thinking {
            ThinkingDialect::ThinkingType | ThinkingDialect::AdaptiveThinkingType => {
                obj.get("thinking").is_some_and(Value::is_object)
                    && obj.get("enable_thinking").is_none()
            }
            ThinkingDialect::EnableThinking => obj.get("thinking").is_none(),
            ThinkingDialect::None => false,
            ThinkingDialect::Verbatim => true,
        };
        if !already_native {
            obj.remove("thinking");
            obj.remove("enable_thinking");
            insert_thinking_switch(obj, rules.thinking, enabled);
        } else if rules.thinking == ThinkingDialect::AdaptiveThinkingType {
            // MiniMax spells "on" as `adaptive`; `enabled` is not a value
            // its reference lists.
            if enabled {
                obj.insert("thinking".into(), serde_json::json!({ "type": "adaptive" }));
            }
        }
    }

    // Effort vocabulary.
    if let Some(raw) = obj
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        match rules.effort.fold(&raw) {
            Some(folded) if folded != raw => {
                obj.insert("reasoning_effort".into(), Value::String(folded.to_string()));
            }
            Some(_) => {}
            None => {
                obj.remove("reasoning_effort");
            }
        }
    }

    // Output cap field.
    if rules.max_tokens_field == MaxTokensField::MaxCompletionTokens {
        if let Some(cap) = obj.remove("max_tokens") {
            obj.entry("max_completion_tokens").or_insert(cap);
        }
    }

    for field in rules.unsupported_fields {
        obj.remove(*field);
    }

    if rules.reasoning_split {
        obj.insert("reasoning_split".into(), Value::Bool(true));
    }
}

fn apply_openai_request_options(
    body: &mut Value,
    request: &CreateMessageRequest,
    config: &OpenAiCompatibleClientConfig,
) {
    let mut effective = config.request_options.clone();
    if let Some(model_options) = config.model_request_options.get(&request.model) {
        effective.merge_from(model_options);
    }
    if effective.is_empty() {
        return;
    }

    let Some(obj) = body.as_object_mut() else {
        return;
    };

    for field in &effective.omit_body_fields {
        let field = normalize_openai_body_field_name(field);
        if !is_protected_openai_body_field(&field) {
            obj.remove(&field);
        }
    }
    merge_openai_body_options(obj, &effective.body);
    merge_openai_body_options(obj, &effective.extra_body);
}

fn merge_openai_body_options(
    target: &mut serde_json::Map<String, Value>,
    source: &serde_json::Map<String, Value>,
) {
    for (key, value) in source {
        let normalized_key = normalize_openai_body_field_name(key);
        if is_protected_openai_body_field(&normalized_key) {
            continue;
        }
        target.insert(
            normalized_key.clone(),
            normalize_openai_body_value(&normalized_key, value.clone()),
        );
    }
}

fn is_protected_openai_body_field(field: &str) -> bool {
    matches!(field, "model" | "messages" | "tools" | "stream")
}

fn normalize_openai_body_field_name(field: &str) -> String {
    match field {
        "reasoningEffort" => "reasoning_effort",
        "streamOptions" => "stream_options",
        "responseFormat" => "response_format",
        "maxTokens" => "max_tokens",
        "maxCompletionTokens" => "max_completion_tokens",
        "topP" => "top_p",
        "parallelToolCalls" => "parallel_tool_calls",
        "presencePenalty" => "presence_penalty",
        "frequencyPenalty" => "frequency_penalty",
        "serviceTier" => "service_tier",
        other => other,
    }
    .to_string()
}

fn normalize_openai_body_value(field: &str, value: Value) -> Value {
    match (field, value) {
        ("reasoning_effort", Value::String(raw)) => {
            Value::String(normalize_reasoning_effort_value(&raw).to_string())
        }
        (_, Value::Object(map)) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let normalized_key = normalize_openai_body_field_name(&key);
                    (
                        normalized_key.clone(),
                        normalize_openai_body_value(&normalized_key, value),
                    )
                })
                .collect(),
        ),
        (_, Value::Array(values)) => Value::Array(
            values
                .into_iter()
                .map(|value| normalize_openai_nested_value(value))
                .collect(),
        ),
        (_, other) => other,
    }
}

fn normalize_openai_nested_value(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let normalized_key = normalize_openai_body_field_name(&key);
                    (
                        normalized_key.clone(),
                        normalize_openai_body_value(&normalized_key, value),
                    )
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(normalize_openai_nested_value)
                .collect(),
        ),
        other => other,
    }
}

fn normalize_reasoning_effort_value(raw: &str) -> &str {
    match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        _ => raw,
    }
}

fn convert_message_to_openai(msg: &Message, compat: ChatCompletionsCompat) -> Vec<Value> {
    match msg.role {
        Role::User => match convert_user_message(msg) {
            Value::Array(messages) => messages,
            message => vec![message],
        },
        Role::Assistant => vec![convert_assistant_message(msg, compat)],
        Role::System => Vec::new(),
    }
}

fn openai_image_part(image: &ImageBlock) -> Value {
    let image_url = format!(
        "data:{};base64,{}",
        image.source.media_type, image.source.data
    );
    serde_json::json!({
        "type": "image_url",
        "image_url": { "url": image_url },
    })
}

fn append_tool_result_for_openai(
    tr: &crate::types::ToolResultBlock,
    tool_results: &mut Vec<Value>,
    text_parts: &mut Vec<Value>,
) {
    match &tr.content {
        ToolResultContent::Text(text) => tool_results.push(serde_json::json!({
            "role": "tool",
            "tool_call_id": tr.tool_use_id,
            "content": text,
        })),
        ToolResultContent::Blocks(blocks) => {
            let mut text = Vec::new();
            for block in blocks {
                match block {
                    ToolResultContentBlock::Text(part) => text.push(part.text.clone()),
                    ToolResultContentBlock::Image(image) => {
                        text_parts.push(openai_image_part(image))
                    }
                    ToolResultContentBlock::Document(document) => {
                        text_parts.push(serde_json::json!({
                            "type": "file",
                            "file": {
                                "filename": "document.pdf",
                                "file_data": format!(
                                    "data:{};base64,{}",
                                    document.source.media_type,
                                    document.source.data
                                ),
                            }
                        }))
                    }
                }
            }
            tool_results.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": tr.tool_use_id,
                "content": text.join("\n"),
            }));
            for line in text.into_iter().filter(|line| !line.is_empty()) {
                text_parts.push(serde_json::json!({
                    "type": "text",
                    "text": line,
                }));
            }
        }
    }
}

fn convert_user_message(msg: &Message) -> Value {
    let mut any_tool = false;
    let mut tool_results: Vec<Value> = Vec::new();
    let mut text_parts: Vec<Value> = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text(t) => text_parts.push(serde_json::json!({
                "type": "text",
                "text": t.text,
            })),
            ContentBlock::Image(img) => {
                text_parts.push(openai_image_part(img));
            }
            ContentBlock::ToolResult(tr) => {
                any_tool = true;
                append_tool_result_for_openai(tr, &mut tool_results, &mut text_parts);
            }
            _ => {}
        }
    }
    if any_tool && text_parts.is_empty() {
        return Value::Array(tool_results);
    }
    if any_tool {
        let mut combined = tool_results;
        combined.push(serde_json::json!({
            "role": "user",
            "content": user_content_from_parts(text_parts),
        }));
        return Value::Array(combined);
    }
    serde_json::json!({
        "role": "user",
        "content": user_content_from_parts(text_parts),
    })
}

fn user_content_from_parts(text_parts: Vec<Value>) -> Value {
    if text_parts.len() == 1 && text_parts[0]["type"] == "text" {
        text_parts[0]["text"].clone()
    } else {
        Value::Array(text_parts)
    }
}

fn convert_assistant_message(msg: &Message, compat: ChatCompletionsCompat) -> Value {
    // Wire-shape of historical assistant turns governs prompt-cache hit
    // rates because providers cache complete prefix units byte-exactly:
    // any field-level divergence on a replayed assistant turn collapses
    // the cache to the short common-prefix detection unit. Both fields
    // we shape below (`reasoning_content` and `content`) are subject to
    // *non-symmetric* provider rules — see ChatCompletionsCompat docs.
    let mut text_parts: Vec<String> = Vec::new();
    let mut thinking_parts: Vec<String> = Vec::new();
    let mut encrypted_reasoning: Option<String> = None;
    let mut tool_calls: Vec<Value> = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text(t) => text_parts.push(t.text.clone()),
            ContentBlock::Thinking(t) => {
                thinking_parts.push(t.thinking.clone());
                if encrypted_reasoning.is_none() {
                    encrypted_reasoning = t
                        .signature
                        .as_deref()
                        .filter(|sig| !sig.is_empty())
                        .map(str::to_owned);
                }
            }
            ContentBlock::ToolUse(tu) => {
                tool_calls.push(serde_json::json!({
                    "id": tu.id,
                    "type": "function",
                    "function": {
                        "name": tu.name,
                        "arguments": tu.input.to_string(),
                    }
                }));
            }
            // Chat Completions has no compaction item, so a blob minted
            // by a server-side compaction elsewhere has nowhere to go.
            // Dropping it takes the history it replaced with it — say so
            // rather than letting the context quietly shrink.
            block if block.is_backend_bound_compaction() => {
                tracing::warn!(
                    "openai: dropping a server-side compaction block the session carried over; \
                     the history it replaced is not recoverable on this provider"
                );
            }
            _ => {}
        }
    }
    let has_tool_calls = !tool_calls.is_empty();
    let content_text = text_parts.join("");

    let replay_reasoning = !thinking_parts.is_empty()
        && match compat.reasoning_replay {
            ReasoningReplayMode::Never => false,
            ReasoningReplayMode::OnlyOnToolCallTurns => has_tool_calls,
            ReasoningReplayMode::Always => true,
        };

    let mut obj = serde_json::Map::new();
    obj.insert("role".to_string(), Value::String("assistant".into()));

    if content_text.is_empty() && has_tool_calls {
        match compat.tool_call_content {
            ToolCallContentMode::OmitIfAllowed => {
                // No `content` key at all — OpenAI's schema permits this
                // when `tool_calls` is present, and omitting matches the
                // streamed assistant turn byte-for-byte.
            }
            ToolCallContentMode::NullIfAllowed => {
                obj.insert("content".to_string(), Value::Null);
            }
            ToolCallContentMode::EmptyStringRequired => {
                obj.insert("content".to_string(), Value::String(String::new()));
            }
        }
    } else {
        obj.insert("content".to_string(), Value::String(content_text));
    }

    if replay_reasoning {
        obj.insert(
            "reasoning_content".to_string(),
            Value::String(thinking_parts.join("")),
        );
        if compat.replay_encrypted_reasoning {
            if let Some(encrypted) = encrypted_reasoning {
                obj.insert("encrypted_content".to_string(), Value::String(encrypted));
            }
        }
    }
    if has_tool_calls {
        obj.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ModelClient;
    use crate::request::CacheTraceContext;
    use crate::types::{TextBlock, ToolResultContent, ToolResultContentBlock, ToolUseBlock};

    fn drain(t: &mut OpenAiTranslator) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(e) = t.next_pending() {
            out.push(e);
        }
        out
    }

    #[test]
    fn translator_handles_plain_text_stream() {
        let mut t = OpenAiTranslator::default();
        t.push_chunk(
            r#"{"id":"chatcmpl_1","model":"gpt-4o","choices":[{"delta":{"role":"assistant","content":"Hello"},"index":0,"finish_reason":null}]}"#,
        )
        .unwrap();
        t.push_chunk(
            r#"{"id":"chatcmpl_1","model":"gpt-4o","choices":[{"delta":{"content":" world"},"index":0,"finish_reason":null}]}"#,
        )
        .unwrap();
        t.push_chunk(
            r#"{"id":"chatcmpl_1","model":"gpt-4o","choices":[{"delta":{},"index":0,"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
        )
        .unwrap();
        t.finalize();

        let events = drain(&mut t);
        assert!(matches!(events[0], StreamEvent::MessageStart { .. }));
        assert!(matches!(events[1], StreamEvent::ContentBlockStart { .. }));
        match &events[2] {
            StreamEvent::ContentBlockDelta {
                delta: ContentBlockDelta::TextDelta { text },
                ..
            } => assert_eq!(text, "Hello"),
            other => panic!("unexpected event: {other:?}"),
        }
        match &events[3] {
            StreamEvent::ContentBlockDelta {
                delta: ContentBlockDelta::TextDelta { text },
                ..
            } => assert_eq!(text, " world"),
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(matches!(events[4], StreamEvent::ContentBlockStop { .. }));
        match &events[5] {
            StreamEvent::MessageDelta { delta } => {
                assert_eq!(delta.stop_reason, Some(StopReason::EndTurn));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(matches!(events[6], StreamEvent::MessageStop));
    }

    #[test]
    fn translator_handles_tool_call_stream() {
        let mut t = OpenAiTranslator::default();
        t.push_chunk(
            r#"{"id":"chatcmpl_2","model":"gpt-4o","choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_123","type":"function","function":{"name":"Read","arguments":"{\"path"}}]},"index":0}]}"#,
        )
        .unwrap();
        t.push_chunk(
            r#"{"id":"chatcmpl_2","model":"gpt-4o","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\":\"foo.rs\"}"}}]},"index":0,"finish_reason":"tool_calls"}]}"#,
        )
        .unwrap();
        t.finalize();

        let events = drain(&mut t);
        assert!(matches!(events[0], StreamEvent::MessageStart { .. }));
        match &events[1] {
            StreamEvent::ContentBlockStart {
                content_block: ContentBlockStart::ToolUse { id, name },
                ..
            } => {
                assert_eq!(id, "call_123");
                assert_eq!(name, "Read");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let mut combined = String::new();
        let mut saw_stop = false;
        let mut saw_delta = false;
        for e in &events[2..] {
            match e {
                StreamEvent::ContentBlockDelta {
                    delta: ContentBlockDelta::InputJsonDelta { partial_json },
                    ..
                } => combined.push_str(partial_json),
                StreamEvent::ContentBlockStop { .. } => saw_stop = true,
                StreamEvent::MessageDelta { delta } => {
                    assert_eq!(delta.stop_reason, Some(StopReason::ToolUse));
                    saw_delta = true;
                }
                StreamEvent::MessageStop => {}
                _ => {}
            }
        }
        assert_eq!(combined, r#"{"path":"foo.rs"}"#);
        assert!(saw_stop);
        assert!(saw_delta);
    }

    #[test]
    fn translator_handles_reasoning_content_stream() {
        let mut t = OpenAiTranslator::default();
        t.push_chunk(
            r#"{"id":"chatcmpl_ds","model":"deepseek-v4-pro","choices":[{"delta":{"role":"assistant","reasoning_content":"Think"},"index":0,"finish_reason":null}]}"#,
        )
        .unwrap();
        t.push_chunk(
            r#"{"id":"chatcmpl_ds","model":"deepseek-v4-pro","choices":[{"delta":{"reasoning_content":" first","content":"Answer"},"index":0,"finish_reason":"stop"}]}"#,
        )
        .unwrap();
        t.finalize();

        let events = drain(&mut t);
        assert!(matches!(
            events[1],
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Thinking { .. }
            }
        ));
        match &events[2] {
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ThinkingDelta { thinking },
            } => assert_eq!(thinking, "Think"),
            other => panic!("unexpected event: {other:?}"),
        }
        match &events[3] {
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ThinkingDelta { thinking },
            } => assert_eq!(thinking, " first"),
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(matches!(
            events[4],
            StreamEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlockStart::Text { .. }
            }
        ));
        match &events[5] {
            StreamEvent::ContentBlockDelta {
                index: 1,
                delta: ContentBlockDelta::TextDelta { text },
            } => assert_eq!(text, "Answer"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_errors_when_transport_ends_without_finish_reason() {
        let data = "data: {\"id\":\"chatcmpl_1\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"content\":\"{\\\"a\\\":\"},\"index\":0,\"finish_reason\":null}]}\n\n";
        let (_, error) = drive_translator_stream(vec![data]).await;
        assert!(error
            .expect("truncated stream must fail")
            .to_string()
            .contains("ended before a finish_reason"));
    }

    async fn drive_translator_stream(chunks: Vec<&str>) -> (Vec<StreamEvent>, Option<ModelError>) {
        let bytes = futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok(bytes::Bytes::from(c.to_string())))
                .collect::<Vec<_>>(),
        );
        let mut stream = decode_sse_stream(Box::pin(bytes), OpenAiSseDecoder::default());
        let mut events = Vec::new();
        let mut error = None;
        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => events.push(event),
                Err(err) => {
                    error = Some(err);
                    break;
                }
            }
        }
        (events, error)
    }

    #[tokio::test]
    async fn stream_completes_when_final_frame_lacks_trailing_blank_line() {
        // Gateway closes right after the terminal chunk's newline —
        // no blank line, no [DONE]. The EOF flush must still see the
        // finish_reason and finalize cleanly.
        let (events, error) = drive_translator_stream(vec![
            "data: {\"id\":\"chatcmpl_1\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"index\":0,\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl_1\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{},\"index\":0,\"finish_reason\":\"stop\"}]}\n",
        ])
        .await;
        assert!(error.is_none(), "unexpected error: {error:?}");
        assert!(matches!(events.last(), Some(StreamEvent::MessageStop)));
    }

    #[tokio::test]
    async fn stream_completes_when_done_marker_lacks_trailing_newline() {
        let (events, error) = drive_translator_stream(vec![
            "data: {\"id\":\"chatcmpl_1\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"index\":0,\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]",
        ])
        .await;
        assert!(error.is_none(), "unexpected error: {error:?}");
        assert!(matches!(events.last(), Some(StreamEvent::MessageStop)));
    }

    #[tokio::test]
    async fn stream_surfaces_gateway_error_frame() {
        let (_, error) = drive_translator_stream(vec![
            "data: {\"error\":{\"message\":\"upstream exploded\",\"type\":\"bad_gateway\",\"code\":502}}\n\n",
        ])
        .await;
        let message = error.expect("expected an error").to_string();
        assert!(message.contains("upstream exploded"), "{message}");
        assert!(message.contains("bad_gateway"), "{message}");
        assert!(message.contains("502"), "{message}");
    }

    #[tokio::test]
    async fn stream_truncation_error_reports_frame_count_and_residue() {
        // Connection cut mid-chunk: one complete delta frame, then a
        // trailing chunk truncated mid-JSON with no finish_reason.
        let (_, error) = drive_translator_stream(vec![
            "data: {\"id\":\"chatcmpl_1\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"index\":0,\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl_1\",\"cho",
        ])
        .await;
        let message = error.expect("expected an error").to_string();
        assert!(
            message.contains("ended before a finish_reason"),
            "{message}"
        );
        assert!(message.contains("data frames seen: 2"), "{message}");
        assert!(
            message.contains("{\\\"id\\\":\\\"chatcmpl_1\\\",\\\"cho"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn stream_ignores_trailing_junk_after_terminal_chunk() {
        // Cut-off [DONE] after the finish_reason chunk — the reply is
        // complete; the junk must not surface as an error.
        let (events, error) = drive_translator_stream(vec![
            "data: {\"id\":\"chatcmpl_1\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"index\":0,\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DO",
        ])
        .await;
        assert!(error.is_none(), "unexpected error: {error:?}");
        assert!(matches!(events.last(), Some(StreamEvent::MessageStop)));
    }

    #[test]
    fn effective_prompt_cache_key_prefers_request_trace_over_config() {
        let mut config = OpenAiCompatibleClientConfig::default();
        config.prompt_cache_key = Some("config-key".into());
        config.prompt_cache_retention = Some("config-retention".into());

        let mut request = CreateMessageRequest::simple("gpt-4o", "ping");
        request.cache_trace_context = Some(CacheTraceContext {
            prompt_cache_key: Some("trace-key".into()),
            prompt_cache_retention: Some("trace-retention".into()),
            ..Default::default()
        });

        assert_eq!(
            effective_prompt_cache_key(&request, &config).as_deref(),
            Some("trace-key")
        );
        assert_eq!(
            effective_prompt_cache_retention(&request, &config).as_deref(),
            Some("trace-retention")
        );
    }

    #[test]
    fn build_openai_body_includes_prompt_cache_key_when_configured() {
        let mut config = OpenAiCompatibleClientConfig::default();
        config.prompt_cache_key = Some("worker-family-key".into());

        let body =
            build_openai_request_body(&CreateMessageRequest::simple("gpt-4o", "ping"), &config);

        assert_eq!(body["prompt_cache_key"], "worker-family-key");
    }

    #[test]
    fn build_openai_body_emits_only_wire_legal_prompt_cache_retention() {
        let request = CreateMessageRequest::simple("gpt-4o", "ping");

        let mut config = OpenAiCompatibleClientConfig::default();
        config.prompt_cache_retention = Some("24h".into());
        let body = build_openai_request_body(&request, &config);
        assert_eq!(body["prompt_cache_retention"], "24h");

        config.prompt_cache_retention = Some("in-memory".into());
        let body = build_openai_request_body(&request, &config);
        assert_eq!(body["prompt_cache_retention"], "in-memory");

        // Rebon-internal hints must never reach the wire: endpoints that
        // validate the field would reject the whole request.
        for internal in ["session", "provider_native", "same_dispatch"] {
            config.prompt_cache_retention = Some(internal.into());
            let body = build_openai_request_body(&request, &config);
            assert!(
                body.get("prompt_cache_retention").is_none(),
                "internal hint {internal:?} leaked to the wire"
            );
        }
    }

    #[test]
    fn build_openai_body_includes_system_tools_and_stream_options() {
        let req = CreateMessageRequest::simple("gpt-4o", "ping")
            .with_system("Be terse.")
            .with_tools(vec![Tool {
                name: "Read".into(),
                description: "Read a file".into(),
                input_schema: serde_json::json!({"type":"object"}),
            }]);
        let body = build_openai_request_body(&req, &OpenAiCompatibleClientConfig::default());
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "Be terse.");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "ping");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "Read");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn build_openai_body_emits_per_turn_reasoning_effort_for_non_deepseek() {
        // Non-DeepSeek (here gpt-4o → compat = OPENAI): reasoning_effort still
        // propagates (OpenAI o-series accepts it), but `thinking` does not —
        // OpenAI Chat Completions has no such field, sending it can 400.
        let mut req = CreateMessageRequest::simple("gpt-4o", "ping");
        req.temperature = Some(0.7);
        req.reasoning_effort = Some(crate::request::ReasoningEffort::XHigh);
        req.thinking = Some(crate::request::ThinkingConfig::Enabled {
            budget_tokens: 8192,
        });

        let body = build_openai_request_body(&req, &OpenAiCompatibleClientConfig::default());

        assert_eq!(body["reasoning_effort"], "xhigh");
        assert!(body.get("thinking").is_none());
        assert_eq!(body["temperature"], serde_json::json!(0.7_f32));
    }

    #[test]
    fn build_openai_body_emits_per_turn_thinking_and_drops_temperature_for_deepseek() {
        // DeepSeek + per-turn xhigh: must emit reasoning_effort, thinking, and
        // strip temperature (DeepSeek's thinking endpoint rejects temperature).
        let mut req = CreateMessageRequest::simple("deepseek-v4-pro", "ping");
        req.temperature = Some(0.7);
        req.reasoning_effort = Some(crate::request::ReasoningEffort::XHigh);
        req.thinking = Some(crate::request::ThinkingConfig::Enabled {
            budget_tokens: 16_000,
        });

        let body = build_openai_request_body(&req, &OpenAiCompatibleClientConfig::default());

        assert_eq!(body["reasoning_effort"], "xhigh");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert!(
            body.get("temperature").is_none(),
            "temperature must be dropped when DeepSeek thinking is enabled per-turn"
        );
    }

    #[test]
    fn build_openai_body_per_turn_effort_overrides_static_config() {
        // Static config pins effort=medium; per-turn request says xhigh.
        // Per-turn must win.
        let mut config = OpenAiCompatibleClientConfig::default();
        config
            .request_options
            .body
            .insert("reasoning_effort".into(), serde_json::json!("medium"));

        let mut req = CreateMessageRequest::simple("deepseek-v4-pro", "ping");
        req.reasoning_effort = Some(crate::request::ReasoningEffort::XHigh);
        req.thinking = Some(crate::request::ThinkingConfig::Enabled {
            budget_tokens: 16_000,
        });

        let body = build_openai_request_body(&req, &config);

        assert_eq!(body["reasoning_effort"], "xhigh");
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn build_openai_body_per_turn_disabled_thinking_emits_disabled_marker_for_deepseek() {
        let mut req = CreateMessageRequest::simple("deepseek-v4-pro", "ping");
        req.thinking = Some(crate::request::ThinkingConfig::Disabled);

        let body = build_openai_request_body(&req, &OpenAiCompatibleClientConfig::default());

        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn build_openai_body_applies_provider_request_options() {
        let mut config = OpenAiCompatibleClientConfig::default();
        config
            .request_options
            .body
            .insert("reasoningEffort".into(), serde_json::json!("xhigh"));
        config
            .request_options
            .extra_body
            .insert("thinking".into(), serde_json::json!({ "type": "enabled" }));
        config.request_options.omit_body_fields = vec!["temperature".into()];

        let body = build_openai_request_body(
            &CreateMessageRequest::simple("deepseek-v4-pro", "ping"),
            &config,
        );

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "xhigh");
        assert!(body.get("temperature").is_none());
    }

    fn config_for(base_url: &str) -> OpenAiCompatibleClientConfig {
        OpenAiCompatibleClientConfig::with_base_url(base_url, "sk")
    }

    fn thinking_request(model: &str) -> CreateMessageRequest {
        let mut req = CreateMessageRequest::simple(model, "ping");
        req.temperature = Some(0.7);
        req.reasoning_effort = Some(crate::request::ReasoningEffort::XHigh);
        req.thinking = Some(crate::request::ThinkingConfig::Enabled {
            budget_tokens: 8192,
        });
        req
    }

    #[test]
    fn vendor_rules_zhipu_narrows_effort_and_spells_thinking_type() {
        // docs.bigmodel.cn: GLM-5.3 accepts only low|high|max.
        let body = build_openai_request_body(
            &thinking_request("glm-5.3"),
            &config_for("https://open.bigmodel.cn/api/paas/v4"),
        );
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "max");
        assert_eq!(body["max_tokens"].as_u64().is_some(), true);
        assert!(body.get("max_completion_tokens").is_none());
        // Zhipu's thinking endpoint takes temperature; only DeepSeek drops it.
        assert_eq!(body["temperature"], serde_json::json!(0.7_f32));
    }

    #[test]
    fn vendor_rules_kimi_k3_uses_effort_and_never_the_thinking_object() {
        let body = build_openai_request_body(
            &thinking_request("kimi-k3"),
            &config_for("https://api.moonshot.cn/v1"),
        );
        assert!(body.get("thinking").is_none(), "K3 rejects `thinking`");
        assert_eq!(body["reasoning_effort"], "max");
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["max_completion_tokens"].as_u64().is_some(), true);

        // K2.x is the other way round.
        let body = build_openai_request_body(
            &thinking_request("kimi-k2.6"),
            &config_for("https://api.moonshot.cn/v1"),
        );
        assert_eq!(body["thinking"]["type"], "enabled");
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn vendor_rules_minimax_strips_unsupported_fields_and_splits_reasoning() {
        let mut req = thinking_request("MiniMax-M3");
        req.stop_sequences = vec!["END".into()];
        req.tool_choice = Some(ToolChoice::Auto);
        let body = build_openai_request_body(&req, &config_for("https://api.minimaxi.com/v1"));
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("stop").is_none());
        assert!(body.get("tool_choice").is_none());
        assert_eq!(body["reasoning_split"], true);
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("max_completion_tokens").is_some());

        let mut off = CreateMessageRequest::simple("MiniMax-M3", "ping");
        off.thinking = Some(crate::request::ThinkingConfig::Disabled);
        let body = build_openai_request_body(&off, &config_for("https://api.minimaxi.com/v1"));
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn vendor_rules_qwen_spells_enable_thinking_and_moves_the_cap() {
        let body = build_openai_request_body(
            &thinking_request("qwen3.7-plus"),
            &config_for("https://dashscope.aliyuncs.com/compatible-mode/v1"),
        );
        assert_eq!(body["enable_thinking"], true);
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("max_completion_tokens").is_some());

        // The static DeepSeek-shaped `thinking` object a config wrote is
        // re-spelled, not forwarded.
        let mut config = config_for("https://dashscope.aliyuncs.com/compatible-mode/v1");
        config
            .request_options
            .extra_body
            .insert("thinking".into(), serde_json::json!({ "type": "disabled" }));
        let body = build_openai_request_body(
            &CreateMessageRequest::simple("qwen3.7-plus", "ping"),
            &config,
        );
        assert_eq!(body["enable_thinking"], false);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn vendor_rules_gemini_folds_effort_into_low_medium_high() {
        let body = build_openai_request_body(
            &thinking_request("gemini-3.7-flash"),
            &config_for("https://generativelanguage.googleapis.com/v1beta/openai"),
        );
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("thinking").is_none());
        assert!(body.get("enable_thinking").is_none());
    }

    #[test]
    fn vendor_rules_ollama_drops_tool_choice() {
        let mut req = CreateMessageRequest::simple("qwen3:8b", "ping");
        req.tool_choice = Some(ToolChoice::Auto);
        req.reasoning_effort = Some(crate::request::ReasoningEffort::High);
        let body = build_openai_request_body(&req, &config_for("http://localhost:11434/v1"));
        assert!(body.get("tool_choice").is_none());
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn vendor_rules_openai_leaves_a_static_thinking_object_out() {
        // A `thinking` object copied from a DeepSeek config onto an OpenAI
        // entry would 400 with "unknown field".
        let mut config = config_for("https://api.openai.com/v1");
        config
            .request_options
            .extra_body
            .insert("thinking".into(), serde_json::json!({ "type": "enabled" }));
        let body = build_openai_request_body(
            &CreateMessageRequest::simple("gpt-5.6-sol", "ping"),
            &config,
        );
        assert!(body.get("thinking").is_none());
        assert!(body.get("enable_thinking").is_none());
    }

    #[test]
    fn vendor_rules_unknown_relay_forwards_everything_verbatim() {
        let mut config = config_for("https://relay.example.com/v1");
        config
            .request_options
            .extra_body
            .insert("thinking".into(), serde_json::json!({ "type": "enabled" }));
        let mut req = CreateMessageRequest::simple("some-model", "ping");
        req.reasoning_effort = Some(crate::request::ReasoningEffort::XHigh);
        req.stop_sequences = vec!["END".into()];
        let body = build_openai_request_body(&req, &config);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "xhigh");
        assert_eq!(body["stop"][0], "END");
        assert!(body.get("max_tokens").is_some());
    }

    #[test]
    fn vendor_pin_beats_the_host() {
        let mut config = config_for("https://relay.example.com/v1");
        config.vendor = ProviderVendor::Qwen;
        let body = build_openai_request_body(&thinking_request("qwen3.7-plus"), &config);
        assert_eq!(body["enable_thinking"], true);
        assert!(body.get("max_completion_tokens").is_some());
        assert_eq!(
            effective_chat_completions_compat(&config, "qwen3.7-plus"),
            ChatCompletionsCompat::for_vendor(ProviderVendor::Qwen, "qwen3.7-plus")
        );
    }

    #[test]
    fn volcengine_replays_reasoning_with_its_encrypted_copy() {
        let config = config_for("https://ark.cn-beijing.volces.com/api/v3");
        let compat = effective_chat_completions_compat(&config, "doubao-seed-evolving");
        assert!(compat.replay_encrypted_reasoning);
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking(crate::types::ThinkingBlock {
                    thinking: "plan".into(),
                    signature: Some("enc-blob".into()),
                    data: None,
                }),
                ContentBlock::ToolUse(ToolUseBlock {
                    id: "call_1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "x"}),
                }),
            ],
        };
        let wire = convert_assistant_message(&msg, compat);
        assert_eq!(wire["reasoning_content"], "plan");
        assert_eq!(wire["encrypted_content"], "enc-blob");
        // Nobody else sends the copy, even with a signature present.
        let deepseek = convert_assistant_message(&msg, ChatCompletionsCompat::DEEPSEEK_THINKING);
        assert!(deepseek.get("encrypted_content").is_none());
    }

    #[test]
    fn translator_reads_ollama_reasoning_and_ark_encrypted_content() {
        let mut t = OpenAiTranslator::default();
        t.push_chunk(
            r#"{"id":"c","model":"qwen3","choices":[{"delta":{"reasoning":"hmm"},"index":0,"finish_reason":null}]}"#,
        )
        .unwrap();
        t.push_chunk(
            r#"{"id":"c","model":"qwen3","choices":[{"delta":{"encrypted_content":"blob"},"index":0,"finish_reason":null}]}"#,
        )
        .unwrap();
        t.push_chunk(
            r#"{"id":"c","model":"qwen3","choices":[{"delta":{"content":"ok"},"index":0,"finish_reason":"stop"}]}"#,
        )
        .unwrap();
        let events = drain(&mut t);
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::ContentBlockDelta { delta: ContentBlockDelta::ThinkingDelta { thinking }, .. }
                if thinking == "hmm"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::ContentBlockDelta { delta: ContentBlockDelta::SignatureDelta { signature }, .. }
                if signature == "blob"
        )));
    }

    #[test]
    fn prefix_cache_capability_follows_the_vendor() {
        let deepseek = OpenAiCompatibleProvider::new(config_for("https://api.deepseek.com"));
        assert!(deepseek.prefix_cache_is_byte_exact());
        let kimi = OpenAiCompatibleProvider::new(config_for("https://api.moonshot.cn/v1"));
        assert!(kimi.prefix_cache_is_byte_exact());
        let relay = OpenAiCompatibleProvider::new(config_for("https://relay.example.com/v1"));
        assert!(!relay.prefix_cache_is_byte_exact());
        let ollama = OpenAiCompatibleProvider::new(config_for("http://localhost:11434/v1"));
        assert!(!ollama.prefix_cache_is_byte_exact());
    }

    #[test]
    fn build_openai_body_runtime_fast_tier_overrides_body_options() {
        let handle = ServiceTierHandle::new(false);
        let mut config = OpenAiCompatibleClientConfig::default();
        config.service_tier = Some(handle.clone());
        config
            .request_options
            .extra_body
            .insert("serviceTier".into(), serde_json::json!("fast"));

        let req = CreateMessageRequest::simple("gpt-5.6-sol", "ping");
        let body = build_openai_request_body(&req, &config);
        assert!(body.get("service_tier").is_none());

        handle.set_fast(true);
        let body = build_openai_request_body(&req, &config);
        assert_eq!(body["service_tier"], "priority");

        handle.set_fast(false);
        let body = build_openai_request_body(&req, &config);
        assert!(body.get("service_tier").is_none());
    }

    /// Fast mode is a per-model capability. The endpoint that serves
    /// `gpt-5.6-sol` also serves `gpt-5.5-pro`, which does not take the
    /// field — pro models run their own long-horizon tier — and rebon used
    /// to send it to both.
    #[test]
    fn build_openai_body_omits_the_fast_tier_for_a_model_that_rejects_it() {
        let handle = ServiceTierHandle::new(true);
        let mut config = OpenAiCompatibleClientConfig::default();
        config.service_tier = Some(handle.clone());

        let unsupported = CreateMessageRequest::simple("gpt-5.5-pro", "ping");
        let body = build_openai_request_body(&unsupported, &config);
        assert!(body.get("service_tier").is_none());

        // A model no catalogue lists keeps whatever fast mode was doing:
        // a gap in the table must not take the knob away.
        let unlisted = CreateMessageRequest::simple("some-self-hosted-thing", "ping");
        let body = build_openai_request_body(&unlisted, &config);
        assert_eq!(body["service_tier"], "priority");
    }

    #[test]
    fn build_openai_body_model_options_override_provider_options() {
        let mut config = OpenAiCompatibleClientConfig::default();
        config
            .request_options
            .body
            .insert("reasoningEffort".into(), serde_json::json!("high"));
        config
            .request_options
            .extra_body
            .insert("thinking".into(), serde_json::json!({ "type": "disabled" }));
        let mut model_options = OpenAiRequestOptions::default();
        model_options
            .body
            .insert("reasoningEffort".into(), serde_json::json!("xhigh"));
        model_options
            .extra_body
            .insert("thinking".into(), serde_json::json!({ "type": "enabled" }));
        model_options.omit_body_fields = vec!["temperature".into()];
        config
            .model_request_options
            .insert("deepseek-v4-pro".into(), model_options);

        let mut req = CreateMessageRequest::simple("deepseek-v4-pro", "ping");
        req.temperature = Some(0.5);
        let body = build_openai_request_body(&req, &config);

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "xhigh");
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn build_openai_body_does_not_allow_options_to_override_core_fields() {
        let mut config = OpenAiCompatibleClientConfig::default();
        config
            .request_options
            .extra_body
            .insert("model".into(), serde_json::json!("wrong"));
        config
            .request_options
            .extra_body
            .insert("messages".into(), serde_json::json!([]));
        config
            .request_options
            .omit_body_fields
            .extend(["model".to_string(), "stream".to_string()]);

        let body =
            build_openai_request_body(&CreateMessageRequest::simple("gpt-4o", "ping"), &config);

        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["messages"][0]["content"], "ping");
        assert_eq!(body["stream"], true);
    }

    fn tool_roundtrip_assistant_msg() -> Message {
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking(crate::types::ThinkingBlock {
                    thinking: "need a tool".into(),
                    signature: None,
                    data: None,
                }),
                ContentBlock::ToolUse(ToolUseBlock {
                    id: "call_1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"path":"a.rs"}),
                }),
            ],
        }
    }

    fn no_tool_assistant_msg_with_thinking() -> Message {
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking(crate::types::ThinkingBlock {
                    thinking: "internal scratch".into(),
                    signature: None,
                    data: None,
                }),
                ContentBlock::Text(TextBlock {
                    text: "Here is the answer.".into(),
                }),
            ],
        }
    }

    #[test]
    fn openai_compat_strips_reasoning_and_omits_content_for_tool_only_turn() {
        // OpenAI Chat Completions: reasoning_content is not an input field;
        // tool-only assistant turns omit `content` (schema-canonical).
        let wire = convert_assistant_message(
            &tool_roundtrip_assistant_msg(),
            ChatCompletionsCompat::OPENAI,
        );
        assert!(
            wire.get("reasoning_content").is_none(),
            "OpenAI must not echo reasoning_content"
        );
        assert!(
            wire.get("content").is_none(),
            "OpenAI omits content on tool-only assistant turns"
        );
        assert_eq!(wire["tool_calls"][0]["id"], "call_1");
    }

    #[test]
    fn deepseek_thinking_compat_preserves_reasoning_and_keeps_content_empty_string_on_tool_call() {
        // DeepSeek thinking + tool_calls: reasoning_content MUST be replayed,
        // and `content` must be non-null (empty string is what the provider
        // streams back when there is no assistant text alongside tool_calls).
        let wire = convert_assistant_message(
            &tool_roundtrip_assistant_msg(),
            ChatCompletionsCompat::DEEPSEEK_THINKING,
        );
        assert_eq!(
            wire["reasoning_content"], "need a tool",
            "DeepSeek thinking tool-call turns must replay reasoning_content"
        );
        assert_eq!(
            wire["content"],
            Value::String(String::new()),
            "DeepSeek thinking tool-call turns require non-null content"
        );
        assert_eq!(wire["tool_calls"][0]["id"], "call_1");
    }

    #[test]
    fn deepseek_thinking_compat_replays_reasoning_on_text_only_assistant_turn() {
        // DeepSeek thinking returns 400 invalid_request_error
        // ("The `reasoning_content` in the thinking mode must be passed back
        // to the API.") when an assistant turn that originally carried
        // reasoning_content is replayed without it — even on text-only
        // turns. So `Always` replay is required, not OnlyOnToolCallTurns.
        let wire = convert_assistant_message(
            &no_tool_assistant_msg_with_thinking(),
            ChatCompletionsCompat::DEEPSEEK_THINKING,
        );
        assert_eq!(
            wire["reasoning_content"], "internal scratch",
            "DeepSeek requires reasoning_content replay on every assistant turn that had it"
        );
        assert_eq!(wire["content"], "Here is the answer.");
        assert!(wire.get("tool_calls").is_none());
    }

    #[test]
    fn effective_compat_escalates_deepseek_model_on_default_config() {
        // Relay-routed DeepSeek (axonhub etc.): base_url doesn't advertise
        // deepseek.com, so wiring leaves config.compat at the OPENAI default.
        // The model-name fallback must escalate to DEEPSEEK_THINKING.
        let config = OpenAiCompatibleClientConfig::default();
        assert_eq!(config.compat, ChatCompletionsCompat::OPENAI);
        let resolved = effective_chat_completions_compat(&config, "deepseek-v4-pro");
        assert_eq!(resolved, ChatCompletionsCompat::DEEPSEEK_THINKING);
    }

    #[test]
    fn effective_compat_leaves_non_deepseek_model_on_default_config() {
        let config = OpenAiCompatibleClientConfig::default();
        let resolved = effective_chat_completions_compat(&config, "gpt-4o");
        assert_eq!(resolved, ChatCompletionsCompat::OPENAI);
    }

    #[test]
    fn effective_compat_honors_explicit_deepseek_config_regardless_of_model() {
        // Explicit DEEPSEEK_THINKING set by wiring (e.g., env or direct
        // deepseek.com base_url) must not be downgraded by the fallback.
        let mut config = OpenAiCompatibleClientConfig::default();
        config.compat = ChatCompletionsCompat::DEEPSEEK_THINKING;
        let resolved = effective_chat_completions_compat(&config, "some-custom-model");
        assert_eq!(resolved, ChatCompletionsCompat::DEEPSEEK_THINKING);
    }

    #[test]
    fn effective_compat_is_case_insensitive_for_deepseek_prefix() {
        // Relay aliases may upper-case or mixed-case the model id.
        let config = OpenAiCompatibleClientConfig::default();
        assert_eq!(
            effective_chat_completions_compat(&config, "DeepSeek-V4-Pro"),
            ChatCompletionsCompat::DEEPSEEK_THINKING,
        );
        assert_eq!(
            effective_chat_completions_compat(&config, "  deepseek-reasoner  "),
            ChatCompletionsCompat::DEEPSEEK_THINKING,
        );
    }

    #[test]
    fn convert_user_message_emits_tool_result_image_as_user_image_part() {
        let msg = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult(crate::types::ToolResultBlock {
                tool_use_id: "call_img".into(),
                content: ToolResultContent::blocks(vec![
                    ToolResultContentBlock::Text(TextBlock {
                        text: "Image file: image.jpg".into(),
                    }),
                    ToolResultContentBlock::Image(crate::types::ImageBlock::base64(
                        "image/jpeg",
                        "AAAA",
                    )),
                ]),
                is_error: false,
            })],
        };

        let wire = convert_user_message(&msg);
        let messages = wire.as_array().expect("tool result yields message array");

        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[0]["content"], "Image file: image.jpg");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"][0]["type"], "image_url");
        assert_eq!(
            messages[1]["content"][0]["image_url"]["url"],
            "data:image/jpeg;base64,AAAA"
        );
    }

    #[test]
    fn convert_user_message_emits_tool_result_document_as_user_file_part() {
        let msg = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult(crate::types::ToolResultBlock {
                tool_use_id: "call_pdf".into(),
                content: ToolResultContent::blocks(vec![
                    ToolResultContentBlock::Text(TextBlock {
                        text: "PDF file read: doc.pdf".into(),
                    }),
                    ToolResultContentBlock::Document(crate::types::DocumentBlock::base64(
                        "application/pdf",
                        "JVBERi0=",
                    )),
                ]),
                is_error: false,
            })],
        };

        let wire = convert_user_message(&msg);
        let messages = wire.as_array().expect("tool result yields message array");

        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[0]["content"], "PDF file read: doc.pdf");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"][0]["type"], "file");
        assert_eq!(
            messages[1]["content"][0]["file"]["filename"],
            "document.pdf"
        );
        assert_eq!(
            messages[1]["content"][0]["file"]["file_data"],
            "data:application/pdf;base64,JVBERi0="
        );
    }

    #[test]
    fn build_openai_request_body_flattens_tool_result_message_arrays() {
        let request = CreateMessageRequest {
            model: "gpt-4o".into(),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult(crate::types::ToolResultBlock {
                    tool_use_id: "call_1".into(),
                    content: ToolResultContent::text("ok"),
                    is_error: false,
                })],
            }],
            system: None,
            transient_context: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: true,
            metadata: None,
            thinking: None,
            reasoning_effort: None,
            reasoning_mode: None,
            reasoning_summary: None,
            web_search: None,
            context_management: None,
            cache_trace_context: None,
            compaction_trigger: false,
        };

        let body =
            build_openai_request_body(&request, &OpenAiCompatibleClientConfig::with_api_key("sk"));
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "tool");
        assert!(!messages[0].is_array());
    }

    #[test]
    fn openai_request_body_ignores_role_system_messages() {
        let request = CreateMessageRequest {
            model: "gpt-4o".into(),
            messages: vec![Message {
                role: Role::System,
                content: vec![ContentBlock::Text(TextBlock {
                    text: "hidden".into(),
                })],
            }],
            system: Some("top-level".into()),
            transient_context: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: true,
            metadata: None,
            thinking: None,
            reasoning_effort: None,
            reasoning_mode: None,
            reasoning_summary: None,
            web_search: None,
            context_management: None,
            cache_trace_context: None,
            compaction_trigger: false,
        };

        let body =
            build_openai_request_body(&request, &OpenAiCompatibleClientConfig::with_api_key("sk"));
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "top-level");
    }

    #[test]
    fn openai_endpoint_does_not_duplicate_version_segment() {
        let config =
            OpenAiCompatibleClientConfig::with_base_url("https://api.deepseek.com/v1", "sk");
        let provider = OpenAiCompatibleProvider::new(config);
        assert_eq!(
            provider.endpoint(),
            "https://api.deepseek.com/v1/chat/completions"
        );

        let config = OpenAiCompatibleClientConfig::with_base_url(
            "https://open.bigmodel.cn/api/paas/v4",
            "sk",
        );
        let provider = OpenAiCompatibleProvider::new(config);
        assert_eq!(
            provider.endpoint(),
            "https://open.bigmodel.cn/api/paas/v4/chat/completions"
        );
    }

    /// Copilot's chat API has no version segment, so a bare host must not
    /// grow a `/v1` — while every other bare host still does.
    #[test]
    fn copilot_endpoint_hangs_off_the_host_without_a_version() {
        for base in [
            "https://api.githubcopilot.com",
            "https://api.githubcopilot.com/",
            "https://api.individual.githubcopilot.com",
        ] {
            let provider = OpenAiCompatibleProvider::new(
                OpenAiCompatibleClientConfig::with_base_url(base, "t"),
            );
            assert_eq!(
                provider.endpoint(),
                format!("{}/chat/completions", base.trim_end_matches('/'))
            );
        }
        // A pin wins over the host, in both directions.
        let mut pinned =
            OpenAiCompatibleClientConfig::with_base_url("https://relay.example.com", "t");
        pinned.vendor = ProviderVendor::GithubCopilot;
        assert_eq!(
            OpenAiCompatibleProvider::new(pinned).endpoint(),
            "https://relay.example.com/chat/completions"
        );
        let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleClientConfig::with_base_url(
            "https://api.x.ai",
            "t",
        ));
        assert_eq!(provider.endpoint(), "https://api.x.ai/v1/chat/completions");
    }

    #[test]
    fn convert_assistant_message_emits_tool_calls_array() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text(TextBlock {
                    text: "Reading file".into(),
                }),
                ContentBlock::ToolUse(ToolUseBlock {
                    id: "toolu_1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"path":"a.rs"}),
                }),
            ],
        };
        let wire = convert_assistant_message(&msg, ChatCompletionsCompat::OPENAI);
        assert_eq!(wire["role"], "assistant");
        assert_eq!(wire["content"], "Reading file");
        assert_eq!(wire["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(wire["tool_calls"][0]["function"]["name"], "Read");
        assert_eq!(
            wire["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a.rs\"}"
        );
    }

    #[test]
    fn finish_reason_mapping() {
        assert_eq!(finish_reason_to_stop("stop"), StopReason::EndTurn);
        assert_eq!(finish_reason_to_stop("length"), StopReason::MaxTokens);
        assert_eq!(finish_reason_to_stop("tool_calls"), StopReason::ToolUse);
        assert_eq!(finish_reason_to_stop("content_filter"), StopReason::Refusal);
        assert_eq!(
            finish_reason_to_stop("unknown_reason"),
            StopReason::Other("unknown_reason".into())
        );
    }

    #[test]
    fn convenience_constructor_builds_universal_client() {
        let client = openai_compatible_client(OpenAiCompatibleClientConfig::with_api_key("sk"));
        assert_eq!(
            <crate::provider::UniversalModelClient as ModelClient>::provider_name(&client),
            "openai-compatible"
        );
    }

    async fn start_delayed_sse_http_server(
        body: &'static str,
        delay: Duration,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 4096];
            let mut used = 0usize;
            loop {
                let read = tokio::io::AsyncReadExt::read(&mut stream, &mut buffer[used..])
                    .await
                    .unwrap();
                if read == 0 {
                    break;
                }
                used += read;
                if used >= 4 && buffer[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if used == buffer.len() {
                    buffer.resize(buffer.len() * 2, 0);
                }
            }

            tokio::time::sleep(delay).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
                .await
                .unwrap();
        });
        (format!("http://{addr}"), task)
    }

    const DONE_SSE: &str = concat!(
        "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );

    /// Serves one scripted `(status, body)` per connection, in order, and
    /// records each request's `Authorization` header.
    async fn start_scripted_server(
        replies: Vec<(u16, &'static str)>,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        tokio::spawn(async move {
            for (status, body) in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let read = tokio::io::AsyncReadExt::read(&mut stream, &mut chunk)
                        .await
                        .unwrap();
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&buffer);
                    if let Some(head_end) = text.find("\r\n\r\n") {
                        let length = text[..head_end]
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if buffer.len() >= head_end + 4 + length {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&buffer).to_string();
                let auth = text
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("authorization")
                            .then(|| value.trim().to_string())
                    })
                    .unwrap_or_default();
                recorded.lock().unwrap().push(auth);
                let reason = if status == 200 { "OK" } else { "Error" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
                    .await
                    .unwrap();
            }
        });
        (format!("http://{addr}/v1"), seen)
    }

    #[derive(Debug)]
    struct ScriptedRefresher {
        calls: std::sync::atomic::AtomicUsize,
        answer: Result<&'static str, &'static str>,
    }

    #[async_trait]
    impl TokenRefresher for ScriptedRefresher {
        async fn refresh(&self) -> Result<String, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.answer.map(str::to_string).map_err(str::to_string)
        }
    }

    fn refreshing_client(
        base_url: String,
        answer: Result<&'static str, &'static str>,
    ) -> (UniversalModelClient, Arc<ScriptedRefresher>) {
        let refresher = Arc::new(ScriptedRefresher {
            calls: Default::default(),
            answer,
        });
        let mut config = OpenAiCompatibleClientConfig::with_base_url(base_url, "stale");
        config.refresher = Some(refresher.clone());
        (openai_compatible_client(config), refresher)
    }

    /// The expired-session path: one 401, one refresh, the same request
    /// again with the new bearer, and the fresh bearer kept for next time.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_401_refreshes_the_bearer_and_retries_once() {
        let (base, seen) =
            start_scripted_server(vec![(401, "expired"), (200, DONE_SSE), (200, DONE_SSE)]).await;
        let (client, refresher) = refreshing_client(base, Ok("fresh"));

        client
            .create_message(CreateMessageRequest::simple("m", "ping"))
            .await
            .expect("the retry succeeds");
        client
            .create_message(CreateMessageRequest::simple("m", "again"))
            .await
            .expect("the next turn reuses the fresh bearer");

        assert_eq!(
            *seen.lock().unwrap(),
            vec!["Bearer stale", "Bearer fresh", "Bearer fresh"]
        );
        assert_eq!(refresher.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A second 401 is the answer, not a reason to loop.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_401_ends_the_call_without_another_refresh() {
        let (base, seen) = start_scripted_server(vec![(401, "no"), (401, "still no")]).await;
        let (client, refresher) = refreshing_client(base, Ok("fresh"));

        let err = client
            .create_message(CreateMessageRequest::simple("m", "ping"))
            .await
            .unwrap_err();

        assert!(matches!(err, ModelError::Unauthorized(_)), "{err:?}");
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert_eq!(refresher.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_refresh_is_reported_as_unauthorized() {
        let (base, seen) = start_scripted_server(vec![(401, "no")]).await;
        let (client, _refresher) = refreshing_client(base, Err("login required"));

        let err = client
            .create_message(CreateMessageRequest::simple("m", "ping"))
            .await
            .unwrap_err();

        match err {
            ModelError::Unauthorized(message) => assert!(message.contains("login required")),
            other => panic!("expected Unauthorized, got {other:?}"),
        }
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    /// 403 means "valid but not allowed" (a model the plan does not
    /// include); a new token would not change the answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_403_does_not_refresh() {
        let (base, seen) = start_scripted_server(vec![(403, "forbidden")]).await;
        let (client, refresher) = refreshing_client(base, Ok("fresh"));

        let err = client
            .create_message(CreateMessageRequest::simple("m", "ping"))
            .await
            .unwrap_err();

        assert!(matches!(err, ModelError::Unauthorized(_)));
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(refresher.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// Without a refresher a 401 is returned as before — no retry.
    #[tokio::test(flavor = "multi_thread")]
    async fn without_a_refresher_a_401_is_returned_unchanged() {
        let (base, seen) = start_scripted_server(vec![(401, "no")]).await;
        let client =
            openai_compatible_client(OpenAiCompatibleClientConfig::with_base_url(base, "k"));

        let err = client
            .create_message(CreateMessageRequest::simple("m", "ping"))
            .await
            .unwrap_err();

        assert!(matches!(err, ModelError::Unauthorized(_)));
        assert_eq!(*seen.lock().unwrap(), vec!["Bearer k"]);
    }

    #[tokio::test]
    async fn convenience_constructor_does_not_cut_off_slow_sse_stream() {
        let body = concat!(
            "data: {\"id\":\"chatcmpl_1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl_1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        let (base_url, server) =
            start_delayed_sse_http_server(body, Duration::from_millis(100)).await;
        let mut config = OpenAiCompatibleClientConfig::with_base_url(base_url, "sk");
        config.request_timeout = Some(Duration::from_millis(10));
        let client = openai_compatible_client(config);

        let msg = client
            .create_message(CreateMessageRequest::simple("gpt-4o", "ping"))
            .await
            .unwrap();

        server.await.unwrap();
        assert_eq!(msg.text(), "ok");
    }
}
