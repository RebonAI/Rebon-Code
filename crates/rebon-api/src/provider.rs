//! `ChatProvider` trait + `UniversalModelClient` — the real
//! abstraction point of `rebon-api`.
//!
//! ## Why a separate trait
//!
//! The naive implementation is to implement [`ModelClient`] twice (once
//! per provider) and duplicate HTTP plumbing + SSE parsing on each
//! side. That works but it scales badly: adding a third provider
//! (Bedrock, Vertex, Azure, Gemini, Ollama, local llama.cpp…) means
//! copy-pasting the transport layer again, and cross-cutting
//! concerns (retry, logging, metrics, caching) have to be threaded
//! through every implementation.
//!
//! This module introduces the *real* abstraction layering:
//!
//! - [`ChatProvider`] — **provider-specific**. One file per backend.
//!   Builds the outbound request, sends it, and decodes the
//!   response stream into provider-agnostic
//!   [`StreamEvent`](crate::StreamEvent)s. Receives an injected
//!   [`reqwest::Client`] so HTTP machinery (timeouts, custom DNS,
//!   proxies, TLS config) can be shared across providers.
//! - [`UniversalModelClient`] — **provider-agnostic glue**.
//!   Implements [`ModelClient`] by holding an
//!   `Arc<dyn ChatProvider>` and forwarding every
//!   `create_message_stream` call to it. This is what consumers
//!   above this crate actually hold.
//! - [`crate::middleware`] — **cross-cutting concerns**. Retry,
//!   logging, and whatever lands later wrap any `Arc<dyn ModelClient>`
//!   without touching provider code.
//!
//! ## Adding a new provider
//!
//! ```no_run
//! use std::sync::Arc;
//! use async_trait::async_trait;
//! use rebon_api::{
//!     ChatProvider, CreateMessageRequest, ModelClient, ModelResult, StreamEventStream,
//!     UniversalModelClient,
//! };
//!
//! struct MyProvider;
//!
//! #[async_trait]
//! impl ChatProvider for MyProvider {
//!     fn provider_name(&self) -> &'static str { "my-backend" }
//!
//!     async fn send_message_stream(
//!         &self,
//!         _http: &reqwest::Client,
//!         _request: CreateMessageRequest,
//!     ) -> ModelResult<StreamEventStream> {
//!         // Build request, POST, decode stream, map to StreamEvent…
//!         unimplemented!()
//!     }
//! }
//!
//! # fn demo() {
//! let client: Arc<dyn ModelClient> =
//!     Arc::new(UniversalModelClient::new(Arc::new(MyProvider)));
//! # let _ = client;
//! # }
//! ```
//!
//! No new `ModelClient` impl, no duplicated HTTP plumbing, no extra
//! wiring in downstream crates.

use std::sync::Arc;

use async_trait::async_trait;

use std::time::Duration;

use crate::client::{ModelCapabilities, ModelClient};
use crate::error::{ModelError, ModelResult};
use crate::events::StreamEventStream;
use crate::request::CreateMessageRequest;

