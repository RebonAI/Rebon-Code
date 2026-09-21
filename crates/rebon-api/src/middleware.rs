//! Cross-cutting [`ModelClient`] middleware.
//!
//! Middleware wraps an `Arc<dyn ModelClient>` and implements
//! [`ModelClient`] itself, so layers stack cleanly:
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use std::time::Duration;
//! # use rebon_api::{
//! #     anthropic_client, openai_compatible_client, AnthropicClientConfig,
//! #     LoggingMiddleware, ModelClient, OpenAiCompatibleClientConfig, RetryConfig,
//! #     RetryMiddleware,
//! # };
//! # fn demo() {
//! let base: Arc<dyn ModelClient> =
//!     Arc::new(anthropic_client(AnthropicClientConfig::with_api_key("sk-xxx")));
//!
//! let retried = RetryMiddleware::wrap(
//!     base.clone(),
//!     RetryConfig::default(), // 10 retries, 500ms–32s exponential backoff with jitter
//! );
//! let logged = LoggingMiddleware::wrap(Arc::new(retried));
//! let _client: Arc<dyn ModelClient> = Arc::new(logged);
//! # }
//! ```
//!
//! Every middleware here lives at the [`ModelClient`] level rather
//! than the [`crate::ChatProvider`] level because the concerns are
//! provider-agnostic: a retry wrapper should not care whether it's
//! wrapping Anthropic, OpenAI, Bedrock, or a mock — and a provider
//! implementation should not have to reimplement retry logic.
//!
//! ## Retry behaviour
//!
//! | Feature | Details |
//! |---------|---------|
//! | Max retries | 10 (env `REBON_MAX_RETRIES` overrides) |
//! | Backoff | Exponential 500 ms × 2^(attempt−1), jitter ≤ 25 %, capped at 32 s |
//! | `retry-after` | Honoured on 429 / 529, up to [`MAX_HONOURED_RETRY_AFTER`]; longer than that fails the turn instead of waiting |
//! | 529 overloaded | Tracked separately; after 3 consecutive 529s the request fails |
//! | Context overflow | 400 "max_tokens exceed context limit" → shrink `max_tokens`, retry |
//! | Transport failures | A `reqwest` error with no HTTP status (DNS, TLS, proxy refusal, a dropped connection) is [`ModelError::Http`], which counts as transient — so an unreachable endpoint spends every attempt, about two minutes of backoff, before it fails |
//! | Non-retryable | BadRequest, Unauthorized, Permanent, Protocol, Cancelled → fail immediately |
//!
//! Every attempt logs at INFO, and giving up logs at WARN, so the default
//! log file of either surface explains a turn that spent two minutes
//! retrying. Error text is passed through [`crate::error::redact_secrets`]
//! first: the log is what a user attaches to a bug report.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::client::{ModelCapabilities, ModelClient};
use crate::error::{ModelError, ModelResult, FLOOR_OUTPUT_TOKENS};
use crate::events::StreamEventStream;
use crate::request::CreateMessageRequest;

// ── Constants ──────────────────────────────────────────────────

/// Default maximum retry attempts (including the initial attempt).
const DEFAULT_MAX_RETRIES: u32 = 10;

/// Base delay for exponential backoff.
const BASE_DELAY_MS: u64 = 500;

/// Maximum backoff cap.
const MAX_DELAY_MS: u64 = 32_000;

/// Maximum consecutive 529 (overloaded) errors before giving up.
const MAX_CONSECUTIVE_OVERLOADED: u32 = 3;

/// The longest a server's `retry-after` is waited out before the turn fails
/// instead.
///
/// A per-minute rate limit answers with seconds, and waiting is exactly right:
/// the turn continues and nobody needs to know. A **quota** — a weekly or
/// monthly cap — answers with hours, and honouring that verbatim produces a
/// turn that sits there until tomorrow. From outside it is indistinguishable
/// from a hang: the spinner turns, nothing arrives, and the one thing the user
/// needs to know — that this account is finished until a named time, and that
/// switching accounts is the fix — never reaches them.
///
/// So beyond this, the wait becomes an answer. `REBON_MAX_RETRY_AFTER_SECS`
/// overrides it for someone who would rather wait.
pub const MAX_HONOURED_RETRY_AFTER: Duration = Duration::from_secs(60);

fn get_env_max_retry_after() -> Duration {
    std::env::var("REBON_MAX_RETRY_AFTER_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(MAX_HONOURED_RETRY_AFTER)
}

/// How long to wait before the next attempt, or why there will not be one.
#[derive(Debug, PartialEq, Eq)]
enum NextAttempt {
    After(Duration),
    /// The server named a wait longer than a turn should sit through.
    TooLongToWait(Duration),
}

/// Renders a wait the way someone would say it out loud.
fn humanize(wait: Duration) -> String {
    let secs = wait.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

// ── RetryNotifier ────────────────────────────────────────────────

/// Current retry attempt progress, readable by the UI layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryProgress {
    /// 1-based attempt number (e.g. 2 means "second try").
    pub attempt: u32,
    /// Configured maximum retries.
    pub max_retries: u32,
}

impl std::fmt::Display for RetryProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Retry {}/{}", self.attempt, self.max_retries)
    }
}

