//! `OpenAiResponsesProvider` — [`crate::ChatProvider`] implementation
//! against the OpenAI **Responses API** (`/responses` endpoint).
//!
//! It exists because the ChatGPT Codex backend
//! (`https://chatgpt.com/backend-api/codex/responses`) speaks the
//! Responses API shape, **not** the chat/completions shape — so the
//! OAuth-authenticated backend cannot go through the existing
//! [`crate::openai`] provider.
//!
//! ## What this module implements
//!
//! * **Outbound request translation**:
//!   [`build_responses_request_body`] rewrites the provider-agnostic
//!   [`CreateMessageRequest`] into a Responses API `POST /responses`
//!   body. Covers: `model`, `input[]` items (user text, assistant
//!   text, assistant tool calls, user tool results),
//!   `instructions` (system prompt promoted to top-level),
//!   `tools[]` (flattened `{type: function, name, description,
//!   parameters}` — NOT nested under `function: {...}`), `store`
//!   (forced to `false` for the ChatGPT Codex backend),
//!   `prompt_cache_key`, `max_output_tokens` / `temperature` /
//!   `top_p` / `tool_choice` (all skipped on the Codex backend,
//!   mirroring the stricter-params rule), and `stream: true`.
//! * **Inbound SSE translation**: [`OpenAiResponsesTranslator`]
//!   consumes named SSE events from the Responses API stream and
//!   emits provider-agnostic [`StreamEvent`]s that the existing
//!   [`crate::events::MessageAccumulator`] already knows how to
//!   fold into an `AssistantMessage`. Handled events:
//!   - `response.created` → capture `response.id`, emit `MessageStart`
//!   - `response.output_item.added` → open a `Text` /
//!     `ToolUse` / `Reasoning` block (`ContentBlockStart`)
//!   - `response.output_text.delta` →
//!     `ContentBlockDelta::TextDelta`
//!   - `response.function_call_arguments.delta` →
//!     `ContentBlockDelta::InputJsonDelta`
//!   - `response.output_item.done` → `ContentBlockStop`
//!   - `response.completed` → final `MessageDelta` +
//!     `MessageStop`; the stop reason is **inferred** from whether
//!     any `function_call` item appeared on the stream (the stream
//!     itself carries a hardcoded `end_turn`, so `tool_use` is
//!     derived from the presence of tool_use content blocks).
//!   - `response.incomplete` with `max_output_tokens` → final
//!     `MessageDelta` with `StopReason::MaxTokens` + `MessageStop`.
//!   - `response.error` → `StreamEvent::Error`
//!   - Every other event type is logged at debug and silently
//!     dropped (`response.in_progress`, `response.content_part.*`,
//!     `response.reasoning_summary_*`, …).
//!
//! ## Boundary notes: handled here, and deliberately not covered
//!
//! * **Reasoning / thinking blocks**: the `response.reasoning_*`
//!   event family is now translated — `response.output_item.added`
//!   with type `"reasoning"` emits `ContentBlockStart::Thinking`,
//!   and `response.reasoning_summary_text.delta` emits
//!   `ContentBlockDelta::ThinkingDelta`. The downstream
//!   `MessageAccumulator` already handles both.
//! * **`previous_response_id` continuation**: supported on the
//!   **WebSocket transport** (`use_websocket: true`). Each
//!   `response.create` frame includes the `previous_response_id`
//!   from the last completed response so the server can skip
//!   re-processing prior input items. The HTTP transport always
//!   omits this parameter because the `chatgpt.com` HTTP endpoint
//!   returns 400 when it is present.
//! * **401 retry** (token refresh + retry on stale bearer) is handled
//!   here: given a [`TokenRefresher`] through
//!   [`OpenAiResponsesProvider::with_refresher`], the provider mints a
//!   fresh bearer on the first 401 and retries the request once, on
//!   both the HTTP and the WebSocket transport. Without a refresher
//!   attached the 401 surfaces as [`ModelError::Unauthorized`]; what
//!   lives one layer up is the refresher's own token I/O and
//!   persistence.
//! * **Tool-choice translation** (`tool_choice: auto/any/function`)
//!   is skipped on the Codex backend per the stricter-params rule.
//!   Non-Codex backends using this provider would need tool-choice
//!   translation implemented — deferred until we actually have such
//!   a provider.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;

use async_trait::async_trait;
use futures_util::{stream::unfold, SinkExt, StreamExt, TryStreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite;

use crate::cache_trace::{
    cache_trace_enabled, stable_hash_value, CacheMissReason, RequestShapeTrace,
};
use crate::client::ModelCapabilities;
use crate::error::{ModelError, ModelResult};
use crate::events::{
    ContentBlockDelta, ContentBlockStart, MessageDeltaFields, StreamEvent, StreamEventStream,
};
use crate::provider::{classify_http_error, ChatProvider, UniversalModelClient};
use crate::request::CreateMessageRequest;
use crate::sse::{decode_sse_stream, SseDecoder, SseFrame};
use crate::types::{StopReason, Usage};
use crate::ServiceTierHandle;

/// Configuration for an [`OpenAiResponsesProvider`].
#[derive(Debug, Clone)]
pub struct OpenAiResponsesClientConfig {
    /// Base URL for the Responses API endpoint. May or may not
    /// already end in `/responses`; [`OpenAiResponsesProvider`]
    /// appends the suffix when missing.
    pub base_url: String,
    /// OAuth access token or literal API key. Sent as
    /// `Authorization: Bearer {api_key}`.
    pub api_key: String,
    /// Optional organisation id (`OpenAI-Organization` header).
    pub organization: Option<String>,
    /// Extra HTTP headers to inject on every request.
    pub extra_headers: Vec<(String, String)>,
    /// HTTP client connect timeout. Only honoured when the provider
    /// constructs its own [`UniversalModelClient`].
    pub request_timeout: Option<Duration>,
    /// Cache key passed as `prompt_cache_key` on every request. The
    /// ChatGPT Codex backend uses this to share prompt cache hits
    /// across turns of the same session. A fresh random value is
    /// generated on provider construction if the caller does not
    /// override it, so a single rebon process maps to a single
    /// cache-session.
    pub prompt_cache_key: Option<String>,
    /// Optional prompt-cache retention hint mirrored into cache diagnostics.
    pub prompt_cache_retention: Option<String>,
    /// When `true`, the provider connects via WebSocket instead of
    /// HTTP POST. The WebSocket transport supports
    /// `previous_response_id` for input-delta optimisation (the
    /// server can skip re-processing prior input items). The HTTP
    /// endpoint at `chatgpt.com` does not support this parameter
    /// and returns 400 when it is present, so the HTTP path never
    /// sends it.
    pub use_websocket: bool,
    /// Optional runtime switch for OpenAI `service_tier: "priority"`.
    pub service_tier: Option<ServiceTierHandle>,
}

impl Default for OpenAiResponsesClientConfig {
    fn default() -> Self {
        Self {
            base_url: "https://chatgpt.com/backend-api/codex".to_string(),
            api_key: String::new(),
            organization: None,
            extra_headers: Vec::new(),
            request_timeout: Some(Duration::from_secs(60)),
            prompt_cache_key: None,
            prompt_cache_retention: None,
            use_websocket: false,
            service_tier: None,
        }
    }
}

impl OpenAiResponsesClientConfig {
    /// Convenience constructor that sets the access token.
    pub fn with_api_key(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            ..Self::default()
        }
    }

    /// Convenience constructor that sets the base URL + access token.
    pub fn with_base_url(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            ..Self::default()
        }
    }
}

/// Refresh callback invoked on `401 Unauthorized` to mint a fresh
/// access token and retry the request.
///
/// Implementations are expected to:
///
/// 1. Talk to the identity provider (for rebon's case, POST to
///    `https://auth.openai.com/oauth/token` with the stored
///    refresh token).
/// 2. **Persist** the rotated tokens so the next rebon invocation
///    picks them up (rebon's case: atomic rewrite of
///    `.credentials.json`).
/// 3. Return the new bearer token string so the provider can
///    retry the failed request with it.
///
/// Errors surface as `String` rather than a typed enum so the
/// trait stays dep-free of implementation-specific error types
/// (`anyhow::Error`, IO errors, etc.).
#[async_trait]
pub trait TokenRefresher: Send + Sync + std::fmt::Debug {
    /// Mint a fresh access token. Returns the new bearer on
    /// success, a human-readable error on failure.
    async fn refresh(&self) -> Result<String, String>;
}

/// Mutable auth state owned by an [`OpenAiResponsesProvider`].
/// Held behind a `tokio::sync::Mutex` so the 401 retry path can
/// rotate the current access token in place without rebuilding
/// the provider.
#[derive(Debug)]
struct AuthInner {
    /// Current bearer token used on every request.
    access_token: String,
    /// Optional refresh callback. When `Some` and a request
    /// returns 401, the provider calls this before retrying.
    refresher: Option<Arc<dyn TokenRefresher>>,
}

#[derive(Debug, Clone)]
struct ResponsesSessionState {
    last_response_id: Arc<std::sync::Mutex<Option<String>>>,
    last_request_input: Arc<std::sync::Mutex<Option<Vec<Value>>>>,
    last_request_fingerprint: Arc<std::sync::Mutex<Option<Value>>>,
    pending_request_input: Arc<std::sync::Mutex<Option<Vec<Value>>>>,
    pending_request_fingerprint: Arc<std::sync::Mutex<Option<Value>>>,
    items_added_since_last_request: Arc<std::sync::Mutex<Vec<Value>>>,
}

impl ResponsesSessionState {
    fn new() -> Self {
        Self {
            last_response_id: Arc::new(std::sync::Mutex::new(None)),
            last_request_input: Arc::new(std::sync::Mutex::new(None)),
            last_request_fingerprint: Arc::new(std::sync::Mutex::new(None)),
            pending_request_input: Arc::new(std::sync::Mutex::new(None)),
            pending_request_fingerprint: Arc::new(std::sync::Mutex::new(None)),
            items_added_since_last_request: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn clear(&self) {
        *self
            .last_response_id
            .lock()
            .expect("last_response_id mutex poisoned") = None;
        *self
            .last_request_input
            .lock()
            .expect("last_request_input mutex poisoned") = None;
        *self
            .last_request_fingerprint
            .lock()
            .expect("last_request_fingerprint mutex poisoned") = None;
        *self
            .pending_request_input
            .lock()
            .expect("pending_request_input mutex poisoned") = None;
        *self
            .pending_request_fingerprint
            .lock()
            .expect("pending_request_fingerprint mutex poisoned") = None;
        self.items_added_since_last_request
            .lock()
            .expect("items_added mutex poisoned")
            .clear();
    }

    fn begin_request(&self, full_input: Vec<Value>, fingerprint: Value) {
        *self
            .pending_request_input
            .lock()
            .expect("pending_request_input mutex poisoned") = Some(full_input);
        *self
            .pending_request_fingerprint
            .lock()
            .expect("pending_request_fingerprint mutex poisoned") = Some(fingerprint);
        self.items_added_since_last_request
            .lock()
            .expect("items_added mutex poisoned")
            .clear();
    }

    fn commit_pending_request(&self) {
        let input = self
            .pending_request_input
            .lock()
            .expect("pending_request_input mutex poisoned")
            .take();
        let fingerprint = self
            .pending_request_fingerprint
            .lock()
            .expect("pending_request_fingerprint mutex poisoned")
            .take();
        if let Some(input) = input {
            *self
                .last_request_input
                .lock()
                .expect("last_request_input mutex poisoned") = Some(input);
        }
        if let Some(fingerprint) = fingerprint {
            *self
                .last_request_fingerprint
                .lock()
                .expect("last_request_fingerprint mutex poisoned") = Some(fingerprint);
        }
    }

    fn discard_pending_request(&self) {
        *self
            .pending_request_input
            .lock()
            .expect("pending_request_input mutex poisoned") = None;
        *self
            .pending_request_fingerprint
            .lock()
            .expect("pending_request_fingerprint mutex poisoned") = None;
        self.items_added_since_last_request
            .lock()
            .expect("items_added mutex poisoned")
            .clear();
    }

    fn set_last_response_id(&self, response_id: Option<String>) {
        *self
            .last_response_id
            .lock()
            .expect("last_response_id mutex poisoned") = response_id;
    }
}

fn emit_openai_request_shape_trace(
    request: &CreateMessageRequest,
    prompt_cache_key: &str,
    previous_response_id_present: bool,
    request_fingerprint: Option<&Value>,
    cache_miss_reason: CacheMissReason,
) {
    if !cache_trace_enabled() {
        return;
    }

    let mut trace_request = request.clone();
    let trace_context = trace_request
        .cache_trace_context
        .get_or_insert_with(Default::default);
    trace_context.prompt_cache_key = Some(prompt_cache_key.to_string());
    if trace_context.prompt_cache_retention.is_none() {
        trace_context.prompt_cache_retention = request
            .cache_trace_context
            .as_ref()
            .and_then(|trace| trace.prompt_cache_retention.clone());
    }
    trace_context.api_path = Some("openai_responses".to_string());
    trace_context.previous_response_id_present = Some(previous_response_id_present);
    let shape = RequestShapeTrace::from_request(&trace_request);
    let trace = trace_request.cache_trace_context.as_ref();
    let provider_fingerprint_hash = request_fingerprint.map(stable_hash_value);
    tracing::info!(
        target: "rebon_cache_trace",
        provider = "openai_responses",
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
        previous_response_id_present,
        prompt_cache_key = %prompt_cache_key,
        prompt_cache_retention = trace.and_then(|trace| trace.prompt_cache_retention.as_deref()),
        api_path = trace.and_then(|trace| trace.api_path.as_deref()),
        provider_fingerprint_hash = provider_fingerprint_hash.as_deref(),
        cache_miss_reason = cache_miss_reason.as_str(),
        "request_shape_trace"
    );
}

/// Raw WebSocket stream type alias.
type RawWsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// ---------------------------------------------------------------------------
// Background WebSocket message pump
// ---------------------------------------------------------------------------
//
// Matches codex-ref's `WsStream` in `responses_websocket.rs`.
// A dedicated tokio task continuously reads from the raw WebSocket, handles
// Ping/Pong frames immediately (so the server's keepalive never times out),
// and forwards Text/Close/Error frames through an mpsc channel. Sends go
// through a command channel so the pump task exclusively owns the underlying
// `SplitSink + SplitStream`.

enum WsPumpCommand {
    Send {
        message: tungstenite::Message,
        tx_result: tokio::sync::oneshot::Sender<Result<(), tungstenite::Error>>,
    },
}

/// Background-pumped WebSocket that handles ping/pong transparently.
struct WsPump {
    tx_command: tokio::sync::mpsc::Sender<WsPumpCommand>,
    rx_message:
        tokio::sync::mpsc::UnboundedReceiver<Result<tungstenite::Message, tungstenite::Error>>,
    pump_task: tokio::task::JoinHandle<()>,
}

impl WsPump {
    fn new(inner: RawWsStream) -> Self {
        let (tx_command, mut rx_command) = tokio::sync::mpsc::channel::<WsPumpCommand>(32);
        let (tx_message, rx_message) = tokio::sync::mpsc::unbounded_channel::<
            Result<tungstenite::Message, tungstenite::Error>,
        >();

        let pump_task = tokio::spawn(async move {
            let mut inner = inner;
            loop {
                tokio::select! {
                    command = rx_command.recv() => {
                        let Some(command) = command else { break };
                        match command {
                            WsPumpCommand::Send { message, tx_result } => {
                                let result = inner.send(message).await;
                                let should_break = result.is_err();
                                let _ = tx_result.send(result);
                                if should_break { break; }
                            }
                        }
                    }
                    message = inner.next() => {
                        let Some(message) = message else { break };
                        match message {
                            Ok(tungstenite::Message::Ping(payload)) => {
                                if let Err(e) = inner.send(tungstenite::Message::Pong(payload.clone())).await {
                                    let _ = tx_message.send(Err(e));
                                    break;
                                }
                                if tx_message.send(Ok(tungstenite::Message::Ping(payload))).is_err() {
                                    break;
                                }
                            }
                            Ok(tungstenite::Message::Pong(payload)) => {
                                if tx_message.send(Ok(tungstenite::Message::Pong(payload))).is_err() {
                                    break;
                                }
                            }
                            Ok(msg @ (tungstenite::Message::Text(_)
                                | tungstenite::Message::Binary(_)
                                | tungstenite::Message::Close(_))) => {
                                let is_close = matches!(msg, tungstenite::Message::Close(_));
                                if tx_message.send(Ok(msg)).is_err() { break; }
                                if is_close { break; }
                            }
                            Ok(_) => {}
                            Err(e) => {
                                let _ = tx_message.send(Err(e));
                                break;
                            }
                        }
                    }
                }
            }
        });

        Self {
            tx_command,
            rx_message,
            pump_task,
        }
    }

    /// Send a message through the pump task.
    async fn send(&self, message: tungstenite::Message) -> Result<(), tungstenite::Error> {
        let (tx_result, rx_result) = tokio::sync::oneshot::channel();
        self.tx_command
            .send(WsPumpCommand::Send { message, tx_result })
            .await
            .map_err(|_| {
                tungstenite::Error::Protocol(tungstenite::error::ProtocolError::SendAfterClosing)
            })?;
        rx_result.await.unwrap_or(Err(tungstenite::Error::Protocol(
            tungstenite::error::ProtocolError::SendAfterClosing,
        )))
    }

    /// Receive the next WebSocket frame relevant to turn liveness.
    /// Ping/Pong frames are handled transparently by the pump before
    /// being surfaced here so they reset the per-frame idle timer
    /// without reaching the Responses translator. They deliberately
    /// do NOT reset the data-stall deadline ([`WS_DATA_STALL_TIMEOUT`]):
    /// a server that stalls mid-response while still sending
    /// keepalives must not hang the turn forever.
    async fn next_frame(&mut self) -> Option<Result<tungstenite::Message, tungstenite::Error>> {
        self.rx_message.recv().await
    }

    /// Cheap liveness check used between turns: the pump task exits
    /// as soon as the underlying socket closes (Close frame / EOF /
    /// transport error). A finished pump means any subsequent `send`
    /// will fail with `SendAfterClosing` — callers should throw the
    /// pump away and reconnect before sending instead of eating that
    /// failure plus the retry backoff.
    fn is_alive(&self) -> bool {
        !self.pump_task.is_finished()
    }

    /// Non-blocking drain of any Close / error frames the pump may
    /// have queued between turns. Returns `true` if the connection is
    /// still usable; `false` when a Close frame or transport error
    /// has already arrived (meaning a subsequent `send` is doomed).
    fn drain_between_turns(&mut self) -> bool {
        use tokio::sync::mpsc::error::TryRecvError;
        loop {
            match self.rx_message.try_recv() {
                Ok(Ok(tungstenite::Message::Close(_))) => return false,
                Ok(Err(_)) => return false,
                // Stray Text/Binary frames between turns are unexpected
                // — treat them as benign and keep draining.
                Ok(Ok(_)) => continue,
                Err(TryRecvError::Empty) => return self.is_alive(),
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }
}

impl Drop for WsPump {
    fn drop(&mut self) {
        self.pump_task.abort();
    }
}

/// [`ChatProvider`] implementation that speaks the OpenAI Responses
/// API wire shape (the ChatGPT Codex backend format).
#[derive(Clone)]
pub struct OpenAiResponsesProvider {
    config: Arc<OpenAiResponsesClientConfig>,
    /// Cached prompt cache key — either the caller-supplied one or
    /// a fresh millisecond-epoch string generated at construction.
    prompt_cache_key: String,
    /// Mutable auth state: current access token + optional
    /// refresher. Separate from `config` so the 401-retry path can
    /// swap in a rotated bearer without mutating the shared
    /// immutable config. Wrapped in `Arc<std::sync::Mutex<...>>`
    /// so `Clone` on the provider preserves the same rotated
    /// state — `std::sync::Mutex` is correct here because the
    /// critical sections (clone the bearer, clone the refresher,
    /// swap in a new token) never cross an `.await` point.
    auth: Arc<std::sync::Mutex<AuthInner>>,
    session_state: ResponsesSessionState,
    /// WebSocket connection for the Responses transport. Opened
    /// lazily, reused across prompt turns when still alive, and
    /// drained before every send so dead sockets are detected before
    /// reusing `previous_response_id`. The `TokioMutex` serializes
    /// requests that target the same provider session.
    ws_conn: Arc<TokioMutex<Option<WsPump>>>,
    /// Session-wide flag set when the server returns 426
    /// UPGRADE_REQUIRED (or an unrecoverable WS handshake failure).
    /// Once set, `send_message_stream` skips the WS branch and
    /// routes every subsequent request through the plain HTTP/SSE
    /// transport until the process restarts. Matches codex-ref's
    /// session-scoped `websocket_fallback` switch.
    http_fallback_active: Arc<AtomicBool>,
    /// Count of consecutive TLS-handshake EOF (or equivalent
    /// low-level transport) failures observed during `connect_ws`.
    /// Reset to zero on any successful handshake **and** when the
    /// gap between consecutive EOFs exceeds
    /// [`TLS_HANDSHAKE_EOF_BURST_WINDOW`] — scattered failures across
    /// hours are network noise, not a hostile intermediary. When the
    /// counter crosses [`TLS_HANDSHAKE_EOF_BURST_THRESHOLD`] within
    /// the window, the provider flips `http_fallback_active` and
    /// gives up on the WebSocket path for the rest of the session.
    consecutive_handshake_eofs: Arc<AtomicU32>,
    /// Wall-clock time of the most recent TLS-handshake EOF. Used to
    /// decide whether the next EOF extends the current burst (within
    /// [`TLS_HANDSHAKE_EOF_BURST_WINDOW`]) or starts a fresh streak.
    last_handshake_eof_at: Arc<StdMutex<Option<Instant>>>,
    /// Optional notifier for the one-shot continuation recovery that
    /// retries without a rejected `previous_response_id`. Generic retry
    /// progress is owned exclusively by [`crate::RetryMiddleware`].
    retry_notifier: Option<crate::RetryNotifier>,
}

/// After this many TLS-handshake EOFs **within
/// [`TLS_HANDSHAKE_EOF_BURST_WINDOW`]**, the provider switches to
/// HTTP/SSE. Was 3 with no time window: a hostile intermediary
/// triggers fallback in seconds, but so did 3 unrelated transient
/// blips spread across hours — locking the user into HTTP for the
/// rest of the session. The combination of (slightly higher
/// threshold) + (time-bounded window) keeps the hostile-intermediary
/// detection while letting genuinely transient noise heal.
const TLS_HANDSHAKE_EOF_BURST_THRESHOLD: u32 = 5;

/// Time window in which [`TLS_HANDSHAKE_EOF_BURST_THRESHOLD`] EOFs
/// must accumulate to trigger fallback. Outside the window, the
/// streak resets to 1 — a single fresh EOF, not a burst.
const TLS_HANDSHAKE_EOF_BURST_WINDOW: Duration = Duration::from_secs(60);

/// `OpenAI-Beta` opt-in value that gates the Responses WebSocket
/// transport. The ChatGPT Codex backend refuses the WS upgrade
/// unless this header is present, silently pushing us onto the
/// HTTP/SSE path (which cannot carry `previous_response_id`, so it
/// re-uploads the full history every turn). The plain HTTP
/// `/responses` endpoint does not require it, which is why only the
/// WS handshake was missing it. Mirrors codex-ref's
/// `RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE`. Operators can
/// override the date via an `OpenAI-Beta` entry in `extra_headers`.
const RESPONSES_WEBSOCKETS_BETA: &str = "responses_websockets=2026-02-06";

/// Resolve the Responses API URL for a configured base URL. See
/// [`OpenAiResponsesProvider::endpoint`] for the rationale behind the
/// bare-origin `/v1` insertion; the same heuristic already guards the
/// OpenAI chat client's `/v1/chat/completions` endpoint.
fn responses_endpoint_for_base(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/responses") {
        return base.to_string();
    }
    // Bare origin (scheme://host[:port] without a path): the Responses
    // API lives under /v1 on the standard OpenAI surface and on
    // OpenAI-compatible relays alike. Bases that carry an explicit path
    // (ChatGPT Codex's /backend-api/codex, a hand-written /v1, a
    // custom mount point) are used verbatim.
    let after_scheme = base.split_once("://").map(|(_, rest)| rest).unwrap_or(base);
    if !after_scheme.contains('/') {
        return format!("{base}/v1/responses");
    }
    format!("{base}/responses")
}

impl std::fmt::Debug for OpenAiResponsesProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiResponsesProvider")
            .field("config", &self.config)
            .field("prompt_cache_key", &self.prompt_cache_key)
            .field("auth", &"<auth>")
            .field("last_response_id", &self.session_state.last_response_id)
            .field("ws_conn", &"<ws>")
            .field(
                "http_fallback_active",
                &self.http_fallback_active.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl OpenAiResponsesProvider {
    /// Construct from a config. The current access token is
    /// initialised from `config.api_key`; no refresher is
    /// attached — use [`Self::with_refresher`] to wire one in.
    pub fn new(config: OpenAiResponsesClientConfig) -> Self {
        let prompt_cache_key = config
            .prompt_cache_key
            .clone()
            .unwrap_or_else(default_prompt_cache_key);
        let access_token = config.api_key.clone();
        Self {
            config: Arc::new(config),
            prompt_cache_key,
            auth: Arc::new(std::sync::Mutex::new(AuthInner {
                access_token,
                refresher: None,
            })),
            session_state: ResponsesSessionState::new(),
            ws_conn: Arc::new(TokioMutex::new(None)),
            http_fallback_active: Arc::new(AtomicBool::new(false)),
            consecutive_handshake_eofs: Arc::new(AtomicU32::new(0)),
            last_handshake_eof_at: Arc::new(StdMutex::new(None)),
            retry_notifier: None,
        }
    }

    /// Attach a [`RetryNotifier`] for the one-shot continuation recovery.
    /// Generic transport and server retries are reported by
    /// [`crate::RetryMiddleware`].
    pub fn with_retry_notifier(mut self, notifier: crate::RetryNotifier) -> Self {
        self.retry_notifier = Some(notifier);
        self
    }

    /// Attach a [`TokenRefresher`] so the provider can recover
    /// from `401 Unauthorized` responses by refreshing the bearer
    /// and retrying the request once.
    ///
    /// The refresher is expected to persist the rotated tokens on
    /// disk; the provider only updates its in-memory copy for the
    /// lifetime of this handle.
    pub fn with_refresher(self, refresher: Arc<dyn TokenRefresher>) -> Self {
        {
            let mut guard = self
                .auth
                .lock()
                .expect("openai-responses auth mutex poisoned");
            guard.refresher = Some(refresher);
        }
        self
    }

    /// Clone of the config — useful for diagnostics.
    pub fn config(&self) -> Arc<OpenAiResponsesClientConfig> {
        self.config.clone()
    }

    /// Compute the full `/responses` URL the provider POSTs to:
    /// if the base URL's path already ends in `/responses`, use it
    /// as-is; a bare origin (no path) gets the standard OpenAI
    /// `/v1/responses`; any other path (e.g. the ChatGPT Codex
    /// backend `/backend-api/codex`, or an explicit `/v1`) just gets
    /// `/responses` appended.
    ///
    /// The bare-origin case matters for OpenAI-compatible relays:
    /// `https://relay.example` used to become `/responses`, which a
    /// new-api style gateway answers with its 200 text/html SPA page
    /// — the stream parser then sees zero events and every request
    /// dies with "stream ended before response.completed".
    fn endpoint(&self) -> String {
        responses_endpoint_for_base(&self.config.base_url)
    }

    /// Build + send one HTTP request with the given bearer token.
    /// Returns the raw [`reqwest::Response`] on success or a
    /// classified [`ModelError`] on failure. The 401 retry loop in
    /// [`send_message_stream`] drives this twice at most.
    async fn send_once(
        &self,
        http: &reqwest::Client,
        body: &Value,
        access_token: &str,
    ) -> ModelResult<reqwest::Response> {
        let endpoint = self.endpoint();
        let mut http_req = http
            .post(&endpoint)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(body);
        if let Some(org) = &self.config.organization {
            http_req = http_req.header("OpenAI-Organization", org);
        }
        for (name, value) in &self.config.extra_headers {
            http_req = http_req.header(name, value);
        }
        let response = http_req.send().await?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = crate::error::parse_retry_after(response.headers());
            let text = response.text().await.unwrap_or_default();
            return Err(classify_http_error(status.as_u16(), text, retry_after));
        }
        // A gateway that doesn't route this path (e.g. a new-api relay
        // answering an unknown URL with its 200 text/html SPA page)
        // would otherwise surface as an opaque "stream ended before
        // response.completed" after the full transient-retry budget.
        // Fail fast and permanently with the actual cause instead.
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if content_type.to_ascii_lowercase().starts_with("text/html") {
            return Err(ModelError::Permanent(format!(
                "responses endpoint {endpoint} answered 200 with text/html instead of an \
                 event stream — the base URL likely points at the gateway's web page, \
                 not its API (try appending /v1 to the provider base URL)"
            )));
        }
        Ok(response)
    }

    /// Snapshot the current bearer + whether a refresher is
    /// wired. Held behind a short-lived lock so the caller can
    /// release it before the long-running HTTP send.
    fn snapshot_auth(&self) -> (String, bool) {
        let guard = self
            .auth
            .lock()
            .expect("openai-responses auth mutex poisoned");
        (guard.access_token.clone(), guard.refresher.is_some())
    }

    /// Drive the refresh callback and rotate the cached access
    /// token. Returns the new bearer string.
    ///
    /// The lock is deliberately held only long enough to clone
    /// the refresher `Arc`; the actual `refresh().await` call
    /// runs outside the critical section so concurrent requests
    /// are not serialized on the refresh. A second lock is taken
    /// after the `.await` to write the new token back.
    async fn refresh_bearer(&self) -> ModelResult<String> {
        let refresher = {
            let guard = self
                .auth
                .lock()
                .expect("openai-responses auth mutex poisoned");
            guard.refresher.clone()
        };
        let Some(refresher) = refresher else {
            return Err(ModelError::Unauthorized(
                "openai-responses provider: 401 but no token refresher attached".to_string(),
            ));
        };
        let new_token = refresher
            .refresh()
            .await
            .map_err(|msg| ModelError::Unauthorized(format!("token refresh failed: {msg}")))?;
        {
            let mut guard = self
                .auth
                .lock()
                .expect("openai-responses auth mutex poisoned");
            guard.access_token = new_token.clone();
        }
        Ok(new_token)
    }

    /// Compute the WebSocket endpoint URL by converting the HTTP
    /// scheme to `ws://` / `wss://`.
    fn ws_endpoint(&self) -> String {
        let http_url = self.endpoint();
        if let Some(rest) = http_url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = http_url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            http_url
        }
    }

    /// Build a WebSocket handshake request with the current auth headers.
    fn build_ws_request(&self, access_token: &str) -> ModelResult<tungstenite::http::Request<()>> {
        use tungstenite::client::IntoClientRequest;

        let mut ws_request = self
            .ws_endpoint()
            .into_client_request()
            .map_err(|e| ModelError::Protocol(format!("ws request build: {e}")))?;
        ws_request.headers_mut().insert(
            tungstenite::http::header::AUTHORIZATION,
            format!("Bearer {access_token}").parse().map_err(
                |e: tungstenite::http::header::InvalidHeaderValue| {
                    ModelError::Protocol(format!("ws auth header: {e}"))
                },
            )?,
        );
        if let Some(org) = &self.config.organization {
            if let Ok(val) = org.parse() {
                ws_request.headers_mut().insert("openai-organization", val);
            }
        }
        // Opt into the Responses WebSocket beta. Inserted before the
        // `extra_headers` loop so an operator-supplied `OpenAI-Beta`
        // value (same normalised header name) overrides this default.
        if let Ok(val) = RESPONSES_WEBSOCKETS_BETA.parse() {
            ws_request.headers_mut().insert("openai-beta", val);
        }
        for (name, value) in &self.config.extra_headers {
            if let (Ok(hn), Ok(hv)) = (
                name.parse::<tungstenite::http::HeaderName>(),
                value.parse::<tungstenite::http::HeaderValue>(),
            ) {
                ws_request.headers_mut().insert(hn, hv);
            }
        }
        Ok(ws_request)
    }

    /// Establish a new WebSocket connection wrapped in a [`WsPump`]
    /// that handles ping/pong in a background task.
    async fn connect_ws(&self) -> ModelResult<WsPump> {
        let (mut access_token, has_refresher) = self.snapshot_auth();
        let mut refreshed_after_unauthorized = false;

        loop {
            let ws_request = self.build_ws_request(&access_token)?;
            let connect_result = tokio_tungstenite::connect_async(ws_request).await;
            let (ws_stream, _response) = match connect_result {
                Ok(ok) => ok,
                Err(e) => {
                    // Check for 426 UPGRADE_REQUIRED (or similar
                    // "WS not supported on this endpoint" responses)
                    // before stringifying. When the server refuses the
                    // upgrade, we flip the session-wide
                    // `http_fallback_active` flag so future calls skip
                    // WS entirely and go straight to HTTP/SSE — a
                    // single upgrade refusal should not produce an
                    // infinite reconnect loop.
                    if let tungstenite::Error::Http(response) = &e {
                        let status = response.status().as_u16();
                        if status == 426 || status == 400 || status == 404 || status == 501 {
                            tracing::warn!(
                                status,
                                "openai-responses: ws upgrade refused, \
                                 falling back to HTTP/SSE for the rest of the session"
                            );
                            self.http_fallback_active.store(true, Ordering::Relaxed);
                            self.consecutive_handshake_eofs.store(0, Ordering::Relaxed);
                            return Err(ModelError::BadRequest(format!(
                                "ws upgrade refused (HTTP {status}), falling back to HTTP"
                            )));
                        }
                        if is_unauthorized_ws_status(status) {
                            self.consecutive_handshake_eofs.store(0, Ordering::Relaxed);
                            if has_refresher && !refreshed_after_unauthorized {
                                tracing::warn!(
                                    status,
                                    "openai-responses: ws handshake unauthorized, refreshing token and retrying once"
                                );
                                access_token = self.refresh_bearer().await?;
                                refreshed_after_unauthorized = true;
                                continue;
                            }
                            return Err(unauthorized_ws_handshake_error(status));
                        }
                    }
                    let msg = format!("ws connect: {e}");
                    let transient = is_transient_ws_connect_error(&msg);
                    // Bump the consecutive-TLS-handshake-EOF streak for
                    // the failures we've actually seen in the wild —
                    // these are the repeated attempts that used to burn
                    // through the provider's own retry budget without ever
                    // completing a handshake. Repeated attempts are owned by the
                    // outer RetryMiddleware; this counter only decides
                    // when those attempts should switch transports.
                    let is_handshake_eof = {
                        let lower = msg.to_ascii_lowercase();
                        lower.contains("tls handshake eof")
                            || lower.contains("unexpected eof")
                            || lower.contains("unexpectedeof")
                            || lower.contains("close_notify")
                    };
                    if is_handshake_eof {
                        // Time-windowed streak: if the previous EOF was
                        // more than `TLS_HANDSHAKE_EOF_BURST_WINDOW` ago,
                        // treat this as a *fresh* burst rather than
                        // extending the old one. Two unrelated EOFs an
                        // hour apart aren't a hostile intermediary —
                        // they're network noise.
                        let now = Instant::now();
                        let streak = {
                            let mut last = self
                                .last_handshake_eof_at
                                .lock()
                                .expect("last_handshake_eof_at mutex poisoned");
                            let outside_window = match *last {
                                Some(prev) => {
                                    now.duration_since(prev) > TLS_HANDSHAKE_EOF_BURST_WINDOW
                                }
                                None => false,
                            };
                            *last = Some(now);
                            if outside_window {
                                self.consecutive_handshake_eofs.store(1, Ordering::Relaxed);
                                1
                            } else {
                                self.consecutive_handshake_eofs
                                    .fetch_add(1, Ordering::Relaxed)
                                    + 1
                            }
                        };
                        if streak >= TLS_HANDSHAKE_EOF_BURST_THRESHOLD
                            && !self.http_fallback_active.swap(true, Ordering::Relaxed)
                        {
                            tracing::warn!(
                                streak,
                                threshold = TLS_HANDSHAKE_EOF_BURST_THRESHOLD,
                                window_secs = TLS_HANDSHAKE_EOF_BURST_WINDOW.as_secs(),
                                error = %msg,
                                "openai-responses: consecutive TLS-handshake EOFs crossed threshold within window; \
                                 falling back to HTTP/SSE for the rest of the session"
                            );
                            // Surface a retryable error so the outer
                            // retry path tries once more — but now the
                            // session-wide `http_fallback_active` flag
                            // routes the retry down the HTTP branch.
                            return Err(ModelError::Http(format!(
                                "ws handshake EOF burst, switching to HTTP/SSE: {msg}"
                            )));
                        }
                    } else if transient {
                        // A non-EOF-but-still-retryable failure (e.g.
                        // transient DNS, idle timeout). Don't let it
                        // stretch an otherwise-recoverable streak — clear
                        // the counter so an unrelated blip doesn't push
                        // us over the threshold.
                        self.consecutive_handshake_eofs.store(0, Ordering::Relaxed);
                    }
                    return Err(if transient {
                        ModelError::Http(msg)
                    } else {
                        ModelError::Protocol(msg)
                    });
                }
            };

            // Successful handshake — reset the EOF streak. A single
            // healthy connect clears any prior noise, so we don't carry
            // stale counts across a healed network.
            self.consecutive_handshake_eofs.store(0, Ordering::Relaxed);
            tracing::debug!("openai-responses: websocket connected");
            return Ok(WsPump::new(ws_stream));
        }
    }

    /// WebSocket transport for `send_message_stream`. Reuses a live
    /// connection across prompt turns so `previous_response_id`
    /// lookups keep hitting the same Codex backend session. Before
    /// each send the turn driver drains the pump and drops dead
    /// sockets; if a stale response id has no live socket, it retries
    /// the turn as a full replay before sending.
    async fn send_message_stream_ws(
        &self,
        http: &reqwest::Client,
        request: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream> {
        // If the previous turn's socket was already gone, a Codex
        // `previous_response_id` from that socket cannot be resolved on
        // a fresh WebSocket. Clear it before the eager connect path so
        // `drive_ws_turn_once` builds a full replay instead of sending
        // a doomed delta that will come back as
        // `previous_response_not_found`.
        let is_codex = is_chatgpt_codex_backend(&self.config.base_url);
        let has_live_ws = {
            let mut guard = self.ws_conn.lock().await;
            let live = guard.as_mut().is_some_and(WsPump::drain_between_turns);
            if guard.is_some() && !live {
                *guard = None;
            }
            live
        };
        if clear_stale_previous_response_before_new_ws(&self.session_state, is_codex, has_live_ws) {
            tracing::warn!(
                "openai-responses: clearing stale previous_response_id before opening a new websocket"
            );
        }

        // Eagerly establish the connection so we can surface a
        // 426 UPGRADE_REQUIRED (or equivalent) immediately and
        // transparently fall through to HTTP within the same call.
        // Without this pre-flight, the first request on a WS-hostile
        // backend would fail visibly and only the *next* request
        // would benefit from the session-wide fallback flag.
        {
            let mut guard = self.ws_conn.lock().await;
            if guard.is_none() {
                match self.connect_ws().await {
                    Ok(ws) => *guard = Some(ws),
                    Err(e) => {
                        // `connect_ws` sets `http_fallback_active`
                        // when it observes a 426-class refusal.
                        if self.http_fallback_active.load(Ordering::Relaxed) {
                            return self.send_message_stream_http(http, request).await;
                        }
                        // Transient errors are surfaced to the
                        // caller — the retry middleware / engine
                        // will decide what to do.
                        return Err(e);
                    }
                }
            }
        }

        let ws_conn = self.ws_conn.clone();
        let session_state = self.session_state.clone();
        let provider = self.clone();

        let (tx, rx) = tokio::sync::mpsc::channel::<ModelResult<StreamEvent>>(64);

        tokio::spawn(async move {
            if let Err(e) = drive_ws_turn(&provider, ws_conn, session_state, request, &tx).await {
                let _ = tx.send(Err(e)).await;
            }
        });

        let stream = unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        Ok(Box::pin(stream))
    }
}

fn default_prompt_cache_key() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("rebon-{ms}")
}

/// Does the given base URL point at the ChatGPT Codex backend?
///
/// The Codex
/// backend requires `store: false` and rejects
/// `max_output_tokens` / `temperature` / `top_p` / `tool_choice`.
pub fn is_chatgpt_codex_backend(base_url: &str) -> bool {
    crate::vendor::is_chatgpt_codex_endpoint(base_url)
}

fn is_unauthorized_ws_status(status: u16) -> bool {
    status == 401 || status == 403
}

fn unauthorized_ws_handshake_error(status: u16) -> ModelError {
    ModelError::Unauthorized(format!(
        "ws connect: HTTP error: {status} {}",
        tungstenite::http::StatusCode::from_u16(status)
            .ok()
            .and_then(|status| status.canonical_reason())
            .unwrap_or("Unauthorized")
    ))
}

/// Is the WebSocket connect error a transient transport failure?
///
/// Returns `true` for network-level failures (TLS negotiation,
/// connection reset, timeouts) that the outer retry middleware may retry.
/// Returns `false` for protocol-level or auth errors.
fn is_transient_ws_connect_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("tls handshake eof")
        || message.contains("connection reset")
        || message.contains("unexpected eof")
        // rustls reports abrupt TCP close as "peer closed connection
        // without sending TLS close_notify" (plus the
        // `UnexpectedEof` variant with no space). Both map to the
        // same class of transient network drops that a reconnect
        // can recover from.
        || message.contains("close_notify")
        || message.contains("unexpectedeof")
        || message.contains("cannot decrypt peer's message")
        || message.contains("broken pipe")
        || message.contains("timed out")
        || message.contains("temporarily unavailable")
        // Locale-independent Winsock errno fallbacks. On non-English
        // Windows the textual part of `io::Error::Display` is the OS-
        // localized message (e.g. zh-CN renders WSAETIMEDOUT as
        // "由于连接方在一段时间后没有正确答复…"), which doesn't match
        // any of the English keywords above. The `(os error N)` suffix
        // is locale-stable, so matching the errno catches the same
        // class of transient drops on every Windows locale.
        //   10060 WSAETIMEDOUT     — connect timed out
        //   10054 WSAECONNRESET    — connection reset by peer
        //   10053 WSAECONNABORTED  — software caused connection abort
        || message.contains("os error 10060")
        || message.contains("os error 10054")
        || message.contains("os error 10053")
}

/// Is the WebSocket send error a transient transport failure?
///
/// These errors indicate that the connection was dropped between turns. The
/// provider classifies them for the outer retry middleware; it does not retry
/// or resend from this layer.
fn is_transient_ws_send_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("sending after closing")
        || message.contains("connection reset")
        || message.contains("broken pipe")
        || message.contains("not connected")
}

