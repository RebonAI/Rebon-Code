//! `AnthropicProvider` — [`crate::ChatProvider`] implementation
//! against the Anthropic Messages API.
//!
//! Rather than going through an SDK package, we call the REST
//! endpoint directly via `reqwest`. Retry / rate-limit /
//! prompt-cache-break detection live in `crate::middleware`, not
//! here.
//!
//! ## Request
//!
//! `POST {base_url}/v1/messages` with headers:
//! - `x-api-key: {api_key}`
//! - `anthropic-version: 2023-06-01`
//! - `Content-Type: application/json`
//! - `Accept: text/event-stream`
//!
//! Body is a JSON object carrying the rewrite of
//! [`CreateMessageRequest`] produced by [`build_request_body`].
//!
//! ## Response
//!
//! SSE stream. Each `data:` line is a JSON object with a top-level
//! `type` discriminant. We decode into the provider-agnostic
//! [`StreamEvent`] via [`parse_anthropic_event`], which is exposed
//! publicly so alternate transports (e.g. a Bedrock proxy that
//! speaks Anthropic wire) can reuse it.

#[cfg(test)]
use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use serde::Serialize;
use serde_json::Value;

use crate::client::ModelCapabilities;
use crate::error::{ModelError, ModelResult};
use crate::events::{
    ContentBlockDelta, ContentBlockStart, MessageDeltaFields, StreamEvent, StreamEventStream,
};
use crate::provider::{classify_http_error, ChatProvider, UniversalModelClient};
use crate::request::{ContextEditStrategy, CreateMessageRequest, ThinkingConfig};
use crate::sse::{decode_sse_stream, SseDecoder, SseFrame};
use crate::types::{Message, Role, StopReason, Tool, ToolChoice, Usage};

#[cfg(test)]
thread_local! {
    static PROMPT_CACHE_ENV_READ_ENABLED: Cell<bool> = const { Cell::new(false) };
}

/// Configuration for an [`AnthropicProvider`].
#[derive(Debug, Clone)]
pub struct AnthropicClientConfig {
    /// Base URL including scheme. The endpoint path (`/v1/messages`)
    /// is appended internally. Defaults to
    /// `https://api.anthropic.com`.
    pub base_url: String,
    /// API key. Sent unchanged as `x-api-key`.
    pub api_key: String,
    /// `anthropic-version` header value. Defaults to `"2023-06-01"`.
    pub anthropic_version: String,
    /// Optional beta headers (`anthropic-beta`).
    pub betas: Vec<String>,
    /// HTTP client connect timeout. Only honoured when the provider
    /// constructs its own [`UniversalModelClient`] via the
    /// convenience constructor — when the caller injects an
    /// existing `reqwest::Client`, this field is ignored.
    pub request_timeout: Option<Duration>,
    /// Whether volatile runtime context may stay request-scoped instead of
    /// being materialized into durable history.
    pub request_scoped_transient_context: bool,
}

impl Default for AnthropicClientConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.anthropic.com".to_string(),
            api_key: String::new(),
            anthropic_version: "2023-06-01".to_string(),
            betas: Vec::new(),
            request_timeout: Some(Duration::from_secs(60)),
            request_scoped_transient_context: true,
        }
    }
}

impl AnthropicClientConfig {
    /// Convenience constructor that sets the API key and leaves the
    /// rest at the default.
    pub fn with_api_key(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            ..Self::default()
        }
    }
}

/// [`ChatProvider`] implementation against the Anthropic Messages
/// API.
///
/// Callers normally wrap this in a
/// [`UniversalModelClient`](crate::UniversalModelClient) via
/// [`anthropic_client`] and then treat the result as an
/// [`crate::ModelClient`] dyn trait object.
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    config: Arc<AnthropicClientConfig>,
}

impl AnthropicProvider {
    /// Construct from a config.
    pub fn new(config: AnthropicClientConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    /// Clone of the config — useful for diagnostics.
    pub fn config(&self) -> Arc<AnthropicClientConfig> {
        self.config.clone()
    }

    fn endpoint(&self) -> String {
        messages_endpoint_for_base(&self.config.base_url)
    }
}

fn messages_endpoint_for_base(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1/messages") {
        return base.to_string();
    }
    if base.ends_with("/v1") {
        return format!("{base}/messages");
    }
    format!("{base}/v1/messages")
}

/// Whether a Messages-protocol endpoint that is not Anthropic's own
/// should also be handed the key as a bearer token.
///
/// Anthropic reads `x-api-key`. Several gateways that speak the same
/// wire read `Authorization: Bearer` instead — Zhipu's GLM Coding Plan
/// endpoint (`open.bigmodel.cn/api/anthropic`) is the one that surfaced
/// this: its own instructions say to set `ANTHROPIC_AUTH_TOKEN`, never
/// `ANTHROPIC_API_KEY`. Sending only `x-api-key` there means sending no
/// credential the server looks at.
///
/// Both headers go out rather than one or the other. They carry the same
/// key to the same host, so there is nothing extra disclosed, and it
/// removes a configuration question nobody can answer from the outside:
/// which header this particular gateway happens to read. Anthropic's own
/// endpoint is excluded so the official request stays byte-identical to
/// what it has always been.
pub fn anthropic_compatible_gateway_needs_bearer(base_url: &str) -> bool {
    !is_official_anthropic_base_url(base_url) && !is_vertex_base_url(base_url)
}

pub fn is_official_anthropic_base_url(base_url: &str) -> bool {
    matches!(
        base_url.trim_end_matches('/').to_ascii_lowercase().as_str(),
        "https://api.anthropic.com"
            | "https://api.anthropic.com/v1"
            | "https://api.anthropic.com/v1/messages"
    )
}

/// Whether `base_url` is Claude on Vertex AI — the one Claude deployment
/// that does not speak the plain Messages wire.
///
/// platform.claude.com/docs/en/api/claude-on-vertex-ai: the model goes in
/// the URL (`…/publishers/anthropic/models/{model}:streamRawPredict`), auth
/// is a Google OAuth bearer token, and the body carries
/// `anthropic_version: "vertex-2023-10-16"` instead of a `model`. Bedrock
/// (`bedrock-mantle.*.api.aws/anthropic`) and Foundry
/// (`*.services.ai.azure.com/anthropic`) take the standard wire with the
/// key in `x-api-key`, so they need nothing here.
pub fn is_vertex_base_url(base_url: &str) -> bool {
    crate::vendor::host_of(base_url).is_some_and(|host| {
        host.ends_with("aiplatform.googleapis.com") || host.ends_with(".rep.googleapis.com")
    })
}