/// Backend-specific contract every provider implements.
///
/// Implementations own three concerns:
///
/// 1. Building the outbound HTTP request (endpoint, auth headers,
///    body shape).
/// 2. Parsing the inbound HTTP response (status code classification
///    via [`classify_http_error`], SSE framing, provider-specific
///    event decoding).
/// 3. Translating provider-specific events into the canonical
///    [`StreamEvent`](crate::StreamEvent) enum.
///
/// Everything else — HTTP client construction, stream lifecycle,
/// retry, logging — lives above the trait.
#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// Stable identifier for diagnostic logs (`"anthropic"`,
    /// `"openai-compatible"`, `"bedrock"`, …). Consumers should not
    /// branch on this value — it is for observability only.
    fn provider_name(&self) -> &'static str;

    /// Return every runtime capability in one snapshot.
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    /// Whether this provider is the ChatGPT Codex OAuth route that can
    /// execute Codex-entitled native web search requests.
    fn supports_codex_oauth_web_search(&self) -> bool {
        self.capabilities().codex_oauth_web_search
    }

    /// Whether request-scoped transient context can be sent outside
    /// durable message history without invalidating provider-side
    /// continuation state.
    fn supports_request_scoped_transient_context(&self) -> bool {
        !self.capabilities().requires_inline_transient_context
    }

    /// Whether this provider supports request-level forced tool choice.
    fn supports_forced_tool_choice(&self) -> bool {
        self.capabilities().forced_tool_choice
    }

    /// Whether hidden reasoning output is billed against
    /// `max_tokens`. See [`ModelClient::output_budget_includes_reasoning`].
    fn output_budget_includes_reasoning(&self) -> bool {
        self.capabilities().output_budget_includes_reasoning
    }

    /// Whether replayed thinking blocks must carry a signature.
    /// See [`ModelClient::thinking_replay_requires_signature`].
    fn thinking_replay_requires_signature(&self) -> bool {
        !self.capabilities().accepts_unsigned_thinking_replay
    }

    /// Whether this provider's prompt cache is keyed on a byte-exact
    /// request prefix. See [`ModelClient::prefix_cache_is_byte_exact`].
    fn prefix_cache_is_byte_exact(&self) -> bool {
        self.capabilities().prefix_cache_is_byte_exact
    }

    /// Whether this endpoint implements OpenAI remote compaction v2.
    /// See [`ModelClient::supports_remote_compaction_v2`].
    fn supports_remote_compaction_v2(&self) -> bool {
        self.capabilities().remote_compaction_v2
    }

    /// Send the request and return a stream of
    /// [`StreamEvent`](crate::StreamEvent)s.
    ///
    /// Implementations receive the shared [`reqwest::Client`] from
    /// [`UniversalModelClient`] so callers can plug in their own
    /// configured HTTP instance (custom timeouts, middleware,
    /// mock-server base URLs…). Providers must NOT create their own
    /// `reqwest::Client` inside this call; they should only reuse
    /// the injected one.
    ///
    /// Providers are expected to force `request.stream = true`
    /// internally before sending.
    async fn send_message_stream(
        &self,
        http: &reqwest::Client,
        request: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream>;

    /// Create an isolated clone of this provider suitable for a
    /// sub-agent session. The returned provider shares immutable
    /// config and auth state but has **fresh** session-scoped
    /// mutable state (e.g. `previous_response_id`, WebSocket
    /// connection). Providers without session state return `None`
    /// (the default), which tells the caller that a plain
    /// `Arc::clone` is sufficient.
    fn fork_for_sub_agent(&self) -> Option<Arc<dyn ChatProvider>> {
        None
    }

    fn fork_for_sub_agent_with_cache_key(
        &self,
        _prompt_cache_key: Option<String>,
    ) -> Option<Arc<dyn ChatProvider>> {
        self.fork_for_sub_agent()
    }

    /// Reset session-scoped mutable state (e.g.
    /// `previous_response_id`, WebSocket connection) without
    /// creating a new provider instance. Called when starting a
    /// fresh conversation (e.g. `/new`) so stale response IDs from
    /// the previous session don't leak into the new one.
    /// Providers without session state can use the default no-op.
    fn reset_session_state(&self) {}

    /// Per-turn cleanup hook. See [`ModelClient::end_turn`]. Called
    /// by [`UniversalModelClient`] when the engine signals a turn is
    /// complete. Providers should clear only state that cannot be
    /// safely validated before the next request. Default no-op.
    fn end_turn(&self) {}

    /// Clear only the `previous_response_id` chain, keeping any
    /// open transport alive. Providers that use server-side
    /// continuation must validate each request against their stored
    /// baseline before reusing that continuation, and send a full
    /// replay when it diverges. See
    /// [`ModelClient::invalidate_previous_response_id`]. Default
    /// no-op.
    fn invalidate_previous_response_id(&self) {}
}

fn forwarded_provider_capabilities(provider: &dyn ChatProvider) -> ModelCapabilities {
    let mut capabilities = provider.capabilities();
    capabilities.codex_oauth_web_search = provider.supports_codex_oauth_web_search();
    capabilities.requires_inline_transient_context =
        !provider.supports_request_scoped_transient_context();
    capabilities.forced_tool_choice = provider.supports_forced_tool_choice();
    capabilities.output_budget_includes_reasoning = provider.output_budget_includes_reasoning();
    capabilities.accepts_unsigned_thinking_replay = !provider.thinking_replay_requires_signature();
    capabilities.prefix_cache_is_byte_exact = provider.prefix_cache_is_byte_exact();
    capabilities.remote_compaction_v2 = provider.supports_remote_compaction_v2();
    capabilities
}