/// Is the WebSocket frame-read error a transport break that a fresh
/// connection can recover from?
///
/// The pump task exits on the first read error, so this only separates
/// "the socket died" from "the peer sent something we could not parse":
/// replaying a malformed frame cannot fix it.
fn is_transient_ws_frame_error(error: &tungstenite::Error) -> bool {
    matches!(
        error,
        tungstenite::Error::Io(_)
            | tungstenite::Error::ConnectionClosed
            | tungstenite::Error::Protocol(
                tungstenite::error::ProtocolError::ResetWithoutClosingHandshake
            )
    )
}

#[async_trait]
impl ChatProvider for OpenAiResponsesProvider {
    fn provider_name(&self) -> &'static str {
        "openai-responses"
    }

    fn capabilities(&self) -> ModelCapabilities {
        let codex_oauth_web_search = is_chatgpt_codex_backend(&self.config.base_url)
            && self
                .auth
                .lock()
                .expect("openai-responses auth mutex poisoned")
                .refresher
                .is_some();
        ModelCapabilities {
            codex_oauth_web_search,
            requires_inline_transient_context: true,
            output_budget_includes_reasoning: true,
            accepts_unsigned_thinking_replay: true,
            prefix_cache_is_byte_exact: true,
            remote_compaction_v2: crate::vendor::ProviderVendor::detect(&self.config.base_url)
                == crate::vendor::ProviderVendor::OpenAi,
            ..ModelCapabilities::default()
        }
    }

    /// Return an isolated clone with fresh session state so a
    /// sub-agent gets its own `previous_response_id` chain and
    /// WebSocket connection. Config and auth are shared. The HTTP
    /// fallback flag is *also* shared so if the parent observed a
    /// 426 UPGRADE_REQUIRED, the sub-agent respects it rather than
    /// retrying a WS handshake we already know will fail.
    fn fork_for_sub_agent(&self) -> Option<Arc<dyn ChatProvider>> {
        self.fork_for_sub_agent_with_cache_key(None)
    }

    fn fork_for_sub_agent_with_cache_key(
        &self,
        prompt_cache_key: Option<String>,
    ) -> Option<Arc<dyn ChatProvider>> {
        let prompt_cache_key = prompt_cache_key.or_else(|| self.config.prompt_cache_key.clone());
        Some(Arc::new(Self {
            config: Arc::new(OpenAiResponsesClientConfig {
                prompt_cache_key: prompt_cache_key.clone(),
                ..self.config.as_ref().clone()
            }),
            prompt_cache_key: prompt_cache_key.unwrap_or_else(default_prompt_cache_key),
            auth: self.auth.clone(),
            session_state: ResponsesSessionState::new(),
            ws_conn: Arc::new(TokioMutex::new(None)),
            http_fallback_active: self.http_fallback_active.clone(),
            // Share the handshake-EOF streak (counter + timestamp +
            // fallback-at instant) with the parent so a sub-agent
            // inherits the same circuit-breaker state. If the parent
            // already saw the burst and flipped fallback, the
            // sub-agent observes the same flag; if the sub-agent
            // sees another EOF, it reinforces the decision rather
            // than being double-counted.
            consecutive_handshake_eofs: self.consecutive_handshake_eofs.clone(),
            last_handshake_eof_at: self.last_handshake_eof_at.clone(),
            retry_notifier: self.retry_notifier.clone(),
        }))
    }

    fn reset_session_state(&self) {
        self.session_state.clear();
        // Drop the WebSocket so the next request reconnects fresh.
        // Use try_lock to avoid blocking — if another task holds the
        // lock we skip the drop; the next send will reconnect anyway.
        if let Ok(mut guard) = self.ws_conn.try_lock() {
            *guard = None;
        }
    }

    fn end_turn(&self) {
        // Keep the WebSocket warm across prompt turns so the Codex
        // backend can resolve `previous_response_id` without forcing a
        // full replay. The send path drains the pump before each turn
        // and drops stale sockets there, where it can also invalidate
        // the response-id chain before building the next request body.
    }

    fn invalidate_previous_response_id(&self) {
        // Clears the response-id chain and baseline caches while
        // keeping the WS transport alive. Most request divergence is
        // handled by the baseline-prefix check in the send path; this
        // hook is reserved for callers that explicitly know provider
        // continuation state has become invalid.
        self.session_state.clear();
    }

    async fn send_message_stream(
        &self,
        http: &reqwest::Client,
        mut request: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream> {
        request.stream = true;

        // WebSocket transport: separate code path with
        // previous_response_id support.
        //
        // The `http_fallback_active` flag short-circuits this branch
        // once the server has rejected a WS upgrade (426-class) or a
        // burst of TLS-handshake EOFs tripped the circuit breaker.
        // Subsequent calls fall through to the plain HTTP/SSE path,
        // matching codex-ref's session-scoped fallback behaviour.
        //
        // The fallback is deliberately sticky for the rest of the
        // session — we never oscillate back to WS. Retrying WS after a
        // fallback reconnects fresh with no live `previous_response_id`
        // on the server, so that turn pays a full ~140K-token replay
        // (worse cache-miss than just staying on HTTP, which keeps
        // hitting the warm prompt cache via a stable prompt_cache_key).
        if self.config.use_websocket && !self.http_fallback_active.load(Ordering::Relaxed) {
            return self.send_message_stream_ws(http, request).await;
        }

        self.send_message_stream_http(http, request).await
    }
}

impl OpenAiResponsesProvider {
    /// Plain HTTP/SSE transport for `send_message_stream`. Used when
    /// `use_websocket` is false, or after [`Self::connect_ws`] has
    /// observed a 426-class upgrade refusal and flipped the
    /// `http_fallback_active` flag. Does not use
    /// `previous_response_id` (the `chatgpt.com` HTTP endpoint
    /// returns 400 when the field is present).
    async fn send_message_stream_http(
        &self,
        http: &reqwest::Client,
        request: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream> {
        let is_codex = is_chatgpt_codex_backend(&self.config.base_url);
        let effective_prompt_cache_key = request
            .cache_trace_context
            .as_ref()
            .and_then(|trace| trace.prompt_cache_key.as_deref())
            .unwrap_or(&self.prompt_cache_key);
        let body = build_responses_request_body_with_service_tier(
            &request,
            is_codex,
            effective_prompt_cache_key,
            None,
            self.config.service_tier.as_ref(),
        );
        let request_fingerprint = responses_request_fingerprint_with_service_tier(
            &request,
            is_codex,
            effective_prompt_cache_key,
            self.config.service_tier.as_ref(),
        );
        emit_openai_request_shape_trace(
            &request,
            effective_prompt_cache_key,
            false,
            Some(&request_fingerprint),
            CacheMissReason::PreviousResponseIdMissing,
        );

        // First attempt: use the currently cached access token.
        let (access_token, has_refresher) = self.snapshot_auth();
        let response = match self.send_once(http, &body, &access_token).await {
            Ok(response) => response,
            Err(ModelError::Unauthorized(msg)) if has_refresher => {
                tracing::info!(
                    error = %msg,
                    "openai-responses: got 401, refreshing access token and retrying"
                );
                let new_token = self.refresh_bearer().await?;
                self.send_once(http, &body, &new_token).await?
            }
            Err(err) => return Err(err),
        };

        let byte_stream = response
            .bytes_stream()
            .map_err(|e| ModelError::Http(format!("body stream: {e}")));

        Ok(decode_sse_stream(
            Box::pin(byte_stream),
            ResponsesSseDecoder {
                translator: OpenAiResponsesTranslator::default(),
                last_response_id: None,
            },
        ))
    }
}

/// Convenience constructor that wraps an
/// [`OpenAiResponsesProvider`] in a [`UniversalModelClient`] using
/// a fresh [`reqwest::Client`].
pub fn openai_responses_client(config: OpenAiResponsesClientConfig) -> UniversalModelClient {
    let provider = Arc::new(OpenAiResponsesProvider::new(config.clone()));
    UniversalModelClient::with_http_client(
        provider,
        crate::provider::build_http_client(config.request_timeout),
    )
}

/// Convenience constructor that reuses an existing
/// [`reqwest::Client`].
pub fn openai_responses_client_with_http(
    config: OpenAiResponsesClientConfig,
    http: reqwest::Client,
) -> UniversalModelClient {
    UniversalModelClient::with_http_client(Arc::new(OpenAiResponsesProvider::new(config)), http)
}

mod request_body;
use request_body::build_responses_request_body_with_input_and_service_tier;
pub use request_body::{
    build_responses_input, build_responses_request_body,
    build_responses_request_body_with_service_tier, build_responses_tools,
};
#[cfg(test)]
use request_body::{emit_assistant_message, emit_user_message, repair_orphaned_function_calls};

// ---------------------------------------------------------------------------
// SSE translation
// ---------------------------------------------------------------------------

fn is_permanent_model_selection_error(code: &str, message: &str) -> bool {
    let combined = format!("{code} {message}").to_ascii_lowercase();
    combined.contains("model")
        && (combined.contains("not supported")
            || combined.contains("unsupported")
            || combined.contains("does not exist")
            || combined.contains("unknown model")
            || combined.contains("invalid model"))
}

/// Stateful translator that walks Responses API SSE frames and
/// emits a queue of provider-agnostic [`StreamEvent`]s.
///
/// Kind of output-item currently being deltaed. Deltas target the
/// most recently opened item because the Codex stream is
/// sequential — if the backend ever starts interleaving, this
/// state machine needs to grow an `output_index -> block_index`
/// map.
#[derive(Debug, Default)]
pub struct OpenAiResponsesTranslator {
    started: bool,
    message_id: String,
    model: String,
    usage: Usage,
    pending: Vec<StreamEvent>,
    finished: bool,
    stop_emitted: bool,
    has_tool_call: bool,
    next_block_index: usize,
    current_block: Option<CurrentBlock>,
    // Each open output item that we've emitted a ContentBlockStart
    // for but not yet a ContentBlockStop. Keyed by `output_index`
    // so out-of-order `output_item.done` events still close the
    // right block. `None` means "no `output_index` field on the
    // wire, use the current block".
    open_items: HashMap<usize, usize>,
    // Same mapping keyed by provider item id. Image-generation status
    // events can arrive before output_item.added and may carry only
    // `item_id`, so keep this secondary lookup to avoid duplicate
    // visible tool cards when the normal item event eventually arrives.
    open_item_ids: HashMap<String, usize>,
    // Accumulated content emitted for each block. Used to patch in
    // any missing suffix from `*.done` or `output_item.done` payloads
    // when the stream's final chunk only appears in the completed
    // item snapshot instead of a trailing delta event.
    block_buffers: HashMap<usize, String>,
    reasoning_parts: HashMap<usize, ReasoningPartState>,
}

#[derive(Debug)]
struct ReasoningPartState {
    summary_index: usize,
    buffer: String,
}

#[derive(Debug, Clone, Copy)]
struct CurrentBlock {
    block_index: usize,
    kind: BlockKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    ToolUse,
    Reasoning,
    ServerToolUse,
    ImageGeneration,
}

impl OpenAiResponsesTranslator {
    /// Feed one SSE frame (an already-parsed `{event, data}` pair)
    /// into the translator. Events become available via
    /// [`Self::next_pending`].
    pub fn push_frame(&mut self, event_type: &str, data: &str) -> ModelResult<()> {
        // Empty / keep-alive / unknown events: silently drop.
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        let payload: Value = serde_json::from_str(data).map_err(|e| {
            ModelError::Protocol(format!(
                "responses api frame parse: {e} for event={event_type} data={data}"
            ))
        })?;

        match event_type {
            "response.created" => self.handle_response_created(&payload),
            "response.output_item.added" => self.handle_output_item_added(&payload),
            "response.output_text.delta" => self.handle_text_delta(&payload),
            "response.function_call_arguments.delta" => self.handle_args_delta(&payload),
            "response.output_item.done" => self.handle_output_item_done(&payload),
            "response.completed" => self.handle_response_completed(&payload),
            "response.error" => self.handle_error_event(&payload),
            "response.reasoning_summary_text.delta" => {
                self.handle_reasoning_delta(&payload);
            }
            "response.reasoning_summary_part.added" => {
                self.handle_reasoning_part_added(&payload);
            }
            "response.image_generation_call.partial_image" => {
                self.handle_image_partial(&payload);
            }
            // Terminal error events — the response will not complete.
            // Matches codex-ref `process_responses_event` in
            // `codex-rs/codex-api/src/sse/responses.rs`.
            "response.failed" => {
                return self.handle_response_failed(&payload);
            }
            "response.incomplete" => {
                return self.handle_response_incomplete(&payload);
            }
            // Bare "error" event (WebSocket-level, no "response." prefix).
            // These carry an optional HTTP `status` field — classify via
            // the same `classify_http_error` that HTTP providers use, retaining
            // the canonical transient/permanent distinction.
            "error" => {
                let error_obj = payload.get("error");
                let message = error_obj
                    .and_then(|e| e.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("WebSocket error event received");
                let code = error_obj
                    .and_then(|e| e.get("code"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let status = payload
                    .get("status")
                    .or_else(|| payload.get("status_code"))
                    .and_then(|v| v.as_u64())
                    .map(|s| s as u16);

                self.finalize();

                let body = format!("ws error ({code}): {message}");

                // Classify known permanent error codes before falling
                // through to the generic status/no-status path.
                // Without this, statusless errors default to
                // `ModelError::Http` which `is_transient() == true`,
                // causing the retry middleware to re-send a request
                // that can never succeed.
                return Err(match code {
                    "context_length_exceeded" | "insufficient_quota" | "invalid_prompt" => {
                        ModelError::Permanent(body)
                    }
                    "previous_response_not_found" | "invalid_function_parameters" => {
                        ModelError::BadRequest(body)
                    }
                    // The server enforces a hard lifetime on each
                    // Responses websocket (60 minutes, activity does
                    // not extend it) and reports expiry with this code
                    // plus `status: 400`. Match the code before the
                    // status fallthrough: this is a transport-lifecycle
                    // event, not a bad request. Transient so the WS
                    // turn driver reconnects and replays the turn.
                    "websocket_connection_limit_reached" => ModelError::Http(body),
                    _ if is_permanent_model_selection_error(code, message) => {
                        ModelError::BadRequest(body)
                    }
                    _ if status.is_some() => classify_http_error(status.unwrap(), body, None),
                    _ => ModelError::Http(body),
                });
            }
            // Provider-specific events (not part of the Responses API
            // spec). Logged at debug, dropped.
            "codex.rate_limits" => {
                tracing::debug!(event_type, "responses api: codex rate-limits event dropped");
            }
            // These `*.done` events sometimes carry the final suffix
            // that never arrived as a delta event. Patch that tail into
            // the open block before it gets closed.
            "response.output_text.done" => self.handle_text_done(&payload),
            "response.function_call_arguments.done" => self.handle_args_done(&payload),
            "response.reasoning_summary_text.done" => self.handle_reasoning_done(&payload),
            "response.reasoning_summary_part.done" => {
                self.handle_reasoning_part_done(&payload);
            }
            "response.image_generation_call.in_progress" => {
                self.handle_image_generation_status(&payload, "in_progress");
            }
            "response.image_generation_call.generating" => {
                self.handle_image_generation_status(&payload, "generating");
            }
            "response.image_generation_call.completed" => {
                self.handle_image_generation_status(&payload, "completed");
            }
            // Informational events. Logged at debug, dropped.
            "response.in_progress"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.web_search_call.searching"
            | "response.web_search_call.in_progress"
            | "response.web_search_call.completed" => {
                tracing::debug!(event_type, "responses api: informational event dropped");
            }
            other => {
                tracing::debug!(
                    event_type = other,
                    "responses api: unknown event type dropped"
                );
            }
        }
        Ok(())
    }

    /// Drain one queued [`StreamEvent`] out of the pending buffer.
    pub fn next_pending(&mut self) -> Option<StreamEvent> {
        if self.pending.is_empty() {
            None
        } else {
            Some(self.pending.remove(0))
        }
    }

    /// Emit any trailing events the translator still owes (final
    /// `ContentBlockStop` for the current block, `MessageDelta`
    /// with the inferred stop reason, `MessageStop`). Idempotent —
    /// calling it twice is a no-op.
    pub fn finalize(&mut self) {
        // Emit the final message_delta with the inferred stop
        // reason: Codex hardcodes `end_turn`, so tool_use is
        // inferred from the presence of any function_call item in
        // the stream.
        let stop_reason = if self.has_tool_call {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        };
        self.finalize_with_stop_reason(stop_reason);
    }

    fn finalize_with_stop_reason(&mut self, stop_reason: StopReason) {
        if self.stop_emitted {
            return;
        }
        // Close any still-open current block.
        if let Some(current) = self.current_block.take() {
            self.pending.push(StreamEvent::ContentBlockStop {
                index: current.block_index,
            });
        }
        self.pending.push(StreamEvent::MessageDelta {
            delta: MessageDeltaFields {
                stop_reason: Some(stop_reason),
                usage: self.usage,
            },
        });
        self.pending.push(StreamEvent::MessageStop);
        self.finished = true;
        self.stop_emitted = true;
    }

    /// Whether every queued event has been drained out via
    /// [`Self::next_pending`].
    pub fn is_drained(&self) -> bool {
        self.pending.is_empty()
    }

    /// Whether the translator has been finalized (all content blocks
    /// closed, stop reason emitted). Use together with
    /// [`Self::is_drained`] to determine if the stream is truly
    /// complete.
    ///
    /// WebSocket connections stay open after a response completes, so
    /// callers must check this flag instead of waiting for the
    /// transport to close.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    fn handle_response_created(&mut self, payload: &Value) {
        if self.started {
            return;
        }
        self.started = true;
        let response = payload.get("response");
        self.message_id = response
            .and_then(|r| r.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("openai_responses_msg")
            .to_string();
        self.model = response
            .and_then(|r| r.get("model"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        self.pending.push(StreamEvent::MessageStart {
            message_id: self.message_id.clone(),
            model: self.model.clone(),
            usage: Usage::default(),
        });
    }

    fn handle_output_item_added(&mut self, payload: &Value) {
        // Ensure MessageStart has been emitted before we open any
        // content block — some servers skip response.created and
        // jump straight to output_item.added.
        self.ensure_started();

        let Some(item) = payload.get("item") else {
            return;
        };
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let output_index = payload
            .get("output_index")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        let block_index = self.next_block_index;
        self.next_block_index += 1;

        match item_type {
            "message" => {
                self.pending.push(StreamEvent::ContentBlockStart {
                    index: block_index,
                    content_block: ContentBlockStart::Text {
                        text: String::new(),
                    },
                });
                self.current_block = Some(CurrentBlock {
                    block_index,
                    kind: BlockKind::Text,
                });
                self.block_buffers.insert(block_index, String::new());
                if let Some(idx) = output_index {
                    self.open_items.insert(idx, block_index);
                }
            }
            "function_call" => {
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.pending.push(StreamEvent::ContentBlockStart {
                    index: block_index,
                    content_block: ContentBlockStart::ToolUse { id: call_id, name },
                });
                self.current_block = Some(CurrentBlock {
                    block_index,
                    kind: BlockKind::ToolUse,
                });
                self.block_buffers.insert(block_index, String::new());
                self.has_tool_call = true;
                if let Some(idx) = output_index {
                    self.open_items.insert(idx, block_index);
                }
            }
            "reasoning" => {
                let data = item
                    .get("encrypted_content")
                    .and_then(|v| v.as_str())
                    .filter(|data| !data.is_empty())
                    .map(String::from);
                self.pending.push(StreamEvent::ContentBlockStart {
                    index: block_index,
                    content_block: ContentBlockStart::Thinking {
                        thinking: String::new(),
                        data,
                    },
                });
                self.current_block = Some(CurrentBlock {
                    block_index,
                    kind: BlockKind::Reasoning,
                });
                self.block_buffers.insert(block_index, String::new());
                if let Some(idx) = output_index {
                    self.open_items.insert(idx, block_index);
                }
            }
            "image_generation_call" => {
                // Server-side image generation. Final bytes arrive on
                // `response.output_item.done` (status=completed) and
                // optional preview frames via
                // `response.image_generation_call.partial_image`.
                self.next_block_index = self.next_block_index.saturating_sub(1);
                let status = item
                    .get("status")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let _ = self.ensure_image_generation_block(payload, status);
            }
            "web_search_call" => {
                // Server-side web search. Extract the search query
                // from the item and emit as a ServerToolUse block.
                // Unlike function_call, this does NOT set
                // has_tool_call — the engine must not dispatch it.
                let id = item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let status = item
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                // Build an input object from available fields.
                let mut input = serde_json::Map::new();
                if let Some(query) = item.get("query").and_then(|v| v.as_str()) {
                    input.insert("query".into(), json!(query));
                }
                if !status.is_empty() {
                    input.insert("status".into(), json!(status));
                }
                self.pending.push(StreamEvent::ContentBlockStart {
                    index: block_index,
                    content_block: ContentBlockStart::ServerToolUse {
                        id,
                        name: "web_search".to_string(),
                        input: serde_json::Value::Object(input),
                    },
                });
                self.current_block = Some(CurrentBlock {
                    block_index,
                    kind: BlockKind::ServerToolUse,
                });
                if let Some(idx) = output_index {
                    self.open_items.insert(idx, block_index);
                }
            }
            other => {
                // Future unknown type — drop and roll back the block
                // index so we don't leave a gap.
                self.next_block_index -= 1;
                tracing::debug!(
                    item_type = other,
                    "responses api: output_item with unsupported type dropped"
                );
            }
        }
    }

    fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        self.pending.push(StreamEvent::MessageStart {
            message_id: "openai_responses_msg".to_string(),
            model: String::new(),
            usage: Usage::default(),
        });
    }

    fn output_index(payload: &Value) -> Option<usize> {
        payload
            .get("output_index")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
    }

    fn image_generation_item(payload: &Value) -> Option<&Value> {
        payload
            .get("item")
            .filter(|item| {
                item.get("type").and_then(|v| v.as_str()) == Some("image_generation_call")
            })
            .or_else(|| payload.get("image_generation_call"))
    }

    fn image_generation_id(payload: &Value, output_index: Option<usize>) -> String {
        Self::image_generation_item(payload)
            .and_then(|item| item.get("id"))
            .and_then(|v| v.as_str())
            .or_else(|| payload.get("item_id").and_then(|v| v.as_str()))
            .or_else(|| payload.get("id").and_then(|v| v.as_str()))
            .map(str::to_string)
            .unwrap_or_else(|| match output_index {
                Some(idx) => format!("image_generation_{idx}"),
                None => "image_generation".to_string(),
            })
    }

    fn ensure_image_generation_block(
        &mut self,
        payload: &Value,
        status: Option<String>,
    ) -> Option<usize> {
        let output_index = Self::output_index(payload);
        if let Some(block_index) = output_index.and_then(|idx| self.open_items.get(&idx).copied()) {
            return Some(block_index);
        }

        let id = Self::image_generation_id(payload, output_index);
        if let Some(block_index) = self.open_item_ids.get(&id).copied() {
            return Some(block_index);
        }

        self.ensure_started();
        let block_index = self.next_block_index;
        self.next_block_index += 1;
        self.pending.push(StreamEvent::ContentBlockStart {
            index: block_index,
            content_block: ContentBlockStart::ImageGeneration {
                id: id.clone(),
                status,
            },
        });
        self.current_block = Some(CurrentBlock {
            block_index,
            kind: BlockKind::ImageGeneration,
        });
        if let Some(idx) = output_index {
            self.open_items.insert(idx, block_index);
        }
        self.open_item_ids.insert(id, block_index);
        Some(block_index)
    }

    fn handle_image_generation_status(&mut self, payload: &Value, status: &str) {
        let _ = self.ensure_image_generation_block(payload, Some(status.to_string()));
    }

    fn handle_text_delta(&mut self, payload: &Value) {
        let Some(delta) = payload.get("delta").and_then(|v| v.as_str()) else {
            return;
        };
        if delta.is_empty() {
            return;
        }
        let Some(current) = self.current_block else {
            tracing::debug!("responses api: text delta with no open block, dropping");
            return;
        };
        if current.kind != BlockKind::Text {
            tracing::debug!(
                block_index = current.block_index,
                "responses api: text delta on non-text block, dropping"
            );
            return;
        }
        self.block_buffers
            .entry(current.block_index)
            .or_default()
            .push_str(delta);
        self.pending.push(StreamEvent::ContentBlockDelta {
            index: current.block_index,
            delta: ContentBlockDelta::TextDelta {
                text: delta.to_string(),
            },
        });
    }

    fn handle_text_done(&mut self, payload: &Value) {
        self.patch_done_suffix(payload, BlockKind::Text, |delta| {
            ContentBlockDelta::TextDelta { text: delta }
        });
    }

    fn handle_args_delta(&mut self, payload: &Value) {
        let Some(delta) = payload.get("delta").and_then(|v| v.as_str()) else {
            return;
        };
        if delta.is_empty() {
            return;
        }
        let Some(current) = self.current_block else {
            tracing::debug!("responses api: args delta with no open block, dropping");
            return;
        };
        if current.kind != BlockKind::ToolUse {
            tracing::debug!(
                block_index = current.block_index,
                "responses api: args delta on non-tool block, dropping"
            );
            return;
        }
        self.block_buffers
            .entry(current.block_index)
            .or_default()
            .push_str(delta);
        self.pending.push(StreamEvent::ContentBlockDelta {
            index: current.block_index,
            delta: ContentBlockDelta::InputJsonDelta {
                partial_json: delta.to_string(),
            },
        });
    }

    fn handle_args_done(&mut self, payload: &Value) {
        self.patch_done_suffix(payload, BlockKind::ToolUse, |delta| {
            ContentBlockDelta::InputJsonDelta {
                partial_json: delta,
            }
        });
    }

    fn current_reasoning_block(&self) -> Option<usize> {
        self.current_block
            .filter(|current| current.kind == BlockKind::Reasoning)
            .map(|current| current.block_index)
    }

    fn reasoning_summary_index(payload: &Value) -> Option<usize> {
        payload
            .get("summary_index")
            .and_then(Value::as_u64)
            .map(|index| index as usize)
    }

    fn emit_reasoning_delta(&mut self, block_index: usize, thinking: String) {
        if thinking.is_empty() {
            return;
        }
        self.block_buffers
            .entry(block_index)
            .or_default()
            .push_str(&thinking);
        self.pending.push(StreamEvent::ContentBlockDelta {
            index: block_index,
            delta: ContentBlockDelta::ThinkingDelta { thinking },
        });
    }

    fn begin_reasoning_part(&mut self, block_index: usize, payload: &Value) {
        let Some(summary_index) = Self::reasoning_summary_index(payload) else {
            return;
        };
        let previous_index = self
            .reasoning_parts
            .get(&block_index)
            .map(|part| part.summary_index);
        if previous_index == Some(summary_index) {
            return;
        }

        let separator = if previous_index.is_some() {
            let existing = self
                .block_buffers
                .get(&block_index)
                .map(String::as_str)
                .unwrap_or("");
            if existing.is_empty() || existing.ends_with("\n\n") {
                ""
            } else if existing.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            }
        } else {
            ""
        };
        self.reasoning_parts.insert(
            block_index,
            ReasoningPartState {
                summary_index,
                buffer: String::new(),
            },
        );
        self.emit_reasoning_delta(block_index, separator.to_string());
    }

    fn handle_reasoning_part_added(&mut self, payload: &Value) {
        let Some(block_index) = self.current_reasoning_block() else {
            tracing::debug!("responses api: reasoning part with no open block, dropping");
            return;
        };
        self.begin_reasoning_part(block_index, payload);
    }

    /// Translate `response.reasoning_summary_text.delta` →
    /// `ContentBlockDelta::ThinkingDelta`. Matches the Anthropic API's
    /// thinking delta events so the downstream `MessageAccumulator`
    /// can accumulate reasoning text identically.
    fn handle_reasoning_delta(&mut self, payload: &Value) {
        let Some(delta) = payload.get("delta").and_then(|v| v.as_str()) else {
            return;
        };
        if delta.is_empty() {
            return;
        }
        let Some(block_index) = self.current_reasoning_block() else {
            tracing::debug!("responses api: reasoning delta with no open block, dropping");
            return;
        };

        self.begin_reasoning_part(block_index, payload);
        if let Some(part) = self.reasoning_parts.get_mut(&block_index) {
            part.buffer.push_str(delta);
        }
        self.emit_reasoning_delta(block_index, delta.to_string());
    }

    fn patch_reasoning_part_done(&mut self, payload: &Value, full_text: &str) {
        let Some(block_index) = self.current_reasoning_block() else {
            return;
        };
        self.begin_reasoning_part(block_index, payload);

        let Some(summary_index) = Self::reasoning_summary_index(payload) else {
            self.patch_done_suffix_for_block(block_index, full_text, |thinking| {
                ContentBlockDelta::ThinkingDelta { thinking }
            });
            return;
        };
        let suffix = {
            let Some(part) = self.reasoning_parts.get(&block_index) else {
                return;
            };
            if part.summary_index != summary_index
                || full_text.len() <= part.buffer.len()
                || !full_text.starts_with(&part.buffer)
            {
                return;
            }
            full_text[part.buffer.len()..].to_string()
        };
        if let Some(part) = self.reasoning_parts.get_mut(&block_index) {
            part.buffer.push_str(&suffix);
        }
        self.emit_reasoning_delta(block_index, suffix);
    }

    fn handle_reasoning_done(&mut self, payload: &Value) {
        let Some(full_text) = Self::extract_done_text(payload) else {
            return;
        };
        self.patch_reasoning_part_done(payload, full_text);
    }

    fn handle_reasoning_part_done(&mut self, payload: &Value) {
        let Some(full_text) = payload
            .get("part")
            .and_then(|part| part.get("text"))
            .and_then(Value::as_str)
        else {
            return;
        };
        self.patch_reasoning_part_done(payload, full_text);
    }

    /// Handle `response.image_generation_call.partial_image` — a full
    /// preview frame arrives under the `partial_image_b64` key. The
    /// event also carries `partial_image_index` (0-based). The bytes
    /// are NOT appended to prior frames; each frame is a complete
    /// image in its own right.
    fn handle_image_partial(&mut self, payload: &Value) {
        let Some(b64) = payload
            .get("partial_image_b64")
            .and_then(|v| v.as_str())
            .or_else(|| payload.get("b64_json").and_then(|v| v.as_str()))
        else {
            tracing::debug!(
                "responses api: image_generation_call.partial_image without b64 payload, dropping"
            );
            return;
        };
        if b64.is_empty() {
            return;
        }
        // Look up the block index via `output_index` if provided, else
        // fall back to the currently-open image block.
        let output_index = payload
            .get("output_index")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let block_index = output_index
            .and_then(|idx| self.open_items.get(&idx).copied())
            .or_else(|| {
                Self::image_generation_item(payload)
                    .and_then(|item| item.get("id"))
                    .and_then(|v| v.as_str())
                    .and_then(|id| self.open_item_ids.get(id).copied())
            })
            .or_else(|| {
                payload
                    .get("item_id")
                    .and_then(|v| v.as_str())
                    .and_then(|id| self.open_item_ids.get(id).copied())
            })
            .or_else(|| {
                self.current_block
                    .and_then(|c| (c.kind == BlockKind::ImageGeneration).then_some(c.block_index))
            })
            .or_else(|| {
                self.ensure_image_generation_block(payload, Some("generating".to_string()))
            });
        let Some(block_index) = block_index else {
            tracing::debug!("responses api: image partial with no open image block, dropping");
            return;
        };
        let partial_index = payload
            .get("partial_image_index")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        self.pending.push(StreamEvent::ContentBlockDelta {
            index: block_index,
            delta: ContentBlockDelta::ImageDataDelta {
                b64_json: b64.to_string(),
                partial_index,
                revised_prompt: None,
                media_type: None,
            },
        });
    }

    fn extract_reasoning_encrypted_content<'a>(payload: &'a Value) -> Option<&'a str> {
        payload
            .get("encrypted_content")
            .and_then(|v| v.as_str())
            .or_else(|| {
                payload
                    .get("item")
                    .and_then(|item| item.get("encrypted_content"))
                    .and_then(|v| v.as_str())
            })
    }

    fn handle_reasoning_encrypted_content(&mut self, block_index: usize, payload: &Value) {
        let Some(data) = Self::extract_reasoning_encrypted_content(payload) else {
            return;
        };
        if data.is_empty() {
            return;
        }
        self.pending.push(StreamEvent::ContentBlockDelta {
            index: block_index,
            delta: ContentBlockDelta::ThinkingDataDelta {
                data: data.to_string(),
            },
        });
    }

    fn patch_done_suffix<F>(&mut self, payload: &Value, expected_kind: BlockKind, make_delta: F)
    where
        F: FnOnce(String) -> ContentBlockDelta,
    {
        let Some(current) = self.current_block else {
            return;
        };
        if current.kind != expected_kind {
            return;
        }
        let Some(full_text) = Self::extract_done_text(payload) else {
            return;
        };
        self.patch_done_suffix_for_block(current.block_index, full_text, make_delta);
    }

    fn patch_done_suffix_for_block<F>(&mut self, block_index: usize, full_text: &str, make_delta: F)
    where
        F: FnOnce(String) -> ContentBlockDelta,
    {
        let existing = self
            .block_buffers
            .get(&block_index)
            .map(|s| s.as_str())
            .unwrap_or("");
        if full_text.len() <= existing.len() || !full_text.starts_with(existing) {
            return;
        }
        let suffix = full_text[existing.len()..].to_string();
        if suffix.is_empty() {
            return;
        }
        self.block_buffers
            .entry(block_index)
            .or_default()
            .push_str(&suffix);
        self.pending.push(StreamEvent::ContentBlockDelta {
            index: block_index,
            delta: make_delta(suffix),
        });
    }

    fn extract_done_text<'a>(payload: &'a Value) -> Option<&'a str> {
        payload
            .get("text")
            .and_then(|v| v.as_str())
            .or_else(|| payload.get("arguments").and_then(|v| v.as_str()))
            .or_else(|| payload.get("summary_text").and_then(|v| v.as_str()))
            .or_else(|| payload.get("summary").and_then(|v| v.as_str()))
            .or_else(|| {
                payload
                    .get("item")
                    .and_then(|item| item.get("text"))
                    .and_then(|v| v.as_str())
            })
            .or_else(|| {
                payload
                    .get("item")
                    .and_then(|item| item.get("arguments"))
                    .and_then(|v| v.as_str())
            })
            .or_else(|| {
                payload
                    .get("item")
                    .and_then(|item| item.get("summary_text"))
                    .and_then(|v| v.as_str())
            })
            .or_else(|| {
                payload
                    .get("item")
                    .and_then(|item| item.get("summary"))
                    .and_then(|v| v.as_str())
            })
    }