/// Shared handle that the [`RetryMiddleware`] writes to and the TUI
/// reads each render frame. Cheap to clone (inner `Arc`).
///
/// ```no_run
/// # use rebon_api::RetryNotifier;
/// let notifier = RetryNotifier::new();
/// // middleware writes:
/// notifier.set(2, 10); // "Retry 2/10"
/// // TUI reads:
/// if let Some(p) = notifier.current() {
///     println!("{}", p); // "Retry 2/10"
/// }
/// notifier.clear();
/// ```
#[derive(Debug, Clone, Default)]
pub struct RetryNotifier {
    state: Arc<std::sync::Mutex<Option<RetryProgress>>>,
}

impl RetryNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the current retry progress.
    pub fn set(&self, attempt: u32, max_retries: u32) {
        *self.state.lock().expect("retry notifier poisoned") = Some(RetryProgress {
            attempt,
            max_retries,
        });
    }

    /// Clear retry progress (request succeeded or finally failed).
    pub fn clear(&self) {
        *self.state.lock().expect("retry notifier poisoned") = None;
    }

    /// Read the current retry progress, if any.
    pub fn current(&self) -> Option<RetryProgress> {
        *self.state.lock().expect("retry notifier poisoned")
    }
}

// ── RetryConfig ──────────────────────────────────────────────────

/// Retry configuration for [`RetryMiddleware`].
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Total attempts (including the initial attempt). Must be at
    /// least `1`; values below are clamped to `1`.
    pub max_retries: u32,
    /// Initial backoff delay before the second attempt.
    pub initial_backoff: Duration,
    /// Exponential backoff multiplier applied between attempts.
    pub backoff_multiplier: f64,
    /// Upper bound on backoff between attempts. Prevents runaway
    /// exponential growth.
    pub max_backoff: Duration,
    /// Maximum consecutive 529 errors before the middleware gives up.
    /// Prevents infinite retry loops during sustained overload.
    pub max_consecutive_overloaded: u32,
    /// Longest server-requested `retry-after` to wait out. Beyond this the
    /// turn fails with what the server said, rather than sitting through it.
    pub max_retry_after: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: get_env_max_retries(),
            initial_backoff: Duration::from_millis(BASE_DELAY_MS),
            backoff_multiplier: 2.0,
            max_backoff: Duration::from_millis(MAX_DELAY_MS),
            max_retry_after: get_env_max_retry_after(),
            max_consecutive_overloaded: MAX_CONSECUTIVE_OVERLOADED,
        }
    }
}

fn get_env_max_retries() -> u32 {
    std::env::var("REBON_MAX_RETRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_RETRIES)
}

impl RetryConfig {
    /// Fast defaults for tests: 3 attempts with sub-millisecond
    /// backoff so the suite stays snappy.
    pub fn for_tests() -> Self {
        Self {
            max_retries: 3,
            initial_backoff: Duration::from_millis(1),
            backoff_multiplier: 1.0,
            max_backoff: Duration::from_millis(1),
            max_consecutive_overloaded: MAX_CONSECUTIVE_OVERLOADED,
            // The production cap, not a test-shrunk one: a test that pushes a
            // long `retry-after` is testing the cap, and shrinking it here
            // would make that test pass for the wrong reason.
            max_retry_after: MAX_HONOURED_RETRY_AFTER,
        }
    }

    fn clamped_max_retries(&self) -> u32 {
        self.max_retries.max(1)
    }

    /// Exponential backoff with ≤25% jitter.
    ///
    /// Sequence (500 ms base, 32 s cap):
    /// attempt 1 → 500 ms + jitter
    /// attempt 2 → 1 s + jitter
    /// attempt 3 → 2 s + jitter
    /// …
    /// attempt 7+ → 32 s + jitter
    fn backoff_for_attempt(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }
        let base = self.initial_backoff.as_secs_f64();
        let mult = self.backoff_multiplier.max(1.0);
        let seconds = base * mult.powi((attempt - 1) as i32);
        let capped = seconds.min(self.max_backoff.as_secs_f64());

        // Add up to 25% jitter to avoid thundering herd.
        let jitter = jitter_fraction() * 0.25 * capped;
        Duration::from_secs_f64((capped + jitter).max(0.0))
    }
}

/// Returns a random fraction in [0, 1). Falls back to a
/// timestamp-based hash when `rand` is not available (which is fine
/// for jitter — we need decorrelation, not cryptographic quality).
fn jitter_fraction() -> f64 {
    // Use a simple xorshift-like approach seeded from the current
    // instant's nanoseconds. Each call gets a different value
    // because `Instant::now()` has sub-microsecond granularity.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    // Simple hash to spread bits.
    let mut x = nanos.wrapping_mul(6364136223846793005).wrapping_add(1);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    (x as f64) / (u64::MAX as f64)
}

// ── RetryMiddleware ──────────────────────────────────────────────