/// The Vertex AI endpoint for `model` under a base of the form
/// `https://{host}/v1/projects/{project}/locations/{location}`.
///
/// A base that already ends in `/publishers/anthropic/models` (or names
/// a model) is not doubled up; a base ending in `/v1/messages` — someone
/// pasting the standard shape — has that stripped.
pub fn vertex_endpoint_for_base(base_url: &str, model: &str) -> String {
    let mut base = base_url.trim().trim_end_matches('/').to_string();
    for suffix in ["/v1/messages", "/messages"] {
        if let Some(stripped) = base.strip_suffix(suffix) {
            base = stripped.to_string();
            break;
        }
    }
    let model = model.trim();
    if base.contains("/publishers/anthropic/models/") {
        // The base names a model already; trust it.
        return match base.rsplit_once(':') {
            Some((without_verb, verb)) if !verb.contains('/') => {
                format!("{without_verb}:streamRawPredict")
            }
            _ => format!("{base}:streamRawPredict"),
        };
    }
    if let Some(root) = base.strip_suffix("/publishers/anthropic/models") {
        return format!("{root}/publishers/anthropic/models/{model}:streamRawPredict");
    }
    format!("{base}/publishers/anthropic/models/{model}:streamRawPredict")
}

/// Vertex's body: no `model` (it is in the URL), plus the fixed
/// `anthropic_version` the platform requires.
pub fn vertex_body(mut body: Value) -> Value {
    if let Some(obj) = body.as_object_mut() {
        obj.remove("model");
        obj.insert(
            "anthropic_version".to_string(),
            Value::String("vertex-2023-10-16".to_string()),
        );
    }
    body
}

#[async_trait]
impl ChatProvider for AnthropicProvider {
    fn provider_name(&self) -> &'static str {
        "anthropic"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            requires_inline_transient_context: !self.config.request_scoped_transient_context,
            prefix_cache_is_byte_exact: true,
            ..ModelCapabilities::default()
        }
    }

    async fn send_message_stream(
        &self,
        http: &reqwest::Client,
        mut request: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream> {
        request.stream = true;

        let vertex = is_vertex_base_url(&self.config.base_url);
        let body = if vertex {
            vertex_body(build_request_body(&request))
        } else {
            build_request_body(&request)
        };
        let url = if vertex {
            vertex_endpoint_for_base(&self.config.base_url, &request.model)
        } else {
            self.endpoint()
        };
        let mut http_req = http
            .post(&url)
            .header("Accept", "text/event-stream")
            .header("Content-Type", "application/json")
            .json(&body);
        if vertex {
            // Google OAuth: the "API key" is an access token
            // (`gcloud auth print-access-token`), sent as a bearer.
            http_req = http_req.header("Authorization", format!("Bearer {}", self.config.api_key));
        } else {
            http_req = http_req
                .header("x-api-key", &self.config.api_key)
                .header("anthropic-version", &self.config.anthropic_version);
            if anthropic_compatible_gateway_needs_bearer(&self.config.base_url) {
                http_req =
                    http_req.header("Authorization", format!("Bearer {}", self.config.api_key));
            }
        }

        // Collect beta headers: start from config, then add
        // interleaved-thinking when thinking is enabled, and
        // web-search when web search is configured.
        let mut betas = self.config.betas.clone();
        if matches!(request.thinking, Some(ThinkingConfig::Enabled { .. })) {
            add_beta(&mut betas, "interleaved-thinking-2025-05-14");
        }
        if request.web_search.is_some() {
            add_beta(&mut betas, "web-search-2025-03-05");
        }
        add_context_management_betas(&mut betas, request.context_management.as_ref());
        if !betas.is_empty() {
            http_req = http_req.header("anthropic-beta", betas.join(","));
        }

        let response = http_req.send().await?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = crate::error::parse_retry_after(response.headers());
            let text = response.text().await.unwrap_or_default();
            return Err(classify_http_error(status.as_u16(), text, retry_after));
        }

        let byte_stream = response
            .bytes_stream()
            .map_err(|e| ModelError::Http(format!("body stream: {e}")));

        Ok(decode_sse_stream(
            Box::pin(byte_stream),
            AnthropicSseDecoder::default(),
        ))
    }

    fn fork_for_sub_agent(&self) -> Option<Arc<dyn ChatProvider>> {
        Some(Arc::new(self.clone()))
    }
}

/// Convenience constructor that wraps an [`AnthropicProvider`] in a
/// [`UniversalModelClient`] using a fresh [`reqwest::Client`].
pub fn anthropic_client(config: AnthropicClientConfig) -> UniversalModelClient {
    let provider = Arc::new(AnthropicProvider::new(config.clone()));
    UniversalModelClient::with_http_client(
        provider,
        crate::provider::build_http_client(config.request_timeout),
    )
}

/// Convenience constructor that wraps an [`AnthropicProvider`] in a
/// [`UniversalModelClient`] reusing an existing [`reqwest::Client`].
pub fn anthropic_client_with_http(
    config: AnthropicClientConfig,
    http: reqwest::Client,
) -> UniversalModelClient {
    UniversalModelClient::with_http_client(Arc::new(AnthropicProvider::new(config)), http)
}

fn add_beta(betas: &mut Vec<String>, beta: &str) {
    if !betas.iter().any(|existing| existing == beta) {
        betas.push(beta.to_string());
    }
}

fn add_context_management_betas(
    betas: &mut Vec<String>,
    config: Option<&crate::request::ContextManagementConfig>,
) {
    let Some(config) = config else {
        return;
    };
    let mut needs_context_editing = false;
    let mut needs_compaction = false;
    for edit in &config.edits {
        match edit {
            ContextEditStrategy::Compact { .. } => needs_compaction = true,
            ContextEditStrategy::ClearToolUses { .. }
            | ContextEditStrategy::ClearThinking { .. } => needs_context_editing = true,
        }
    }
    if needs_context_editing {
        add_beta(betas, "context-management-2025-06-27");
    }
    if needs_compaction {
        add_beta(betas, "compact-2026-01-12");
    }
}

#[derive(Default)]
struct AnthropicSseDecoder {
    pending: Option<StreamEvent>,
    done: bool,
}

impl SseDecoder for AnthropicSseDecoder {
    fn next_event(&mut self) -> Option<StreamEvent> {
        self.pending.take()
    }

    fn push_frame(&mut self, frame: SseFrame, _at_eof: bool) -> ModelResult<()> {
        if !frame.has_data() {
            return Ok(());
        }
        if frame.data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        self.pending = parse_anthropic_event(&frame.data)?;
        Ok(())
    }

    fn is_terminal(&self) -> bool {
        self.done && self.pending.is_none()
    }

    fn finish(&mut self) -> ModelResult<()> {
        // Anthropic's native `message_stop` is already a canonical terminal
        // event. Some compatible gateways close immediately after it without
        // a separate `[DONE]`, which has always been accepted.
        self.done = true;
        Ok(())
    }
}