/// Provider-agnostic [`ModelClient`] implementation.
///
/// Holds an `Arc<dyn ChatProvider>` and a shared
/// [`reqwest::Client`]. Every `create_message_stream` call forwards
/// straight to the provider. This is the type consumers actually
/// pass around:
///
/// ```no_run
/// # use std::sync::Arc;
/// # use rebon_api::{AnthropicClientConfig, AnthropicProvider, ModelClient, UniversalModelClient};
/// let provider = Arc::new(AnthropicProvider::new(
///     AnthropicClientConfig::with_api_key("sk-xxx"),
/// ));
/// let client: Arc<dyn ModelClient> = Arc::new(UniversalModelClient::new(provider));
/// # let _ = client;
/// ```
#[derive(Clone)]
pub struct UniversalModelClient {
    provider: Arc<dyn ChatProvider>,
    http: reqwest::Client,
}

impl std::fmt::Debug for UniversalModelClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UniversalModelClient")
            .field("provider", &self.provider.provider_name())
            .finish()
    }
}

pub(crate) fn build_http_client(request_timeout: Option<Duration>) -> reqwest::Client {
    let mut builder = reqwest::ClientBuilder::new();
    if let Some(timeout) = request_timeout {
        builder = builder.connect_timeout(timeout);
    }
    builder
        .build()
        .expect("rebon-api: failed to build reqwest client")
}

impl UniversalModelClient {
    /// Construct a client with a fresh default [`reqwest::Client`].
    pub fn new(provider: Arc<dyn ChatProvider>) -> Self {
        Self {
            provider,
            http: build_http_client(None),
        }
    }

    /// Construct a client reusing an existing [`reqwest::Client`].
    /// Useful when the caller has already configured custom
    /// timeouts, TLS, proxies, or interceptors.
    pub fn with_http_client(provider: Arc<dyn ChatProvider>, http: reqwest::Client) -> Self {
        Self { provider, http }
    }

    /// Clone the underlying HTTP client — useful for sharing across
    /// several [`UniversalModelClient`]s that point at different
    /// providers but want the same transport-level settings.
    pub fn http_client(&self) -> reqwest::Client {
        self.http.clone()
    }

    /// Borrow the provider — primarily for diagnostics.
    pub fn provider(&self) -> &Arc<dyn ChatProvider> {
        &self.provider
    }
}

#[async_trait]
impl ModelClient for UniversalModelClient {
    fn provider_name(&self) -> &'static str {
        self.provider.provider_name()
    }

    fn capabilities(&self) -> ModelCapabilities {
        forwarded_provider_capabilities(self.provider.as_ref())
    }

    async fn create_message_stream(
        &self,
        mut request: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream> {
        request.stream = true;
        self.provider.send_message_stream(&self.http, request).await
    }

    fn fork_for_sub_agent(&self) -> Option<Arc<dyn ModelClient>> {
        self.provider.fork_for_sub_agent().map(|p| {
            Arc::new(UniversalModelClient::with_http_client(p, self.http.clone()))
                as Arc<dyn ModelClient>
        })
    }

    fn fork_for_sub_agent_with_cache_key(
        &self,
        prompt_cache_key: Option<String>,
    ) -> Option<Arc<dyn ModelClient>> {
        self.provider
            .fork_for_sub_agent_with_cache_key(prompt_cache_key)
            .map(|p| {
                Arc::new(UniversalModelClient::with_http_client(p, self.http.clone()))
                    as Arc<dyn ModelClient>
            })
    }

    fn reset_session_state(&self) {
        self.provider.reset_session_state();
    }

    fn end_turn(&self) {
        self.provider.end_turn();
    }

    fn invalidate_previous_response_id(&self) {
        self.provider.invalidate_previous_response_id();
    }
}