/// [`ModelClient`] middleware that retries transient failures.
///
/// Retries kick in when the inner client returns an error whose
/// [`ModelError::is_transient`] is `true` — matching the classification
/// the provider layer stamps via [`crate::classify_http_error`]. Any
/// permanent failure, auth error, or cancellation is surfaced to the
/// caller immediately without retry.
///
/// ## Special retry behaviours
///
/// - **`retry-after` header**: when a transient error carries a
///   server-requested delay, the middleware honours it instead of
///   the computed backoff.
/// - **529 overloaded**: tracked with a consecutive counter. After
///   [`RetryConfig::max_consecutive_overloaded`] consecutive 529
///   errors the request fails — this prevents infinite retries
///   during sustained capacity issues.
/// - **Context overflow (400)**: when the API rejects a request
///   because `input_tokens + max_tokens > context_limit`, the
///   middleware reduces `max_tokens` to fit (floor
///   [`FLOOR_OUTPUT_TOKENS`]) and retries once.
#[derive(Clone)]
pub struct RetryMiddleware {
    inner: Arc<dyn ModelClient>,
    config: RetryConfig,
    notifier: RetryNotifier,
}

impl std::fmt::Debug for RetryMiddleware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryMiddleware")
            .field("provider", &self.inner.provider_name())
            .field("config", &self.config)
            .finish()
    }
}

struct RetryNotifierClearGuard {
    notifier: RetryNotifier,
}

impl Drop for RetryNotifierClearGuard {
    fn drop(&mut self) {
        self.notifier.clear();
    }
}

impl RetryMiddleware {
    /// Wrap `inner` with the given retry config. Creates an internal
    /// notifier that is not externally observable — use
    /// [`Self::wrap_with_notifier`] when the TUI needs to display
    /// retry progress.
    pub fn wrap(inner: Arc<dyn ModelClient>, config: RetryConfig) -> Self {
        Self {
            inner,
            config,
            notifier: RetryNotifier::new(),
        }
    }

    /// Wrap `inner` with an externally-observable [`RetryNotifier`].
    /// The caller keeps a clone of `notifier` and polls
    /// [`RetryNotifier::current`] each render frame to show retry
    /// progress in the UI.
    pub fn wrap_with_notifier(
        inner: Arc<dyn ModelClient>,
        config: RetryConfig,
        notifier: RetryNotifier,
    ) -> Self {
        Self {
            inner,
            config,
            notifier,
        }
    }
}