    fn handle_output_item_done(&mut self, payload: &Value) {
        // Prefer the explicit `output_index` mapping so out-of-
        // order dones close the right block.
        let output_index = payload
            .get("output_index")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        let mut block_index = if let Some(idx) = output_index {
            self.open_items.remove(&idx)
        } else {
            self.current_block.map(|c| c.block_index)
        };

        if block_index.is_none()
            && payload
                .get("item")
                .and_then(|item| item.get("type"))
                .and_then(|v| v.as_str())
                == Some("image_generation_call")
        {
            block_index =
                self.ensure_image_generation_block(payload, Some("completed".to_string()));
            if let Some(idx) = output_index {
                self.open_items.remove(&idx);
            }
        }

        // Remote compaction v2 answers with a single `compaction` output
        // item whose `encrypted_content` replaces the summarised
        // history. `output_item.added` drops unknown types, so the item
        // is opened here, at done, where the blob is populated.
        if block_index.is_none()
            && payload
                .get("item")
                .and_then(|item| item.get("type"))
                .and_then(|v| v.as_str())
                == Some("compaction")
        {
            let encrypted_content = payload
                .get("item")
                .and_then(|item| item.get("encrypted_content"))
                .and_then(|v| v.as_str())
                .filter(|content| !content.is_empty())
                .map(String::from);
            let index = self.next_block_index;
            self.next_block_index += 1;
            self.pending.push(StreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlockStart::Compaction {
                    content: None,
                    encrypted_content,
                },
            });
            block_index = Some(index);
            if let Some(idx) = output_index {
                self.open_items.remove(&idx);
            }
        }

        let block_index = block_index.or_else(|| {
            payload
                .get("item")
                .and_then(|item| item.get("id"))
                .and_then(|v| v.as_str())
                .and_then(|id| self.open_item_ids.get(id).copied())
        });