/// Parse one `data:` payload into a [`StreamEvent`].
///
/// Returns `Ok(None)` for events we silently drop (`ping`, unknown
/// types we don't care about yet). Exposed publicly so alternate
/// transports that speak the Anthropic wire shape (e.g. a caching
/// proxy) can reuse the decoder without reimplementing it.
pub fn parse_anthropic_event(data: &str) -> ModelResult<Option<StreamEvent>> {
    let value: Value = serde_json::from_str(data)?;
    let Some(kind) = value.get("type").and_then(|v| v.as_str()) else {
        return Err(ModelError::Protocol(format!(
            "anthropic sse event missing type: {data}"
        )));
    };
    match kind {
        "ping" => Ok(None),
        "message_start" => {
            let message = value
                .get("message")
                .ok_or_else(|| ModelError::Protocol("message_start missing message".into()))?;
            let message_id = message
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let model = message
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let usage = message
                .get("usage")
                .map(|u| serde_json::from_value::<Usage>(u.clone()).unwrap_or_default())
                .unwrap_or_default();
            Ok(Some(StreamEvent::MessageStart {
                message_id,
                model,
                usage,
            }))
        }
        "content_block_start" => {
            let index =
                value.get("index").and_then(|v| v.as_u64()).ok_or_else(|| {
                    ModelError::Protocol("content_block_start missing index".into())
                })? as usize;
            let content_block = value
                .get("content_block")
                .ok_or_else(|| ModelError::Protocol("content_block_start missing block".into()))?;
            let block_type = content_block
                .get("type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ModelError::Protocol("content_block missing type".into()))?;
            let block = match block_type {
                "text" => ContentBlockStart::Text {
                    text: content_block
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                },
                "tool_use" => ContentBlockStart::ToolUse {
                    id: content_block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    name: content_block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                },
                "thinking" => ContentBlockStart::Thinking {
                    thinking: content_block
                        .get("thinking")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    data: None,
                },
                "redacted_thinking" => ContentBlockStart::Thinking {
                    // A redacted_thinking block carries its (encrypted)
                    // reasoning in `data`, not `thinking`, and has no
                    // signature. Preserve `data` so the block round-trips
                    // as a valid `redacted_thinking` block instead of
                    // collapsing into a signatureless empty thinking
                    // block that Anthropic 400-rejects on the next turn.
                    thinking: String::new(),
                    data: content_block
                        .get("data")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                },
                "server_tool_use" => ContentBlockStart::ServerToolUse {
                    id: content_block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    name: content_block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    input: content_block
                        .get("input")
                        .cloned()
                        .unwrap_or(serde_json::Value::Object(Default::default())),
                },
                "web_search_tool_result" => ContentBlockStart::WebSearchResult {
                    tool_use_id: content_block
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    content: content_block
                        .get("content")
                        .cloned()
                        .unwrap_or(serde_json::Value::Array(Vec::new())),
                },
                "compaction" => ContentBlockStart::Compaction {
                    content: content_block
                        .get("content")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    // Anthropic's compaction summary is plaintext; the
                    // encrypted variant is an OpenAI Responses concept.
                    encrypted_content: None,
                },
                other => {
                    tracing::debug!(block = other, "ignoring unknown content_block start");
                    return Ok(None);
                }
            };
            Ok(Some(StreamEvent::ContentBlockStart {
                index,
                content_block: block,
            }))
        }
        "content_block_delta" => {
            let index =
                value.get("index").and_then(|v| v.as_u64()).ok_or_else(|| {
                    ModelError::Protocol("content_block_delta missing index".into())
                })? as usize;
            let delta_value = value
                .get("delta")
                .ok_or_else(|| ModelError::Protocol("content_block_delta missing delta".into()))?;
            let delta_type = delta_value
                .get("type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ModelError::Protocol("delta missing type".into()))?;
            let delta = match delta_type {
                "text_delta" => ContentBlockDelta::TextDelta {
                    text: delta_value
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                },
                "input_json_delta" => ContentBlockDelta::InputJsonDelta {
                    partial_json: delta_value
                        .get("partial_json")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                },
                "thinking_delta" => ContentBlockDelta::ThinkingDelta {
                    thinking: delta_value
                        .get("thinking")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                },
                "signature_delta" => ContentBlockDelta::SignatureDelta {
                    signature: delta_value
                        .get("signature")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                },
                "compaction_delta" => ContentBlockDelta::CompactionDelta {
                    content: delta_value
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                },
                other => {
                    tracing::debug!(delta = other, "ignoring unknown content_block_delta");
                    return Ok(None);
                }
            };
            Ok(Some(StreamEvent::ContentBlockDelta { index, delta }))
        }
        "content_block_stop" => {
            let index =
                value.get("index").and_then(|v| v.as_u64()).ok_or_else(|| {
                    ModelError::Protocol("content_block_stop missing index".into())
                })? as usize;
            Ok(Some(StreamEvent::ContentBlockStop { index }))
        }
        "message_delta" => {
            let delta_value = value.get("delta").cloned().unwrap_or(Value::Null);
            let stop_reason = delta_value
                .get("stop_reason")
                .and_then(|v| v.as_str())
                .map(|s| serde_json::from_value::<StopReason>(Value::String(s.to_string())))
                .transpose()
                .map_err(|e| ModelError::Protocol(format!("stop_reason parse: {e}")))?;
            let usage = value
                .get("usage")
                .map(|u| serde_json::from_value::<Usage>(u.clone()).unwrap_or_default())
                .unwrap_or_default();
            if let Some(context_management) = value.get("context_management") {
                tracing::debug!(
                    context_management = %context_management,
                    "anthropic context management edits applied"
                );
            }
            Ok(Some(StreamEvent::MessageDelta {
                delta: MessageDeltaFields { stop_reason, usage },
            }))
        }
        "message_stop" => Ok(Some(StreamEvent::MessageStop)),
        "error" => {
            let error_value = value.get("error").cloned().unwrap_or(value);
            let error_type = error_value
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("error")
                .to_string();
            let message = error_value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Ok(Some(StreamEvent::Error {
                error_type,
                message,
            }))
        }
        other => {
            tracing::debug!(event = other, "ignoring unknown anthropic sse event type");
            Ok(None)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnthropicPromptCacheMode {
    Disabled,
    System,
    Tools,
    SystemAndTools,
}

impl AnthropicPromptCacheMode {
    fn from_env() -> Self {
        #[cfg(test)]
        if !PROMPT_CACHE_ENV_READ_ENABLED.with(|enabled| enabled.get()) {
            return Self::Disabled;
        }

        match std::env::var("REBON_ANTHROPIC_PROMPT_CACHE") {
            Ok(value) => Self::parse(&value),
            Err(_) => Self::Disabled,
        }
    }

    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "0" | "false" => Self::Disabled,
            "1" | "true" | "system_and_tools" | "system-tools" => Self::SystemAndTools,
            "system" => Self::System,
            "tools" => Self::Tools,
            _ => Self::Disabled,
        }
    }

    fn caches_system(self) -> bool {
        matches!(self, Self::System | Self::SystemAndTools)
    }

    fn caches_tools(self) -> bool {
        matches!(self, Self::Tools | Self::SystemAndTools)
    }
}

/// Build the JSON body for `POST /v1/messages`.
///
/// Kept public so tests (and alternate transports that speak the
/// Anthropic wire) can snapshot what a given
/// [`CreateMessageRequest`] turns into on the wire.
///
/// When thinking is enabled, the body includes a `thinking`
/// parameter (`{ type: "enabled", budget_tokens }`) and
/// `temperature` is omitted, letting the API apply its default
/// of `1` (the Anthropic requirement). The caller
/// must ensure `max_tokens > budget_tokens`; this function
/// enforces the constraint by clamping `budget_tokens` to
/// `max_tokens - 1`.
pub fn build_request_body(request: &CreateMessageRequest) -> Value {
    #[derive(Serialize)]
    struct Wire<'a> {
        model: &'a str,
        messages: Vec<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        system: Option<Value>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tools: Vec<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_choice: Option<Value>,
        max_tokens: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        temperature: Option<f32>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        stop_sequences: Vec<String>,
        stream: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<&'a Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking: Option<Value>,
        /// Anthropic `context_management` beta. When present, the
        /// server automatically clears old tool results and thinking
        /// blocks instead of the client doing it.
        #[serde(skip_serializing_if = "Option::is_none")]
        context_management: Option<&'a crate::request::ContextManagementConfig>,
    }

    let prompt_cache_mode = AnthropicPromptCacheMode::from_env();
    let messages = request
        .messages_with_transient_context()
        .into_iter()
        .filter(|msg| msg.role != Role::System)
        .map(|message| message_to_wire(&message))
        .collect::<Vec<_>>();
    let mut tools = request
        .tools
        .iter()
        .map(|t: &Tool| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            })
        })
        .collect::<Vec<_>>();
    if prompt_cache_mode.caches_tools() {
        if let Some(last_tool) = tools.last_mut() {
            last_tool["cache_control"] = ephemeral_cache_control();
        }
    }

    // Inject the native web search server tool when configured.
    if let Some(ws) = &request.web_search {
        let mut ws_tool = serde_json::json!({
            "type": "web_search_20250305",
        });
        if let Some(max) = ws.max_uses {
            ws_tool["max_uses"] = serde_json::json!(max);
        }
        if let Some(ref domains) = ws.allowed_domains {
            ws_tool["allowed_domains"] = serde_json::json!(domains);
        }
        if let Some(ref domains) = ws.blocked_domains {
            ws_tool["blocked_domains"] = serde_json::json!(domains);
        }
        tools.push(ws_tool);
    }
    let tool_choice = request.tool_choice.as_ref().map(tool_choice_to_wire);

    // Build thinking parameter and adjust temperature/max_tokens
    // constraints per Anthropic API requirements.
    let (thinking_param, temperature) = match &request.thinking {
        Some(ThinkingConfig::Enabled { budget_tokens }) => {
            // Anthropic requires max_tokens > budget_tokens.
            let clamped = (*budget_tokens).min(request.max_tokens.saturating_sub(1));
            let param = serde_json::json!({
                "type": "enabled",
                "budget_tokens": clamped,
            });
            // Anthropic requires temperature = 1 when thinking is
            // enabled; passing `None` lets the API use its default (1).
            (Some(param), None)
        }
        _ => (None, request.temperature),
    };

    let system = request.system.as_deref().map(|text| {
        if prompt_cache_mode.caches_system() {
            serde_json::json!([{
                "type": "text",
                "text": text,
                "cache_control": ephemeral_cache_control(),
            }])
        } else {
            Value::String(text.to_owned())
        }
    });

    let wire = Wire {
        model: &request.model,
        messages,
        system,
        tools,
        tool_choice,
        max_tokens: request.max_tokens,
        temperature,
        stop_sequences: request.stop_sequences.clone(),
        stream: true,
        metadata: request.metadata.as_ref(),
        thinking: thinking_param,
        context_management: request.context_management.as_ref(),
    };

    serde_json::to_value(&wire).expect("wire request serializes")
}