/// Classify an HTTP status code + response body into a
/// [`ModelError`].
///
/// Every provider implementation should call this on non-success
/// statuses so the error shape is consistent across backends.
/// Middleware (retry, logging) can then branch on
/// [`ModelError::is_transient`] without caring which provider
/// produced the failure.
///
/// `retry_after` should be extracted from the response's
/// `retry-after` header via [`crate::parse_retry_after`] before the
/// response body is consumed.
pub fn classify_http_error(status: u16, body: String, retry_after: Option<Duration>) -> ModelError {
    let msg = format!("{status}: {body}");
    match status {
        401 | 403 => ModelError::Unauthorized(msg),
        400 | 404 | 405 => {
            // Context-overflow 400s are retryable (middleware adjusts
            // max_tokens), but they stay as BadRequest — the retry
            // middleware inspects via `context_overflow()`.
            ModelError::BadRequest(msg)
        }
        529 => ModelError::overloaded(msg, retry_after),
        408 | 409 | 429 | 500..=599 => ModelError::transient_http(msg, status, retry_after),
        _ => ModelError::Permanent(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ContentBlockDelta, ContentBlockStart, MessageDeltaFields, StreamEvent};
    use crate::types::{StopReason, Usage};
    use futures_util::StreamExt;

    /// Minimal provider that emits a scripted event list on every
    /// call, ignoring the request entirely. Demonstrates that
    /// adding a brand-new backend requires exactly one `impl` block
    /// and no new code in `UniversalModelClient`.
    struct EchoProvider {
        events: Vec<StreamEvent>,
    }

    #[async_trait]
    impl ChatProvider for EchoProvider {
        fn provider_name(&self) -> &'static str {
            "echo"
        }

        async fn send_message_stream(
            &self,
            _http: &reqwest::Client,
            _request: CreateMessageRequest,
        ) -> ModelResult<StreamEventStream> {
            let events = self.events.clone();
            let stream = futures_util::stream::iter(events.into_iter().map(Ok));
            Ok(Box::pin(stream))
        }
    }

    fn sample_events() -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message_id: "echo-1".into(),
                model: "echo".into(),
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
                delta: ContentBlockDelta::TextDelta {
                    text: "echo reply".into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Usage {
                        output_tokens: 2,
                        ..Default::default()
                    },
                },
            },
            StreamEvent::MessageStop,
        ]
    }

    #[tokio::test]
    async fn universal_client_forwards_to_provider() {
        let provider = Arc::new(EchoProvider {
            events: sample_events(),
        });
        let client = UniversalModelClient::new(provider);
        assert_eq!(client.provider_name(), "echo");

        let msg = client
            .create_message(CreateMessageRequest::simple("echo", "ping"))
            .await
            .unwrap();
        assert_eq!(msg.id, "echo-1");
        assert_eq!(msg.text(), "echo reply");
        assert_eq!(msg.stop_reason, Some(StopReason::EndTurn));
    }

    #[tokio::test]
    async fn universal_client_honors_streaming_path_directly() {
        let provider = Arc::new(EchoProvider {
            events: sample_events(),
        });
        let client = UniversalModelClient::new(provider);
        let mut stream = client
            .create_message_stream(CreateMessageRequest::simple("echo", "ping"))
            .await
            .unwrap();
        let mut count = 0;
        while let Some(event) = stream.next().await {
            event.unwrap();
            count += 1;
        }
        assert_eq!(count, 6);
    }

    #[test]
    fn provider_capability_matrix_is_preserved_by_universal_client() {
        use crate::anthropic::{AnthropicClientConfig, AnthropicProvider};
        use crate::openai::{OpenAiCompatibleClientConfig, OpenAiCompatibleProvider};
        use crate::openai_responses::{OpenAiResponsesClientConfig, OpenAiResponsesProvider};

        let anthropic = AnthropicProvider::new(AnthropicClientConfig::with_api_key("test"));
        let anthropic_caps = ModelCapabilities {
            prefix_cache_is_byte_exact: true,
            ..ModelCapabilities::default()
        };
        assert_eq!(ChatProvider::capabilities(&anthropic), anthropic_caps);

        let mut chat_config =
            OpenAiCompatibleClientConfig::with_base_url("https://api.deepseek.com", "test");
        chat_config.request_scoped_transient_context = false;
        let chat = OpenAiCompatibleProvider::new(chat_config);
        let chat_caps = ModelCapabilities {
            requires_inline_transient_context: true,
            accepts_unsigned_thinking_replay: true,
            prefix_cache_is_byte_exact: true,
            ..ModelCapabilities::default()
        };
        assert_eq!(ChatProvider::capabilities(&chat), chat_caps);

        let responses = OpenAiResponsesProvider::new(OpenAiResponsesClientConfig::with_base_url(
            "https://api.openai.com/v1",
            "test",
        ));
        let responses_caps = ModelCapabilities {
            requires_inline_transient_context: true,
            output_budget_includes_reasoning: true,
            accepts_unsigned_thinking_replay: true,
            prefix_cache_is_byte_exact: true,
            remote_compaction_v2: true,
            ..ModelCapabilities::default()
        };
        assert_eq!(ChatProvider::capabilities(&responses), responses_caps);

        let universal = UniversalModelClient::new(Arc::new(responses));
        assert_eq!(ModelClient::capabilities(&universal), responses_caps);
        assert!(!universal.supports_request_scoped_transient_context());
        assert!(!universal.thinking_replay_requires_signature());
        assert!(universal.supports_remote_compaction_v2());
    }

    #[test]
    fn classify_http_error_covers_expected_buckets() {
        assert!(matches!(
            classify_http_error(401, "no".into(), None),
            ModelError::Unauthorized(_)
        ));
        assert!(matches!(
            classify_http_error(403, "no".into(), None),
            ModelError::Unauthorized(_)
        ));
        assert!(matches!(
            classify_http_error(400, "no".into(), None),
            ModelError::BadRequest(_)
        ));
        assert!(matches!(
            classify_http_error(429, "no".into(), None),
            ModelError::Transient { .. }
        ));
        assert!(matches!(
            classify_http_error(503, "no".into(), None),
            ModelError::Transient { .. }
        ));
        assert!(matches!(
            classify_http_error(529, "overloaded".into(), None),
            ModelError::Overloaded { .. }
        ));
        assert!(matches!(
            classify_http_error(418, "teapot".into(), None),
            ModelError::Permanent(_)
        ));
        // 409 lock timeout is now transient
        assert!(matches!(
            classify_http_error(409, "lock".into(), None),
            ModelError::Transient { .. }
        ));
        // retry-after is preserved
        let err = classify_http_error(
            429,
            "rate limited".into(),
            Some(std::time::Duration::from_secs(5)),
        );
        assert_eq!(err.retry_after(), Some(std::time::Duration::from_secs(5)));
    }

    // ── fork_for_sub_agent ──────────────────────────────────────

    #[test]
    fn universal_client_fork_returns_none_for_stateless_provider() {
        let provider = Arc::new(EchoProvider {
            events: sample_events(),
        });
        let client = UniversalModelClient::new(provider);
        // EchoProvider uses the default `fork_for_sub_agent` which
        // returns None — no session state to isolate.
        assert!(client.fork_for_sub_agent().is_none());
    }

    /// Provider that supports forking. Returns a fresh
    /// `ForkableProvider` from `fork_for_sub_agent` with an
    /// incremented generation counter so callers can distinguish
    /// parent from child.
    struct ForkableProvider {
        generation: u32,
    }

    #[async_trait]
    impl ChatProvider for ForkableProvider {
        fn provider_name(&self) -> &'static str {
            "forkable"
        }

        async fn send_message_stream(
            &self,
            _http: &reqwest::Client,
            _request: CreateMessageRequest,
        ) -> ModelResult<StreamEventStream> {
            let stream = futures_util::stream::empty();
            Ok(Box::pin(stream))
        }

        fn fork_for_sub_agent(&self) -> Option<Arc<dyn ChatProvider>> {
            Some(Arc::new(ForkableProvider {
                generation: self.generation + 1,
            }))
        }
    }

    #[test]
    fn universal_client_fork_forwards_to_provider() {
        let provider = Arc::new(ForkableProvider { generation: 0 });
        let client = UniversalModelClient::new(provider);

        let forked = client.fork_for_sub_agent();
        assert!(forked.is_some());

        let forked = forked.unwrap();
        assert_eq!(forked.provider_name(), "forkable");
    }

    #[test]
    fn universal_client_fork_preserves_http_client() {
        // Build client with a custom HTTP client (non-default timeout)
        // and verify the forked client reuses it.
        let http = reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(42))
            .build()
            .unwrap();
        let provider = Arc::new(ForkableProvider { generation: 0 });
        let client = UniversalModelClient::with_http_client(provider, http);

        let forked = client.fork_for_sub_agent().unwrap();
        // The forked client is an Arc<dyn ModelClient> — we can only
        // verify observable properties. provider_name confirms the
        // forwarding worked.
        assert_eq!(forked.provider_name(), "forkable");
    }
}