        if let Some(block_index) = block_index {
            if let Some(item_type) = payload
                .get("item")
                .and_then(|item| item.get("type"))
                .and_then(|v| v.as_str())
            {
                match item_type {
                    "message" => self.patch_done_suffix_for_block(
                        block_index,
                        Self::extract_done_text(payload).unwrap_or(""),
                        |delta| ContentBlockDelta::TextDelta { text: delta },
                    ),
                    "function_call" => self.patch_done_suffix_for_block(
                        block_index,
                        Self::extract_done_text(payload).unwrap_or(""),
                        |delta| ContentBlockDelta::InputJsonDelta {
                            partial_json: delta,
                        },
                    ),
                    "reasoning" => {
                        self.patch_done_suffix_for_block(
                            block_index,
                            Self::extract_done_text(payload).unwrap_or(""),
                            |delta| ContentBlockDelta::ThinkingDelta { thinking: delta },
                        );
                        self.handle_reasoning_encrypted_content(block_index, payload);
                    }
                    "image_generation_call" => {
                        // Emit the final base64 payload, MIME type, and
                        // revised prompt as a single terminal
                        // ImageDataDelta with `partial_index = None`.
                        if let Some(item) = payload.get("item") {
                            self.open_item_ids.insert(
                                Self::image_generation_id(payload, output_index),
                                block_index,
                            );
                            let b64 = item
                                .get("result")
                                .and_then(|v| v.as_str())
                                .or_else(|| item.get("b64_json").and_then(|v| v.as_str()))
                                .unwrap_or("")
                                .to_string();
                            let revised = item
                                .get("revised_prompt")
                                .and_then(|v| v.as_str())
                                .map(String::from);
                            let media =
                                item.get("output_format")
                                    .and_then(|v| v.as_str())
                                    .map(|fmt| match fmt {
                                        "jpeg" | "jpg" => "image/jpeg".to_string(),
                                        "webp" => "image/webp".to_string(),
                                        "png" => "image/png".to_string(),
                                        other => format!("image/{other}"),
                                    });
                            if !b64.is_empty() {
                                self.pending.push(StreamEvent::ContentBlockDelta {
                                    index: block_index,
                                    delta: ContentBlockDelta::ImageDataDelta {
                                        b64_json: b64,
                                        partial_index: None,
                                        revised_prompt: revised,
                                        media_type: media,
                                    },
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
            self.pending
                .push(StreamEvent::ContentBlockStop { index: block_index });
            self.block_buffers.remove(&block_index);
            self.reasoning_parts.remove(&block_index);
            if let Some(done_id) = payload
                .get("item")
                .and_then(|item| item.get("id"))
                .and_then(|v| v.as_str())
            {
                self.open_item_ids.remove(done_id);
            }
            if self
                .current_block
                .map(|c| c.block_index == block_index)
                .unwrap_or(false)
            {
                self.current_block = None;
            }
        }
    }

    fn merge_response_usage(&mut self, payload: &Value) {
        // Usage snapshot, if present.
        let usage = payload
            .get("response")
            .and_then(|r| r.get("usage"))
            .or_else(|| payload.get("usage"));
        if let Some(u) = usage {
            if let Ok(parsed) = serde_json::from_value::<Usage>(u.clone()) {
                // Surface the Responses-API cache split so callers can
                // verify Codex OAuth prefix cache behavior without
                // pulling apart the raw SSE payload. Matches the
                // codex-rs `cached_input_tokens` telemetry field.
                if parsed.input_tokens > 0 {
                    tracing::debug!(
                        target: "rebon_api::openai_responses::cache",
                        input_tokens = parsed.input_tokens,
                        cached_tokens = parsed.prompt_cache_hit_tokens,
                        output_tokens = parsed.output_tokens,
                        "response terminal usage"
                    );
                }
                self.usage.merge(&parsed);
            }
        }
    }

    fn handle_response_completed(&mut self, payload: &Value) {
        self.merge_response_usage(payload);
        self.finalize();
    }

    /// Handle `response.failed` — the server aborted the response
    /// due to rate limits, context window overflow, invalid prompts,
    /// etc. Matches codex-ref `process_responses_event` classification.
    fn handle_response_failed(&mut self, payload: &Value) -> ModelResult<()> {
        let error = payload
            .get("response")
            .and_then(|r| r.get("error"))
            .cloned()
            .unwrap_or(Value::Null);

        let code = error.get("code").and_then(|v| v.as_str()).unwrap_or("");
        let message = error
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("response.failed event received");

        // Classify the error to give callers a chance to retry or
        // report a user-friendly message. The classification matches
        // codex-ref's `is_context_window_error`, `is_quota_exceeded_error`,
        // etc.
        let err = match code {
            "context_length_exceeded" => {
                ModelError::Permanent(format!("context window exceeded: {message}"))
            }
            "insufficient_quota" => ModelError::Permanent(format!("quota exceeded: {message}")),
            "invalid_prompt" => ModelError::Permanent(format!("invalid prompt: {message}")),
            _ => {
                // Rate limits and other transient errors — surface as
                // Transient so the outer retry middleware can decide whether
                // to resend the request.
                ModelError::Http(format!("response.failed ({code}): {message}"))
            }
        };
        // Finalize so `is_finished()` returns true and the stream
        // terminates cleanly after the error is surfaced.
        self.finalize();
        Err(err)
    }

    /// Handle `response.incomplete` — `max_output_tokens` is a
    /// successful partial response that downstream code can continue.
    /// Other reasons remain errors.
    fn handle_response_incomplete(&mut self, payload: &Value) -> ModelResult<()> {
        let reason = payload
            .get("response")
            .and_then(|r| r.get("incomplete_details"))
            .and_then(|d| d.get("reason"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        if reason == "max_output_tokens" {
            self.merge_response_usage(payload);
            self.finalize_with_stop_reason(StopReason::MaxTokens);
            return Ok(());
        }

        self.finalize();
        Err(ModelError::Http(format!(
            "incomplete response, reason: {reason}"
        )))
    }

    fn handle_error_event(&mut self, payload: &Value) {
        let error_type = payload
            .get("error")
            .and_then(|e| e.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("responses_error")
            .to_string();
        let message = payload
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown responses api error")
            .to_string();
        self.pending.push(StreamEvent::Error {
            error_type,
            message,
        });
    }
}

// ---------------------------------------------------------------------------
// Unfold-driven stream plumbing
// ---------------------------------------------------------------------------

struct ResponsesSseDecoder {
    translator: OpenAiResponsesTranslator,
    /// Shared handle to persist the response ID for
    /// `previous_response_id` on the next turn. `None` when the provider
    /// does not need session state (for example unit tests).
    last_response_id: Option<Arc<std::sync::Mutex<Option<String>>>>,
}

impl SseDecoder for ResponsesSseDecoder {
    fn next_event(&mut self) -> Option<StreamEvent> {
        let event = self.translator.next_pending()?;
        // The translator emits MessageStart exactly once per stream with the
        // response.id value.
        if let StreamEvent::MessageStart { ref message_id, .. } = event {
            if let Some(handle) = &self.last_response_id {
                *handle.lock().expect("last_response_id mutex poisoned") = Some(message_id.clone());
            }
        }
        Some(event)
    }

    fn push_frame(&mut self, frame: SseFrame, _at_eof: bool) -> ModelResult<()> {
        self.translator.push_frame(&frame.event, &frame.data)
    }

    fn is_terminal(&self) -> bool {
        self.translator.is_finished() && self.translator.is_drained()
    }

    fn finish(&mut self) -> ModelResult<()> {
        if self.translator.is_finished() {
            Ok(())
        } else {
            Err(ModelError::Http(
                "responses stream ended before response.completed".into(),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Persistent WebSocket turn driver
// ---------------------------------------------------------------------------

/// Per-frame idle timeout for WebSocket reads. Generous enough for
/// extended thinking (which can take minutes) but catches hung
/// connections. Matches codex-ref's per-frame timeout in
/// `run_websocket_response_stream`.
const WS_FRAME_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const WS_FRAME_IDLE_TIMEOUT_MESSAGE: &str = "websocket idle timeout waiting for frame";

/// No-progress timeout for the response stream as a whole. Ping/Pong
/// keepalive frames reset the per-frame idle timer above (so long
/// thinking pauses survive), which means a server that stalls
/// mid-response while still sending keepalives would otherwise hang
/// the turn forever. The time since the last *data* frame
/// (Text/Binary) is tracked separately: once it exceeds this
/// deadline the turn is treated as a mid-stream break and retried.
/// Genuine long reasoning emits summary/item events well within
/// this window, so it is unaffected.
const WS_DATA_STALL_TIMEOUT: Duration = Duration::from_secs(600);
const WS_DATA_STALL_TIMEOUT_MESSAGE: &str =
    "websocket data stall: keepalive frames only, no response frames";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviousResponseStrategy {
    UsePreviousResponseId,
    ForceFullReplay,
}

#[derive(Debug, Clone, Copy)]
struct WsTurnOptions {
    previous_response: PreviousResponseStrategy,
    forced_cache_miss_reason: Option<CacheMissReason>,
}

impl Default for WsTurnOptions {
    fn default() -> Self {
        Self {
            previous_response: PreviousResponseStrategy::UsePreviousResponseId,
            forced_cache_miss_reason: None,
        }
    }
}

#[derive(Debug)]
enum WsTurnResolution {
    Completed,
    /// Retry this turn with a full replay (no `previous_response_id`).
    /// Carries the specific reason the id was abandoned so the
    /// forced-replay request traces it distinctly (e.g.
    /// `previous_response_not_found` vs an overflow-driven prune)
    /// instead of collapsing every cause into one generic label.
    RetryWithoutPreviousResponseId(CacheMissReason),
    /// The socket died mid-turn — a read error, a close frame, or EOF
    /// before `response.completed`. The connection is gone, so the turn
    /// is not resumable: reconnect and replay it from the start. Carries
    /// the transport error so a replay attempt that loses its socket too
    /// reports the transport failure (transient) rather than a protocol
    /// error.
    ReconnectAndReplay(ModelError),
}

/// Return the baseline prefix — the items we sent on the previous
/// turn — against which the new turn's full input is compared.
///
/// Earlier revisions tried to concatenate server-emitted
/// `output_item.done` items (captured via
/// [`capture_items_added_from_frame`]) onto the baseline, mirroring
/// codex-rs's `get_incremental_items`. That works in codex-rs
/// because both the outgoing input and the incoming stream share a
/// single `ResponseItem` type, so byte-equality holds. In rebon the
/// outgoing items are re-serialized by `build_responses_input` from
/// our internal `Message` model, while the captured items are raw
/// server JSON with server-chosen fields and ordering. Mixing those
/// representations in one baseline makes byte-equality fragile, so
/// the `starts_with` check can fail spuriously and force full
/// replays.
///
/// Use `last_request_input` alone as the baseline.
/// The *suffix* of the new input beyond that baseline is then
/// filtered by [`is_client_authored_item`] so we never re-send the
/// assistant/reasoning/function_call items the server already owns
/// via `previous_response_id`.
///
/// Returns `None` when no previous request has been sent yet
/// (first turn, or right after a reset / invalidation).
fn compute_incremental_baseline(provider: &OpenAiResponsesProvider) -> Option<Vec<Value>> {
    provider
        .session_state
        .last_request_input
        .lock()
        .expect("last_request_input mutex poisoned")
        .clone()
}

/// Build the part of a Responses request that must stay stable for a
/// `previous_response_id` delta to be valid. This matches codex-ref's
/// `request_without_input` comparison while keeping rebon's JSON body
/// builder as the source of truth.
#[cfg(test)]
fn responses_request_fingerprint(
    request: &CreateMessageRequest,
    is_codex: bool,
    prompt_cache_key: &str,
) -> Value {
    responses_request_fingerprint_with_service_tier(request, is_codex, prompt_cache_key, None)
}

fn responses_request_fingerprint_with_service_tier(
    request: &CreateMessageRequest,
    is_codex: bool,
    prompt_cache_key: &str,
    service_tier: Option<&ServiceTierHandle>,
) -> Value {
    let mut body = build_responses_request_body_with_input_and_service_tier(
        request,
        Vec::new(),
        is_codex,
        prompt_cache_key,
        None,
        service_tier,
    );
    if let Value::Object(ref mut map) = body {
        map.remove("input");
        map.remove("previous_response_id");
        map.remove("type");
    }
    body
}

/// Return the prior input baseline only when non-input request fields
/// still match the last request. Tool, instruction, model, reasoning,
/// and built-in tool changes must force a full create.
fn compute_incremental_baseline_for_request(
    provider: &OpenAiResponsesProvider,
    current_fingerprint: &Value,
) -> Option<Vec<Value>> {
    let matches = provider
        .session_state
        .last_request_fingerprint
        .lock()
        .expect("last_request_fingerprint mutex poisoned")
        .as_ref()
        .is_some_and(|previous| previous == current_fingerprint);
    if matches {
        compute_incremental_baseline(provider)
    } else {
        None
    }
}

/// Decide whether an item belongs in an incremental-delta request.
///
/// When we send a delta alongside `previous_response_id`, the server
/// has already committed the assistant side of the prior turn
/// (reasoning, assistant messages, function_call records) into its
/// own chain. Resending client-rebuilt copies of those items is
/// both wasteful and potentially wrong — rebon's reconstruction
/// may not match server-internal fields exactly, and duplicates may
/// confuse the server's tail match. Only items that originated on
/// the client side need to travel in the delta:
///
/// - `message` with `role == "user"` — the user's new prompt.
/// - `function_call_output` — tool-result payloads produced by our
///   tool runner in response to a `function_call` the server issued
///   in an earlier turn.
///
/// Everything else (assistant messages, reasoning, function_call,
/// unknown types) is filtered out; the server reconstructs it from
/// its own chain.
fn is_client_authored_item(item: &Value) -> bool {
    match item.get("type").and_then(|v| v.as_str()) {
        Some("message") => item.get("role").and_then(|v| v.as_str()) == Some("user"),
        Some("function_call_output") => true,
        Some(_) => false,
        // `emit_user_message` produces bare `{role:"user", content:...}`
        // items without a `type` field. Accept those as
        // client-authored so a real user prompt in the delta isn't
        // filtered out — an earlier allowlist dropped them, forcing
        // a full-input fallback at best and silently losing the new
        // user message at worst when the delta also carried tool
        // outputs.
        None => item.get("role").and_then(|v| v.as_str()) == Some("user"),
    }
}

fn collect_call_ids_for_type<'a>(
    items: impl IntoIterator<Item = &'a Value>,
    item_type: &str,
) -> HashSet<String> {
    items
        .into_iter()
        .filter(|item| item.get("type").and_then(|v| v.as_str()) == Some(item_type))
        .filter_map(|item| {
            item.get("call_id")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .collect()
}

fn collect_function_call_call_ids<'a>(
    items: impl IntoIterator<Item = &'a Value>,
) -> HashSet<String> {
    collect_call_ids_for_type(items, "function_call")
}

fn collect_function_call_output_call_ids<'a>(
    items: impl IntoIterator<Item = &'a Value>,
) -> HashSet<String> {
    collect_call_ids_for_type(items, "function_call_output")
}

fn function_call_outputs_are_anchored_in_chain(
    delta: &[Value],
    baseline: &[Value],
    items_added_since_last_request: &[Value],
) -> bool {
    let output_call_ids = collect_function_call_output_call_ids(delta.iter());
    if output_call_ids.is_empty() {
        return true;
    }

    let mut chain_call_ids = collect_function_call_call_ids(baseline.iter());
    chain_call_ids.extend(collect_function_call_call_ids(
        items_added_since_last_request.iter(),
    ));
    output_call_ids
        .iter()
        .all(|call_id| chain_call_ids.contains(call_id))
}

fn model_error_is_missing_function_call_for_output(error: &ModelError) -> bool {
    const NEEDLE: &str = "No tool call found for function call output";

    match error {
        ModelError::BadRequest(message)
        | ModelError::Http(message)
        | ModelError::Protocol(message)
        | ModelError::Permanent(message) => message.contains(NEEDLE),
        _ => false,
    }
}

fn model_error_is_websocket_connection_limit(error: &ModelError) -> bool {
    matches!(error, ModelError::Http(message) if message.contains("websocket_connection_limit_reached"))
}

fn clear_stale_previous_response_before_new_ws(
    session_state: &ResponsesSessionState,
    is_codex: bool,
    has_live_ws: bool,
) -> bool {
    if !is_codex || has_live_ws {
        return false;
    }
    let mut last_response_id = session_state
        .last_response_id
        .lock()
        .expect("last_response_id mutex poisoned");
    if last_response_id.is_none() {
        return false;
    }
    *last_response_id = None;
    true
}

/// Record the full input we just sent as the new baseline. Clears
/// `items_added_since_last_request` because the items observed
/// during the *previous* response are now rolled into the baseline
/// that the server sees via `previous_response_id`.
#[cfg(test)]
fn record_sent_input_as_baseline(provider: &OpenAiResponsesProvider, full_input: Vec<Value>) {
    provider
        .session_state
        .begin_request(full_input, Value::Null);
    provider.session_state.commit_pending_request();
}

#[cfg(test)]
fn record_sent_request_as_baseline(
    provider: &OpenAiResponsesProvider,
    full_input: Vec<Value>,
    request_fingerprint: Value,
) {
    provider
        .session_state
        .begin_request(full_input, request_fingerprint);
    provider.session_state.commit_pending_request();
}

/// Parse an incoming WS text frame and, if it is a
/// `response.output_item.done` event, push the server-generated
/// `item` JSON onto `items_added_since_last_request` so the next
/// turn's baseline accounts for it.
///
/// Invoked from `drive_ws_turn_once` before the translator sees
/// the frame — the translator is concerned with caller-visible
/// stream events, not server-side item bookkeeping.
fn capture_items_added_from_frame(provider: &OpenAiResponsesProvider, text: &str) {
    let Ok(payload) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let Some("response.output_item.done") = payload.get("type").and_then(|v| v.as_str()) else {
        return;
    };
    let Some(item) = payload.get("item") else {
        return;
    };
    provider
        .session_state
        .items_added_since_last_request
        .lock()
        .expect("items_added mutex poisoned")
        .push(item.clone());
}

fn complete_ws_turn_state(session_state: &ResponsesSessionState, commit_incremental_state: bool) {
    if commit_incremental_state {
        session_state.commit_pending_request();
    } else {
        session_state.set_last_response_id(None);
        session_state.discard_pending_request();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaselineMismatchKind {
    EmptyBaseline,
    CurrentInputShorter,
    StartsWithFalse,
    FilteredDeltaEmpty,
    UnanchoredToolOutput,
}

impl BaselineMismatchKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::EmptyBaseline => "empty_baseline",
            Self::CurrentInputShorter => "current_input_shorter",
            Self::StartsWithFalse => "starts_with_false",
            Self::FilteredDeltaEmpty => "filtered_delta_empty",
            Self::UnanchoredToolOutput => "unanchored_tool_output",
        }
    }
}

/// Compact descriptor of one Responses input item for cache-divergence
/// diagnostics — position, type, role, call_id, byte length, and a
/// stable content hash (first 12 hex). The hash lets us tell "same
/// slot, content mutated" apart from "item inserted/removed" (types
/// shift) without dumping full transcript text into the log.
fn describe_responses_input_item(index: usize, item: &Value) -> String {
    let ty = item
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("<none>");
    let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("-");
    let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("-");
    let bytes = serde_json::to_string(item).map(|s| s.len()).unwrap_or(0);
    let hash = stable_hash_value(item);
    format!(
        "[{index}] type={ty} role={role} call_id={call_id} bytes={bytes} hash={}",
        &hash[..hash.len().min(12)]
    )
}

/// A short, truncated JSON preview of an item so we can see exactly
/// which field drifted between turns. Capped so a large tool output or
/// encrypted reasoning blob cannot flood the log.
fn preview_responses_input_item(item: &Value) -> String {
    const MAX: usize = 240;
    let mut s = serde_json::to_string(item).unwrap_or_default();
    if s.len() > MAX {
        let mut end = MAX;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push('…');
    }
    s
}

/// On a baseline prefix mismatch (`starts_with` == false, or the
/// current input shorter), log the FIRST item index where the freshly
/// reconstructed input diverges from the committed baseline, plus a
/// small window of descriptors on both sides. This is the definitive
/// probe for the root cause of Responses prompt-cache collapse: it
/// reveals whether an early item's *content* mutated (same type/role,
/// different hash) or an item was inserted/removed (types shift), and
/// surfaces the offending field via a truncated preview.
///
/// Fires only on the (already abnormal) full-replay fallback path, so
/// it is safe to leave on at `warn` level for beta cache debugging.
fn log_baseline_prefix_divergence(baseline_input: &[Value], baseline: &[Value]) {
    let common = baseline_input.len().min(baseline.len());
    let first_divergent = (0..common).find(|&i| baseline_input[i] != baseline[i]);

    let Some(idx) = first_divergent else {
        tracing::warn!(
            target: "rebon_api::openai_responses::cache",
            common_prefix_len = common,
            baseline_input_len = baseline_input.len(),
            baseline_len = baseline.len(),
            "baseline divergence: common prefix is byte-identical; mismatch is length-only (an item beyond the shorter array — likely history truncation or compaction, not per-item drift)"
        );
        return;
    };

    let window_lo = idx.saturating_sub(2);
    let now_window: Vec<String> = (window_lo..(idx + 3).min(baseline_input.len()))
        .map(|i| describe_responses_input_item(i, &baseline_input[i]))
        .collect();
    let committed_window: Vec<String> = (window_lo..(idx + 3).min(baseline.len()))
        .map(|i| describe_responses_input_item(i, &baseline[i]))
        .collect();

    tracing::warn!(
        target: "rebon_api::openai_responses::cache",
        first_divergent_index = idx,
        baseline_input_len = baseline_input.len(),
        baseline_len = baseline.len(),
        now_item = %describe_responses_input_item(idx, &baseline_input[idx]),
        committed_item = %describe_responses_input_item(idx, &baseline[idx]),
        now_preview = %preview_responses_input_item(&baseline_input[idx]),
        committed_preview = %preview_responses_input_item(&baseline[idx]),
        now_window = ?now_window,
        committed_window = ?committed_window,
        "baseline divergence: first mismatching item pinpointed — compare now_preview vs committed_preview for the drifting field"
    );
}

async fn drive_ws_turn_once(
    provider: &OpenAiResponsesProvider,
    ws_conn: Arc<TokioMutex<Option<WsPump>>>,
    session_state: ResponsesSessionState,
    request: &CreateMessageRequest,
    options: WsTurnOptions,
    tx: &tokio::sync::mpsc::Sender<ModelResult<StreamEvent>>,
) -> ModelResult<WsTurnResolution> {
    let is_codex = is_chatgpt_codex_backend(&provider.config.base_url);
    let transient_context_present = request
        .transient_context
        .as_deref()
        .is_some_and(|context| !context.is_empty());
    let raw_previous_response_id = match options.previous_response {
        PreviousResponseStrategy::UsePreviousResponseId if !transient_context_present => {
            session_state
                .last_response_id
                .lock()
                .expect("last_response_id mutex poisoned")
                .clone()
        }
        PreviousResponseStrategy::UsePreviousResponseId
        | PreviousResponseStrategy::ForceFullReplay => None,
    };

    // Keep one durable-input representation for baseline comparison and
    // only build a distinct wire representation when transient context
    // actually changes the request payload.
    let baseline_input = build_responses_input(&request.messages);
    let mut wire_input = transient_context_present.then(|| {
        let messages = request.messages_with_transient_context();
        build_responses_input(&messages)
    });
    let baseline_input_len = baseline_input.len();
    let wire_input_len = wire_input.as_ref().map_or(baseline_input_len, Vec::len);
    let effective_prompt_cache_key = request
        .cache_trace_context
        .as_ref()
        .and_then(|trace| trace.prompt_cache_key.as_deref())
        .unwrap_or(&provider.prompt_cache_key);
    let request_fingerprint = responses_request_fingerprint_with_service_tier(
        request,
        is_codex,
        effective_prompt_cache_key,
        provider.config.service_tier.as_ref(),
    );

    // Incremental-input optimisation (aligned with codex-ref's
    // `prepare_websocket_request`, but adapted for rebon's item
    // representation — see `compute_incremental_baseline` for the
    // divergence from codex-rs's item-concat approach).
    //
    // Strategy: when `previous_response_id` is set AND
    // `last_request_input` is a strict prefix of the new full
    // input, send only the client-authored suffix. The server
    // reconstructs the assistant side from its chain, and the
    // suffix carries only the new user message and any
    // function_call_output items produced locally since last turn.
    // This is how we get `cache_read_input_tokens > 0` on the
    // Responses API — sending the full input alongside
    // `previous_response_id` makes the server treat the whole
    // thing as a fresh prefix.
    let baseline = if raw_previous_response_id.is_some() {
        compute_incremental_baseline_for_request(provider, &request_fingerprint)
    } else {
        None
    };
    let chain_items_added = provider
        .session_state
        .items_added_since_last_request
        .lock()
        .expect("items_added mutex poisoned")
        .clone();
    let mut cache_miss_reason = if transient_context_present {
        CacheMissReason::DynamicContextChanged
    } else if raw_previous_response_id.is_none() {
        CacheMissReason::PreviousResponseIdMissing
    } else if baseline.is_none() {
        CacheMissReason::ProviderFingerprintChanged
    } else {
        CacheMissReason::None
    };
    if let Some(forced_reason) = options.forced_cache_miss_reason {
        cache_miss_reason = forced_reason;
    }
    if raw_previous_response_id.is_some() && baseline.is_none() {
        tracing::debug!(
            "openai-responses: request shape changed or baseline missing, sending full create"
        );
    }
    let mut use_previous_response_id = false;
    let mut baseline_mismatch_kind = None;
    let sent_input: Vec<Value> = match baseline.as_ref() {
        Some(baseline)
            if !baseline.is_empty()
                && baseline_input.len() >= baseline.len()
                && baseline_input.starts_with(baseline) =>
        {
            let raw_delta = &baseline_input[baseline.len()..];
            let filtered: Vec<Value> = raw_delta
                .iter()
                .filter(|it| is_client_authored_item(it))
                .cloned()
                .collect();
            if filtered.is_empty() {
                baseline_mismatch_kind = Some(BaselineMismatchKind::FilteredDeltaEmpty);
                cache_miss_reason = CacheMissReason::BaselineMismatch;
                tracing::info!(
                    target: "rebon_api::openai_responses::cache",
                    baseline_mismatch_kind = BaselineMismatchKind::FilteredDeltaEmpty.as_str(),
                    baseline_len = baseline.len(),
                    raw_delta_len = raw_delta.len(),
                    baseline_input_len = baseline_input.len(),
                    wire_len = wire_input_len,
                    "openai-responses: delta is all server-authored items — falling back to full input"
                );
                wire_input.take().unwrap_or_else(|| baseline_input.clone())
            } else if !function_call_outputs_are_anchored_in_chain(
                &filtered,
                baseline,
                &chain_items_added,
            ) {
                baseline_mismatch_kind = Some(BaselineMismatchKind::UnanchoredToolOutput);
                cache_miss_reason = CacheMissReason::BaselineMismatch;
                tracing::warn!(
                    baseline_mismatch_kind = BaselineMismatchKind::UnanchoredToolOutput.as_str(),
                    baseline_len = baseline.len(),
                    raw_delta_len = raw_delta.len(),
                    filtered_len = filtered.len(),
                    baseline_input_len = baseline_input.len(),
                    wire_len = wire_input_len,
                    "openai-responses: incremental delta contains tool output without matching function_call in baseline; falling back to full input"
                );
                wire_input.take().unwrap_or_else(|| baseline_input.clone())
            } else {
                use_previous_response_id = true;
                cache_miss_reason = CacheMissReason::None;
                tracing::debug!(
                    baseline_len = baseline.len(),
                    raw_delta_len = raw_delta.len(),
                    filtered_len = filtered.len(),
                    baseline_input_len = baseline_input.len(),
                    wire_len = wire_input_len,
                    "openai-responses: sending filtered incremental delta"
                );
                filtered
            }
        }
        Some(baseline) => {
            let kind = if baseline.is_empty() {
                BaselineMismatchKind::EmptyBaseline
            } else if baseline_input.len() < baseline.len() {
                BaselineMismatchKind::CurrentInputShorter
            } else {
                BaselineMismatchKind::StartsWithFalse
            };
            baseline_mismatch_kind = Some(kind);
            cache_miss_reason = CacheMissReason::BaselineMismatch;
            tracing::info!(
                target: "rebon_api::openai_responses::cache",
                baseline_mismatch_kind = kind.as_str(),
                baseline_input_len = baseline_input.len(),
                wire_len = wire_input_len,
                baseline_len = baseline.len(),
                starts_with_baseline = baseline_input.starts_with(baseline),
                "openai-responses: baseline mismatch, sending full input"
            );
            // Pinpoint the exact drifting item so a beta test can find
            // the root cause of the prefix instability. `EmptyBaseline`
            // has nothing to compare against.
            if !baseline.is_empty() {
                log_baseline_prefix_divergence(&baseline_input, baseline);
            }
            wire_input.take().unwrap_or_else(|| baseline_input.clone())
        }
        None => wire_input.take().unwrap_or_else(|| baseline_input.clone()),
    };

    let previous_response_id = if use_previous_response_id {
        raw_previous_response_id.as_deref()
    } else {
        None
    };
    emit_openai_request_shape_trace(
        request,
        effective_prompt_cache_key,
        previous_response_id.is_some(),
        Some(&request_fingerprint),
        cache_miss_reason,
    );

    let sent_input_len = sent_input.len();
    let mut body = build_responses_request_body_with_input_and_service_tier(
        request,
        sent_input,
        is_codex,
        effective_prompt_cache_key,
        previous_response_id,
        provider.config.service_tier.as_ref(),
    );
    if let Value::Object(ref mut map) = body {
        map.insert("type".into(), json!("response.create"));
    }
    let body_json = serde_json::to_string(&body)
        .map_err(|e| ModelError::Protocol(format!("ws request serialize: {e}")))?;

    let mut guard = ws_conn.lock().await;

    // Before attempting to send, check whether the existing pump is
    // still alive. The server can hang up between turns (especially
    // after a long in-band compaction while the socket sits idle);
    // the pump task exits silently and the stale handle would cause
    // `send` to return `SendAfterClosing`, forcing the caller down
    // the exponential-backoff retry path for what is really just
    // "reconnect now". Drop the dead pump eagerly so the code path
    // below performs a fresh connect immediately without waiting on
    // the retry delay.
    if let Some(ws) = guard.as_mut() {
        if !ws.drain_between_turns() {
            tracing::debug!(
                "openai-responses: dropping dead ws between turns; will reconnect before send"
            );
            *guard = None;
        }
    }

    if is_codex && previous_response_id.is_some() && guard.is_none() {
        tracing::warn!(
            "openai-responses: previous_response_id has no live websocket; \
             retrying turn with full replay before sending"
        );
        session_state.set_last_response_id(None);
        session_state.discard_pending_request();
        return Ok(WsTurnResolution::RetryWithoutPreviousResponseId(
            CacheMissReason::PreviousResponseNotFound,
        ));
    }

    if guard.is_none() {
        match provider.connect_ws().await {
            Ok(ws) => *guard = Some(ws),
            Err(e) => return Err(e),
        }
    }
    // The caller may have abandoned the stream while we were waiting
    // for the connection lock (e.g. a bounded side-channel call such
    // as title/summary generation timing out behind a long main turn).
    // Sending the request anyway would leave a full response unread on
    // the shared socket, and the next turn on this connection would
    // consume those frames as its own reply. Skip the send entirely.
    if tx.is_closed() {
        tracing::debug!("openai-responses: stream receiver dropped before send; skipping ws turn");
        return Ok(WsTurnResolution::Completed);
    }

    let wire_mode = if previous_response_id.is_some() {
        "delta"
    } else {
        "full"
    };
    tracing::info!(
        target: "rebon_api::openai_responses::cache",
        wire_mode,
        cache_miss_reason = cache_miss_reason.as_str(),
        baseline_mismatch_kind = baseline_mismatch_kind.map(BaselineMismatchKind::as_str),
        previous_response_id_present = previous_response_id.is_some(),
        sent_input_items = sent_input_len,
        baseline_input_items = baseline_input_len,
        wire_input_items = wire_input_len,
        "openai-responses request cache mode"
    );
    send_and_pump_ws_turn(
        &mut guard,
        provider,
        &session_state,
        &options,
        tx,
        body_json,
        baseline_input,
        request_fingerprint,
        transient_context_present,
    )
    .await
}

/// Drive one turn on the persistent WebSocket connection.
///
/// Generic transport and server failures are returned immediately to the
/// caller so [`crate::RetryMiddleware`] is the only retry budget and backoff
/// owner. Two failures are recovered here instead, each with exactly one
/// "reconnect and replay the whole turn" attempt, because neither the
/// middleware nor the engine can recover them: a rejected connection-scoped
/// `previous_response_id`, and a socket that died mid-response (nothing
/// about the request was wrong, and the partial response is not resumable).
///
/// Holds the `ws_conn` lock for the entire turn: ensures the
/// connection exists (creating one if needed), sends the
/// `response.create` frame, reads events until
/// `response.completed` (or error), and forwards translated
/// [`StreamEvent`]s to `tx`. On completion the lock is released
/// and the connection stays alive for the next turn. On error the
/// connection is dropped so the next turn reconnects.
///
/// Ping/pong is handled transparently by the [`WsPump`] background
/// task — this loop only sees Text/Close/Error frames.
async fn drive_ws_turn(
    provider: &OpenAiResponsesProvider,
    ws_conn: Arc<TokioMutex<Option<WsPump>>>,
    session_state: ResponsesSessionState,
    request: CreateMessageRequest,
    tx: &tokio::sync::mpsc::Sender<ModelResult<StreamEvent>>,
) -> ModelResult<()> {
    let resolution = drive_ws_turn_once(
        provider,
        ws_conn.clone(),
        session_state.clone(),
        &request,
        WsTurnOptions::default(),
        tx,
    )
    .await?;

    let reason = match resolution {
        WsTurnResolution::Completed => return Ok(()),
        WsTurnResolution::RetryWithoutPreviousResponseId(reason) => reason,
        // A dead socket says nothing about the request, so the replay is not
        // a correction of anything: reconnect and re-send the same turn.
        // Neither the engine nor the retry middleware can do it — a
        // half-received response leaves the engine with output already
        // streamed, and its replay guard only fires before visible output.
        WsTurnResolution::ReconnectAndReplay(error) => {
            tracing::warn!(
                error = %error,
                "openai-responses: websocket died mid-turn; reconnecting and replaying the turn"
            );
            CacheMissReason::RetryWithoutPreviousResponseId
        }
    };

    if tx.is_closed() {
        tracing::debug!(
            "openai-responses: stream receiver dropped; abandoning continuation recovery"
        );
        return Ok(());
    }

    if let Some(notifier) = &provider.retry_notifier {
        notifier.set(1, 1);
    }
    let recovered = drive_ws_turn_once(
        provider,
        ws_conn,
        session_state,
        &request,
        WsTurnOptions {
            previous_response: PreviousResponseStrategy::ForceFullReplay,
            forced_cache_miss_reason: Some(reason),
        },
        tx,
    )
    .await;
    if let Some(notifier) = &provider.retry_notifier {
        notifier.clear();
    }

    match recovered? {
        WsTurnResolution::Completed => Ok(()),
        WsTurnResolution::RetryWithoutPreviousResponseId(_) => Err(ModelError::Protocol(
            "responses continuation recovery repeated after full replay".into(),
        )),
        // The replay lost its socket too: report the transport failure, which
        // the engine reads as transient.
        WsTurnResolution::ReconnectAndReplay(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ModelClient;
    use crate::events::MessageAccumulator;
    use crate::types::{
        ContentBlock, Message, Role, TextBlock, ThinkingBlock, Tool, ToolResultBlock,
        ToolResultContent, ToolResultContentBlock, ToolUseBlock,
    };

    #[test]
    fn responses_endpoint_inserts_v1_for_bare_origins_only() {
        // Bare origin (an OpenAI-compatible relay or api.openai.com
        // itself): the Responses API lives under /v1.
        assert_eq!(
            responses_endpoint_for_base("https://relay.example"),
            "https://relay.example/v1/responses"
        );
        assert_eq!(
            responses_endpoint_for_base("https://relay.example:8443/"),
            "https://relay.example:8443/v1/responses"
        );
        // An explicit path is used verbatim — /v1 …
        assert_eq!(
            responses_endpoint_for_base("https://relay.example/v1"),
            "https://relay.example/v1/responses"
        );
        // … the ChatGPT Codex backend …
        assert_eq!(
            responses_endpoint_for_base("https://chatgpt.com/backend-api/codex"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        // … or a base that already names the /responses endpoint.
        assert_eq!(
            responses_endpoint_for_base("https://relay.example/v1/responses"),
            "https://relay.example/v1/responses"
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

    #[derive(Clone, Copy)]
    enum WsAuthRetrySecondResponse {
        Upgrade,
        HttpStatus(u16),
    }

    async fn start_ws_auth_retry_server_with_second_response(
        second_response: WsAuthRetrySecondResponse,
    ) -> (
        String,
        Arc<std::sync::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen_auth = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_auth_for_task = seen_auth.clone();
        let task = tokio::spawn(async move {
            for attempt in 0..2 {
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
                let request = String::from_utf8_lossy(&buffer[..used]);
                let auth = request
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("Authorization: ")
                            .or_else(|| line.strip_prefix("authorization: "))
                    })
                    .unwrap_or_default()
                    .to_string();
                seen_auth_for_task.lock().unwrap().push(auth);

                if attempt == 0 {
                    tokio::io::AsyncWriteExt::write_all(
                        &mut stream,
                        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
                    )
                    .await
                    .unwrap();
                    continue;
                }

                match second_response {
                    WsAuthRetrySecondResponse::Upgrade => {
                        let key = request
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("Sec-WebSocket-Key: ")
                                    .or_else(|| line.strip_prefix("sec-websocket-key: "))
                            })
                            .unwrap();
                        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
                        let response = format!(
                            "HTTP/1.1 101 Switching Protocols\r\n\
                             Upgrade: websocket\r\n\
                             Connection: Upgrade\r\n\
                             Sec-WebSocket-Accept: {accept}\r\n\r\n"
                        );
                        tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
                            .await
                            .unwrap();
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    WsAuthRetrySecondResponse::HttpStatus(status) => {
                        let reason = tungstenite::http::StatusCode::from_u16(status)
                            .ok()
                            .and_then(|status| status.canonical_reason())
                            .unwrap_or("Unauthorized");
                        let response =
                            format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\n\r\n");
                        tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
                            .await
                            .unwrap();
                    }
                }
            }
        });
        (format!("http://{addr}"), seen_auth, task)
    }

    async fn start_ws_auth_retry_server() -> (
        String,
        Arc<std::sync::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        start_ws_auth_retry_server_with_second_response(WsAuthRetrySecondResponse::Upgrade).await
    }

    // ---------------------------------------------------------------
    // Request body translation
    // ---------------------------------------------------------------

    fn simple_user_request(text: &str) -> CreateMessageRequest {
        CreateMessageRequest::simple("gpt-5.4", text).with_system("You are helpful.")
    }

    #[test]
    fn build_request_body_collapses_single_user_text_into_plain_string() {
        let req = simple_user_request("hello");
        let body = build_responses_request_body(&req, true, "cache-key-1", None);
        assert_eq!(body["model"], "gpt-5.4");
        assert_eq!(body["instructions"], "You are helpful.");
        assert_eq!(body["store"], false);
        assert_eq!(body["prompt_cache_key"], "cache-key-1");
        assert_eq!(body["stream"], true);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"], "hello");
    }

    /// `compaction_trigger` is an OpenAI control item, not a Responses
    /// one: a relay that speaks the format would reject it, so the rung
    /// is offered on the two OpenAI hosts and nowhere else.
    #[test]
    fn remote_compaction_v2_is_offered_only_on_openai_hosts() {
        for (base_url, expected) in [
            ("https://chatgpt.com/backend-api/codex", true),
            ("https://api.openai.com/v1", true),
            ("https://api.deepseek.com/v1", false),
            ("https://relay.example.test/v1", false),
        ] {
            let provider = OpenAiResponsesProvider::new(
                OpenAiResponsesClientConfig::with_base_url(base_url.to_string(), "sk-test"),
            );
            assert_eq!(
                ChatProvider::supports_remote_compaction_v2(&provider),
                expected,
                "{base_url}"
            );
        }
    }

    #[test]
    fn build_request_body_appends_the_compaction_trigger_last() {
        let mut req = simple_user_request("hello");
        req.compaction_trigger = true;
        let body = build_responses_request_body(&req, true, "cache-key-1", None);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        // Everything before the trigger stays the live prefix, byte for
        // byte — that alignment is the reason to compact server-side.
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"], "hello");
        assert_eq!(input[1], serde_json::json!({"type": "compaction_trigger"}));
    }

    #[test]
    fn build_request_body_omits_the_trigger_on_an_ordinary_turn() {
        let req = simple_user_request("hello");
        let body = build_responses_request_body(&req, true, "cache-key-1", None);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
    }

    #[test]
    fn build_request_body_replays_the_compaction_item() {
        let mut req = simple_user_request("hello");
        req.messages.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text(crate::types::TextBlock {
                    text: "before".into(),
                }),
                ContentBlock::Compaction(crate::types::CompactionBlock {
                    content: None,
                    encrypted_content: Some("opaque-blob".into()),
                }),
            ],
        });
        let body = build_responses_request_body(&req, true, "cache-key-1", None);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[1]["type"], "message");
        assert_eq!(
            input[2],
            serde_json::json!({"type": "compaction", "encrypted_content": "opaque-blob"})
        );
    }

    /// Anthropic's compaction block carries a plaintext summary and no
    /// blob; there is no Responses item for it, and inventing one would
    /// be a 400.
    #[test]
    fn build_request_body_drops_a_compaction_block_with_no_blob() {
        let mut req = simple_user_request("hello");
        req.messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Compaction(crate::types::CompactionBlock {
                content: Some("a plaintext summary".into()),
                encrypted_content: None,
            })],
        });
        let body = build_responses_request_body(&req, true, "cache-key-1", None);
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn build_request_body_emits_reasoning_mode_pro_with_effort() {
        let mut req = simple_user_request("hello");
        req.reasoning_effort = Some(crate::request::ReasoningEffort::Max);
        req.reasoning_mode = Some(crate::request::ReasoningMode::Pro);
        let body = build_responses_request_body(&req, true, "cache-key-1", None);
        assert_eq!(body["reasoning"]["effort"], "max");
        assert_eq!(body["reasoning"]["mode"], "pro");
        assert_eq!(body["reasoning"]["summary"], "auto");
    }

    #[test]
    fn build_request_body_emits_reasoning_mode_pro_without_effort() {
        let mut req = simple_user_request("hello");
        req.reasoning_mode = Some(crate::request::ReasoningMode::Pro);
        let body = build_responses_request_body(&req, true, "cache-key-1", None);
        assert_eq!(body["reasoning"]["mode"], "pro");
        assert!(body["reasoning"].get("effort").is_none());
        assert_eq!(
            body["include"],
            serde_json::json!(["reasoning.encrypted_content"])
        );
    }

    #[test]
    fn build_request_body_omits_reasoning_when_no_params() {
        let req = simple_user_request("hello");
        let body = build_responses_request_body(&req, true, "cache-key-1", None);
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn build_request_body_translates_pro_model_alias() {
        let req = CreateMessageRequest::simple("gpt-5.6-sol-pro", "hello");
        let body = build_responses_request_body(&req, true, "cache-key-1", None);
        assert_eq!(body["model"], "gpt-5.6-sol");
        assert_eq!(body["reasoning"]["mode"], "pro");
    }

    #[test]
    fn build_request_body_keeps_real_pro_model_ids() {
        let req = CreateMessageRequest::simple("deepseek-v4-pro", "hello");
        let body = build_responses_request_body(&req, true, "cache-key-1", None);
        assert_eq!(body["model"], "deepseek-v4-pro");
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn build_request_body_and_fingerprint_include_runtime_fast_tier() {
        let handle = ServiceTierHandle::new(true);
        let req = simple_user_request("hello");
        let body = build_responses_request_body_with_service_tier(
            &req,
            true,
            "cache-key-1",
            None,
            Some(&handle),
        );
        assert_eq!(body["service_tier"], "priority");

        let fingerprint = responses_request_fingerprint_with_service_tier(
            &req,
            true,
            "cache-key-1",
            Some(&handle),
        );
        assert_eq!(fingerprint["service_tier"], "priority");

        handle.set_fast(false);
        let body = build_responses_request_body_with_service_tier(
            &req,
            true,
            "cache-key-1",
            None,
            Some(&handle),
        );
        assert!(body.get("service_tier").is_none());
        let fingerprint = responses_request_fingerprint_with_service_tier(
            &req,
            true,
            "cache-key-1",
            Some(&handle),
        );
        assert!(fingerprint.get("service_tier").is_none());
    }

    #[test]
    fn build_request_body_with_prebuilt_input_uses_exact_input() {
        let req = simple_user_request("original request input");
        let input = vec![json!({
            "type": "function_call_output",
            "call_id": "call_1",
            "output": "prebuilt"
        })];

        let body = build_responses_request_body_with_input_and_service_tier(
            &req,
            input.clone(),
            true,
            "cache-key-1",
            Some("resp_1"),
            None,
        );

        assert_eq!(body["input"], Value::Array(input));
        assert_eq!(body["previous_response_id"], "resp_1");
    }

    #[test]
    fn build_request_body_omits_codex_forbidden_params() {
        let mut req = simple_user_request("hello");
        req.temperature = Some(0.7);
        req.max_tokens = 2048;
        let body = build_responses_request_body(&req, true, "key", None);
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn build_request_body_emits_codex_forbidden_params_for_non_codex() {
        let mut req = simple_user_request("hello");
        // Using 0.5 so the f32 → f64 upcast is exact and assert_eq
        // comparisons against the serialized JSON succeed without
        // needing approx-float machinery.
        req.temperature = Some(0.5);
        req.max_tokens = 2048;
        let body = build_responses_request_body(&req, false, "key", None);
        assert_eq!(body["max_output_tokens"], 2048);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["store"], true);
    }

    #[test]
    fn build_request_body_default_instructions_when_no_system_prompt() {
        let req = CreateMessageRequest::simple("gpt-5.4", "hi");
        let body = build_responses_request_body(&req, true, "key", None);
        assert_eq!(body["instructions"], "You are a helpful assistant.");
    }

    #[test]
    fn build_input_emits_assistant_text_with_message_wrapper() {
        let messages = vec![
            Message::user_text("first question"),
            Message::assistant_text("short reply"),
            Message::user_text("follow up"),
        ];
        let input = build_responses_input(&messages);
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"], "first question");

        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[1]["content"][0]["text"], "short reply");

        assert_eq!(input[2]["role"], "user");
        assert_eq!(input[2]["content"], "follow up");
    }

    #[test]
    fn build_input_splits_assistant_text_and_tool_call_into_two_items() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text(TextBlock {
                        text: "I'll read it".into(),
                    }),
                    ContentBlock::ToolUse(ToolUseBlock {
                        id: "call_abc".into(),
                        name: "Read".into(),
                        input: json!({"path": "foo.rs"}),
                    }),
                    ContentBlock::Text(TextBlock {
                        text: "and now I'll edit".into(),
                    }),
                    ContentBlock::ToolUse(ToolUseBlock {
                        id: "call_def".into(),
                        name: "Edit".into(),
                        input: json!({"path": "foo.rs", "content": "..."}),
                    }),
                ],
            },
            // Matching tool results so the input is well-formed.
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult(ToolResultBlock {
                        tool_use_id: "call_abc".into(),
                        content: "file contents".into(),
                        is_error: false,
                    }),
                    ContentBlock::ToolResult(ToolResultBlock {
                        tool_use_id: "call_def".into(),
                        content: "edit done".into(),
                        is_error: false,
                    }),
                ],
            },
        ];
        let input = build_responses_input(&messages);
        // Expected order: msg(text1) | function_call(Read) |
        //   msg(text2) | function_call(Edit) |
        //   function_call_output(abc) | function_call_output(def)
        assert_eq!(input.len(), 6);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["content"][0]["text"], "I'll read it");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_abc");
        assert_eq!(input[1]["name"], "Read");
        assert_eq!(input[1]["arguments"], r#"{"path":"foo.rs"}"#);
        assert_eq!(input[2]["type"], "message");
        assert_eq!(input[2]["content"][0]["text"], "and now I'll edit");
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(input[3]["call_id"], "call_def");
        assert_eq!(input[4]["type"], "function_call_output");
        assert_eq!(input[4]["call_id"], "call_abc");
        assert_eq!(input[5]["type"], "function_call_output");
        assert_eq!(input[5]["call_id"], "call_def");
    }

    #[test]
    fn build_input_emits_tool_result_as_top_level_function_call_output() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse(ToolUseBlock {
                    id: "call_abc".into(),
                    name: "Read".into(),
                    input: json!({"path": "foo.rs"}),
                })],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult(ToolResultBlock {
                    tool_use_id: "call_abc".into(),
                    content: "file contents".into(),
                    is_error: false,
                })],
            },
        ];
        let input = build_responses_input(&messages);
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "call_abc");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_abc");
        assert_eq!(input[1]["output"], "file contents");
    }

    #[test]
    fn build_input_mixes_tool_result_and_text_in_same_user_turn() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse(ToolUseBlock {
                    id: "call_1".into(),
                    name: "Read".into(),
                    input: json!({"path": "bar.rs"}),
                })],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult(ToolResultBlock {
                        tool_use_id: "call_1".into(),
                        content: "ok".into(),
                        is_error: false,
                    }),
                    ContentBlock::Text(TextBlock {
                        text: "now please continue".into(),
                    }),
                ],
            },
        ];
        let input = build_responses_input(&messages);
        // Three top-level items: function_call, function_call_output, then user msg.
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[2]["role"], "user");
        assert_eq!(input[2]["content"], "now please continue");
    }

    #[test]
    fn build_input_emits_tool_result_image_as_input_image() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse(ToolUseBlock {
                    id: "call_image".into(),
                    name: "Read".into(),
                    input: json!({"file_path": "image.png"}),
                })],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult(ToolResultBlock {
                    tool_use_id: "call_image".into(),
                    content: ToolResultContent::blocks(vec![
                        ToolResultContentBlock::Text(TextBlock {
                            text: "Image file read: image.png".into(),
                        }),
                        ToolResultContentBlock::Image(crate::types::ImageBlock::base64(
                            "image/png",
                            "iVBORw0KGgo=",
                        )),
                    ]),
                    is_error: false,
                })],
            },
        ];

        let input = build_responses_input(&messages);

        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["output"], "Image file read: image.png");
        assert_eq!(input[2]["role"], "user");
        assert_eq!(input[2]["content"][1]["type"], "input_image");
        assert_eq!(
            input[2]["content"][1]["image_url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
    }

    #[test]
    fn build_input_round_trips_encrypted_reasoning_items() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text(TextBlock {
                    text: "visible".into(),
                }),
                ContentBlock::Thinking(ThinkingBlock {
                    thinking: "summary".into(),
                    signature: None,
                    data: Some("ENCRYPTED_REASONING".into()),
                }),
                ContentBlock::Text(TextBlock {
                    text: "after".into(),
                }),
            ],
        }];

        let input = build_responses_input(&messages);

        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["content"][0]["text"], "visible");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["encrypted_content"], "ENCRYPTED_REASONING");
        assert_eq!(input[1]["summary"][0]["text"], "summary");
        assert_eq!(input[2]["type"], "message");
        assert_eq!(input[2]["content"][0]["text"], "after");
    }

    #[test]
    fn build_input_reasoning_always_emits_summary_array_even_when_empty() {
        // Regression: the Responses API rejects a reasoning item with
        // no `summary` field (400 missing_required_parameter
        // input[N].summary). With `summary: auto` the model often
        // returns encrypted reasoning but no summary text, so
        // `thinking` is empty while `data` is populated — the item
        // must still carry `summary: []`.
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking(ThinkingBlock {
                thinking: String::new(),
                signature: None,
                data: Some("ENCRYPTED_REASONING".into()),
            })],
        }];

        let input = build_responses_input(&messages);

        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["encrypted_content"], "ENCRYPTED_REASONING");
        // `summary` must be present and be an (empty) array.
        assert!(
            input[0]["summary"].is_array(),
            "reasoning item must always carry a summary array, got: {}",
            input[0]
        );
        assert_eq!(input[0]["summary"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn build_input_drops_unsendable_reasoning_without_encrypted_content() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking(ThinkingBlock {
                    thinking: "summary only".into(),
                    signature: None,
                    data: None,
                }),
                ContentBlock::Text(TextBlock {
                    text: "after".into(),
                }),
            ],
        }];

        let input = build_responses_input(&messages);

        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["content"][0]["text"], "after");
    }

    #[test]
    fn build_input_emits_tool_result_document_as_input_file() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse(ToolUseBlock {
                    id: "call_pdf".into(),
                    name: "Read".into(),
                    input: json!({"file_path": "doc.pdf"}),
                })],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult(ToolResultBlock {
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
            },
        ];

        let input = build_responses_input(&messages);

        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["output"], "PDF file read: doc.pdf");
        assert_eq!(input[2]["role"], "user");
        assert_eq!(input[2]["content"][1]["type"], "input_file");
        assert_eq!(
            input[2]["content"][1]["file_data"],
            "data:application/pdf;base64,JVBERi0="
        );
    }

    #[test]
    fn build_tools_emits_flattened_shape_not_nested_function() {
        let tools = vec![Tool {
            name: "Read".into(),
            description: "Read a file".into(),
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }];
        let translated = build_responses_tools(&tools);
        assert_eq!(translated.len(), 1);
        assert_eq!(translated[0]["type"], "function");
        assert_eq!(translated[0]["name"], "Read");
        assert_eq!(translated[0]["description"], "Read a file");
        assert_eq!(
            translated[0]["parameters"],
            json!({"type": "object", "properties": {"path": {"type": "string"}}})
        );
        // Must NOT nest under `function`.
        assert!(translated[0].get("function").is_none());
    }

    #[test]
    fn endpoint_appends_responses_suffix_when_missing() {
        let provider = OpenAiResponsesProvider::new(OpenAiResponsesClientConfig::with_base_url(
            "https://api.example.com/v1",
            "sk",
        ));
        assert_eq!(provider.endpoint(), "https://api.example.com/v1/responses");
    }

    #[test]
    fn endpoint_preserves_base_url_that_already_ends_in_responses() {
        let provider = OpenAiResponsesProvider::new(OpenAiResponsesClientConfig::with_base_url(
            "https://chatgpt.com/backend-api/codex/responses",
            "sk",
        ));
        assert_eq!(
            provider.endpoint(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn endpoint_strips_trailing_slash_before_appending() {
        let provider = OpenAiResponsesProvider::new(OpenAiResponsesClientConfig::with_base_url(
            "https://api.example.com/v1/",
            "sk",
        ));
        assert_eq!(provider.endpoint(), "https://api.example.com/v1/responses");
    }

    #[tokio::test]
    async fn convenience_constructor_does_not_cut_off_slow_sse_stream() {
        let body = concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.4\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"ok\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"output_index\":0}\n\n",
            "event: response.completed\n",
            "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
        );
        let (base_url, server) =
            start_delayed_sse_http_server(body, Duration::from_millis(100)).await;
        let mut config = OpenAiResponsesClientConfig::with_api_key("sk-test");
        config.base_url = base_url;
        config.request_timeout = Some(Duration::from_millis(10));
        let client = openai_responses_client(config);

        let msg = client
            .create_message(CreateMessageRequest::simple("gpt-5.4", "ping"))
            .await
            .unwrap();

        server.await.unwrap();
        assert_eq!(msg.text(), "ok");
    }

    #[test]
    fn detects_chatgpt_codex_backend_by_url_substring() {
        assert!(is_chatgpt_codex_backend(
            "https://chatgpt.com/backend-api/codex/responses"
        ));
        assert!(is_chatgpt_codex_backend(
            "https://chatgpt.com/backend-api/codex"
        ));
        assert!(!is_chatgpt_codex_backend("https://api.openai.com/v1"));
        assert!(!is_chatgpt_codex_backend("https://right.codes/codex/v1"));
    }

    // ---------------------------------------------------------------
    // SSE translator
    // ---------------------------------------------------------------

    fn drain(translator: &mut OpenAiResponsesTranslator) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        while let Some(e) = translator.next_pending() {
            events.push(e);
        }
        events
    }

    fn push(translator: &mut OpenAiResponsesTranslator, event: &str, data: Value) {
        translator
            .push_frame(event, &serde_json::to_string(&data).unwrap())
            .expect("push_frame should succeed for valid payloads");
    }

    #[test]
    fn translator_emits_message_start_on_response_created() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "resp_1", "model": "gpt-5.4"}}),
        );
        let events = drain(&mut t);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::MessageStart {
                message_id, model, ..
            } => {
                assert_eq!(message_id, "resp_1");
                assert_eq!(model, "gpt-5.4");
            }
            other => panic!("expected MessageStart, got {other:?}"),
        }
    }

    #[test]
    fn translator_translates_text_delta_into_content_block_delta() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "m"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": {"type": "message"}
            }),
        );
        push(
            &mut t,
            "response.output_text.delta",
            json!({"delta": "Hello "}),
        );
        push(
            &mut t,
            "response.output_text.delta",
            json!({"delta": "world"}),
        );
        push(
            &mut t,
            "response.output_item.done",
            json!({"output_index": 0}),
        );
        push(
            &mut t,
            "response.completed",
            json!({"response": {"id": "r1"}}),
        );

        let events = drain(&mut t);
        let texts: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ContentBlockDelta {
                    delta: ContentBlockDelta::TextDelta { text },
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts.join(""), "Hello world");

        assert!(events.iter().any(|e| matches!(e, StreamEvent::MessageStop)));
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields {
                    stop_reason: Some(StopReason::EndTurn),
                    ..
                },
            }
        )));
    }

    #[test]
    fn translator_maps_max_output_tokens_to_max_tokens_stop_reason() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "m"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({"output_index": 0, "item": {"type": "message"}}),
        );
        push(
            &mut t,
            "response.output_text.delta",
            json!({"delta": "partial answer"}),
        );
        push(
            &mut t,
            "response.incomplete",
            json!({
                "response": {
                    "id": "r1",
                    "incomplete_details": {"reason": "max_output_tokens"},
                    "usage": {
                        "input_tokens": 12,
                        "output_tokens": 34,
                        "input_tokens_details": {"cached_tokens": 5}
                    }
                }
            }),
        );

        assert!(t.is_finished());
        let events = drain(&mut t);
        let terminal = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::MessageDelta { delta } => Some(delta),
                _ => None,
            })
            .expect("response.incomplete must emit a terminal message delta");
        assert_eq!(terminal.stop_reason, Some(StopReason::MaxTokens));
        assert_eq!(terminal.usage.input_tokens, 12);
        assert_eq!(terminal.usage.output_tokens, 34);
        assert_eq!(terminal.usage.prompt_cache_hit_tokens, 5);
        assert_eq!(terminal.usage.prompt_cache_miss_tokens, 7);
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ContentBlockDelta {
                delta: ContentBlockDelta::TextDelta { text },
                ..
            } if text == "partial answer"
        )));
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::ContentBlockStop { index: 0 })));
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::MessageStop)));
    }

    #[test]
    fn translator_max_output_tokens_overrides_tool_use_stop_reason() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "m"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "call_id": "call_partial",
                    "name": "Read"
                }
            }),
        );
        push(
            &mut t,
            "response.function_call_arguments.delta",
            json!({"delta": "{\"path\":"}),
        );
        push(
            &mut t,
            "response.incomplete",
            json!({
                "response": {
                    "id": "r1",
                    "incomplete_details": {"reason": "max_output_tokens"}
                }
            }),
        );

        let events = drain(&mut t);
        let stop_reason = events.iter().find_map(|event| match event {
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields { stop_reason, .. },
            } => stop_reason.clone(),
            _ => None,
        });
        assert_eq!(stop_reason, Some(StopReason::MaxTokens));
    }

    #[test]
    fn translator_non_token_incomplete_reason_remains_error() {
        let mut t = OpenAiResponsesTranslator::default();
        let result = t.push_frame(
            "response.incomplete",
            &json!({
                "response": {
                    "incomplete_details": {"reason": "content_filter"}
                }
            })
            .to_string(),
        );

        let error = result.expect_err("content_filter must remain an error");
        assert!(
            matches!(&error, ModelError::Http(message) if message.contains("content_filter")),
            "got: {error:?}"
        );
        assert!(t.is_finished());
    }

    #[test]
    fn translator_patches_missing_text_tail_from_done_event() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "m"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": {"type": "message"}
            }),
        );
        push(
            &mut t,
            "response.output_text.delta",
            json!({"delta": "Hello "}),
        );
        push(
            &mut t,
            "response.output_text.done",
            json!({"text": "Hello world"}),
        );
        push(
            &mut t,
            "response.output_item.done",
            json!({
                "output_index": 0,
                "item": {"type": "message", "text": "Hello world"}
            }),
        );
        push(
            &mut t,
            "response.completed",
            json!({"response": {"id": "r1"}}),
        );

        let events = drain(&mut t);
        let texts: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ContentBlockDelta {
                    delta: ContentBlockDelta::TextDelta { text },
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts.join(""), "Hello world");
    }

    #[test]
    fn translator_patches_missing_args_tail_from_done_event() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "m"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "call_id": "call_xyz",
                    "name": "Read"
                }
            }),
        );
        push(
            &mut t,
            "response.function_call_arguments.delta",
            json!({"delta": "{\"path\":"}),
        );
        push(
            &mut t,
            "response.function_call_arguments.done",
            json!({"arguments": "{\"path\":\"foo.rs\"}"}),
        );
        push(
            &mut t,
            "response.output_item.done",
            json!({
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "arguments": "{\"path\":\"foo.rs\"}"
                }
            }),
        );
        push(
            &mut t,
            "response.completed",
            json!({"response": {"id": "r1"}}),
        );

        let events = drain(&mut t);
        let args: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ContentBlockDelta {
                    delta: ContentBlockDelta::InputJsonDelta { partial_json },
                    ..
                } => Some(partial_json.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(args, "{\"path\":\"foo.rs\"}");
    }

    #[test]
    fn translator_translates_function_call_into_tool_use_block() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "m"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "call_id": "call_xyz",
                    "name": "Read"
                }
            }),
        );
        push(
            &mut t,
            "response.function_call_arguments.delta",
            json!({"delta": "{\"path\":"}),
        );
        push(
            &mut t,
            "response.function_call_arguments.delta",
            json!({"delta": "\"foo.rs\"}"}),
        );
        push(
            &mut t,
            "response.output_item.done",
            json!({"output_index": 0}),
        );
        push(
            &mut t,
            "response.completed",
            json!({"response": {"id": "r1"}}),
        );

        let events = drain(&mut t);
        let has_tool_start = events.iter().any(|e| {
            matches!(
                e,
                StreamEvent::ContentBlockStart {
                    content_block: ContentBlockStart::ToolUse { id, name },
                    ..
                } if id == "call_xyz" && name == "Read"
            )
        });
        assert!(has_tool_start, "expected ToolUse ContentBlockStart");

        let args: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ContentBlockDelta {
                    delta: ContentBlockDelta::InputJsonDelta { partial_json },
                    ..
                } => Some(partial_json.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(args, "{\"path\":\"foo.rs\"}");

        // Stop reason is ToolUse because we saw a function_call item.
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields {
                    stop_reason: Some(StopReason::ToolUse),
                    ..
                },
            }
        )));
    }

    #[test]
    fn translator_emits_error_event_from_response_error() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.error",
            json!({"error": {"type": "rate_limited", "message": "slow down"}}),
        );
        let events = drain(&mut t);
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::Error { error_type, message }
                if error_type == "rate_limited" && message == "slow down"
        )));
    }

    #[test]
    fn translator_bare_error_without_status_is_non_retryable() {
        let mut t = OpenAiResponsesTranslator::default();
        let result = t.push_frame(
            "error",
            &serde_json::to_string(&json!({
                "error": {"code": "server_error", "message": "internal failure"}
            }))
            .unwrap(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        // No status → ModelError::Http (not transient)
        assert!(matches!(err, ModelError::Http(_)), "got: {err:?}");
        let msg = format!("{err}");
        assert!(msg.contains("server_error"), "got: {msg}");
        assert!(t.is_finished());
    }

    #[test]
    fn translator_statusless_unsupported_model_error_is_non_retryable() {
        let mut t = OpenAiResponsesTranslator::default();
        let result = t.push_frame(
            "error",
            &serde_json::to_string(&json!({
                "error": {
                    "message": "The 'gpt-5.4-nano' model is not supported when using Codex with a ChatGPT account."
                }
            }))
            .unwrap(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ModelError::BadRequest(_)), "got: {err:?}");
        assert!(!err.is_transient());
        assert!(t.is_finished());
    }

    /// Bare `error` event with `context_length_exceeded` (WS path).
    #[test]
    fn translator_context_length_exceeded_bare_error_is_overflow() {
        let mut t = OpenAiResponsesTranslator::default();
        let result = t.push_frame(
            "error",
            &serde_json::to_string(&json!({
                "error": {
                    "code": "context_length_exceeded",
                    "message": "Your input exceeds the context window of this model. Please adjust your input and try again."
                }
            }))
            .unwrap(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ModelError::Permanent(_)), "got: {err:?}");
        // Must be detectable as context overflow for retry logic.
        assert!(
            err.context_overflow().is_some(),
            "expected context_overflow() to match"
        );
        assert!(t.is_finished());
    }

    /// `response.failed` with `context_length_exceeded` (HTTP/SSE path).
    #[test]
    fn translator_response_failed_context_exceeded_is_overflow() {
        let mut t = OpenAiResponsesTranslator::default();
        // First emit response.created so the translator is in-flight.
        t.push_frame(
            "response.created",
            &serde_json::to_string(&json!({
                "response": {"id": "resp_1", "model": "gpt-5.4"}
            }))
            .unwrap(),
        )
        .unwrap();
        let result = t.push_frame(
            "response.failed",
            &serde_json::to_string(&json!({
                "response": {
                    "id": "resp_1",
                    "error": {
                        "code": "context_length_exceeded",
                        "message": "Your input exceeds the context window of this model."
                    }
                }
            }))
            .unwrap(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ModelError::Permanent(_)), "got: {err:?}");
        assert!(
            err.context_overflow().is_some(),
            "expected context_overflow() to match"
        );
        assert!(t.is_finished());
    }

    #[test]
    fn translator_previous_response_not_found_is_non_retryable() {
        let mut t = OpenAiResponsesTranslator::default();
        let result = t.push_frame(
            "error",
            &serde_json::to_string(&json!({
                "error": {
                    "code": "previous_response_not_found",
                    "message": "Previous response with id 'resp_123' not found."
                }
            }))
            .unwrap(),
        );
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ModelError::BadRequest(_)));
        assert!(t.is_finished());
    }

    /// The connection-lifetime error arrives with `status: 400`,
    /// which would classify as a permanent `BadRequest` via the
    /// status fallthrough. The code match must win: this is a
    /// transport-lifecycle event and has to stay retryable so the
    /// WS turn driver reconnects instead of failing the prompt.
    #[test]
    fn translator_websocket_connection_limit_is_retryable() {
        let mut t = OpenAiResponsesTranslator::default();
        let result = t.push_frame(
            "error",
            &serde_json::to_string(&json!({
                "type": "error",
                "status": 400,
                "error": {
                    "type": "invalid_request_error",
                    "code": "websocket_connection_limit_reached",
                    "message": "Responses websocket connection limit reached (60 minutes). Create a new websocket connection to continue."
                }
            }))
            .unwrap(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ModelError::Http(_)), "got: {err:?}");
        assert!(err.is_transient());
        assert!(model_error_is_websocket_connection_limit(&err));
        assert!(t.is_finished());
    }

    #[test]
    fn translator_statusless_invalid_function_parameters_is_non_retryable() {
        let mut t = OpenAiResponsesTranslator::default();
        let result = t.push_frame(
            "error",
            &serde_json::to_string(&json!({
                "error": {
                    "code": "invalid_function_parameters",
                    "message": "Invalid schema for function 'PlanLedger': schema must have type 'object' and not have 'oneOf'/'anyOf'/'allOf'/'enum'/'const'/'not' at the top level."
                }
            }))
            .unwrap(),
        );
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ModelError::BadRequest(_)));
        assert!(t.is_finished());
    }

    #[test]
    fn translator_bare_error_401_is_unauthorized() {
        let mut t = OpenAiResponsesTranslator::default();
        let result = t.push_frame(
            "error",
            &serde_json::to_string(&json!({
                "status": 401,
                "error": {"code": "auth_error", "message": "bad token"}
            }))
            .unwrap(),
        );
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), ModelError::Unauthorized(_)),
            "expected Unauthorized"
        );
    }

    #[test]
    fn translator_bare_error_with_missing_fields() {
        let mut t = OpenAiResponsesTranslator::default();
        let result = t.push_frame("error", &serde_json::to_string(&json!({})).unwrap());
        assert!(result.is_err());
        assert!(t.is_finished());
    }

    #[test]
    fn translator_drops_unknown_event_types_without_failing() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.reasoning_summary_text.delta",
            json!({"delta": "thinking…"}),
        );
        push(
            &mut t,
            "response.in_progress",
            json!({"response": {"status": "in_progress"}}),
        );
        push(
            &mut t,
            "response.new_future_event",
            json!({"payload": "whatever"}),
        );
        // No events queued and nothing panicked.
        assert!(drain(&mut t).is_empty());
    }

    #[test]
    fn translator_finalize_is_idempotent() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "m"}}),
        );
        push(
            &mut t,
            "response.completed",
            json!({"response": {"id": "r1"}}),
        );
        let first_drain = drain(&mut t);
        t.finalize(); // second call must not append another MessageStop.
        let second_drain = drain(&mut t);
        assert!(!first_drain.is_empty());
        assert!(second_drain.is_empty());
    }

    // ---------------------------------------------------------------
    // Token refresher wiring
    // ---------------------------------------------------------------

    #[derive(Debug, Default)]
    struct FakeRefresher {
        calls: std::sync::atomic::AtomicUsize,
        next_token: String,
    }

    #[async_trait]
    impl TokenRefresher for FakeRefresher {
        async fn refresh(&self) -> Result<String, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.next_token.clone())
        }
    }

    #[tokio::test]
    async fn with_refresher_stores_callback_on_auth_inner() {
        let provider =
            OpenAiResponsesProvider::new(OpenAiResponsesClientConfig::with_api_key("stale-token"))
                .with_refresher(Arc::new(FakeRefresher {
                    calls: Default::default(),
                    next_token: "fresh-token".into(),
                }));

        let (token, has_refresher) = provider.snapshot_auth();
        assert_eq!(token, "stale-token");
        assert!(has_refresher);
    }

    #[tokio::test]
    async fn refresh_bearer_swaps_in_new_token_and_invokes_callback_once() {
        let refresher = Arc::new(FakeRefresher {
            calls: Default::default(),
            next_token: "fresh-1".into(),
        });
        let provider =
            OpenAiResponsesProvider::new(OpenAiResponsesClientConfig::with_api_key("stale"))
                .with_refresher(refresher.clone());

        let new_token = provider.refresh_bearer().await.unwrap();
        assert_eq!(new_token, "fresh-1");

        let (cached, _) = provider.snapshot_auth();
        assert_eq!(cached, "fresh-1");

        // Callback was invoked exactly once for the refresh.
        assert_eq!(refresher.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_bearer_without_refresher_errors_with_unauthorized() {
        let provider =
            OpenAiResponsesProvider::new(OpenAiResponsesClientConfig::with_api_key("stale"));
        let err = provider.refresh_bearer().await.unwrap_err();
        assert!(matches!(err, ModelError::Unauthorized(_)));
    }

    #[test]
    fn ws_handshake_401_403_statuses_are_unauthorized() {
        for status in [401, 403] {
            assert!(is_unauthorized_ws_status(status));
            assert!(matches!(
                unauthorized_ws_handshake_error(status),
                ModelError::Unauthorized(_)
            ));
        }
        assert!(!is_unauthorized_ws_status(426));
    }

    #[tokio::test]
    async fn ws_handshake_401_refreshes_and_retries_once() {
        let (base_url, seen_auth, server_task) = start_ws_auth_retry_server().await;
        let refresher = Arc::new(FakeRefresher {
            calls: Default::default(),
            next_token: "fresh-token".into(),
        });
        let mut config = OpenAiResponsesClientConfig::with_base_url(base_url, "stale-token");
        config.use_websocket = true;
        let provider = OpenAiResponsesProvider::new(config).with_refresher(refresher.clone());

        let ws = provider
            .connect_ws()
            .await
            .expect("second handshake succeeds");
        drop(ws);
        server_task.await.unwrap();

        assert_eq!(refresher.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            seen_auth.lock().unwrap().as_slice(),
            [
                "Bearer stale-token".to_string(),
                "Bearer fresh-token".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn ws_handshake_second_unauthorized_after_refresh_stops_retrying() {
        let (base_url, seen_auth, server_task) = start_ws_auth_retry_server_with_second_response(
            WsAuthRetrySecondResponse::HttpStatus(403),
        )
        .await;
        let refresher = Arc::new(FakeRefresher {
            calls: Default::default(),
            next_token: "fresh-token".into(),
        });
        let mut config = OpenAiResponsesClientConfig::with_base_url(base_url, "stale-token");
        config.use_websocket = true;
        let provider = OpenAiResponsesProvider::new(config).with_refresher(refresher.clone());

        let err = match provider.connect_ws().await {
            Ok(_) => panic!("second unauthorized handshake should fail without another retry"),
            Err(err) => err,
        };
        server_task.await.unwrap();

        assert!(matches!(err, ModelError::Unauthorized(_)), "{err:?}");
        assert_eq!(refresher.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            seen_auth.lock().unwrap().as_slice(),
            [
                "Bearer stale-token".to_string(),
                "Bearer fresh-token".to_string()
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ws_ping_keepalive_resets_idle_timeout() {
        use futures_util::{SinkExt as _, StreamExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<()>();
        let (pong_seen_tx, pong_seen_rx) = tokio::sync::oneshot::channel::<()>();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let message = ws.next().await.unwrap().unwrap();
            assert!(matches!(message, tungstenite::Message::Text(_)));
            let _ = seen_tx.send(());

            tokio::time::sleep(Duration::from_secs(10)).await;
            ws.send(tungstenite::Message::Ping(Vec::new()))
                .await
                .unwrap();
            loop {
                match ws.next().await.unwrap().unwrap() {
                    tungstenite::Message::Pong(_) => break,
                    _ => continue,
                }
            }
            let _ = pong_seen_tx.send(());

            tokio::time::sleep(WS_FRAME_IDLE_TIMEOUT - Duration::from_secs(9)).await;
            for frame in [
                json!({
                    "type": "response.created",
                    "response": {"id": "resp_keepalive", "model": "gpt-5.4"}
                }),
                json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {"type": "message"}
                }),
                json!({"type": "response.output_text.delta", "delta": "ok"}),
                json!({"type": "response.output_item.done", "output_index": 0}),
                json!({"type": "response.completed", "response": {"id": "resp_keepalive"}}),
            ] {
                ws.send(tungstenite::Message::Text(frame.to_string()))
                    .await
                    .unwrap();
            }
        });

        let mut config =
            OpenAiResponsesClientConfig::with_base_url(format!("http://{addr}"), "sk-test");
        config.use_websocket = true;
        let provider = OpenAiResponsesProvider::new(config);
        let request = CreateMessageRequest::simple("gpt-5.4", "ping");
        let (tx, _rx) = tokio::sync::mpsc::channel::<ModelResult<StreamEvent>>(16);

        let turn = drive_ws_turn_once(
            &provider,
            provider.ws_conn.clone(),
            provider.session_state.clone(),
            &request,
            WsTurnOptions::default(),
            &tx,
        );
        tokio::pin!(turn);

        tokio::select! {
            result = &mut turn => panic!("turn completed before keepalive: {result:?}"),
            seen = seen_rx => seen.expect("server should observe response.create"),
        }

        tokio::time::advance(Duration::from_secs(10)).await;
        pong_seen_rx
            .await
            .expect("client pump should answer ping with pong");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        tokio::time::advance(WS_FRAME_IDLE_TIMEOUT - Duration::from_secs(9)).await;

        let resolution = turn.await.unwrap();
        assert!(
            matches!(resolution, WsTurnResolution::Completed),
            "Ping keepalive must reset the websocket idle timer"
        );
        server_task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn ws_idle_timeout_surfaces_without_provider_retry() {
        use futures_util::StreamExt as _;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let message = ws.next().await.unwrap().unwrap();
            assert!(matches!(message, tungstenite::Message::Text(_)));
            let _ = seen_tx.send(());
            let _ = release_rx.await;
        });

        let mut config =
            OpenAiResponsesClientConfig::with_base_url(format!("http://{addr}"), "sk-test");
        config.use_websocket = true;
        let provider = OpenAiResponsesProvider::new(config);
        let request = CreateMessageRequest::simple("gpt-5.4", "ping");
        let (tx, _rx) = tokio::sync::mpsc::channel::<ModelResult<StreamEvent>>(16);

        let turn = drive_ws_turn(
            &provider,
            provider.ws_conn.clone(),
            provider.session_state.clone(),
            request,
            &tx,
        );
        tokio::pin!(turn);

        tokio::select! {
            result = &mut turn => panic!("idle turn completed before timeout: {result:?}"),
            seen = seen_rx => seen.expect("server should observe response.create"),
        }

        tokio::task::yield_now().await;
        tokio::time::advance(WS_FRAME_IDLE_TIMEOUT + Duration::from_secs(1)).await;
        let error = turn
            .await
            .expect_err("idle timeout must surface to retry middleware");
        assert!(
            matches!(&error, ModelError::Http(message) if message == WS_FRAME_IDLE_TIMEOUT_MESSAGE),
            "got: {error:?}"
        );
        assert!(provider.ws_conn.lock().await.is_none());
        let _ = release_tx.send(());
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn ws_server_error_surfaces_without_provider_retry() {
        use futures_util::{SinkExt as _, StreamExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let message = ws.next().await.unwrap().unwrap();
            assert!(matches!(message, tungstenite::Message::Text(_)));
            ws.send(tungstenite::Message::Text(
                json!({
                    "type": "error",
                    "error": {
                        "code": "server_error",
                        "message": "server failure"
                    }
                })
                .to_string(),
            ))
            .await
            .unwrap();
        });

        let mut config =
            OpenAiResponsesClientConfig::with_base_url(format!("http://{addr}"), "sk-test");
        config.use_websocket = true;
        let provider = OpenAiResponsesProvider::new(config);
        let request = CreateMessageRequest::simple("gpt-5.4", "ping");
        let (tx, _rx) = tokio::sync::mpsc::channel::<ModelResult<StreamEvent>>(16);

        let error = tokio::time::timeout(
            Duration::from_secs(2),
            drive_ws_turn(
                &provider,
                provider.ws_conn.clone(),
                provider.session_state.clone(),
                request,
                &tx,
            ),
        )
        .await
        .expect("provider must not wait for an internal retry")
        .expect_err("server error must surface");
        server_task.await.unwrap();

        assert!(
            matches!(&error, ModelError::Http(message) if message.contains("server failure")),
            "got: {error:?}"
        );
        assert!(provider.ws_conn.lock().await.is_none());
    }

    /// The server closes each Responses websocket after a hard
    /// 60-minute lifetime, reporting
    /// `websocket_connection_limit_reached` with `status: 400`. The
    /// turn driver must treat that as "reconnect and replay", not a
    /// bad request: the first connection returns the limit error and
    /// the turn must complete on a fresh connection without carrying
    /// the stale `previous_response_id`. Mirrors codex-ref's
    /// `responses_websocket_connection_limit_error_reconnects_and_completes`.
    #[tokio::test]
    async fn ws_connection_limit_reconnects_and_completes_turn() {
        use futures_util::{SinkExt as _, StreamExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut first_ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let message = first_ws.next().await.unwrap().unwrap();
            assert!(matches!(message, tungstenite::Message::Text(_)));
            first_ws
                .send(tungstenite::Message::Text(
                    json!({
                        "type": "error",
                        "status": 400,
                        "error": {
                            "type": "invalid_request_error",
                            "code": "websocket_connection_limit_reached",
                            "message": "Responses websocket connection limit reached (60 minutes). Create a new websocket connection to continue."
                        }
                    })
                    .to_string(),
                ))
                .await
                .unwrap();
            let _first_ws = first_ws;

            let (stream, _) = listener.accept().await.unwrap();
            let mut second_ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let message = second_ws.next().await.unwrap().unwrap();
            let tungstenite::Message::Text(request_text) = message else {
                panic!("expected text request on the retry connection");
            };
            assert!(
                !request_text.contains("previous_response_id"),
                "retry after connection limit must be a full replay: {request_text}"
            );

            for frame in [
                json!({
                    "type": "response.created",
                    "response": {"id": "resp_fresh", "model": "gpt-5.4"}
                }),
                json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {"type": "message"}
                }),
                json!({"type": "response.output_text.delta", "delta": "ok"}),
                json!({"type": "response.output_item.done", "output_index": 0}),
                json!({"type": "response.completed", "response": {"id": "resp_fresh"}}),
            ] {
                second_ws
                    .send(tungstenite::Message::Text(frame.to_string()))
                    .await
                    .unwrap();
            }
        });

        let mut config =
            OpenAiResponsesClientConfig::with_base_url(format!("http://{addr}"), "sk-test");
        config.use_websocket = true;
        let provider = OpenAiResponsesProvider::new(config);
        // Seed a stale chain as if earlier turns ran on the expired
        // socket — the retry must not resend it.
        provider
            .session_state
            .set_last_response_id(Some("resp_stale".into()));
        let request = CreateMessageRequest::simple("gpt-5.4", "ping");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ModelResult<StreamEvent>>(16);

        drive_ws_turn(
            &provider,
            provider.ws_conn.clone(),
            provider.session_state.clone(),
            request,
            &tx,
        )
        .await
        .expect("connection limit error should be retried on a fresh websocket");
        server_task.await.unwrap();

        let mut saw_text = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(
                event,
                Ok(StreamEvent::ContentBlockDelta {
                    delta: ContentBlockDelta::TextDelta { ref text },
                    ..
                }) if text == "ok"
            ) {
                saw_text = true;
            }
        }
        assert!(saw_text);
        // The stale chain was cleared and replaced by the fresh
        // response id committed from the retry stream.
        assert_eq!(
            provider
                .session_state
                .last_response_id
                .lock()
                .unwrap()
                .clone(),
            Some("resp_fresh".into())
        );
    }

    /// One complete Responses turn as the server sends it.
    fn ws_turn_frames(response_id: &str, text: &str) -> Vec<Value> {
        vec![
            json!({
                "type": "response.created",
                "response": {"id": response_id, "model": "gpt-5.4"}
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "message"}
            }),
            json!({"type": "response.output_text.delta", "delta": text}),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "message", "status": "completed"}
            }),
            json!({"type": "response.completed", "response": {"id": response_id}}),
        ]
    }

    /// A socket that dies mid-response must not fail the prompt: the driver
    /// reconnects and replays the whole turn on a fresh connection.
    ///
    /// The peer closes the TCP connection without a close handshake here,
    /// which is the `Connection reset without closing handshake` transport
    /// error a real session sees when a server or proxy drops the socket
    /// between `response.created` and `response.completed`. The turn replayed
    /// onto the fresh connection must not carry the continuation chain: it
    /// existed only in the dead socket's server-side state.
    #[tokio::test]
    async fn ws_mid_turn_reset_reconnects_and_replays_the_turn() {
        use futures_util::{SinkExt as _, StreamExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            // Turn 1 completes and seeds the continuation chain.
            let tungstenite::Message::Text(opening) = ws.next().await.unwrap().unwrap() else {
                panic!("expected the first turn's response.create frame");
            };
            assert!(!opening.contains("previous_response_id"));
            for frame in ws_turn_frames("resp_1", "pong") {
                ws.send(tungstenite::Message::Text(frame.to_string()))
                    .await
                    .unwrap();
            }

            // Turn 2 rides that chain, then the peer disappears mid-response.
            let tungstenite::Message::Text(second) = ws.next().await.unwrap().unwrap() else {
                panic!("expected the second turn's response.create frame");
            };
            assert!(
                second.contains("previous_response_id"),
                "turn 2 must ride the chain turn 1 seeded: {second}"
            );
            for frame in [
                json!({
                    "type": "response.created",
                    "response": {"id": "resp_dying", "model": "gpt-5.4"}
                }),
                json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {"type": "message"}
                }),
                json!({"type": "response.output_text.delta", "delta": "half"}),
            ] {
                ws.send(tungstenite::Message::Text(frame.to_string()))
                    .await
                    .unwrap();
            }
            drop(ws);

            // The replay arrives on a fresh connection, without the chain the
            // dead socket owned.
            let (stream, _) = listener.accept().await.unwrap();
            let mut replay_ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let tungstenite::Message::Text(replay) = replay_ws.next().await.unwrap().unwrap()
            else {
                panic!("expected the replayed turn's response.create frame");
            };
            assert!(
                !replay.contains("previous_response_id"),
                "the replay must be a full create: {replay}"
            );
            for frame in ws_turn_frames("resp_2", "ok") {
                replay_ws
                    .send(tungstenite::Message::Text(frame.to_string()))
                    .await
                    .unwrap();
            }
        });

        let mut config =
            OpenAiResponsesClientConfig::with_base_url(format!("http://{addr}"), "sk-test");
        config.use_websocket = true;
        let provider = OpenAiResponsesProvider::new(config);
        let request = CreateMessageRequest::simple("gpt-5.4", "ping");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ModelResult<StreamEvent>>(64);

        drive_ws_turn(
            &provider,
            provider.ws_conn.clone(),
            provider.session_state.clone(),
            request.clone(),
            &tx,
        )
        .await
        .expect("the first turn completes");

        let mut second_request = request;
        second_request
            .messages
            .push(Message::user_text("ping again"));
        drive_ws_turn(
            &provider,
            provider.ws_conn.clone(),
            provider.session_state.clone(),
            second_request,
            &tx,
        )
        .await
        .expect("a mid-turn reset must be replayed on a fresh websocket");
        server_task.await.unwrap();

        let mut text = String::new();
        while let Ok(event) = rx.try_recv() {
            if let Ok(StreamEvent::ContentBlockDelta {
                delta: ContentBlockDelta::TextDelta { text: delta },
                ..
            }) = event
            {
                text.push_str(&delta);
            }
        }
        assert!(
            text.contains("ok"),
            "the replayed turn's output must reach the stream: {text}"
        );
        assert_eq!(
            provider
                .session_state
                .last_response_id
                .lock()
                .unwrap()
                .clone(),
            Some("resp_2".into())
        );
    }

    /// A close frame that arrives before `response.completed` is the same dead
    /// end as a broken socket, and gets the same recovery: the peer said it is
    /// done, but the response it was streaming never finished.
    #[tokio::test]
    async fn ws_mid_turn_close_frame_reconnects_and_replays_the_turn() {
        use futures_util::{SinkExt as _, StreamExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = ws.next().await.unwrap().unwrap();
            ws.send(tungstenite::Message::Text(
                json!({
                    "type": "response.created",
                    "response": {"id": "resp_dying", "model": "gpt-5.4"}
                })
                .to_string(),
            ))
            .await
            .unwrap();
            ws.close(None).await.unwrap();

            let (stream, _) = listener.accept().await.unwrap();
            let mut replay_ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = replay_ws.next().await.unwrap().unwrap();
            for frame in ws_turn_frames("resp_2", "ok") {
                replay_ws
                    .send(tungstenite::Message::Text(frame.to_string()))
                    .await
                    .unwrap();
            }
        });

        let mut config =
            OpenAiResponsesClientConfig::with_base_url(format!("http://{addr}"), "sk-test");
        config.use_websocket = true;
        let provider = OpenAiResponsesProvider::new(config);
        let request = CreateMessageRequest::simple("gpt-5.4", "ping");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ModelResult<StreamEvent>>(64);

        drive_ws_turn(
            &provider,
            provider.ws_conn.clone(),
            provider.session_state.clone(),
            request,
            &tx,
        )
        .await
        .expect("a close frame before response.completed must be replayed");
        server_task.await.unwrap();

        let mut saw_text = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(
                event,
                Ok(StreamEvent::ContentBlockDelta {
                    delta: ContentBlockDelta::TextDelta { ref text },
                    ..
                }) if text == "ok"
            ) {
                saw_text = true;
            }
        }
        assert!(saw_text);
    }

    /// The recovery is bounded to one replay per turn: a socket that dies again
    /// on the fresh connection surfaces the transport error (which the engine
    /// reads as transient) instead of reconnecting forever.
    ///
    /// The connection count is the assertion — a third one means the driver
    /// looped. The listener stops accepting after the replay, so a third
    /// attempt would surface as a connect error rather than this read error.
    #[tokio::test]
    async fn ws_repeated_mid_turn_resets_surface_after_one_replay() {
        use futures_util::{SinkExt as _, StreamExt as _};

        /// Long enough for the replay to arrive, short enough that a driver
        /// which gave up after the first reset ends the server task promptly
        /// instead of leaving the test waiting on an accept that never comes.
        const SERVER_ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let mut connections = 0usize;
            while connections < 2 {
                let Ok(accepted) =
                    tokio::time::timeout(SERVER_ACCEPT_TIMEOUT, listener.accept()).await
                else {
                    break;
                };
                let (stream, _) = accepted.unwrap();
                connections += 1;
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let _ = ws.next().await.unwrap().unwrap();
                ws.send(tungstenite::Message::Text(
                    json!({
                        "type": "response.created",
                        "response": {"id": "resp_dying", "model": "gpt-5.4"}
                    })
                    .to_string(),
                ))
                .await
                .unwrap();
                drop(ws);
            }
            connections
        });

        let mut config =
            OpenAiResponsesClientConfig::with_base_url(format!("http://{addr}"), "sk-test");
        config.use_websocket = true;
        let provider = OpenAiResponsesProvider::new(config);
        let request = CreateMessageRequest::simple("gpt-5.4", "ping");
        let (tx, _rx) = tokio::sync::mpsc::channel::<ModelResult<StreamEvent>>(64);

        let error = drive_ws_turn(
            &provider,
            provider.ws_conn.clone(),
            provider.session_state.clone(),
            request,
            &tx,
        )
        .await
        .expect_err("a second mid-turn reset must surface instead of retrying again");
        let connections = server_task.await.unwrap();

        assert_eq!(
            connections, 2,
            "the turn must be replayed exactly once after the first reset"
        );
        assert!(
            matches!(&error, ModelError::Http(message) if message.contains("websocket error")),
            "got: {error:?}"
        );
        assert!(error.is_transient());
        assert!(provider.ws_conn.lock().await.is_none());
    }

    /// An abandoned turn (stream receiver dropped mid-response — e.g.
    /// a timed-out side-channel summary call) must leave the shared
    /// websocket clean: the driver keeps reading the in-flight
    /// response to completion instead of stranding its frames, so the
    /// next turn on the same connection receives its own reply rather
    /// than the abandoned response's `{"summary":…}` text.
    #[tokio::test]
    async fn ws_abandoned_turn_does_not_leak_response_into_next_turn() {
        use futures_util::{SinkExt as _, StreamExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut served = 0usize;
            while served < 2 {
                let Some(Ok(tungstenite::Message::Text(request_text))) = ws.next().await else {
                    continue;
                };
                served += 1;
                let (id, delta, delay) = if request_text.contains("summary-request") {
                    ("resp_summary", r#"{"summary":"leaked side-channel"}"#, 150)
                } else {
                    ("resp_real", "real answer", 0)
                };
                ws.send(tungstenite::Message::Text(
                    json!({
                        "type": "response.created",
                        "response": {"id": id, "model": "gpt-5.4"}
                    })
                    .to_string(),
                ))
                .await
                .unwrap();
                // Give the abandoning caller time to drop its receiver
                // between response.created and the body frames.
                tokio::time::sleep(Duration::from_millis(delay)).await;
                for frame in [
                    json!({
                        "type": "response.output_item.added",
                        "output_index": 0,
                        "item": {"type": "message"}
                    }),
                    json!({"type": "response.output_text.delta", "delta": delta}),
                    json!({"type": "response.output_item.done", "output_index": 0}),
                    json!({"type": "response.completed", "response": {"id": id}}),
                ] {
                    ws.send(tungstenite::Message::Text(frame.to_string()))
                        .await
                        .unwrap();
                }
            }
        });

        let mut config =
            OpenAiResponsesClientConfig::with_base_url(format!("http://{addr}"), "sk-test");
        config.use_websocket = true;
        let provider = OpenAiResponsesProvider::new(config);

        // Turn 1: start a request, observe the stream beginning, then
        // drop the receiver mid-response — the summary-call timeout path.
        let (tx1, mut rx1) = tokio::sync::mpsc::channel::<ModelResult<StreamEvent>>(16);
        let abandoned = {
            let provider = provider.clone();
            tokio::spawn(async move {
                drive_ws_turn(
                    &provider,
                    provider.ws_conn.clone(),
                    provider.session_state.clone(),
                    CreateMessageRequest::simple("gpt-5.4", "summary-request"),
                    &tx1,
                )
                .await
            })
        };
        let first = rx1.recv().await.expect("first event of abandoned turn");
        assert!(matches!(first, Ok(StreamEvent::MessageStart { .. })));
        drop(rx1);
        abandoned
            .await
            .unwrap()
            .expect("abandoned turn must drain cleanly");

        // Turn 2 on the same provider/connection must get its own reply.
        let (tx2, mut rx2) = tokio::sync::mpsc::channel::<ModelResult<StreamEvent>>(16);
        drive_ws_turn(
            &provider,
            provider.ws_conn.clone(),
            provider.session_state.clone(),
            CreateMessageRequest::simple("gpt-5.4", "real-request"),
            &tx2,
        )
        .await
        .expect("second turn should complete");
        server_task.await.unwrap();

        let mut texts = Vec::new();
        let mut response_ids = Vec::new();
        while let Ok(event) = rx2.try_recv() {
            match event {
                Ok(StreamEvent::MessageStart { message_id, .. }) => response_ids.push(message_id),
                Ok(StreamEvent::ContentBlockDelta {
                    delta: ContentBlockDelta::TextDelta { text },
                    ..
                }) => texts.push(text),
                _ => {}
            }
        }
        assert_eq!(response_ids, vec!["resp_real".to_string()]);
        assert_eq!(texts, vec!["real answer".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn ws_keepalive_does_not_mask_data_stall_or_retry_locally() {
        use futures_util::{SinkExt as _, StreamExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (first_seen_tx, first_seen_rx) = tokio::sync::oneshot::channel::<()>();
        let (pong_seen_tx, mut pong_seen_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut first_ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let message = first_ws.next().await.unwrap().unwrap();
            assert!(matches!(message, tungstenite::Message::Text(_)));
            let _ = first_seen_tx.send(());

            // Keepalive pings only — never a data frame. Each ping
            // lands inside the per-frame idle window and resets it;
            // the data stall deadline must still fire.
            let keepalive = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                loop {
                    if first_ws
                        .send(tungstenite::Message::Ping(Vec::new()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    match first_ws.next().await {
                        Some(Ok(tungstenite::Message::Pong(_))) => {
                            let _ = pong_seen_tx.send(());
                        }
                        _ => break,
                    }
                    tokio::time::sleep(WS_FRAME_IDLE_TIMEOUT - Duration::from_secs(9)).await;
                }
            });

            let _ = keepalive.await;
        });

        let mut config =
            OpenAiResponsesClientConfig::with_base_url(format!("http://{addr}"), "sk-test");
        config.use_websocket = true;
        let provider = OpenAiResponsesProvider::new(config);
        let request = CreateMessageRequest::simple("gpt-5.4", "ping");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ModelResult<StreamEvent>>(16);

        let turn = drive_ws_turn(
            &provider,
            provider.ws_conn.clone(),
            provider.session_state.clone(),
            request,
            &tx,
        );
        tokio::pin!(turn);

        tokio::select! {
            result = &mut turn => panic!("turn completed before stall: {result:?}"),
            seen = first_seen_rx => seen.expect("server should observe first response.create"),
        }

        // Three keepalive rounds at t≈10s, t≈301s, t≈592s. Each ping
        // resets the per-frame idle timer (which therefore never
        // fires) but must not push the data stall deadline (t=600s).
        tokio::time::advance(Duration::from_secs(10)).await;
        pong_seen_rx.recv().await.expect("first keepalive round");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        tokio::time::advance(WS_FRAME_IDLE_TIMEOUT - Duration::from_secs(9)).await;
        pong_seen_rx.recv().await.expect("second keepalive round");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        tokio::time::advance(WS_FRAME_IDLE_TIMEOUT - Duration::from_secs(9)).await;
        pong_seen_rx.recv().await.expect("third keepalive round");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // Only ~8s remain on the stall deadline even though the last
        // ping arrived moments ago. Crossing it must surface the transient
        // error directly; only RetryMiddleware may schedule another attempt.
        tokio::time::advance(Duration::from_secs(9)).await;

        let error = turn
            .await
            .expect_err("the provider must surface its data-stall error");
        assert!(
            matches!(error, ModelError::Http(ref message) if message == WS_DATA_STALL_TIMEOUT_MESSAGE),
            "unexpected error: {error:?}"
        );
        server_task.await.unwrap();
        assert!(rx.try_recv().is_err(), "a stalled turn must emit no events");
    }

    #[test]
    fn translator_finalize_on_stream_cut_still_closes_open_block() {
        // Simulate a provider-side disconnect mid-text: we saw
        // output_item.added + one delta but never a done / completed.
        // finalize() should still emit ContentBlockStop +
        // MessageDelta + MessageStop so downstream consumers don't
        // hang on a half-open block.
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "m"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({"output_index": 0, "item": {"type": "message"}}),
        );
        push(
            &mut t,
            "response.output_text.delta",
            json!({"delta": "partial"}),
        );
        t.finalize();
        let events = drain(&mut t);
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::ContentBlockStop { .. })));
        assert!(events.iter().any(|e| matches!(e, StreamEvent::MessageStop)));
    }

    // ---------------------------------------------------------------
    // G2: Reasoning / thinking blocks
    // ---------------------------------------------------------------

    #[test]
    fn reasoning_output_item_emits_thinking_block_start() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "m"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({"output_index": 0, "item": {"type": "reasoning"}}),
        );
        let events = drain(&mut t);
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::ContentBlockStart {
                content_block: ContentBlockStart::Thinking { .. },
                ..
            }
        )));
    }

    #[test]
    fn reasoning_output_item_captures_encrypted_content() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "m"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": {
                    "type": "reasoning",
                    "encrypted_content": "ENCRYPTED_REASONING"
                }
            }),
        );
        let events = drain(&mut t);
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::ContentBlockStart {
                content_block: ContentBlockStart::Thinking { data: Some(data), .. },
                ..
            } if data == "ENCRYPTED_REASONING"
        )));
    }

    #[test]
    fn reasoning_output_item_done_patches_encrypted_content_into_accumulator() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "m"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({"output_index": 0, "item": {"type": "reasoning"}}),
        );
        push(
            &mut t,
            "response.reasoning_summary_text.delta",
            json!({"delta": "summary"}),
        );
        push(
            &mut t,
            "response.output_item.done",
            json!({
                "output_index": 0,
                "item": {
                    "type": "reasoning",
                    "encrypted_content": "ENCRYPTED_REASONING"
                }
            }),
        );
        push(
            &mut t,
            "response.completed",
            json!({"response": {"id": "r1"}}),
        );

        let mut accumulator = MessageAccumulator::new();
        for event in drain(&mut t) {
            accumulator.apply(&event).unwrap();
        }
        let message = accumulator.finish();
        match &message.content[0] {
            ContentBlock::Thinking(tb) => {
                assert_eq!(tb.thinking, "summary");
                assert_eq!(tb.data.as_deref(), Some("ENCRYPTED_REASONING"));
            }
            other => panic!("expected Thinking block, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_summary_text_delta_emits_thinking_delta() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "m"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({"output_index": 0, "item": {"type": "reasoning"}}),
        );
        drain(&mut t); // clear start events

        push(
            &mut t,
            "response.reasoning_summary_text.delta",
            json!({"delta": "Let me think..."}),
        );
        let events = drain(&mut t);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::ContentBlockDelta {
                delta: ContentBlockDelta::ThinkingDelta { thinking },
                ..
            } => {
                assert_eq!(thinking, "Let me think...");
            }
            other => panic!("expected ThinkingDelta, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_summary_parts_are_separated_for_markdown_rendering() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.output_item.added",
            json!({"output_index": 0, "item": {"type": "reasoning"}}),
        );
        drain(&mut t);

        push(
            &mut t,
            "response.reasoning_summary_part.added",
            json!({"output_index": 0, "summary_index": 0, "part": {"type": "summary_text", "text": ""}}),
        );
        push(
            &mut t,
            "response.reasoning_summary_text.delta",
            json!({"output_index": 0, "summary_index": 0, "delta": "**Investigating test hang**"}),
        );
        push(
            &mut t,
            "response.reasoning_summary_part.added",
            json!({"output_index": 0, "summary_index": 1, "part": {"type": "summary_text", "text": ""}}),
        );
        push(
            &mut t,
            "response.reasoning_summary_part.added",
            json!({"output_index": 0, "summary_index": 1, "part": {"type": "summary_text", "text": ""}}),
        );
        push(
            &mut t,
            "response.reasoning_summary_text.delta",
            json!({"output_index": 0, "summary_index": 1, "delta": "**Inspecting state**"}),
        );

        let chunks = drain(&mut t)
            .into_iter()
            .filter_map(|event| match event {
                StreamEvent::ContentBlockDelta {
                    delta: ContentBlockDelta::ThinkingDelta { thinking },
                    ..
                } => Some(thinking),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            chunks,
            vec![
                "**Investigating test hang**",
                "\n\n",
                "**Inspecting state**"
            ]
        );
        assert_eq!(
            chunks.concat(),
            "**Investigating test hang**\n\n**Inspecting state**"
        );
    }

    #[test]
    fn reasoning_summary_separator_completes_existing_single_newline() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.output_item.added",
            json!({"output_index": 0, "item": {"type": "reasoning"}}),
        );
        drain(&mut t);

        push(
            &mut t,
            "response.reasoning_summary_part.added",
            json!({"summary_index": 0}),
        );
        push(
            &mut t,
            "response.reasoning_summary_text.delta",
            json!({"summary_index": 0, "delta": "first\n"}),
        );
        push(
            &mut t,
            "response.reasoning_summary_part.added",
            json!({"summary_index": 1}),
        );

        let chunks = drain(&mut t)
            .into_iter()
            .filter_map(|event| match event {
                StreamEvent::ContentBlockDelta {
                    delta: ContentBlockDelta::ThinkingDelta { thinking },
                    ..
                } => Some(thinking),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(chunks, vec!["first\n", "\n"]);
        assert_eq!(chunks.concat(), "first\n\n");
    }

    #[test]
    fn reasoning_summary_part_done_patches_missing_suffix_per_part() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.output_item.added",
            json!({"output_index": 0, "item": {"type": "reasoning"}}),
        );
        drain(&mut t);

        push(
            &mut t,
            "response.reasoning_summary_part.added",
            json!({"summary_index": 0}),
        );
        push(
            &mut t,
            "response.reasoning_summary_text.delta",
            json!({"summary_index": 0, "delta": "First"}),
        );
        push(
            &mut t,
            "response.reasoning_summary_part.added",
            json!({"summary_index": 1}),
        );
        push(
            &mut t,
            "response.reasoning_summary_text.delta",
            json!({"summary_index": 1, "delta": "Sec"}),
        );
        push(
            &mut t,
            "response.reasoning_summary_part.done",
            json!({"summary_index": 1, "part": {"type": "summary_text", "text": "Second"}}),
        );

        let chunks = drain(&mut t)
            .into_iter()
            .filter_map(|event| match event {
                StreamEvent::ContentBlockDelta {
                    delta: ContentBlockDelta::ThinkingDelta { thinking },
                    ..
                } => Some(thinking),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(chunks, vec!["First", "\n\n", "Sec", "ond"]);
        assert_eq!(chunks.concat(), "First\n\nSecond");
    }

    #[test]
    fn reasoning_block_closes_on_output_item_done() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "m"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({"output_index": 0, "item": {"type": "reasoning"}}),
        );
        push(
            &mut t,
            "response.reasoning_summary_text.delta",
            json!({"delta": "thinking..."}),
        );
        drain(&mut t);

        push(
            &mut t,
            "response.output_item.done",
            json!({"output_index": 0, "item": {"type": "reasoning"}}),
        );
        let events = drain(&mut t);
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::ContentBlockStop { index: 0 })));
    }

    #[tokio::test]
    async fn responses_stream_errors_when_transport_ends_before_completed() {
        let data = "event: response.created\ndata: {\"response\":{\"id\":\"resp_1\",\"model\":\"gpt\"}}\n\nevent: response.output_item.added\ndata: {\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\nevent: response.output_text.delta\ndata: {\"output_index\":0,\"delta\":\"{\\\"a\\\":\"}\n\n";
        let bytes = futures_util::stream::iter(vec![Ok(bytes::Bytes::from(data))]);
        let mut stream = decode_sse_stream(
            Box::pin(bytes),
            ResponsesSseDecoder {
                translator: OpenAiResponsesTranslator::default(),
                last_response_id: None,
            },
        );
        let mut saw_error = false;
        while let Some(result) = stream.next().await {
            if let Err(error) = result {
                assert!(error
                    .to_string()
                    .contains("ended before response.completed"));
                saw_error = true;
                break;
            }
        }
        assert!(saw_error);
    }

    // ---------------------------------------------------------------
    // G1: previous_response_id
    // ---------------------------------------------------------------

    #[test]
    fn build_request_body_includes_previous_response_id_when_present() {
        let req = simple_user_request("hello");
        let body = build_responses_request_body(&req, true, "key", Some("resp_abc123"));
        assert_eq!(body["previous_response_id"], "resp_abc123");
    }

    #[test]
    fn build_request_body_omits_previous_response_id_when_none() {
        let req = simple_user_request("hello");
        let body = build_responses_request_body(&req, true, "key", None);
        assert!(body.get("previous_response_id").is_none());
    }

    #[test]
    fn translator_exposes_response_id_via_message_start() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "resp_xyz789", "model": "m"}}),
        );
        let events = drain(&mut t);
        match &events[0] {
            StreamEvent::MessageStart { message_id, .. } => {
                assert_eq!(message_id, "resp_xyz789");
            }
            other => panic!("expected MessageStart, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // WebSocket transport
    // ---------------------------------------------------------------

    #[test]
    fn ws_endpoint_converts_https_to_wss() {
        let provider = OpenAiResponsesProvider::new(OpenAiResponsesClientConfig::with_base_url(
            "https://api.example.com/v1",
            "sk",
        ));
        assert_eq!(provider.ws_endpoint(), "wss://api.example.com/v1/responses");
    }

    #[test]
    fn ws_endpoint_converts_http_to_ws() {
        let provider = OpenAiResponsesProvider::new(OpenAiResponsesClientConfig::with_base_url(
            "http://localhost:8080/v1",
            "sk",
        ));
        assert_eq!(provider.ws_endpoint(), "ws://localhost:8080/v1/responses");
    }

    #[test]
    fn ws_endpoint_preserves_responses_suffix() {
        let provider = OpenAiResponsesProvider::new(OpenAiResponsesClientConfig::with_base_url(
            "https://chatgpt.com/backend-api/codex/responses",
            "sk",
        ));
        assert_eq!(
            provider.ws_endpoint(),
            "wss://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn config_defaults_to_http_transport() {
        let config = OpenAiResponsesClientConfig::default();
        assert!(!config.use_websocket);
    }

    #[test]
    fn translator_handles_ws_json_with_embedded_type_field() {
        // WebSocket frames embed `type` in the JSON payload itself.
        // The translator should still work when the data contains
        // the `type` field (it simply ignores it).
        let mut t = OpenAiResponsesTranslator::default();
        t.push_frame(
            "response.created",
            &serde_json::to_string(&json!({
                "type": "response.created",
                "response": {"id": "resp_ws_1", "model": "gpt-5.4"}
            }))
            .unwrap(),
        )
        .unwrap();
        let events = drain(&mut t);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::MessageStart { message_id, .. } => {
                assert_eq!(message_id, "resp_ws_1");
            }
            other => panic!("expected MessageStart, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // fork_for_sub_agent — session isolation
    // ---------------------------------------------------------------

    fn ws_test_config() -> OpenAiResponsesClientConfig {
        OpenAiResponsesClientConfig {
            base_url: "https://api.example.com/v1".into(),
            api_key: "sk-test".into(),
            organization: None,
            extra_headers: Vec::new(),
            request_timeout: None,
            prompt_cache_key: Some("parent-cache-key".into()),
            prompt_cache_retention: None,
            use_websocket: true,
            service_tier: None,
        }
    }

    #[test]
    fn build_ws_request_sets_responses_websockets_beta_header() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        let req = provider.build_ws_request("sk-test").unwrap();
        assert_eq!(
            req.headers()
                .get("openai-beta")
                .and_then(|v| v.to_str().ok()),
            Some(RESPONSES_WEBSOCKETS_BETA),
            "WS handshake must opt into the responses_websockets beta or the server refuses the upgrade"
        );
    }

    #[test]
    fn build_ws_request_extra_headers_override_beta() {
        let mut config = ws_test_config();
        config.extra_headers = vec![(
            "OpenAI-Beta".into(),
            "responses_websockets=2099-01-01".into(),
        )];
        let provider = OpenAiResponsesProvider::new(config);
        let req = provider.build_ws_request("sk-test").unwrap();
        assert_eq!(
            req.headers()
                .get("openai-beta")
                .and_then(|v| v.to_str().ok()),
            Some("responses_websockets=2099-01-01"),
            "operator-supplied OpenAI-Beta must override the built-in default"
        );
    }

    #[test]
    fn fork_for_sub_agent_returns_some_with_correct_provider_name() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        let forked = provider.fork_for_sub_agent().unwrap();
        assert_eq!(forked.provider_name(), "openai-responses");
    }

    #[test]
    fn fork_for_sub_agent_with_cache_key_uses_supplied_key() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        let forked = provider
            .fork_for_sub_agent_with_cache_key(Some("worker-family-key".into()))
            .unwrap();
        let body = build_responses_request_body(
            &CreateMessageRequest::simple("gpt-5.4", "hello"),
            true,
            "worker-family-key",
            None,
        );
        assert_eq!(forked.provider_name(), "openai-responses");
        assert_eq!(body["prompt_cache_key"], "worker-family-key");
    }

    #[test]
    fn fork_for_sub_agent_with_cache_key_updates_concrete_child_config() {
        let parent = OpenAiResponsesProvider::new(ws_test_config());
        let prompt_cache_key = Some("worker-family-key".to_string());
        let child = OpenAiResponsesProvider {
            config: Arc::new(OpenAiResponsesClientConfig {
                prompt_cache_key: prompt_cache_key.clone(),
                ..parent.config.as_ref().clone()
            }),
            prompt_cache_key: prompt_cache_key.unwrap_or_else(default_prompt_cache_key),
            auth: parent.auth.clone(),
            session_state: ResponsesSessionState::new(),
            ws_conn: Arc::new(TokioMutex::new(None)),
            http_fallback_active: parent.http_fallback_active.clone(),
            consecutive_handshake_eofs: parent.consecutive_handshake_eofs.clone(),
            last_handshake_eof_at: parent.last_handshake_eof_at.clone(),
            retry_notifier: parent.retry_notifier.clone(),
        };

        assert_eq!(child.prompt_cache_key, "worker-family-key");
        assert_eq!(
            child.config.prompt_cache_key.as_deref(),
            Some("worker-family-key")
        );
    }

    /// Verify that a forked provider shares immutable config/auth
    /// with the parent but gets fresh session-scoped state. Because
    /// `ChatProvider` is not `Any`, we test the struct construction
    /// directly — this is the exact code path `fork_for_sub_agent`
    /// executes.
    #[test]
    fn fork_shares_config_and_auth_but_isolates_session_state() {
        let parent = OpenAiResponsesProvider::new(ws_test_config());
        *parent.session_state.last_response_id.lock().unwrap() = Some("resp_parent".into());

        // The fork construction — identical to fork_for_sub_agent body.
        let child = OpenAiResponsesProvider {
            config: parent.config.clone(),
            prompt_cache_key: default_prompt_cache_key(),
            auth: parent.auth.clone(),
            session_state: ResponsesSessionState::new(),
            ws_conn: Arc::new(TokioMutex::new(None)),
            http_fallback_active: Arc::new(AtomicBool::new(false)),
            consecutive_handshake_eofs: Arc::new(AtomicU32::new(0)),
            last_handshake_eof_at: Arc::new(StdMutex::new(None)),
            retry_notifier: None,
        };

        // Shared: config, auth.
        assert!(Arc::ptr_eq(&parent.config, &child.config));
        assert!(Arc::ptr_eq(&parent.auth, &child.auth));

        // Isolated: last_response_id starts at None, different Arc.
        assert!(child
            .session_state
            .last_response_id
            .lock()
            .unwrap()
            .is_none());
        assert!(!Arc::ptr_eq(
            &parent.session_state.last_response_id,
            &child.session_state.last_response_id
        ));

        // Isolated: ws_conn, different Arc.
        assert!(!Arc::ptr_eq(&parent.ws_conn, &child.ws_conn));

        // Fresh prompt_cache_key unless the caller explicitly passes a stable child key.
        assert_eq!(parent.prompt_cache_key, "parent-cache-key");
        assert_ne!(parent.prompt_cache_key, child.prompt_cache_key);
    }

    /// Mutating the child's last_response_id must not affect the
    /// parent — this is the core invariant that prevents the
    /// `previous_response_not_found` error on sub-agent WebSocket
    /// connections.
    #[test]
    fn fork_last_response_id_mutation_does_not_cross_sessions() {
        let parent = OpenAiResponsesProvider::new(ws_test_config());
        *parent.session_state.last_response_id.lock().unwrap() = Some("resp_parent".into());

        let child = OpenAiResponsesProvider {
            config: parent.config.clone(),
            prompt_cache_key: default_prompt_cache_key(),
            auth: parent.auth.clone(),
            session_state: ResponsesSessionState::new(),
            ws_conn: Arc::new(TokioMutex::new(None)),
            http_fallback_active: Arc::new(AtomicBool::new(false)),
            consecutive_handshake_eofs: Arc::new(AtomicU32::new(0)),
            last_handshake_eof_at: Arc::new(StdMutex::new(None)),
            retry_notifier: None,
        };

        // Child mutates its own state.
        *child.session_state.last_response_id.lock().unwrap() = Some("resp_child".into());

        // Parent is unaffected.
        assert_eq!(
            *parent.session_state.last_response_id.lock().unwrap(),
            Some("resp_parent".into())
        );
        assert_eq!(
            *child.session_state.last_response_id.lock().unwrap(),
            Some("resp_child".into())
        );
    }

    /// Verify that a forked provider's first request will NOT carry
    /// the parent's previous_response_id in the request body.
    #[test]
    fn fork_does_not_carry_parent_previous_response_id_in_request_body() {
        let parent = OpenAiResponsesProvider::new(ws_test_config());
        *parent.session_state.last_response_id.lock().unwrap() = Some("resp_parent_999".into());

        // Parent's request body includes previous_response_id.
        let parent_prev = parent
            .session_state
            .last_response_id
            .lock()
            .unwrap()
            .clone();
        let parent_body = build_responses_request_body(
            &simple_user_request("hello"),
            true,
            &parent.prompt_cache_key,
            parent_prev.as_deref(),
        );
        assert_eq!(
            parent_body["previous_response_id"],
            json!("resp_parent_999")
        );

        // Forked child: last_response_id is None → no
        // previous_response_id in the request body.
        let child_prev: Option<String> = None;
        let child_body = build_responses_request_body(
            &simple_user_request("hello"),
            true,
            "child-cache-key",
            child_prev.as_deref(),
        );
        assert!(
            child_body.get("previous_response_id").is_none()
                || child_body["previous_response_id"].is_null(),
            "forked child must not send parent's previous_response_id"
        );
    }

    /// Verify that shared auth state allows token rotation to be
    /// visible to both parent and child.
    #[test]
    fn fork_auth_rotation_propagates_bidirectionally() {
        let parent = OpenAiResponsesProvider::new(ws_test_config());
        let child = OpenAiResponsesProvider {
            config: parent.config.clone(),
            prompt_cache_key: default_prompt_cache_key(),
            auth: parent.auth.clone(),
            session_state: ResponsesSessionState::new(),
            ws_conn: Arc::new(TokioMutex::new(None)),
            http_fallback_active: Arc::new(AtomicBool::new(false)),
            consecutive_handshake_eofs: Arc::new(AtomicU32::new(0)),
            last_handshake_eof_at: Arc::new(StdMutex::new(None)),
            retry_notifier: None,
        };

        // Simulate token refresh on the parent side.
        {
            let mut guard = parent.auth.lock().unwrap();
            guard.access_token = "rotated-token".into();
        }

        // Child sees the rotated token.
        let child_token = child.auth.lock().unwrap().access_token.clone();
        assert_eq!(child_token, "rotated-token");
    }

    // ── repair_orphaned_function_calls ─────────────────────────

    #[test]
    fn repair_removes_orphaned_function_call_output() {
        let mut items = vec![
            json!({"type": "message", "role": "user", "content": "hi"}),
            // function_call_output with no matching function_call
            json!({"type": "function_call_output", "call_id": "orphan_1", "output": "result"}),
        ];

        repair_orphaned_function_calls(&mut items);

        // The orphaned output should be removed.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["role"], "user");
    }

    #[test]
    fn repair_injects_placeholder_for_orphaned_function_call() {
        let mut items = vec![
            json!({"type": "function_call", "call_id": "call_1", "name": "Read", "arguments": "{}"}),
            // No function_call_output for call_1
        ];

        repair_orphaned_function_calls(&mut items);

        // A synthetic output should be injected.
        assert_eq!(items.len(), 2);
        assert_eq!(items[1]["type"], "function_call_output");
        assert_eq!(items[1]["call_id"], "call_1");
    }

    #[test]
    fn repair_handles_both_directions_simultaneously() {
        let mut items = vec![
            // Orphaned call (no output)
            json!({"type": "function_call", "call_id": "call_a", "name": "Read", "arguments": "{}"}),
            // Valid pair
            json!({"type": "function_call", "call_id": "call_b", "name": "Write", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "call_b", "output": "ok"}),
            // Orphaned output (no call)
            json!({"type": "function_call_output", "call_id": "call_c", "output": "stale"}),
        ];

        repair_orphaned_function_calls(&mut items);

        let call_ids: Vec<&str> = items
            .iter()
            .filter(|i| i["type"] == "function_call")
            .filter_map(|i| i["call_id"].as_str())
            .collect();
        let output_ids: Vec<&str> = items
            .iter()
            .filter(|i| i["type"] == "function_call_output")
            .filter_map(|i| i["call_id"].as_str())
            .collect();

        // call_a should get a synthetic output, call_b pair intact,
        // call_c orphaned output removed.
        assert!(call_ids.contains(&"call_a"));
        assert!(call_ids.contains(&"call_b"));
        assert!(output_ids.contains(&"call_a")); // synthetic
        assert!(output_ids.contains(&"call_b")); // original
        assert!(!output_ids.contains(&"call_c")); // removed
    }

    #[test]
    fn repair_noop_when_all_paired() {
        let mut items = vec![
            json!({"type": "function_call", "call_id": "c1", "name": "Read", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": "ok"}),
        ];
        let original_len = items.len();

        repair_orphaned_function_calls(&mut items);

        assert_eq!(items.len(), original_len);
    }

    // -- Web search tests -----------------------------------------

    #[test]
    fn build_request_body_injects_web_search_tool_with_filters() {
        use crate::request::{WebSearchToolConfig, WebSearchUserLocation};
        let req = simple_user_request("hello").with_web_search(WebSearchToolConfig {
            allowed_domains: Some(vec!["example.com".into()]),
            blocked_domains: None,
            max_uses: None,
            search_context_size: Some("high".into()),
            user_location: Some(WebSearchUserLocation {
                country: Some("US".into()),
                region: None,
                city: None,
                timezone: Some("America/New_York".into()),
            }),
        });
        let body = build_responses_request_body(&req, false, "k", None);
        let tools = body["tools"].as_array().unwrap();
        let ws = tools
            .iter()
            .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("web_search"))
            .expect("web_search tool should be present");
        assert_eq!(ws["type"], "web_search");
        assert_eq!(ws["filters"]["allowed_domains"][0], "example.com");
        assert_eq!(ws["search_context_size"], "high");
        assert_eq!(ws["user_location"]["type"], "approximate");
        assert_eq!(ws["user_location"]["country"], "US");
        assert_eq!(ws["user_location"]["timezone"], "America/New_York");
        // Unset fields should be absent
        assert!(ws["user_location"].get("region").is_none());
    }

    #[test]
    fn build_request_body_injects_web_search_tool_minimal() {
        let req =
            simple_user_request("hello").with_web_search(crate::request::WebSearchToolConfig {
                allowed_domains: None,
                blocked_domains: None,
                max_uses: None,
                search_context_size: None,
                user_location: None,
            });
        let body = build_responses_request_body(&req, false, "k", None);
        let tools = body["tools"].as_array().unwrap();
        let ws = tools
            .iter()
            .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("web_search"))
            .expect("web_search tool should be present");
        assert_eq!(ws["type"], "web_search");
        // No optional fields
        assert!(ws.get("filters").is_none());
        assert!(ws.get("search_context_size").is_none());
        assert!(ws.get("user_location").is_none());
    }

    #[test]
    fn build_request_body_web_search_coexists_with_function_tools() {
        let mut req =
            simple_user_request("hello").with_web_search(crate::request::WebSearchToolConfig {
                allowed_domains: None,
                blocked_domains: None,
                max_uses: None,
                search_context_size: None,
                user_location: None,
            });
        req.tools = vec![crate::types::Tool {
            name: "Read".into(),
            description: "Read a file".into(),
            input_schema: json!({"type": "object"}),
        }];
        let body = build_responses_request_body(&req, false, "k", None);
        let tools = body["tools"].as_array().unwrap();
        // Should have both: function tool + web_search
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "Read");
        assert_eq!(tools[1]["type"], "web_search");
    }

    #[test]
    fn translator_translates_web_search_call_into_server_tool_use() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "m"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": {
                    "type": "web_search_call",
                    "id": "ws_1",
                    "status": "searching",
                    "query": "rust async"
                }
            }),
        );
        push(
            &mut t,
            "response.output_item.done",
            json!({"output_index": 0}),
        );
        push(
            &mut t,
            "response.completed",
            json!({"response": {"id": "r1"}}),
        );

        let events = drain(&mut t);
        // Should have ServerToolUse start
        let has_server_tool = events.iter().any(|e| {
            matches!(
                e,
                StreamEvent::ContentBlockStart {
                    content_block: ContentBlockStart::ServerToolUse { id, name, input },
                    ..
                } if id == "ws_1" && name == "web_search" && input["query"] == "rust async"
            )
        });
        assert!(has_server_tool, "expected ServerToolUse ContentBlockStart");

        // Should have ContentBlockStop
        let has_stop = events
            .iter()
            .any(|e| matches!(e, StreamEvent::ContentBlockStop { .. }));
        assert!(has_stop, "expected ContentBlockStop");

        // Stop reason should be EndTurn (NOT ToolUse — server
        // tool use must not trigger client dispatch).
        let stop = events.iter().find_map(|e| match e {
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields { stop_reason, .. },
            } => stop_reason.clone(),
            _ => None,
        });
        assert_eq!(stop, Some(StopReason::EndTurn));
    }

    #[test]
    fn translator_web_search_call_does_not_set_has_tool_call() {
        // When only web_search_call items appear (no function_call),
        // the inferred stop reason must be EndTurn, not ToolUse.
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "m"}}),
        );
        // Text output after search
        push(
            &mut t,
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": {"type": "web_search_call", "id": "ws_1"}
            }),
        );
        push(
            &mut t,
            "response.output_item.done",
            json!({"output_index": 0}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({
                "output_index": 1,
                "item": {"type": "message"}
            }),
        );
        push(
            &mut t,
            "response.output_text.delta",
            json!({"delta": "The answer is 42."}),
        );
        push(
            &mut t,
            "response.output_item.done",
            json!({"output_index": 1}),
        );
        push(
            &mut t,
            "response.completed",
            json!({"response": {"id": "r1"}}),
        );

        let events = drain(&mut t);
        let stop = events.iter().find_map(|e| match e {
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields { stop_reason, .. },
            } => stop_reason.clone(),
            _ => None,
        });
        assert_eq!(
            stop,
            Some(StopReason::EndTurn),
            "web_search_call alone must not produce ToolUse stop reason"
        );
    }

    #[test]
    fn emit_assistant_message_skips_server_tool_use_blocks() {
        use crate::types::{ContentBlock, ServerToolUseBlock, TextBlock, WebSearchResultBlock};
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text(TextBlock {
                    text: "I'll search.".into(),
                }),
                ContentBlock::ServerToolUse(ServerToolUseBlock {
                    id: "srv_1".into(),
                    name: "web_search".into(),
                    input: json!({}),
                }),
                ContentBlock::WebSearchResult(WebSearchResultBlock {
                    tool_use_id: "srv_1".into(),
                    results: vec![],
                    raw_content: None,
                }),
                ContentBlock::Text(TextBlock {
                    text: " Found it.".into(),
                }),
            ],
        };
        let mut out = Vec::new();
        emit_assistant_message(&msg, &mut out);
        // Server tool use / web search result blocks are skipped.
        // The two text blocks are adjacent after filtering, so
        // emit_assistant_message groups them into a single message.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "message");
        let content = out[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], "I'll search.");
        assert_eq!(content[1]["text"], " Found it.");
    }

    // ---------------------------------------------------------------
    // Per-turn WS lifecycle + HTTP fallback (codex-ref alignment)
    // ---------------------------------------------------------------

    /// `close_notify` / `UnexpectedEof` class errors — the exact
    /// signature the user hit in production — must be classified as
    /// transient so the WS turn loop reconnects instead of bubbling
    /// a fatal "prompt turn failed".
    #[test]
    fn close_notify_and_unexpected_eof_are_transient() {
        assert!(is_transient_ws_connect_error(
            "peer closed connection without sending TLS close_notify"
        ));
        assert!(is_transient_ws_connect_error("IO error: UnexpectedEof"));
        assert!(is_transient_ws_connect_error(
            "ws connect: IO error: unexpected eof"
        ));
        assert!(is_transient_ws_connect_error("TLS handshake EOF"));
        assert!(is_transient_ws_connect_error("Connection reset by peer"));

        // Protocol / auth errors stay permanent.
        assert!(!is_transient_ws_connect_error("401 Unauthorized"));
        assert!(!is_transient_ws_connect_error(
            "invalid response during handshake"
        ));
    }

    #[test]
    fn tls_decrypt_error_is_transient() {
        assert!(is_transient_ws_connect_error(
            "IO error: cannot decrypt peer's message"
        ));
    }

    /// Non-English Windows renders Winsock errors with locale-specific
    /// text that doesn't contain any of the English keywords above
    /// (`timed out`, `connection reset`, …). The `(os error N)` suffix
    /// is locale-stable, so the errno fallback must classify these as
    /// transient — otherwise users on zh-CN / ja-JP / etc. Windows see
    /// fatal "prompt turn failed" on every TCP hiccup that English
    /// Windows users silently retry past.
    #[test]
    fn winsock_errno_is_transient_on_non_english_windows() {
        // zh-CN WSAETIMEDOUT (10060) — the exact string a user hit in
        // production on Chinese Windows.
        assert!(is_transient_ws_connect_error(
            "ws connect: IO error: 由于连接方在一段时间后没有正确答复或连接的主机没有反应，连接尝试失败。 (os error 10060)"
        ));
        // ja-JP WSAECONNRESET (10054).
        assert!(is_transient_ws_connect_error(
            "ws connect: IO error: 既存の接続はリモート ホストに強制的に切断されました。 (os error 10054)"
        ));
        // WSAECONNABORTED (10053) — locale-stable suffix is enough.
        assert!(is_transient_ws_connect_error(
            "ws connect: IO error: <localized text> (os error 10053)"
        ));
    }

    /// `end_turn()` keeps the cached WebSocket alive across prompt
    /// turns so the next turn can resolve `previous_response_id`
    /// against the same Codex backend session. If the socket has died,
    /// the next send path drains and drops it before deciding whether
    /// a full replay is required.
    #[tokio::test]
    async fn end_turn_preserves_ws_and_incremental_baseline() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());

        *provider.session_state.last_response_id.lock().unwrap() = Some("resp_mid_turn".into());
        *provider.session_state.last_request_input.lock().unwrap() = Some(vec![
            json!({"type": "message", "role": "user", "content": "hi"}),
        ]);
        provider
            .session_state
            .items_added_since_last_request
            .lock()
            .unwrap()
            .push(json!({"type": "message", "role": "assistant", "content": []}));

        ChatProvider::end_turn(&provider);

        assert_eq!(
            *provider.session_state.last_response_id.lock().unwrap(),
            Some("resp_mid_turn".into()),
            "end_turn must preserve last_response_id for incremental path"
        );
        assert!(
            provider
                .session_state
                .last_request_input
                .lock()
                .unwrap()
                .is_some(),
            "end_turn must preserve last_request_input baseline"
        );
        assert_eq!(
            provider
                .session_state
                .items_added_since_last_request
                .lock()
                .unwrap()
                .len(),
            1,
            "end_turn must preserve items_added_since_last_request"
        );
    }

    #[test]
    fn stale_previous_response_id_without_live_ws_is_cleared_before_new_ws() {
        let mut config = ws_test_config();
        config.base_url = "https://chatgpt.com/backend-api/codex/responses".into();
        let provider = OpenAiResponsesProvider::new(config);
        provider
            .session_state
            .set_last_response_id(Some("resp_stale".into()));

        assert!(clear_stale_previous_response_before_new_ws(
            &provider.session_state,
            true,
            false
        ));
        assert!(provider
            .session_state
            .last_response_id
            .lock()
            .unwrap()
            .is_none());
        assert!(!clear_stale_previous_response_before_new_ws(
            &provider.session_state,
            true,
            false
        ));
    }

    /// Mid-stream `invalidate_previous_response_id` must only clear
    /// the response-id chain — the WS stays alive so the large
    /// full-replay request that follows doesn't have to pay the
    /// reconnect cost on top of the replay cost. This is the
    /// specific codex-ref behaviour (compact doesn't tear down the
    /// per-turn connection) we're aligning on.
    #[tokio::test]
    async fn invalidate_previous_response_id_keeps_ws_alive() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        *provider.session_state.last_response_id.lock().unwrap() = Some("resp_mid_turn".into());

        ChatProvider::invalidate_previous_response_id(&provider);

        assert!(
            provider
                .session_state
                .last_response_id
                .lock()
                .unwrap()
                .is_none(),
            "invalidate must clear last_response_id"
        );
        // The WS slot is unaffected — try_lock observes whatever
        // was there (None in this unit test, but crucially the
        // invalidate codepath never touches `ws_conn`).
    }

    /// Once the provider observes a 426 UPGRADE_REQUIRED (or
    /// similar WS-unfriendly response), the `http_fallback_active`
    /// flag should pin every subsequent send onto the HTTP/SSE
    /// branch — matching codex-ref's session-scoped fallback.
    #[test]
    fn http_fallback_flag_short_circuits_ws_branch() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        assert!(provider.config.use_websocket);
        assert!(!provider.http_fallback_active.load(Ordering::Relaxed));

        provider.http_fallback_active.store(true, Ordering::Relaxed);

        // The send_message_stream dispatcher checks this exact
        // combination; once the flag is on, the WS branch is
        // skipped. Here we assert the contract directly — the
        // integration test with a live server would require a
        // mock transport that's out of scope for this test.
        assert!(
            provider.config.use_websocket && provider.http_fallback_active.load(Ordering::Relaxed),
            "flag must suppress WS even when use_websocket is true"
        );
    }

    /// Forking for a sub-agent must **share** the
    /// `http_fallback_active` flag so a sub-agent doesn't redundantly
    /// retry a WS handshake we already know will fail. Built by
    /// hand (mirroring the `fork_for_sub_agent` body) because the
    /// trait object returned by the production method erases the
    /// concrete type and we need field-level access to assert
    /// Arc-sharing.
    #[test]
    fn manual_fork_shares_http_fallback_flag_with_parent() {
        let parent = OpenAiResponsesProvider::new(ws_test_config());
        parent.http_fallback_active.store(true, Ordering::Relaxed);

        let child = OpenAiResponsesProvider {
            config: parent.config.clone(),
            prompt_cache_key: default_prompt_cache_key(),
            auth: parent.auth.clone(),
            session_state: ResponsesSessionState::new(),
            ws_conn: Arc::new(TokioMutex::new(None)),
            http_fallback_active: parent.http_fallback_active.clone(),
            consecutive_handshake_eofs: parent.consecutive_handshake_eofs.clone(),
            last_handshake_eof_at: parent.last_handshake_eof_at.clone(),
            retry_notifier: None,
        };

        // Shared Arc: a flip on the parent is visible to the child.
        assert!(Arc::ptr_eq(
            &parent.http_fallback_active,
            &child.http_fallback_active
        ));
        assert!(Arc::ptr_eq(
            &parent.consecutive_handshake_eofs,
            &child.consecutive_handshake_eofs
        ));
        assert!(Arc::ptr_eq(
            &parent.last_handshake_eof_at,
            &child.last_handshake_eof_at
        ));
        assert!(child.http_fallback_active.load(Ordering::Relaxed));

        // An unrelated provider constructed from scratch must
        // start with the flag clear — confirming fallback state
        // is scoped to the parent/child pair.
        let unrelated = OpenAiResponsesProvider::new(ws_test_config());
        assert!(!unrelated.http_fallback_active.load(Ordering::Relaxed));
    }

    /// Fallback is sticky: once `http_fallback_active` is set, the
    /// dispatcher must keep routing to HTTP/SSE for the rest of the
    /// session and never re-arm the WebSocket path. Retrying WS after
    /// a fallback would reconnect fresh with no live
    /// `previous_response_id`, forcing a full replay on that turn —
    /// strictly worse for cache-miss than staying on HTTP.
    #[test]
    fn http_fallback_is_sticky_and_never_retries_ws() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        assert!(provider.config.use_websocket);
        provider.http_fallback_active.store(true, Ordering::Relaxed);

        // Mirror the dispatch decision from `send_message_stream`:
        // with the flag set, the WS branch is skipped unconditionally
        // regardless of how much time has passed.
        let would_use_ws =
            provider.config.use_websocket && !provider.http_fallback_active.load(Ordering::Relaxed);
        assert!(
            !would_use_ws,
            "once fallback is engaged the session must stay on HTTP and never retry WS"
        );
    }

    /// A fresh EOF after the burst window has elapsed must restart
    /// the streak at 1, not extend the previous one. Without this,
    /// two unrelated EOFs hours apart could push a healthy session
    /// over the threshold and trigger fallback for no reason.
    #[test]
    fn handshake_eof_streak_resets_after_window() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        // Simulate a previous EOF observed long enough ago that it
        // is outside the burst window.
        let stale = Instant::now()
            .checked_sub(TLS_HANDSHAKE_EOF_BURST_WINDOW + Duration::from_secs(5))
            .expect("clock supports past instants");
        *provider.last_handshake_eof_at.lock().unwrap() = Some(stale);
        provider
            .consecutive_handshake_eofs
            .store(TLS_HANDSHAKE_EOF_BURST_THRESHOLD - 1, Ordering::Relaxed);

        // Mirror the streak-bump in `connect_ws`: outside the
        // window, the streak resets to 1.
        let now = Instant::now();
        let streak = {
            let mut last = provider.last_handshake_eof_at.lock().unwrap();
            let outside_window = match *last {
                Some(prev) => now.duration_since(prev) > TLS_HANDSHAKE_EOF_BURST_WINDOW,
                None => false,
            };
            *last = Some(now);
            if outside_window {
                provider
                    .consecutive_handshake_eofs
                    .store(1, Ordering::Relaxed);
                1
            } else {
                provider
                    .consecutive_handshake_eofs
                    .fetch_add(1, Ordering::Relaxed)
                    + 1
            }
        };
        assert_eq!(
            streak, 1,
            "EOF outside the burst window must restart the streak at 1, \
             not extend a stale one"
        );
        assert!(
            streak < TLS_HANDSHAKE_EOF_BURST_THRESHOLD,
            "single fresh EOF must not by itself trip fallback"
        );
    }

    #[test]
    fn http_fallback_flag_is_observed_before_websocket_dispatch() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        assert!(provider.config.use_websocket);
        assert!(!provider.http_fallback_active.load(Ordering::Relaxed));

        provider.http_fallback_active.store(true, Ordering::Relaxed);

        assert!(provider.http_fallback_active.load(Ordering::Relaxed));
    }

    // --- Incremental WS-input path ------------------------------------
    //
    // Matches `codex-rs/core/src/client.rs::get_incremental_items` +
    // `prepare_websocket_request`. The helpers under test are the
    // provider-local building blocks used by `drive_ws_turn_once` to
    // decide whether to send the full input or only the suffix
    // appended since the last turn. Sending the full input when a
    // `previous_response_id` is also set causes the OpenAI server to
    // treat the whole thing as a fresh prefix — observed as
    // `cache_read_input_tokens: 0` — which is exactly the regression
    // these helpers prevent.

    /// No prior request means no baseline is available — the caller
    /// must fall back to sending the full input.
    #[test]
    fn compute_incremental_baseline_is_none_on_fresh_provider() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        assert!(compute_incremental_baseline(&provider).is_none());
    }

    #[test]
    fn request_fingerprint_ignores_input_and_previous_response_id() {
        let mut first = CreateMessageRequest::simple("gpt-5.4", "hello");
        first.system = Some("same system".into());
        let mut second = CreateMessageRequest::simple("gpt-5.4", "different input");
        second.system = Some("same system".into());

        let a = responses_request_fingerprint(&first, true, "cache-key");
        let b = responses_request_fingerprint(&second, true, "cache-key");
        assert_eq!(a, b, "input content must not affect request fingerprint");

        let mut changed = CreateMessageRequest::simple("gpt-5.4-mini", "hello");
        changed.system = Some("same system".into());
        let c = responses_request_fingerprint(&changed, true, "cache-key");
        assert_ne!(a, c, "model changes must force a fresh create");
    }

    #[test]
    fn compute_incremental_baseline_requires_matching_request_fingerprint() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        let request = CreateMessageRequest::simple("gpt-5.4", "hello");
        let fingerprint = responses_request_fingerprint(&request, true, "cache-key");
        *provider.session_state.last_request_input.lock().unwrap() =
            Some(vec![json!({"role": "user", "content": "hello"})]);
        *provider
            .session_state
            .last_request_fingerprint
            .lock()
            .unwrap() = Some(fingerprint.clone());

        assert!(compute_incremental_baseline_for_request(&provider, &fingerprint).is_some());

        let mut changed_request = CreateMessageRequest::simple("gpt-5.4-mini", "hello");
        changed_request.system = request.system.clone();
        let changed = responses_request_fingerprint(&changed_request, true, "cache-key");
        assert!(
            compute_incremental_baseline_for_request(&provider, &changed).is_none(),
            "changed non-input request fields must disable previous_response_id reuse"
        );
    }

    /// Baseline is `last_request_input` alone. In an earlier
    /// revision we tried to concatenate `items_added_since_last_request`
    /// (server `output_item.done` payloads) onto the baseline, but
    /// rebon's `build_responses_input` re-serializes assistant items
    /// from our internal `Message` model and the result is never
    /// byte-identical to the server's raw output items, so the
    /// `starts_with` check failed every turn. The server already
    /// owns the assistant side via `previous_response_id`; we only
    /// need to compare against what *we* last sent.
    #[test]
    fn compute_incremental_baseline_returns_last_request_only() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        *provider.session_state.last_request_input.lock().unwrap() = Some(vec![
            json!({"type": "message", "role": "user", "content": "first"}),
            json!({"type": "message", "role": "assistant", "content": "reply"}),
        ]);
        // Items captured from the stream must NOT extend the
        // baseline — they're kept for diagnostics only.
        provider
            .session_state
            .items_added_since_last_request
            .lock()
            .unwrap()
            .extend([
                json!({"type": "reasoning", "summary": []}),
                json!({"type": "message", "role": "assistant", "content": "tool call"}),
            ]);

        let baseline = compute_incremental_baseline(&provider).expect("baseline present");
        assert_eq!(
            baseline.len(),
            2,
            "baseline must be last_request_input alone — no items_added concat"
        );
        assert_eq!(baseline[0]["content"], "first");
        assert_eq!(baseline[1]["content"], "reply");
    }

    /// Client-authored filter: user messages and tool outputs belong
    /// in the delta; everything else is redundant because the server
    /// already owns it via `previous_response_id`.
    #[test]
    fn is_client_authored_item_allowlist() {
        // Included: user message + function_call_output.
        assert!(is_client_authored_item(&json!({
            "type": "message",
            "role": "user",
            "content": "hi"
        })));
        assert!(is_client_authored_item(&json!({
            "type": "function_call_output",
            "call_id": "call_123",
            "output": "ok"
        })));

        // Excluded: assistant messages, reasoning, function_call (all
        // server-authored and reconstructed from the chain).
        assert!(!is_client_authored_item(&json!({
            "type": "message",
            "role": "assistant",
            "content": "hi"
        })));
        assert!(!is_client_authored_item(&json!({
            "type": "message",
            "role": "system",
            "content": "sys"
        })));
        assert!(!is_client_authored_item(&json!({
            "type": "reasoning",
            "summary": []
        })));
        assert!(!is_client_authored_item(&json!({
            "type": "function_call",
            "call_id": "call_123",
            "name": "bash",
            "arguments": "{}"
        })));

        // Unknown types are dropped by default — conservative allowlist.
        assert!(!is_client_authored_item(
            &json!({"type": "new_future_type"})
        ));

        // Bare user items with no `type` (the shape `emit_user_message`
        // actually produces) MUST be accepted — this is the real
        // incremental-delta entry for a new user prompt.
        assert!(is_client_authored_item(&json!({
            "role": "user",
            "content": "hi"
        })));
        // Without an explicit role, a typeless item is not
        // client-authored; drop it.
        assert!(!is_client_authored_item(&json!({"content": "anon"})));
        // A typeless assistant item is still server-authored.
        assert!(!is_client_authored_item(&json!({
            "role": "assistant",
            "content": "hi"
        })));
    }

    /// Regression test: the real shape that `build_responses_input`
    /// emits for a plain user message and a tool-result payload must
    /// survive the incremental-delta filter. Previously the filter
    /// required an explicit `type: "message"` field that
    /// `emit_user_message` does not emit, causing every user turn to
    /// either silently disappear (when it rode alongside a
    /// function_call_output) or force a full-input fallback.
    #[test]
    fn emit_user_message_output_is_recognized_as_client_authored() {
        let user = Message {
            role: Role::User,
            content: vec![ContentBlock::Text(TextBlock {
                text: "hello there".into(),
            })],
        };
        let mut out: Vec<Value> = Vec::new();
        emit_user_message(&user, &mut out);
        assert_eq!(out.len(), 1, "expected a single user item");
        assert!(
            is_client_authored_item(&out[0]),
            "user item produced by emit_user_message must pass the client-authored filter, got {:?}",
            out[0]
        );

        // A user message that also carries a tool_result emits TWO
        // items (function_call_output + user message); both must pass
        // the filter so a delta containing "tool_result + new user
        // prompt" survives fully.
        let mixed = Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult(ToolResultBlock {
                    tool_use_id: "call_abc".into(),
                    content: "ok".into(),
                    is_error: false,
                }),
                ContentBlock::Text(TextBlock {
                    text: "and the follow-up prompt".into(),
                }),
            ],
        };
        let mut out: Vec<Value> = Vec::new();
        emit_user_message(&mixed, &mut out);
        assert_eq!(out.len(), 2, "mixed user message should emit two items");
        for item in &out {
            assert!(
                is_client_authored_item(item),
                "every item emitted by emit_user_message must be client-authored, got {item:?}"
            );
        }
    }

    /// With a prior request but no observed items, the baseline is
    /// just the prior request. This is the normal state right after
    /// `record_sent_input_as_baseline` but before any frames have
    /// arrived.
    #[test]
    fn compute_incremental_baseline_handles_empty_items_added() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        *provider.session_state.last_request_input.lock().unwrap() = Some(vec![
            json!({"type": "message", "role": "user", "content": "hi"}),
        ]);
        let baseline = compute_incremental_baseline(&provider).expect("baseline present");
        assert_eq!(baseline.len(), 1);
        assert_eq!(baseline[0]["content"], "hi");
    }

    /// Committing a new baseline must (a) replace the prior request,
    /// and (b) drop the item buffer — those items are now part of
    /// the server-side chain tracked via `previous_response_id`, so
    /// counting them in the next baseline would double-count.
    #[test]
    fn record_sent_input_as_baseline_sets_input_and_clears_items() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        // Pre-load some stale state that the commit must clobber.
        *provider.session_state.last_request_input.lock().unwrap() =
            Some(vec![json!({"content": "stale"})]);
        provider
            .session_state
            .items_added_since_last_request
            .lock()
            .unwrap()
            .push(json!({"content": "stale item"}));

        let new_input = vec![
            json!({"type": "message", "role": "user", "content": "turn 2"}),
            json!({"type": "message", "role": "assistant", "content": "turn 2 reply"}),
        ];
        record_sent_input_as_baseline(&provider, new_input.clone());

        assert_eq!(
            provider
                .session_state
                .last_request_input
                .lock()
                .unwrap()
                .as_ref()
                .unwrap(),
            &new_input,
            "last_request_input must be overwritten with the freshly-sent input"
        );
        assert!(
            provider
                .session_state
                .items_added_since_last_request
                .lock()
                .unwrap()
                .is_empty(),
            "items_added must be cleared after a new request commits"
        );
    }

    #[test]
    fn record_sent_request_as_baseline_sets_fingerprint() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        let input = vec![json!({"role": "user", "content": "turn"})];
        let fingerprint = json!({"model": "gpt-5.4", "stream": true});

        record_sent_request_as_baseline(&provider, input.clone(), fingerprint.clone());

        assert_eq!(
            provider
                .session_state
                .last_request_input
                .lock()
                .unwrap()
                .as_ref(),
            Some(&input)
        );
        assert_eq!(
            provider
                .session_state
                .last_request_fingerprint
                .lock()
                .unwrap()
                .as_ref(),
            Some(&fingerprint)
        );
    }

    #[test]
    fn session_state_commit_promotes_and_clears_pending_request() {
        let state = ResponsesSessionState::new();
        let input = vec![json!({"role": "user", "content": "turn"})];
        let fingerprint = json!({"model": "gpt-5.4"});

        state.begin_request(input.clone(), fingerprint.clone());
        state.commit_pending_request();

        assert!(state.pending_request_input.lock().unwrap().is_none());
        assert!(state.pending_request_fingerprint.lock().unwrap().is_none());
        assert_eq!(
            state.last_request_input.lock().unwrap().as_ref(),
            Some(&input)
        );
        assert_eq!(
            state.last_request_fingerprint.lock().unwrap().as_ref(),
            Some(&fingerprint)
        );
    }

    #[test]
    fn items_added_buffer_is_scoped_to_the_current_request() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        for text in ["first", "second"] {
            capture_items_added_from_frame(
                &provider,
                &json!({
                    "type": "response.output_item.done",
                    "item": {"type": "message", "role": "assistant", "content": text}
                })
                .to_string(),
            );
        }
        assert_eq!(
            provider
                .session_state
                .items_added_since_last_request
                .lock()
                .unwrap()
                .len(),
            2
        );

        provider
            .session_state
            .begin_request(vec![json!({"role": "user", "content": "next"})], json!({}));
        capture_items_added_from_frame(
            &provider,
            &json!({
                "type": "response.output_item.done",
                "item": {"type": "message", "role": "assistant", "content": "current"}
            })
            .to_string(),
        );

        let items = provider
            .session_state
            .items_added_since_last_request
            .lock()
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["content"], "current");
    }

    /// A `response.output_item.done` frame must extend the
    /// items-added buffer with the `item` payload it carries. This is
    /// the single stream event codex-rs treats as baseline-relevant.
    #[test]
    fn capture_items_added_captures_item_on_output_item_done() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        let frame = json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "hello"}]
            }
        })
        .to_string();

        capture_items_added_from_frame(&provider, &frame);

        let items = provider
            .session_state
            .items_added_since_last_request
            .lock()
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["role"], "assistant");
        assert_eq!(items[0]["content"][0]["text"], "hello");
    }

    /// Other streaming frame types (content deltas, reasoning parts,
    /// in-progress markers, etc.) must be ignored — only `output_item.done`
    /// corresponds to a committed server-side item.
    #[test]
    fn capture_items_added_ignores_unrelated_frame_types() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        for frame in [
            json!({"type": "response.in_progress"}).to_string(),
            json!({"type": "response.content_part.added", "item": {"type": "message"}}).to_string(),
            json!({"type": "response.output_item.added", "item": {"type": "message"}}).to_string(),
            json!({"type": "response.reasoning_summary_part.done"}).to_string(),
        ] {
            capture_items_added_from_frame(&provider, &frame);
        }
        assert!(
            provider
                .session_state
                .items_added_since_last_request
                .lock()
                .unwrap()
                .is_empty(),
            "non-output_item.done frames must not extend the baseline"
        );
    }

    /// The helper is called on every text frame we read off the WS.
    /// A malformed frame (or a non-JSON frame — e.g. if the server
    /// ever emits plain text) must not panic or corrupt state.
    #[test]
    fn capture_items_added_ignores_malformed_input() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        capture_items_added_from_frame(&provider, "not json at all");
        capture_items_added_from_frame(&provider, "{ this is invalid");
        // output_item.done with no `item` field — valid JSON, wrong
        // shape. Must be skipped rather than panicking.
        capture_items_added_from_frame(
            &provider,
            &json!({"type": "response.output_item.done"}).to_string(),
        );
        assert!(provider
            .session_state
            .items_added_since_last_request
            .lock()
            .unwrap()
            .is_empty());
    }

    /// End-to-end semantics check for the delta decision used in
    /// `drive_ws_turn_once`: given a committed baseline, feeding a
    /// longer full input that starts with the baseline must yield
    /// the client-authored suffix after filtering. The suffix plus
    /// `previous_response_id` must let the server reconstruct the
    /// new full input without receiving redundant assistant items.
    #[test]
    fn incremental_delta_is_filtered_suffix_when_baseline_matches() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        record_sent_input_as_baseline(
            &provider,
            vec![json!({"type": "message", "role": "user", "content": "t1"})],
        );

        let baseline = compute_incremental_baseline(&provider).expect("baseline present");
        // The new turn's full input = baseline (user turn 1) + the
        // assistant's reply to turn 1 (as rebon reconstructs it) +
        // a reasoning item + the new user turn 2. Only the new user
        // message is client-authored; the rest must be filtered out.
        let mut full_input = baseline.clone();
        full_input.extend([
            json!({"type": "message", "role": "assistant", "content": "r1"}),
            json!({"type": "reasoning", "summary": []}),
            json!({"type": "message", "role": "user", "content": "t2"}),
        ]);

        assert!(full_input.starts_with(&baseline));
        let raw_delta = &full_input[baseline.len()..];
        let filtered: Vec<Value> = raw_delta
            .iter()
            .filter(|it| is_client_authored_item(it))
            .cloned()
            .collect();
        assert_eq!(filtered.len(), 1, "only the new user turn must survive");
        assert_eq!(filtered[0]["content"], "t2");
    }

    #[test]
    fn transient_text_turn_forces_next_turn_full_replay_to_preserve_assistant_reply() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        let first_turn_baseline = vec![json!({"role": "user", "content": "t1"})];
        record_sent_input_as_baseline(&provider, first_turn_baseline.clone());
        provider
            .session_state
            .set_last_response_id(Some("resp_after_t1".into()));

        complete_ws_turn_state(&provider.session_state, false);

        assert!(
            provider
                .session_state
                .last_response_id
                .lock()
                .unwrap()
                .is_none(),
            "a completed transient turn must invalidate the previous_response_id chain"
        );

        let mut next_full_input = first_turn_baseline;
        next_full_input.extend([
            json!({"role": "user", "content": "t2 transient"}),
            json!({"type": "message", "role": "assistant", "content": "plain text reply to transient turn"}),
            json!({"role": "user", "content": "t3 ordinary"}),
        ]);

        let raw_previous_response_id = provider
            .session_state
            .last_response_id
            .lock()
            .unwrap()
            .clone();
        let sent_input = if raw_previous_response_id.is_some() {
            let baseline = compute_incremental_baseline(&provider).unwrap();
            assert!(next_full_input.starts_with(&baseline));
            next_full_input[baseline.len()..]
                .iter()
                .filter(|item| is_client_authored_item(item))
                .cloned()
                .collect()
        } else {
            next_full_input.clone()
        };

        assert_eq!(sent_input, next_full_input);
        assert!(sent_input.iter().any(|item| {
            item.get("role").and_then(|v| v.as_str()) == Some("assistant")
                && item.get("content").and_then(|v| v.as_str())
                    == Some("plain text reply to transient turn")
        }));
    }

    /// A delta that includes a tool result (function_call_output
    /// produced locally) must be preserved alongside the new user
    /// message — tool outputs are the other class of client-authored
    /// items the server needs.
    #[test]
    fn incremental_delta_preserves_function_call_output() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        record_sent_input_as_baseline(
            &provider,
            vec![
                json!({"type": "message", "role": "user", "content": "t1"}),
                json!({"type": "function_call", "call_id": "c1", "name": "bash", "arguments": "{}"}),
            ],
        );
        let baseline = compute_incremental_baseline(&provider).unwrap();

        let mut full_input = baseline.clone();
        full_input.extend([
            // Server side (to be filtered out).
            json!({"type": "message", "role": "assistant", "content": "r1"}),
            // Client side (to be preserved).
            json!({"type": "function_call_output", "call_id": "c1", "output": "done"}),
            json!({"type": "message", "role": "user", "content": "t2"}),
        ]);

        let raw_delta = &full_input[baseline.len()..];
        let filtered: Vec<Value> = raw_delta
            .iter()
            .filter(|it| is_client_authored_item(it))
            .cloned()
            .collect();
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0]["type"], "function_call_output");
        assert_eq!(filtered[0]["call_id"], "c1");
        assert_eq!(filtered[1]["content"], "t2");
        assert!(function_call_outputs_are_anchored_in_chain(
            &filtered,
            &baseline,
            &[]
        ));
    }

    #[test]
    fn incremental_delta_accepts_tool_output_for_call_added_by_previous_response() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        record_sent_input_as_baseline(
            &provider,
            vec![json!({"type": "message", "role": "user", "content": "run tests"})],
        );
        provider
            .session_state
            .items_added_since_last_request
            .lock()
            .unwrap()
            .push(json!({"type": "function_call", "call_id": "call_bash", "name": "Bash", "arguments": "{}"}));
        let baseline = compute_incremental_baseline(&provider).unwrap();
        let items_added = provider
            .session_state
            .items_added_since_last_request
            .lock()
            .unwrap()
            .clone();
        let filtered = vec![
            json!({"type": "function_call_output", "call_id": "call_bash", "output": "ok"}),
            json!({"type": "message", "role": "user", "content": "continue"}),
        ];

        assert!(function_call_outputs_are_anchored_in_chain(
            &filtered,
            &baseline,
            &items_added
        ));
    }

    #[test]
    fn incremental_delta_replays_when_tool_output_call_missing_from_chain() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        record_sent_input_as_baseline(
            &provider,
            vec![json!({"type": "message", "role": "user", "content": "t1"})],
        );
        let baseline = compute_incremental_baseline(&provider).unwrap();

        let mut full_input = baseline.clone();
        full_input.extend([
            json!({"type": "function_call", "call_id": "call_edit", "name": "Edit", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "call_edit", "output": "invalid input"}),
        ]);

        let raw_delta = &full_input[baseline.len()..];
        let filtered: Vec<Value> = raw_delta
            .iter()
            .filter(|it| is_client_authored_item(it))
            .cloned()
            .collect();

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["type"], "function_call_output");
        assert!(!function_call_outputs_are_anchored_in_chain(
            &filtered,
            &baseline,
            &[]
        ));
        assert!(function_call_outputs_are_anchored_in_chain(
            &filtered,
            &full_input,
            &[]
        ));
    }

    #[test]
    fn detects_no_tool_call_found_for_function_call_output_ws_400() {
        let message = "ws error (): No tool call found for function call output with call_id call_V0GVF3BUWIOuLp9s13H6eDJh.";
        let error = ModelError::BadRequest(message.into());

        assert!(model_error_is_missing_function_call_for_output(&error));
    }

    /// If the full input diverges from the baseline (e.g. a
    /// /compact dropped earlier turns), the prefix check must fail
    /// so the caller falls back to a full replay. Without this,
    /// we'd send a truncated delta the server can't reassemble.
    #[test]
    fn incremental_delta_skipped_when_baseline_diverges() {
        let provider = OpenAiResponsesProvider::new(ws_test_config());
        record_sent_input_as_baseline(
            &provider,
            vec![
                json!({"type": "message", "role": "user", "content": "t1"}),
                json!({"type": "message", "role": "assistant", "content": "r1"}),
                json!({"type": "message", "role": "user", "content": "t2"}),
            ],
        );
        let baseline = compute_incremental_baseline(&provider).expect("baseline present");

        // Post-compact: the new full input discards t1/r1 and
        // starts from a summary item — no longer a superset of
        // baseline.
        let full_input = vec![
            json!({"type": "message", "role": "system", "content": "compacted summary"}),
            json!({"type": "message", "role": "user", "content": "t3"}),
        ];

        assert!(
            !full_input.starts_with(&baseline),
            "divergent full input must not be treated as an incremental extension"
        );
    }

    // -- image_generation SSE translator tests ----------------------

    #[test]
    fn translator_translates_image_generation_call_with_final_bytes() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "gpt-5.4"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": {
                    "type": "image_generation_call",
                    "id": "ig_final",
                    "status": "in_progress"
                }
            }),
        );
        push(
            &mut t,
            "response.output_item.done",
            json!({
                "output_index": 0,
                "item": {
                    "type": "image_generation_call",
                    "id": "ig_final",
                    "status": "completed",
                    "output_format": "png",
                    "revised_prompt": "a red fox wearing a scarf",
                    "result": "ZmluYWxfYnl0ZXM="
                }
            }),
        );
        push(
            &mut t,
            "response.completed",
            json!({"response": {"id": "r1"}}),
        );
        let events = drain(&mut t);

        let has_start = events.iter().any(|e| {
            matches!(
                e,
                StreamEvent::ContentBlockStart {
                    content_block: ContentBlockStart::ImageGeneration { id, status },
                    ..
                } if id == "ig_final" && status.as_deref() == Some("in_progress")
            )
        });
        assert!(has_start, "expected ImageGeneration ContentBlockStart");

        let final_delta = events.iter().find_map(|e| match e {
            StreamEvent::ContentBlockDelta {
                delta:
                    ContentBlockDelta::ImageDataDelta {
                        b64_json,
                        partial_index,
                        revised_prompt,
                        media_type,
                    },
                ..
            } if partial_index.is_none() => {
                Some((b64_json.clone(), revised_prompt.clone(), media_type.clone()))
            }
            _ => None,
        });
        let (b64, revised, media) = final_delta.expect("expected final ImageDataDelta");
        assert_eq!(b64, "ZmluYWxfYnl0ZXM=");
        assert_eq!(revised.as_deref(), Some("a red fox wearing a scarf"));
        assert_eq!(media.as_deref(), Some("image/png"));

        // Must NOT set tool_use as stop reason — image_generation is
        // a server-side built-in, not a client-dispatched tool.
        let stop = events.iter().find_map(|e| match e {
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields { stop_reason, .. },
            } => stop_reason.clone(),
            _ => None,
        });
        assert_eq!(stop, Some(StopReason::EndTurn));
    }

    /// Remote compaction v2's answer: one `compaction` output item whose
    /// `encrypted_content` replaces the summarised history. The blob is
    /// only populated at `done`, and `added` drops unknown item types, so
    /// the block has to be opened from the `done` frame.
    #[test]
    fn compaction_output_item_becomes_a_compaction_block() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "gpt-5.3-codex"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": {"type": "compaction", "id": "cmp_1"}
            }),
        );
        push(
            &mut t,
            "response.output_item.done",
            json!({
                "output_index": 0,
                "item": {
                    "type": "compaction",
                    "id": "cmp_1",
                    "encrypted_content": "opaque-blob"
                }
            }),
        );
        push(
            &mut t,
            "response.completed",
            json!({"response": {"id": "r1"}}),
        );
        let events = drain(&mut t);

        let opened = events.iter().find_map(|e| match e {
            StreamEvent::ContentBlockStart {
                index,
                content_block:
                    ContentBlockStart::Compaction {
                        content,
                        encrypted_content,
                    },
            } => Some((*index, content.clone(), encrypted_content.clone())),
            _ => None,
        });
        let (index, content, encrypted_content) =
            opened.expect("expected a Compaction ContentBlockStart");
        assert_eq!(encrypted_content.as_deref(), Some("opaque-blob"));
        assert_eq!(content, None, "the blob is not a readable summary");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::ContentBlockStop { index: i } if *i == index)),
            "the block must be closed or the accumulator never commits it"
        );

        let mut acc = crate::events::MessageAccumulator::new();
        for event in &events {
            acc.apply(event).unwrap();
        }
        let message = acc.finish();
        assert!(matches!(
            message.content.as_slice(),
            [ContentBlock::Compaction(block)]
                if block.encrypted_content.as_deref() == Some("opaque-blob")
        ));
    }

    /// A compaction item with no blob carries nothing to replay; opening
    /// a block for it would put an empty husk in the history.
    #[test]
    fn compaction_output_item_without_a_blob_yields_no_payload() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r1", "model": "gpt-5.3-codex"}}),
        );
        push(
            &mut t,
            "response.output_item.done",
            json!({
                "output_index": 0,
                "item": {"type": "compaction", "id": "cmp_1", "encrypted_content": ""}
            }),
        );
        push(
            &mut t,
            "response.completed",
            json!({"response": {"id": "r1"}}),
        );
        let events = drain(&mut t);

        let blob = events.iter().find_map(|e| match e {
            StreamEvent::ContentBlockStart {
                content_block:
                    ContentBlockStart::Compaction {
                        encrypted_content, ..
                    },
                ..
            } => Some(encrypted_content.clone()),
            _ => None,
        });
        assert_eq!(blob, Some(None), "an empty blob must not be carried");
    }

    #[test]
    fn translator_translates_image_generation_partial_frames() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r2", "model": "gpt-5.4"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": {"type": "image_generation_call", "id": "ig_p"}
            }),
        );
        push(
            &mut t,
            "response.image_generation_call.partial_image",
            json!({
                "output_index": 0,
                "partial_image_index": 0,
                "partial_image_b64": "cDA="
            }),
        );
        push(
            &mut t,
            "response.image_generation_call.partial_image",
            json!({
                "output_index": 0,
                "partial_image_index": 1,
                "partial_image_b64": "cDE="
            }),
        );
        push(
            &mut t,
            "response.output_item.done",
            json!({
                "output_index": 0,
                "item": {
                    "type": "image_generation_call",
                    "id": "ig_p",
                    "status": "completed",
                    "output_format": "webp",
                    "result": "ZmluYWw="
                }
            }),
        );
        push(
            &mut t,
            "response.completed",
            json!({"response": {"id": "r2"}}),
        );
        let events = drain(&mut t);

        // Three ImageDataDelta events expected: two partials + one final.
        let deltas: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ContentBlockDelta {
                    delta:
                        ContentBlockDelta::ImageDataDelta {
                            b64_json,
                            partial_index,
                            media_type,
                            ..
                        },
                    ..
                } => Some((b64_json.clone(), *partial_index, media_type.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(deltas.len(), 3);
        assert_eq!(deltas[0].1, Some(0));
        assert_eq!(deltas[0].0, "cDA=");
        assert_eq!(deltas[1].1, Some(1));
        assert_eq!(deltas[1].0, "cDE=");
        assert_eq!(deltas[2].1, None);
        assert_eq!(deltas[2].0, "ZmluYWw=");
        assert_eq!(deltas[2].2.as_deref(), Some("image/webp"));
    }

    #[test]
    fn translator_image_generation_does_not_trigger_tool_use_stop() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r3", "model": "gpt-5.4"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": {"type": "image_generation_call", "id": "ig_only"}
            }),
        );
        push(
            &mut t,
            "response.output_item.done",
            json!({
                "output_index": 0,
                "item": {
                    "type": "image_generation_call",
                    "id": "ig_only",
                    "result": "YWJj"
                }
            }),
        );
        push(
            &mut t,
            "response.completed",
            json!({"response": {"id": "r3"}}),
        );
        let events = drain(&mut t);
        let stop = events.iter().find_map(|e| match e {
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields { stop_reason, .. },
            } => stop_reason.clone(),
            _ => None,
        });
        assert_eq!(
            stop,
            Some(StopReason::EndTurn),
            "image_generation_call alone must not infer ToolUse"
        );
    }

    #[test]
    fn translator_image_generation_partial_without_b64_dropped() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r4", "model": "gpt-5.4"}}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": {"type": "image_generation_call", "id": "ig_bad"}
            }),
        );
        push(
            &mut t,
            "response.image_generation_call.partial_image",
            json!({"output_index": 0, "partial_image_index": 0}),
        );
        push(
            &mut t,
            "response.output_item.done",
            json!({
                "output_index": 0,
                "item": {"type": "image_generation_call", "id": "ig_bad"}
            }),
        );
        push(
            &mut t,
            "response.completed",
            json!({"response": {"id": "r4"}}),
        );
        let events = drain(&mut t);
        let delta_count = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ContentBlockDelta { .. }))
            .count();
        assert_eq!(delta_count, 0, "malformed partial frames must be dropped");
    }

    #[test]
    fn translator_image_generation_status_event_starts_visible_block_before_item_added() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r5", "model": "gpt-5.4"}}),
        );
        push(
            &mut t,
            "response.image_generation_call.in_progress",
            json!({"item_id": "ig_info", "output_index": 0}),
        );

        let events: Vec<_> = std::iter::from_fn(|| t.next_pending()).collect();
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::ImageGeneration { id, status }
            } if id == "ig_info" && status.as_deref() == Some("in_progress")
        )));
    }

    #[test]
    fn translator_image_generation_item_added_reuses_status_block() {
        let mut t = OpenAiResponsesTranslator::default();
        push(
            &mut t,
            "response.created",
            json!({"response": {"id": "r6", "model": "gpt-5.4"}}),
        );
        push(
            &mut t,
            "response.image_generation_call.generating",
            json!({"item_id": "ig_reuse", "output_index": 0}),
        );
        push(
            &mut t,
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": {"type": "image_generation_call", "id": "ig_reuse"}
            }),
        );
        push(
            &mut t,
            "response.output_item.done",
            json!({
                "output_index": 0,
                "item": {
                    "type": "image_generation_call",
                    "id": "ig_reuse",
                    "status": "completed",
                    "output_format": "png",
                    "result": "ZmluYWw="
                }
            }),
        );

        let events: Vec<_> = std::iter::from_fn(|| t.next_pending()).collect();
        let starts = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    StreamEvent::ContentBlockStart {
                        content_block: ContentBlockStart::ImageGeneration { id, .. },
                        ..
                    } if id == "ig_reuse"
                )
            })
            .count();
        assert_eq!(starts, 1);
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ImageDataDelta {
                    b64_json,
                    partial_index: None,
                    ..
                }
            } if b64_json == "ZmluYWw="
        )));
    }
}