fn ephemeral_cache_control() -> Value {
    serde_json::json!({"type": "ephemeral"})
}

fn message_to_wire(msg: &Message) -> Value {
    use crate::types::ContentBlock;
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "user",
    };
    // Filter out blocks the Anthropic API rejects on replay:
    //   - server-side tool blocks (not accepted back as input), and
    //   - unsendable thinking blocks (a thinking block with neither a
    //     signature nor a redacted `data` payload — e.g. a collapsed
    //     redacted block from an older transcript — would 400).
    //   - foreign compaction blocks: an OpenAI `compaction` item is an
    //     opaque blob only its own backend can read, so Anthropic would
    //     reject it. Dropping it loses whatever history it stood in for,
    //     which is worth a line in the log — it only happens when a
    //     session moves providers after a server-side compaction.
    // Also avoid serializing an empty content array.
    let foreign_compactions = msg
        .content
        .iter()
        .filter(|b| b.is_backend_bound_compaction())
        .count();
    if foreign_compactions > 0 {
        tracing::warn!(
            blocks = foreign_compactions,
            "anthropic: dropping OpenAI compaction blocks the session carried over; \
             the history they replaced is not recoverable on this provider"
        );
    }
    let filtered: Vec<&ContentBlock> = msg
        .content
        .iter()
        .filter(|b| {
            !matches!(
                b,
                ContentBlock::ServerToolUse(_)
                    | ContentBlock::WebSearchResult(_)
                    | ContentBlock::GeneratedImage(_)
            )
        })
        .filter(|b| !is_unsendable_thinking(b))
        .filter(|b| !b.is_backend_bound_compaction())
        .collect();
    let to_array = |blocks: &[&ContentBlock]| -> Value {
        Value::Array(blocks.iter().map(|b| content_block_to_wire(b)).collect())
    };
    let content = if filtered.len() == 1 {
        match filtered[0] {
            ContentBlock::ToolResult(tr) if tr.content.is_empty() => Value::String(String::new()),
            _ => to_array(&filtered),
        }
    } else {
        to_array(&filtered)
    };
    serde_json::json!({
        "role": role,
        "content": content,
    })
}

/// A thinking block Anthropic would reject on replay: it has no
/// signature (required for ordinary extended-thinking blocks) and no
/// redacted `data` payload, so there is nothing valid to send back.
fn is_unsendable_thinking(block: &crate::types::ContentBlock) -> bool {
    use crate::types::ContentBlock;
    matches!(
        block,
        ContentBlock::Thinking(tb) if tb.signature.is_none() && tb.data.is_none()
    )
}

/// Serialize a content block to its Anthropic wire shape. Most blocks
/// use their serde representation; a redacted-thinking block (carrying
/// `data`) must be emitted as a `redacted_thinking` block rather than
/// the `thinking`-tagged serde default of [`ThinkingBlock`].
fn content_block_to_wire(block: &crate::types::ContentBlock) -> Value {
    use crate::types::ContentBlock;
    match block {
        ContentBlock::Thinking(tb) if tb.data.is_some() => serde_json::json!({
            "type": "redacted_thinking",
            "data": tb.data,
        }),
        _ => serde_json::to_value(block).unwrap_or(Value::Null),
    }
}