#[async_trait]
impl ModelClient for RetryMiddleware {
    fn provider_name(&self) -> &'static str {
        self.inner.provider_name()
    }

    fn fork_for_sub_agent(&self) -> Option<Arc<dyn ModelClient>> {
        self.inner.fork_for_sub_agent().map(|inner| {
            Arc::new(RetryMiddleware::wrap(inner, self.config.clone())) as Arc<dyn ModelClient>
        })
    }

    fn fork_for_sub_agent_with_cache_key(
        &self,
        prompt_cache_key: Option<String>,
    ) -> Option<Arc<dyn ModelClient>> {
        self.inner
            .fork_for_sub_agent_with_cache_key(prompt_cache_key)
            .map(|inner| {
                Arc::new(RetryMiddleware::wrap(inner, self.config.clone())) as Arc<dyn ModelClient>
            })
    }

    fn context_prune_handle(&self) -> Option<crate::context_prune::PruneLevelHandle> {
        self.inner.context_prune_handle()
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::forwarded(self.inner.as_ref())
    }

    fn reset_session_state(&self) {
        self.inner.reset_session_state();
    }

    fn end_turn(&self) {
        self.inner.end_turn();
    }

    fn invalidate_previous_response_id(&self) {
        self.inner.invalidate_previous_response_id();
    }

    async fn create_message_stream(
        &self,
        request: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream> {
        let _clear_retry_progress = RetryNotifierClearGuard {
            notifier: self.notifier.clone(),
        };
        let total = self.config.clamped_max_retries();
        let mut last_err: Option<ModelError> = None;
        let mut consecutive_overloaded: u32 = 0;
        // Mutable copy of max_tokens for context-overflow adjustment.
        let mut effective_max_tokens = request.max_tokens;

        for attempt in 0..total {
            if attempt > 0 {
                // Notify the UI about the upcoming retry.
                self.notifier.set(attempt, total);
                match self.plan_next_attempt(attempt, last_err.as_ref()) {
                    NextAttempt::After(delay) => {
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                    }
                    NextAttempt::TooLongToWait(asked) => {
                        tracing::warn!(
                            wait_secs = asked.as_secs(),
                            "rebon-api retry: the provider asked for a wait longer than a turn \
                             should sit through; failing instead"
                        );
                        return Err(quota_exhausted(last_err.as_ref(), asked));
                    }
                }
            }

            // Build the request for this attempt (may have adjusted
            // max_tokens from a previous context-overflow error).
            let mut req = request.clone();
            if effective_max_tokens != request.max_tokens {
                req.max_tokens = effective_max_tokens;
            }

            match self.inner.create_message_stream(req).await {
                Ok(stream) => {
                    return Ok(stream);
                }

                Err(err) if err.is_overloaded() => {
                    consecutive_overloaded += 1;
                    if consecutive_overloaded >= self.config.max_consecutive_overloaded {
                        tracing::warn!(
                            consecutive = consecutive_overloaded,
                            "rebon-api retry: consecutive overloaded errors exceeded limit"
                        );
                        return Err(err);
                    }
                    tracing::info!(
                        attempt = attempt + 1,
                        total,
                        consecutive_overloaded,
                        error = %crate::error::redact_secrets(&err.to_string()),
                        "rebon-api retry: overloaded (529), retrying"
                    );
                    last_err = Some(err);
                    continue;
                }

                Err(ref err) if err.context_overflow().is_some() => {
                    let overflow = err.context_overflow().unwrap();

                    if let (Some(input_tokens), Some(context_limit)) =
                        (overflow.input_tokens, overflow.context_limit)
                    {
                        let available = context_limit.saturating_sub(input_tokens);
                        let new_max = available.max(FLOOR_OUTPUT_TOKENS);
                        if new_max >= effective_max_tokens || new_max <= FLOOR_OUTPUT_TOKENS {
                            tracing::warn!(
                                input_tokens,
                                context_limit,
                                "rebon-api retry: context overflow, cannot reduce max_tokens further"
                            );
                        } else {
                            tracing::info!(
                                old_max = effective_max_tokens,
                                new_max,
                                input_tokens,
                                context_limit,
                                "rebon-api retry: reducing max_tokens for context overflow"
                            );
                            effective_max_tokens = new_max;
                            consecutive_overloaded = 0;
                            last_err = None;
                            continue;
                        }
                    } else {
                        tracing::info!(
                            error = %err,
                            "rebon-api retry: context window exceeded without token details; resetting session state and retrying"
                        );
                        self.inner.reset_session_state();
                        consecutive_overloaded = 0;
                        last_err = None;
                        continue;
                    }
                    // Fall through — the `ref err` borrow is released
                    // and we need to re-match to take ownership.
                }

                Err(err) if err.is_transient() => {
                    consecutive_overloaded = 0; // reset on non-529 transient
                                                // INFO, not DEBUG: this is the line a user's log has to
                                                // carry. Both surfaces log at `info` by default, and a
                                                // request that quietly burns ten attempts against a
                                                // provider that is up — a proxy refusing the connection,
                                                // a DNS or TLS failure, all of which arrive with no HTTP
                                                // status — used to leave nothing behind to read.
                    tracing::info!(
                        attempt = attempt + 1,
                        total,
                        status = ?err.http_status(),
                        transport_only = err.http_status().is_none(),
                        error = %crate::error::redact_secrets(&err.to_string()),
                        "rebon-api retry: transient failure, retrying"
                    );
                    last_err = Some(err);
                    continue;
                }

                Err(err) => {
                    tracing::info!(
                        attempt = attempt + 1,
                        total,
                        status = ?err.http_status(),
                        error = %crate::error::redact_secrets(&err.to_string()),
                        "rebon-api retry: permanent failure, not retrying"
                    );
                    return Err(err);
                }
            }
        }
        let exhausted = last_err
            .unwrap_or_else(|| ModelError::other("retry middleware exhausted without error"));
        tracing::warn!(
            attempts = total,
            status = ?exhausted.http_status(),
            transport_only = exhausted.http_status().is_none(),
            error = %crate::error::redact_secrets(&exhausted.to_string()),
            "rebon-api retry: gave up after every attempt failed"
        );
        Err(exhausted)
    }
}

impl RetryMiddleware {
    /// What to do before the next attempt.
    ///
    /// The server's `retry-after` wins over the backoff curve — it knows when
    /// it will serve again and the curve is a guess. It wins only up to
    /// [`RetryConfig::max_retry_after`], though: past that the honest move is
    /// to stop and say so, because a turn cannot tell a user it is waiting.
    fn plan_next_attempt(&self, attempt: u32, last_err: Option<&ModelError>) -> NextAttempt {
        if let Some(asked) = last_err.and_then(ModelError::retry_after) {
            if asked > self.config.max_retry_after {
                return NextAttempt::TooLongToWait(asked);
            }
            return NextAttempt::After(asked);
        }
        NextAttempt::After(self.config.backoff_for_attempt(attempt))
    }
}

/// The error a turn ends with when the provider will not serve it for hours.
///
/// Permanent rather than transient: something above this that retries on
/// transient errors would put the turn straight back into the same wait, and
/// the point is to hand the decision to the person who can change accounts.
fn quota_exhausted(last_err: Option<&ModelError>, asked: Duration) -> ModelError {
    let detail = last_err
        .map(|err| err.to_string())
        .unwrap_or_else(|| "rate limited".to_string());
    ModelError::Permanent(format!(
        "the provider will not serve this request for another {} ({detail}). \
         Rebon stopped rather than waiting that out, which would look like a hung turn. \
         Switch to another account or provider with /provider, or try again after that window.",
        humanize(asked)
    ))
}