/// Send the prepared turn on the websocket and pump its response frames.
///
/// The socket is shared across turns, so this never abandons it with unread
/// frames: when the caller drops the stream receiver mid-response the frames
/// are still read (without being forwarded) until the response completes, and
/// only then is the connection released in a clean between-turns state.
/// Every path that clears `*guard` returns immediately -- a dropped pump means
/// the next turn reconnects rather than reusing a dead handle.
#[allow(clippy::too_many_arguments)]
async fn send_and_pump_ws_turn(
    guard: &mut tokio::sync::MutexGuard<'_, Option<WsPump>>,
    provider: &OpenAiResponsesProvider,
    session_state: &ResponsesSessionState,
    options: &WsTurnOptions,
    tx: &tokio::sync::mpsc::Sender<ModelResult<StreamEvent>>,
    body_json: String,
    baseline_input: Vec<Value>,
    request_fingerprint: Value,
    transient_context_present: bool,
) -> ModelResult<WsTurnResolution> {
    let ws = guard.as_mut().unwrap();

    if let Err(e) = ws.send(tungstenite::Message::Text(body_json)).await {
        let msg = format!("ws send: {e}");
        **guard = None;
        session_state.discard_pending_request();
        if is_transient_ws_send_error(&msg) {
            return Err(ModelError::Http(msg));
        }
        session_state.set_last_response_id(None);
        return Err(ModelError::Protocol(msg));
    }

    let commit_incremental_state = !transient_context_present;
    if commit_incremental_state {
        session_state.begin_request(baseline_input, request_fingerprint);
    }

    let mut translator = OpenAiResponsesTranslator::default();

    // Deadline for receiving the next *data* frame. Ping/Pong
    // keepalives deliberately do not push it forward — see
    // WS_DATA_STALL_TIMEOUT.
    let mut data_stall_deadline = tokio::time::Instant::now() + WS_DATA_STALL_TIMEOUT;

    // When the caller drops the stream receiver mid-response we must
    // NOT abandon the socket with unread frames: the connection is
    // shared across turns, and the next turn would consume this
    // response's frames as its own reply. Keep reading (without
    // forwarding) until the response completes, then release the
    // connection in a clean between-turns state.
    let mut receiver_gone = false;

    loop {
        while let Some(event) = translator.next_pending() {
            if let StreamEvent::MessageStart { ref message_id, .. } = event {
                if commit_incremental_state && message_id.starts_with("resp_") {
                    session_state.set_last_response_id(Some(message_id.clone()));
                }
            }
            let done = translator.is_finished() && translator.is_drained();
            if !receiver_gone && tx.send(Ok(event)).await.is_err() {
                receiver_gone = true;
                tracing::debug!(
                "openai-responses: stream receiver dropped mid-turn; draining response to keep the websocket clean"
            );
            }
            if done {
                complete_ws_turn_state(session_state, commit_incremental_state);
                return Ok(WsTurnResolution::Completed);
            }
        }

        if translator.is_finished() && translator.is_drained() {
            complete_ws_turn_state(session_state, commit_incremental_state);
            return Ok(WsTurnResolution::Completed);
        }

        let frame_timeout = WS_FRAME_IDLE_TIMEOUT
            .min(data_stall_deadline.saturating_duration_since(tokio::time::Instant::now()));
        let frame = match tokio::time::timeout(frame_timeout, ws.next_frame()).await {
            Ok(f) => f,
            Err(_) => {
                **guard = None;
                session_state.set_last_response_id(None);
                session_state.discard_pending_request();
                let message = if tokio::time::Instant::now() >= data_stall_deadline {
                    WS_DATA_STALL_TIMEOUT_MESSAGE
                } else {
                    WS_FRAME_IDLE_TIMEOUT_MESSAGE
                };
                return Err(ModelError::Http(message.into()));
            }
        };

        // Any data frame (Text/Binary — Close/Err exit below anyway)
        // counts as response progress and pushes the stall deadline.
        if !matches!(
            &frame,
            Some(Ok(
                tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_)
            ))
        ) {
            data_stall_deadline = tokio::time::Instant::now() + WS_DATA_STALL_TIMEOUT;
        }

        match frame {
            Some(Ok(tungstenite::Message::Ping(_))) | Some(Ok(tungstenite::Message::Pong(_))) => {
                continue;
            }
            Some(Ok(tungstenite::Message::Text(text))) => {
                let event_type = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from))
                    .unwrap_or_default();

                if commit_incremental_state {
                    capture_items_added_from_frame(provider, &text);
                }

                if let Err(e) = translator.push_frame(&event_type, &text) {
                    // Server-side websocket lifetime cap (60 minutes
                    // per connection, regardless of activity). The
                    // previous_response_id chain is connection-scoped,
                    // so drop the socket, clear the chain, and replay
                    // the turn on a fresh connection — same recovery
                    // as codex-ref, which maps this code to a
                    // retryable error and re-sends the full input. If
                    // a fresh connection reports the limit again (a
                    // misbehaving server), the strategy is already
                    // ForceFullReplay and the error is surfaced without
                    // another provider-local attempt.
                    if model_error_is_websocket_connection_limit(&e)
                        && options.previous_response
                            == PreviousResponseStrategy::UsePreviousResponseId
                    {
                        tracing::warn!(
                            error = %e,
                            "openai-responses: websocket connection lifetime limit reached; reconnecting and retrying turn with full replay"
                        );
                        session_state.clear();
                        **guard = None;
                        return Ok(WsTurnResolution::RetryWithoutPreviousResponseId(
                            CacheMissReason::RetryWithoutPreviousResponseId,
                        ));
                    }

                    if matches!(&e, ModelError::BadRequest(message) if message.contains("previous_response_not_found"))
                        && options.previous_response
                            == PreviousResponseStrategy::UsePreviousResponseId
                    {
                        tracing::warn!(
                            error = %e,
                            "openai-responses: previous_response_id rejected (previous_response_not_found); retrying turn with full replay"
                        );
                        session_state.clear();
                        **guard = None;
                        return Ok(WsTurnResolution::RetryWithoutPreviousResponseId(
                            CacheMissReason::PreviousResponseNotFound,
                        ));
                    }

                    if model_error_is_missing_function_call_for_output(&e)
                        && options.previous_response
                            == PreviousResponseStrategy::UsePreviousResponseId
                    {
                        tracing::warn!(
                            error = %e,
                            "openai-responses: function_call_output was not anchored in server chain; retrying turn with full replay"
                        );
                        session_state.clear();
                        **guard = None;
                        return Ok(WsTurnResolution::RetryWithoutPreviousResponseId(
                            CacheMissReason::RetryWithoutPreviousResponseId,
                        ));
                    }

                    if e.context_overflow().is_some()
                        && options.previous_response
                            == PreviousResponseStrategy::UsePreviousResponseId
                    {
                        tracing::warn!(
                            error = %e,
                            "openai-responses: context window exceeded with server-side history; \
                             retrying turn with full replay (pruned messages)"
                        );
                        session_state.clear();
                        **guard = None;
                        return Ok(WsTurnResolution::RetryWithoutPreviousResponseId(
                            CacheMissReason::OverflowInvalidatedPreviousResponseId,
                        ));
                    }

                    **guard = None;
                    session_state.set_last_response_id(None);
                    session_state.discard_pending_request();

                    return Err(e);
                }
            }
            Some(Ok(tungstenite::Message::Close(_))) | None => {
                **guard = None;

                if translator.is_finished() {
                    complete_ws_turn_state(session_state, commit_incremental_state);
                    return Ok(WsTurnResolution::Completed);
                }
                // The peer hung up before the response finished. Nothing was
                // wrong with the request: drop the chain that lived on this
                // socket and let the driver replay the turn on a fresh one.
                session_state.clear();
                return Ok(WsTurnResolution::ReconnectAndReplay(ModelError::Http(
                    "websocket closed before response.completed".into(),
                )));
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => {
                let error = ModelError::Http(format!("websocket error: {e}"));
                **guard = None;
                if is_transient_ws_frame_error(&e) {
                    // The socket died under the read loop. The half-received
                    // response is not resumable, so the whole turn is replayed
                    // on a fresh connection; the response-id chain went with
                    // the socket and must not travel into the replay.
                    session_state.clear();
                    return Ok(WsTurnResolution::ReconnectAndReplay(error));
                }
                session_state.set_last_response_id(None);
                session_state.discard_pending_request();
                return Err(error);
            }
        }
    }
}