fn tool_choice_to_wire(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => serde_json::json!({"type": "auto"}),
        ToolChoice::Any => serde_json::json!({"type": "any"}),
        ToolChoice::Tool { name } => serde_json::json!({"type": "tool", "name": name}),
        ToolChoice::None => serde_json::json!({"type": "none"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentBlock, TextBlock};
    use std::sync::{Mutex, OnceLock};

    #[test]
    fn vertex_is_recognised_by_host_only() {
        assert!(is_vertex_base_url(
            "https://aiplatform.googleapis.com/v1/projects/p/locations/global"
        ));
        assert!(is_vertex_base_url(
            "https://us-east5-aiplatform.googleapis.com/v1/projects/p/locations/us-east5"
        ));
        assert!(is_vertex_base_url(
            "https://aiplatform.us.rep.googleapis.com/v1/projects/p/locations/us"
        ));
        assert!(!is_vertex_base_url("https://api.anthropic.com"));
        assert!(!is_vertex_base_url(
            "https://bedrock-mantle.us-east-1.api.aws/anthropic"
        ));
        assert!(!is_vertex_base_url(
            "https://x.services.ai.azure.com/anthropic"
        ));
        assert!(!is_vertex_base_url(""));
    }

    #[test]
    fn vertex_endpoint_puts_the_model_in_the_url() {
        let base = "https://aiplatform.googleapis.com/v1/projects/p/locations/global";
        assert_eq!(
            vertex_endpoint_for_base(base, "claude-opus-5"),
            "https://aiplatform.googleapis.com/v1/projects/p/locations/global/publishers/anthropic/models/claude-opus-5:streamRawPredict"
        );
        // Trailing slash, dated id, pasted `/v1/messages` all normalise.
        assert_eq!(
            vertex_endpoint_for_base(&format!("{base}/"), "claude-sonnet-4-5@20250929"),
            format!(
                "{base}/publishers/anthropic/models/claude-sonnet-4-5@20250929:streamRawPredict"
            )
        );
        assert_eq!(
            vertex_endpoint_for_base(&format!("{base}/v1/messages"), "claude-opus-5"),
            format!("{base}/publishers/anthropic/models/claude-opus-5:streamRawPredict")
        );
        // A base that already names the publisher path or a model is not
        // doubled.
        assert_eq!(
            vertex_endpoint_for_base(
                &format!("{base}/publishers/anthropic/models"),
                "claude-opus-5"
            ),
            format!("{base}/publishers/anthropic/models/claude-opus-5:streamRawPredict")
        );
        assert_eq!(
            vertex_endpoint_for_base(
                &format!("{base}/publishers/anthropic/models/claude-opus-5:rawPredict"),
                "ignored"
            ),
            format!("{base}/publishers/anthropic/models/claude-opus-5:streamRawPredict")
        );
    }

    #[test]
    fn vertex_body_drops_model_and_adds_the_platform_version() {
        let request = CreateMessageRequest::simple("claude-opus-5", "hi");
        let body = vertex_body(build_request_body(&request));
        assert!(body.get("model").is_none());
        assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["stream"], true);
        // The plain wire keeps the model where the Messages API wants it.
        let plain = build_request_body(&request);
        assert_eq!(plain["model"], "claude-opus-5");
        assert!(plain.get("anthropic_version").is_none());
    }

    fn prompt_cache_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    struct PromptCacheEnvGuard {
        previous: Option<String>,
    }

    impl PromptCacheEnvGuard {
        fn install(value: Option<&str>) -> Self {
            let previous = std::env::var("REBON_ANTHROPIC_PROMPT_CACHE").ok();
            match value {
                Some(value) => std::env::set_var("REBON_ANTHROPIC_PROMPT_CACHE", value),
                None => std::env::remove_var("REBON_ANTHROPIC_PROMPT_CACHE"),
            }
            PROMPT_CACHE_ENV_READ_ENABLED.with(|enabled| enabled.set(true));
            Self { previous }
        }
    }

    impl Drop for PromptCacheEnvGuard {
        fn drop(&mut self) {
            PROMPT_CACHE_ENV_READ_ENABLED.with(|enabled| enabled.set(false));
            match self.previous.take() {
                Some(previous) => std::env::set_var("REBON_ANTHROPIC_PROMPT_CACHE", previous),
                None => std::env::remove_var("REBON_ANTHROPIC_PROMPT_CACHE"),
            }
        }
    }

    fn with_prompt_cache_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _lock_guard = prompt_cache_env_lock();
        let _env_guard = PromptCacheEnvGuard::install(value);
        f()
    }

    fn cache_test_request() -> CreateMessageRequest {
        CreateMessageRequest::simple("m", "hi")
            .with_system("Be terse.")
            .with_tools(vec![
                Tool {
                    name: "Read".into(),
                    description: "Read a file".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                },
                Tool {
                    name: "Write".into(),
                    description: "Write a file".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                },
            ])
    }

    fn assert_no_cache_control(value: &Value) {
        let rendered = serde_json::to_string(value).unwrap();
        assert!(!rendered.contains("cache_control"), "{rendered}");
    }

    #[test]
    fn build_request_body_prompt_cache_disabled_by_default_preserves_shape() {
        with_prompt_cache_env(None, || {
            let req = cache_test_request();
            let body = build_request_body(&req);

            assert_eq!(body["system"], "Be terse.");
            assert!(body["system"].is_string());
            assert_no_cache_control(&body);
        });
    }

    #[test]
    fn build_request_body_prompt_cache_disabled_aliases_preserve_shape() {
        for value in ["", "0", "false", "unknown"] {
            with_prompt_cache_env(Some(value), || {
                let req = cache_test_request();
                let body = build_request_body(&req);

                assert_eq!(body["system"], "Be terse.");
                assert!(body["system"].is_string(), "value={value:?}: {body}");
                assert_no_cache_control(&body);
            });
        }
    }

    #[test]
    fn build_request_body_prompt_cache_system_mode_marks_system_only() {
        with_prompt_cache_env(Some("system"), || {
            let req = cache_test_request();
            let body = build_request_body(&req);

            assert_eq!(body["system"][0]["type"], "text");
            assert_eq!(body["system"][0]["text"], "Be terse.");
            assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
            assert!(body["tools"][0].get("cache_control").is_none());
            assert!(body["tools"][1].get("cache_control").is_none());
        });
    }

    #[test]
    fn build_request_body_prompt_cache_tools_mode_marks_last_plain_tool_only() {
        with_prompt_cache_env(Some("tools"), || {
            let req = cache_test_request().with_web_search(crate::request::WebSearchToolConfig {
                allowed_domains: None,
                blocked_domains: None,
                max_uses: None,
                search_context_size: None,
                user_location: None,
            });
            let body = build_request_body(&req);

            assert_eq!(body["system"], "Be terse.");
            assert!(body["tools"][0].get("cache_control").is_none());
            assert_eq!(body["tools"][1]["name"], "Write");
            assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
            assert_eq!(body["tools"][2]["type"], "web_search_20250305");
            assert!(body["tools"][2].get("cache_control").is_none());
        });
    }

    #[test]
    fn build_request_body_prompt_cache_system_and_tools_aliases_mark_both() {
        for value in ["1", "true", " TRUE ", "system_and_tools", "system-tools"] {
            with_prompt_cache_env(Some(value), || {
                let req = cache_test_request();
                let body = build_request_body(&req);

                assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
                assert!(body["tools"][0].get("cache_control").is_none());
                assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
            });
        }
    }

    #[test]
    fn build_request_body_prompt_cache_system_alias_trims_and_ignores_case() {
        with_prompt_cache_env(Some(" System "), || {
            let req = cache_test_request();
            let body = build_request_body(&req);

            assert_eq!(body["system"][0]["type"], "text");
            assert_eq!(body["system"][0]["text"], "Be terse.");
            assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
            assert!(body["tools"][0].get("cache_control").is_none());
            assert!(body["tools"][1].get("cache_control").is_none());
        });
    }

    #[test]
    fn build_request_body_prompt_cache_tools_mode_does_not_mark_web_search_only() {
        with_prompt_cache_env(Some("tools"), || {
            let req = CreateMessageRequest::simple("m", "hi")
                .with_system("Be terse.")
                .with_web_search(crate::request::WebSearchToolConfig {
                    allowed_domains: None,
                    blocked_domains: None,
                    max_uses: None,
                    search_context_size: None,
                    user_location: None,
                });
            let body = build_request_body(&req);

            assert_eq!(body["system"], "Be terse.");
            assert_eq!(body["tools"].as_array().unwrap().len(), 1);
            assert_eq!(body["tools"][0]["type"], "web_search_20250305");
            assert!(body["tools"][0].get("cache_control").is_none());
            assert_no_cache_control(&body);
        });
    }

    #[test]
    fn parse_usage_preserves_prompt_cache_token_fields() {
        let raw = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":12,"cache_creation_input_tokens":34,"cache_read_input_tokens":56}}"#;
        let event = parse_anthropic_event(raw).unwrap().unwrap();
        match event {
            StreamEvent::MessageDelta { delta } => {
                assert_eq!(delta.usage.output_tokens, 12);
                assert_eq!(delta.usage.cache_creation_input_tokens, 34);
                assert_eq!(delta.usage.cache_read_input_tokens, 56);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn build_request_body_allows_tool_result_image_content() {
        let mut req = CreateMessageRequest::simple("claude", "");
        req.messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult(crate::types::ToolResultBlock {
                tool_use_id: "toolu_1".into(),
                content: crate::types::ToolResultContent::blocks(vec![
                    crate::types::ToolResultContentBlock::Text(TextBlock {
                        text: "Image file: screenshot.png".into(),
                    }),
                    crate::types::ToolResultContentBlock::Image(crate::types::ImageBlock::base64(
                        "image/png",
                        "AAAA",
                    )),
                ]),
                is_error: false,
            })],
        }];

        let body = build_request_body(&req);

        assert_eq!(body["messages"][0]["content"][0]["type"], "tool_result");
        assert_eq!(
            body["messages"][0]["content"][0]["content"][1]["type"],
            "image"
        );
        assert_eq!(
            body["messages"][0]["content"][0]["content"][1]["source"]["media_type"],
            "image/png"
        );
    }
    #[test]
    fn build_request_body_allows_tool_result_document_content() {
        let mut req = CreateMessageRequest::simple("claude", "");
        req.messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult(crate::types::ToolResultBlock {
                tool_use_id: "toolu_1".into(),
                content: crate::types::ToolResultContent::blocks(vec![
                    crate::types::ToolResultContentBlock::Text(TextBlock {
                        text: "PDF file read: doc.pdf".into(),
                    }),
                    crate::types::ToolResultContentBlock::Document(
                        crate::types::DocumentBlock::base64("application/pdf", "JVBERi0="),
                    ),
                ]),
                is_error: false,
            })],
        }];

        let body = build_request_body(&req);

        assert_eq!(body["messages"][0]["content"][0]["type"], "tool_result");
        assert_eq!(
            body["messages"][0]["content"][0]["content"][1]["type"],
            "document"
        );
        assert_eq!(
            body["messages"][0]["content"][0]["content"][1]["source"]["media_type"],
            "application/pdf"
        );
    }

    #[test]
    fn build_request_body_ignores_role_system_messages() {
        let mut req = CreateMessageRequest::simple("claude", "hello");
        req.system = Some("top-level".into());
        req.messages.insert(
            0,
            Message {
                role: Role::System,
                content: vec![ContentBlock::Text(TextBlock {
                    text: "hidden".into(),
                })],
            },
        );

        let body = build_request_body(&req);
        assert_eq!(body["system"], "top-level");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn parse_message_start_event() {
        let raw = r#"{"type":"message_start","message":{"id":"msg_1","model":"claude","usage":{"input_tokens":4,"output_tokens":0}}}"#;
        let event = parse_anthropic_event(raw).unwrap().unwrap();
        match event {
            StreamEvent::MessageStart {
                message_id,
                model,
                usage,
            } => {
                assert_eq!(message_id, "msg_1");
                assert_eq!(model, "claude");
                assert_eq!(usage.input_tokens, 4);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parse_text_delta() {
        let raw =
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#;
        let event = parse_anthropic_event(raw).unwrap().unwrap();
        match event {
            StreamEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(index, 0);
                assert_eq!(delta, ContentBlockDelta::TextDelta { text: "hi".into() });
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parse_tool_use_block_start() {
        let raw = r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}}"#;
        let event = parse_anthropic_event(raw).unwrap().unwrap();
        match event {
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                assert_eq!(index, 1);
                assert_eq!(
                    content_block,
                    ContentBlockStart::ToolUse {
                        id: "toolu_1".into(),
                        name: "Read".into()
                    }
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parse_message_delta_with_stop_reason() {
        let raw = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":12}}"#;
        let event = parse_anthropic_event(raw).unwrap().unwrap();
        match event {
            StreamEvent::MessageDelta { delta } => {
                assert_eq!(delta.stop_reason, Some(StopReason::EndTurn));
                assert_eq!(delta.usage.output_tokens, 12);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parse_error_event() {
        let raw = r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#;
        let event = parse_anthropic_event(raw).unwrap().unwrap();
        match event {
            StreamEvent::Error {
                error_type,
                message,
            } => {
                assert_eq!(error_type, "overloaded_error");
                assert_eq!(message, "busy");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parse_ping_is_dropped() {
        let event = parse_anthropic_event(r#"{"type":"ping"}"#).unwrap();
        assert!(event.is_none());
    }

    /// Zhipu's GLM Coding Plan endpoint is the case this exists for: its
    /// setup instructions name `ANTHROPIC_AUTH_TOKEN`, so the key has to
    /// arrive as a bearer or the server sees no credential at all.
    #[test]
    fn a_third_party_messages_gateway_also_gets_the_key_as_a_bearer() {
        assert!(anthropic_compatible_gateway_needs_bearer(
            "https://open.bigmodel.cn/api/anthropic"
        ));
        assert!(anthropic_compatible_gateway_needs_bearer(
            "https://gateway.example.com/v1"
        ));
    }

    #[test]
    fn anthropics_own_endpoint_keeps_sending_only_x_api_key() {
        for base in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/v1",
            "https://api.anthropic.com/v1/messages",
            "https://API.Anthropic.com/",
        ] {
            assert!(!anthropic_compatible_gateway_needs_bearer(base), "{base}");
        }
    }

    #[test]
    fn endpoint_appends_v1_messages_to_bare_base_url() {
        assert_eq!(
            messages_endpoint_for_base("https://api.example.com"),
            "https://api.example.com/v1/messages"
        );
    }

    #[test]
    fn endpoint_does_not_duplicate_existing_v1_segment() {
        assert_eq!(
            messages_endpoint_for_base("https://api.example.com/v1"),
            "https://api.example.com/v1/messages"
        );
    }

    #[test]
    fn endpoint_preserves_explicit_messages_endpoint() {
        assert_eq!(
            messages_endpoint_for_base("https://api.example.com/v1/messages"),
            "https://api.example.com/v1/messages"
        );
    }

    #[test]
    fn endpoint_normalizes_trailing_slashes() {
        assert_eq!(
            messages_endpoint_for_base("https://api.example.com/v1/"),
            "https://api.example.com/v1/messages"
        );
        assert_eq!(
            messages_endpoint_for_base("https://api.example.com/v1/messages/"),
            "https://api.example.com/v1/messages"
        );
    }

    #[test]
    fn official_anthropic_base_url_recognizes_supported_shapes() {
        for base_url in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/",
            "https://api.anthropic.com/v1",
            "https://api.anthropic.com/v1/messages/",
        ] {
            assert!(is_official_anthropic_base_url(base_url), "{base_url}");
        }
        assert!(!is_official_anthropic_base_url(
            "https://relay.example.com/v1"
        ));
    }

    #[test]
    fn context_management_request_body_and_betas_are_official_anthropic_shape() {
        let req = CreateMessageRequest::simple("claude-sonnet-4-6", "hi").with_context_management(
            crate::request::ContextManagementConfig::anthropic_full_history_replay_with_thresholds(
                50_000, 100_000,
            ),
        );
        let body = build_request_body(&req);
        let edits = body["context_management"]["edits"].as_array().unwrap();
        assert_eq!(edits[0]["type"], "clear_thinking_20251015");
        assert_eq!(edits[1]["type"], "clear_tool_uses_20250919");
        assert_eq!(edits[2]["type"], "compact_20260112");

        let mut betas = vec!["existing-beta".to_string()];
        add_context_management_betas(&mut betas, req.context_management.as_ref());
        assert_eq!(
            betas,
            vec![
                "existing-beta".to_string(),
                "context-management-2025-06-27".to_string(),
                "compact-2026-01-12".to_string(),
            ]
        );
    }

    #[test]
    fn context_management_without_compaction_omits_compaction_beta() {
        let req = CreateMessageRequest::simple("claude-sonnet-4-6", "hi").with_context_management(
            crate::request::ContextManagementConfig::anthropic_context_management_with_thresholds(
                50_000, None,
            ),
        );
        let body = build_request_body(&req);
        let edits = body["context_management"]["edits"].as_array().unwrap();
        assert_eq!(edits.len(), 2);

        let mut betas = Vec::new();
        add_context_management_betas(&mut betas, req.context_management.as_ref());
        assert_eq!(betas, vec!["context-management-2025-06-27"]);
    }

    #[test]
    fn parse_compaction_block_round_trips_to_wire() {
        let start = r#"{"type":"content_block_start","index":0,"content_block":{"type":"compaction","content":"<summary>state</summary>"}}"#;
        let delta = r#"{"type":"content_block_delta","index":0,"delta":{"type":"compaction_delta","content":" plus"}}"#;
        let event = parse_anthropic_event(start).unwrap().unwrap();
        let mut acc = crate::events::MessageAccumulator::new();
        acc.apply(&StreamEvent::MessageStart {
            message_id: "msg_1".into(),
            model: "claude-sonnet-4-6".into(),
            usage: Usage::default(),
        })
        .unwrap();
        acc.apply(&event).unwrap();
        acc.apply(&parse_anthropic_event(delta).unwrap().unwrap())
            .unwrap();
        acc.apply(&StreamEvent::MessageStop).unwrap();
        let message = acc.finish();
        assert_eq!(message.content.len(), 1);
        let wire = message_to_wire(&Message {
            role: Role::Assistant,
            content: message.content,
        });
        assert_eq!(wire["content"][0]["type"], "compaction");
        assert_eq!(
            wire["content"][0]["content"],
            "<summary>state</summary> plus"
        );
    }

    #[test]
    fn build_body_marshals_minimal_request() {
        let req = CreateMessageRequest::simple("claude-sonnet-4-6", "ping");
        let body = build_request_body(&req);
        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(body["messages"][0]["content"][0]["text"], "ping");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn build_body_marshals_tools_and_system() {
        let req = CreateMessageRequest::simple("m", "hi")
            .with_system("Be terse.")
            .with_tools(vec![Tool {
                name: "Read".into(),
                description: "Read a file".into(),
                input_schema: serde_json::json!({"type":"object"}),
            }]);
        let body = build_request_body(&req);
        assert_eq!(body["system"], "Be terse.");
        assert_eq!(body["tools"][0]["name"], "Read");
        assert_eq!(body["tools"][0]["description"], "Read a file");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn convenience_constructor_builds_universal_client() {
        use crate::client::ModelClient;
        let client = anthropic_client(AnthropicClientConfig::with_api_key("sk-test"));
        assert_eq!(
            <UniversalModelClient as ModelClient>::provider_name(&client),
            "anthropic"
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

    #[tokio::test]
    async fn convenience_constructor_does_not_cut_off_slow_sse_stream() {
        use crate::client::ModelClient;

        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":1}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let (base_url, server) =
            start_delayed_sse_http_server(body, Duration::from_millis(100)).await;
        let mut config = AnthropicClientConfig::with_api_key("sk-test");
        config.base_url = base_url;
        config.request_timeout = Some(Duration::from_millis(10));
        let client = anthropic_client(config);

        let msg = client
            .create_message(CreateMessageRequest::simple("claude-sonnet-4-6", "ping"))
            .await
            .unwrap();

        server.await.unwrap();
        assert_eq!(msg.text(), "ok");
    }

    // -- Web search tests -----------------------------------------

    #[test]
    fn build_body_injects_web_search_tool_with_full_config() {
        use crate::request::WebSearchToolConfig;
        let req = CreateMessageRequest::simple("m", "hi").with_web_search(WebSearchToolConfig {
            allowed_domains: Some(vec!["example.com".into()]),
            blocked_domains: Some(vec!["bad.com".into()]),
            max_uses: Some(3),
            search_context_size: None,
            user_location: None,
        });
        let body = build_request_body(&req);
        let tools = body["tools"].as_array().unwrap();
        let ws = tools
            .iter()
            .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("web_search_20250305"))
            .expect("web_search_20250305 tool should be present");
        assert_eq!(ws["max_uses"], 3);
        assert_eq!(ws["allowed_domains"][0], "example.com");
        assert_eq!(ws["blocked_domains"][0], "bad.com");
    }

    #[test]
    fn build_body_injects_web_search_tool_minimal() {
        let req = CreateMessageRequest::simple("m", "hi").with_web_search(
            crate::request::WebSearchToolConfig {
                allowed_domains: None,
                blocked_domains: None,
                max_uses: None,
                search_context_size: None,
                user_location: None,
            },
        );
        let body = build_request_body(&req);
        let tools = body["tools"].as_array().unwrap();
        let ws = tools
            .iter()
            .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("web_search_20250305"))
            .expect("web_search_20250305 tool should be present");
        // Optional fields should be absent
        assert!(ws.get("max_uses").is_none());
        assert!(ws.get("allowed_domains").is_none());
    }

    #[test]
    fn build_body_omits_web_search_when_none() {
        let req = CreateMessageRequest::simple("m", "hi");
        let body = build_request_body(&req);
        // tools should be absent (empty vec skipped by serde)
        assert!(body.get("tools").is_none() || body["tools"].as_array().unwrap().is_empty());
    }

    #[test]
    fn parse_server_tool_use_block_start() {
        let raw = r#"{"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search_20250305","input":{"query":"rust programming"}}}"#;
        let event = parse_anthropic_event(raw).unwrap().unwrap();
        match event {
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                assert_eq!(index, 1);
                match content_block {
                    ContentBlockStart::ServerToolUse { id, name, input } => {
                        assert_eq!(id, "srvtoolu_1");
                        assert_eq!(name, "web_search_20250305");
                        assert_eq!(input["query"], "rust programming");
                    }
                    other => panic!("expected ServerToolUse, got {other:?}"),
                }
            }
            other => panic!("expected ContentBlockStart, got {other:?}"),
        }
    }

    #[test]
    fn parse_web_search_tool_result_block_start() {
        let raw = r#"{"type":"content_block_start","index":2,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","title":"Rust Lang","url":"https://rust-lang.org"}]}}"#;
        let event = parse_anthropic_event(raw).unwrap().unwrap();
        match event {
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                assert_eq!(index, 2);
                match content_block {
                    ContentBlockStart::WebSearchResult {
                        tool_use_id,
                        content,
                    } => {
                        assert_eq!(tool_use_id, "srvtoolu_1");
                        let arr = content.as_array().unwrap();
                        assert_eq!(arr.len(), 1);
                        assert_eq!(arr[0]["title"], "Rust Lang");
                        assert_eq!(arr[0]["url"], "https://rust-lang.org");
                    }
                    other => panic!("expected WebSearchResult, got {other:?}"),
                }
            }
            other => panic!("expected ContentBlockStart, got {other:?}"),
        }
    }

    #[test]
    fn message_to_wire_filters_out_server_tool_blocks() {
        use crate::types::{ContentBlock, ServerToolUseBlock, TextBlock, WebSearchResultBlock};
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text(TextBlock {
                    text: "Let me search.".into(),
                }),
                ContentBlock::ServerToolUse(ServerToolUseBlock {
                    id: "srv_1".into(),
                    name: "web_search".into(),
                    input: serde_json::json!({}),
                }),
                ContentBlock::WebSearchResult(WebSearchResultBlock {
                    tool_use_id: "srv_1".into(),
                    results: vec![],
                    raw_content: None,
                }),
                ContentBlock::Text(TextBlock {
                    text: "Done.".into(),
                }),
            ],
        };
        let wire = message_to_wire(&msg);
        let content = wire["content"].as_array().unwrap();
        // Only the two text blocks should survive; server tool
        // blocks must be filtered out.
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "Let me search.");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "Done.");
    }

    /// A session that compacted on OpenAI and then switched to Anthropic
    /// carries a blob Anthropic cannot read and would 400 on. It goes;
    /// Anthropic's own plaintext compaction block stays.
    #[test]
    fn message_to_wire_drops_a_foreign_compaction_block() {
        use crate::types::{CompactionBlock, ContentBlock};
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Compaction(CompactionBlock {
                    content: None,
                    encrypted_content: Some("openai-blob".into()),
                }),
                ContentBlock::Compaction(CompactionBlock {
                    content: Some("a plaintext summary".into()),
                    encrypted_content: None,
                }),
            ],
        };
        let wire = message_to_wire(&msg);
        let content = wire["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "compaction");
        assert_eq!(content[0]["content"], "a plaintext summary");
    }

    #[test]
    fn message_to_wire_round_trips_redacted_thinking_and_drops_malformed() {
        use crate::types::{ContentBlock, TextBlock, ThinkingBlock};
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                // Redacted thinking: reasoning lives in `data`, no signature.
                ContentBlock::Thinking(ThinkingBlock {
                    thinking: String::new(),
                    signature: None,
                    data: Some("ENCRYPTED".into()),
                }),
                // Ordinary extended thinking: text + signature.
                ContentBlock::Thinking(ThinkingBlock {
                    thinking: "reasoning".into(),
                    signature: Some("sig".into()),
                    data: None,
                }),
                // Collapsed/malformed thinking (e.g. a redacted block from
                // an older transcript that lost its `data`): empty, no
                // signature, no data — must be dropped so Anthropic does
                // not 400 on a signatureless thinking block.
                ContentBlock::Thinking(ThinkingBlock {
                    thinking: String::new(),
                    signature: None,
                    data: None,
                }),
                ContentBlock::Text(TextBlock {
                    text: "done".into(),
                }),
            ],
        };
        let wire = message_to_wire(&msg);
        let content = wire["content"].as_array().unwrap();
        // The malformed thinking block is dropped; the other three survive.
        assert_eq!(content.len(), 3, "malformed thinking block must be dropped");
        // Redacted thinking round-trips as a `redacted_thinking` block
        // carrying its encrypted payload, NOT a signatureless `thinking`.
        assert_eq!(content[0]["type"], "redacted_thinking");
        assert_eq!(content[0]["data"], "ENCRYPTED");
        assert!(content[0].get("thinking").is_none());
        // Ordinary thinking keeps its text + signature.
        assert_eq!(content[1]["type"], "thinking");
        assert_eq!(content[1]["thinking"], "reasoning");
        assert_eq!(content[1]["signature"], "sig");
        assert_eq!(content[2]["type"], "text");
    }

    #[test]
    fn parse_redacted_thinking_start_captures_data() {
        let raw = r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"ENCRYPTED"}}"#;
        let event = parse_anthropic_event(raw).unwrap().unwrap();
        match event {
            StreamEvent::ContentBlockStart {
                content_block: ContentBlockStart::Thinking { thinking, data },
                ..
            } => {
                assert!(thinking.is_empty());
                assert_eq!(data.as_deref(), Some("ENCRYPTED"));
            }
            other => panic!("expected a redacted thinking block start, got {other:?}"),
        }
    }
}