/// Simple [`ModelClient`] middleware that logs request start and
/// outcome via `tracing`. Useful as a demonstration of the
/// middleware composability pattern and as a cheap debugging aid.
#[derive(Clone)]
pub struct LoggingMiddleware {
    inner: Arc<dyn ModelClient>,
    label: String,
}

impl std::fmt::Debug for LoggingMiddleware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoggingMiddleware")
            .field("provider", &self.inner.provider_name())
            .field("label", &self.label)
            .finish()
    }
}

impl LoggingMiddleware {
    /// Wrap `inner`, using the inner provider name as the log label.
    pub fn wrap(inner: Arc<dyn ModelClient>) -> Self {
        let label = inner.provider_name().to_string();
        Self { inner, label }
    }

    /// Wrap `inner` with an explicit label — useful when stacking
    /// multiple instances and you want them distinguishable in logs.
    pub fn with_label(inner: Arc<dyn ModelClient>, label: impl Into<String>) -> Self {
        Self {
            inner,
            label: label.into(),
        }
    }
}

#[async_trait]
impl ModelClient for LoggingMiddleware {
    fn provider_name(&self) -> &'static str {
        self.inner.provider_name()
    }

    fn fork_for_sub_agent(&self) -> Option<Arc<dyn ModelClient>> {
        self.inner.fork_for_sub_agent().map(|inner| {
            Arc::new(LoggingMiddleware::with_label(inner, self.label.clone()))
                as Arc<dyn ModelClient>
        })
    }

    fn fork_for_sub_agent_with_cache_key(
        &self,
        prompt_cache_key: Option<String>,
    ) -> Option<Arc<dyn ModelClient>> {
        self.inner
            .fork_for_sub_agent_with_cache_key(prompt_cache_key)
            .map(|inner| {
                Arc::new(LoggingMiddleware::with_label(inner, self.label.clone()))
                    as Arc<dyn ModelClient>
            })
    }

    fn context_prune_handle(&self) -> Option<crate::context_prune::PruneLevelHandle> {
        self.inner.context_prune_handle()
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::forwarded(self.inner.as_ref())
    }

    fn reset_session_state(&self) {
        self.inner.reset_session_state();
    }

    fn end_turn(&self) {
        self.inner.end_turn();
    }

    fn invalidate_previous_response_id(&self) {
        self.inner.invalidate_previous_response_id();
    }

    async fn create_message_stream(
        &self,
        request: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream> {
        tracing::debug!(
            provider = %self.label,
            model = %request.model,
            messages = request.messages.len(),
            tools = request.tools.len(),
            "rebon-api model request start"
        );
        let result = self.inner.create_message_stream(request).await;
        match &result {
            Ok(_) => tracing::debug!(provider = %self.label, "rebon-api model request ok"),
            Err(err) => tracing::warn!(
                provider = %self.label,
                error = %crate::error::redact_secrets(&err.to_string()),
                "rebon-api model request err"
            ),
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::StreamEvent;
    use crate::mock::MockModelClient;
    use crate::types::Usage;

    fn dummy_stream() -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message_id: "m".into(),
                model: "mock".into(),
                usage: Usage::default(),
            },
            StreamEvent::MessageStop,
        ]
    }

    #[tokio::test(start_paused = true)]
    async fn retry_middleware_recovers_from_transient_error() {
        let mock = Arc::new(MockModelClient::new());
        // First attempt: transient error. Second attempt: scripted success.
        mock.push_error(ModelError::transient("hiccup"));
        mock.push_turn(dummy_stream());

        let client: Arc<dyn ModelClient> = mock.clone();
        let retry = RetryMiddleware::wrap(client, RetryConfig::for_tests());
        let msg = retry
            .create_message(CreateMessageRequest::simple("mock", "ping"))
            .await
            .unwrap();
        assert_eq!(msg.id, "m");
        assert_eq!(mock.call_count(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_middleware_surfaces_permanent_error_without_retry() {
        let mock = Arc::new(MockModelClient::new());
        mock.push_error(ModelError::Permanent("gone".into()));

        let client: Arc<dyn ModelClient> = mock.clone();
        let retry = RetryMiddleware::wrap(client, RetryConfig::for_tests());
        let err = retry
            .create_message(CreateMessageRequest::simple("mock", "ping"))
            .await
            .unwrap_err();
        assert!(matches!(err, ModelError::Permanent(_)));
        assert_eq!(mock.call_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_middleware_exhausts_after_max_retries() {
        let mock = Arc::new(MockModelClient::new());
        // Queue 5 transient errors; max_retries = 3, so only 3 should be consumed.
        for _ in 0..5 {
            mock.push_error(ModelError::transient("still no"));
        }
        let client: Arc<dyn ModelClient> = mock.clone();
        let retry = RetryMiddleware::wrap(client, RetryConfig::for_tests());
        let err = retry
            .create_message(CreateMessageRequest::simple("mock", "ping"))
            .await
            .unwrap_err();
        assert!(matches!(err, ModelError::Transient { .. }));
        assert_eq!(mock.call_count(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_middleware_stops_after_consecutive_overloaded() {
        let mock = Arc::new(MockModelClient::new());
        // Queue more than MAX_CONSECUTIVE_OVERLOADED 529 errors.
        for _ in 0..5 {
            mock.push_error(ModelError::overloaded("overloaded", None));
        }
        let client: Arc<dyn ModelClient> = mock.clone();
        let retry = RetryMiddleware::wrap(client, RetryConfig::for_tests());
        let err = retry
            .create_message(CreateMessageRequest::simple("mock", "ping"))
            .await
            .unwrap_err();
        assert!(err.is_overloaded());
        // Should have tried MAX_CONSECUTIVE_OVERLOADED times.
        assert_eq!(mock.call_count(), MAX_CONSECUTIVE_OVERLOADED as usize);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_middleware_resets_overloaded_counter_on_non_529() {
        let mock = Arc::new(MockModelClient::new());
        // 2 overloaded, then 1 transient (resets counter), then 2 more overloaded, then success.
        mock.push_error(ModelError::overloaded("529-1", None));
        mock.push_error(ModelError::overloaded("529-2", None));
        mock.push_error(ModelError::transient("503")); // resets consecutive counter
        mock.push_error(ModelError::overloaded("529-3", None));
        mock.push_error(ModelError::overloaded("529-4", None));
        mock.push_turn(dummy_stream());

        let client: Arc<dyn ModelClient> = mock.clone();
        let config = RetryConfig {
            max_retries: 10,
            ..RetryConfig::for_tests()
        };
        let retry = RetryMiddleware::wrap(client, config);
        let msg = retry
            .create_message(CreateMessageRequest::simple("mock", "ping"))
            .await
            .unwrap();
        assert_eq!(msg.id, "m");
        assert_eq!(mock.call_count(), 6);
    }

    /// A quota answers with hours. Waiting it out is indistinguishable from a
    /// hang, so the wait becomes the answer instead.
    #[tokio::test(start_paused = true)]
    async fn a_retry_after_measured_in_hours_ends_the_turn_instead_of_waiting() {
        let mock = Arc::new(MockModelClient::new());
        mock.push_error(ModelError::transient_http(
            "weekly limit reached",
            429,
            Some(Duration::from_secs(2 * 60 * 60)),
        ));
        // Would succeed if anything ever asked again — nothing should.
        mock.push_turn(dummy_stream());

        let client: Arc<dyn ModelClient> = mock.clone();
        let retry = RetryMiddleware::wrap(client, RetryConfig::for_tests());
        let err = retry
            .create_message(CreateMessageRequest::simple("mock", "ping"))
            .await
            .expect_err("a two-hour wait is not waited out");

        assert!(
            matches!(err, ModelError::Permanent(_)),
            "must not look transient, or the layer above retries straight back into it: {err:?}"
        );
        let text = err.to_string();
        assert!(text.contains("2h"), "says how long: {text}");
        assert!(
            text.contains("weekly limit reached"),
            "keeps what the server said: {text}"
        );
        assert!(
            text.contains("/provider"),
            "says what to do about it: {text}"
        );
        // The queued success is still queued: the second attempt never
        // happened, which is the point — nobody waited for it.
        let leftover = mock
            .create_message(CreateMessageRequest::simple("mock", "ping"))
            .await;
        assert!(
            leftover.is_ok(),
            "the success was never consumed by a retry"
        );
    }

    /// A per-minute limit is still waited out: that is the case the header is
    /// for, and the turn continues without anyone needing to know.
    #[tokio::test(start_paused = true)]
    async fn a_short_retry_after_is_still_waited_out() {
        let mock = Arc::new(MockModelClient::new());
        mock.push_error(ModelError::transient_http(
            "slow down",
            429,
            Some(Duration::from_secs(30)),
        ));
        mock.push_turn(dummy_stream());

        let client: Arc<dyn ModelClient> = mock.clone();
        let retry = RetryMiddleware::wrap(client, RetryConfig::for_tests());
        retry
            .create_message(CreateMessageRequest::simple("mock", "ping"))
            .await
            .expect("thirty seconds is worth waiting");
    }

    /// Exactly at the cap is still honoured — the cap is what is too long, not
    /// what is long enough.
    #[tokio::test(start_paused = true)]
    async fn the_cap_itself_is_honoured() {
        let mock = Arc::new(MockModelClient::new());
        mock.push_error(ModelError::transient_http(
            "at the edge",
            429,
            Some(MAX_HONOURED_RETRY_AFTER),
        ));
        mock.push_turn(dummy_stream());

        let client: Arc<dyn ModelClient> = mock.clone();
        let retry = RetryMiddleware::wrap(client, RetryConfig::for_tests());
        retry
            .create_message(CreateMessageRequest::simple("mock", "ping"))
            .await
            .expect("the cap is inclusive");
    }

    /// Without a `retry-after` nothing changes: the backoff curve keeps its own
    /// 32-second ceiling and the cap never comes into it.
    #[tokio::test(start_paused = true)]
    async fn a_transient_error_without_retry_after_still_uses_the_backoff_curve() {
        let config = RetryConfig::for_tests();
        let retry = RetryMiddleware::wrap(
            Arc::new(MockModelClient::new()) as Arc<dyn ModelClient>,
            config,
        );
        let plain = ModelError::transient("connection reset");
        assert!(matches!(
            retry.plan_next_attempt(1, Some(&plain)),
            NextAttempt::After(_)
        ));
    }

    #[test]
    fn waits_are_rendered_the_way_someone_would_say_them() {
        assert_eq!(humanize(Duration::from_secs(45)), "45s");
        assert_eq!(humanize(Duration::from_secs(90)), "1m");
        assert_eq!(humanize(Duration::from_secs(3 * 3600 + 25 * 60)), "3h 25m");
        assert_eq!(humanize(Duration::from_secs(50 * 3600)), "50h");
    }

    #[tokio::test(start_paused = true)]
    async fn retry_middleware_honours_retry_after() {
        let mock = Arc::new(MockModelClient::new());
        mock.push_error(ModelError::transient_http(
            "rate limited",
            429,
            Some(Duration::from_secs(2)),
        ));
        mock.push_turn(dummy_stream());

        let client: Arc<dyn ModelClient> = mock.clone();
        let config = RetryConfig {
            max_retries: 3,
            initial_backoff: Duration::from_millis(1),
            backoff_multiplier: 1.0,
            max_backoff: Duration::from_millis(1),
            ..RetryConfig::for_tests()
        };
        let retry = RetryMiddleware::wrap(client, config);

        let start = tokio::time::Instant::now();
        let msg = retry
            .create_message(CreateMessageRequest::simple("mock", "ping"))
            .await
            .unwrap();
        assert_eq!(msg.id, "m");
        // In paused time, the sleep(2s) should have advanced the clock.
        assert!(start.elapsed() >= Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn retry_middleware_adjusts_max_tokens_on_context_overflow() {
        let mock = Arc::new(MockModelClient::new());
        // First call: context overflow error.
        mock.push_error(ModelError::BadRequest(
            "400: input length and `max_tokens` exceed context limit: 180000 + 20000 > 200000"
                .into(),
        ));
        // Second call should use adjusted max_tokens and succeed.
        mock.push_turn(dummy_stream());

        let client: Arc<dyn ModelClient> = mock.clone();
        let retry = RetryMiddleware::wrap(client, RetryConfig::for_tests());
        let msg = retry
            .create_message(CreateMessageRequest::simple("mock", "ping").with_max_tokens(20000))
            .await
            .unwrap();
        assert_eq!(msg.id, "m");
        assert_eq!(mock.call_count(), 2);
        // Verify the second request had reduced max_tokens.
        let captured = mock.captured_requests();
        assert_eq!(captured[1].max_tokens, 20000); // 200000 - 180000 = 20000
    }

    #[tokio::test(start_paused = true)]
    async fn logging_middleware_passes_through_and_records_label() {
        let mock = Arc::new(MockModelClient::new());
        mock.push_turn(dummy_stream());
        let client: Arc<dyn ModelClient> = mock.clone();
        let logged = LoggingMiddleware::with_label(client, "demo");
        assert_eq!(logged.provider_name(), "mock");
        let msg = logged
            .create_message(CreateMessageRequest::simple("mock", "ping"))
            .await
            .unwrap();
        assert_eq!(msg.id, "m");
    }

    #[tokio::test(start_paused = true)]
    async fn middleware_stacks_cleanly() {
        // Compose: logging → retry → mock. First call fails
        // transiently, second succeeds — both layers stay transparent.
        let mock = Arc::new(MockModelClient::new());
        mock.push_error(ModelError::transient("first try"));
        mock.push_turn(dummy_stream());

        let base: Arc<dyn ModelClient> = mock.clone();
        let retry = Arc::new(RetryMiddleware::wrap(base, RetryConfig::for_tests()));
        let logged = LoggingMiddleware::wrap(retry);
        let msg = logged
            .create_message(CreateMessageRequest::simple("mock", "ping"))
            .await
            .unwrap();
        assert_eq!(msg.id, "m");
        assert_eq!(mock.call_count(), 2);
    }

    #[test]
    fn stacked_middleware_forwards_the_entire_capability_snapshot() {
        let mock = Arc::new(MockModelClient::new());
        mock.set_supports_forced_tool_choice(true);
        mock.set_supports_anchored_minimal(true);
        mock.set_supports_request_scoped_transient_context(false);
        mock.set_thinking_replay_requires_signature(false);
        let expected = mock.capabilities();

        let base: Arc<dyn ModelClient> = mock;
        let retry = Arc::new(RetryMiddleware::wrap(base, RetryConfig::for_tests()));
        let logged = LoggingMiddleware::wrap(retry);

        assert_eq!(logged.capabilities(), expected);
        assert!(logged.supports_forced_tool_choice());
        assert!(logged.supports_anchored_minimal());
        assert!(!logged.supports_request_scoped_transient_context());
        assert!(!logged.thinking_replay_requires_signature());
    }

    #[test]
    fn backoff_for_attempt_is_bounded_by_max_backoff() {
        let config = RetryConfig {
            max_retries: 10,
            initial_backoff: Duration::from_secs(1),
            backoff_multiplier: 3.0,
            max_backoff: Duration::from_secs(2),
            ..RetryConfig::for_tests()
        };
        assert_eq!(config.backoff_for_attempt(0), Duration::ZERO);
        // attempt 1: 1s + jitter (≤0.25s), so between 1.0 and 1.25s
        let d1 = config.backoff_for_attempt(1);
        assert!(d1 >= Duration::from_secs(1) && d1 <= Duration::from_millis(1250));
        // attempt 2: min(3s, 2s cap) = 2s + jitter ≤ 0.5s
        let d2 = config.backoff_for_attempt(2);
        assert!(d2 >= Duration::from_secs(2) && d2 <= Duration::from_millis(2500));
        // attempt 3: min(9s, 2s cap) = 2s + jitter
        let d3 = config.backoff_for_attempt(3);
        assert!(d3 >= Duration::from_secs(2) && d3 <= Duration::from_millis(2500));
    }

    #[test]
    fn clamped_max_retries_is_at_least_one() {
        let config = RetryConfig {
            max_retries: 0,
            ..RetryConfig::for_tests()
        };
        assert_eq!(config.clamped_max_retries(), 1);
    }

    #[test]
    fn default_config_has_production_values() {
        // Clear env to test actual defaults.
        std::env::remove_var("REBON_MAX_RETRIES");
        let config = RetryConfig {
            max_retries: DEFAULT_MAX_RETRIES,
            ..RetryConfig::default()
        };
        assert_eq!(config.max_retries, 10);
        assert_eq!(config.initial_backoff, Duration::from_millis(500));
        assert_eq!(config.max_backoff, Duration::from_millis(32_000));
        assert_eq!(config.max_consecutive_overloaded, 3);
    }

    // ── fork_for_sub_agent through middleware ────────────────────

    /// Wrapper around MockModelClient that implements
    /// `fork_for_sub_agent` by returning a fresh MockModelClient
    /// with a distinct `provider_name`.
    struct ForkableMock {
        inner: MockModelClient,
    }

    #[async_trait]
    impl ModelClient for ForkableMock {
        fn provider_name(&self) -> &'static str {
            "forkable-mock"
        }

        async fn create_message_stream(
            &self,
            request: CreateMessageRequest,
        ) -> ModelResult<StreamEventStream> {
            self.inner.create_message_stream(request).await
        }

        fn fork_for_sub_agent(&self) -> Option<Arc<dyn ModelClient>> {
            let child = MockModelClient::new();
            child.push_turn(dummy_stream());
            Some(Arc::new(ForkableMock { inner: child }))
        }
    }

    #[test]
    fn retry_middleware_fork_passes_through_when_inner_supports_fork() {
        let inner: Arc<dyn ModelClient> = Arc::new(ForkableMock {
            inner: MockModelClient::new(),
        });
        let retry = RetryMiddleware::wrap(inner, RetryConfig::for_tests());

        let forked = retry.fork_for_sub_agent();
        assert!(forked.is_some());
        // The forked client is a new RetryMiddleware wrapping the
        // forked inner. provider_name comes from the inner.
        assert_eq!(forked.unwrap().provider_name(), "forkable-mock");
    }

    #[test]
    fn retry_middleware_fork_returns_none_when_inner_does_not_support_fork() {
        let inner: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let retry = RetryMiddleware::wrap(inner, RetryConfig::for_tests());

        assert!(retry.fork_for_sub_agent().is_none());
    }

    #[test]
    fn logging_middleware_fork_passes_through_when_inner_supports_fork() {
        let inner: Arc<dyn ModelClient> = Arc::new(ForkableMock {
            inner: MockModelClient::new(),
        });
        let logged = LoggingMiddleware::wrap(inner);

        let forked = logged.fork_for_sub_agent();
        assert!(forked.is_some());
        assert_eq!(forked.unwrap().provider_name(), "forkable-mock");
    }

    #[test]
    fn logging_middleware_fork_returns_none_when_inner_does_not_support_fork() {
        let inner: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let logged = LoggingMiddleware::wrap(inner);

        assert!(logged.fork_for_sub_agent().is_none());
    }

    #[test]
    fn stacked_middleware_fork_chains_through_all_layers() {
        let inner: Arc<dyn ModelClient> = Arc::new(ForkableMock {
            inner: MockModelClient::new(),
        });
        let retry = Arc::new(RetryMiddleware::wrap(inner, RetryConfig::for_tests()));
        let logged = LoggingMiddleware::wrap(retry);

        let forked = logged.fork_for_sub_agent();
        assert!(forked.is_some());
        assert_eq!(forked.unwrap().provider_name(), "forkable-mock");
    }

    #[test]
    fn mock_model_client_fork_returns_none_by_default() {
        let mock = MockModelClient::new();
        assert!(mock.fork_for_sub_agent().is_none());
    }
}
