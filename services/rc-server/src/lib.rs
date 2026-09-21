//! Rebon Remote Control (RC) service — the server half of RFC-0008.
//!
//! RC is **not** `relay-server`. Relay is a zero-knowledge frame
//! forwarder with two slots and no persisted payloads; RC is a
//! plaintext control plane with accounts, durable environments and a
//! work queue that survives both ends going offline. They are separate
//! services with separate databases on purpose — see RFC-0008 §2.
//!
//! This crate implements the data model, the
//! credential hierarchy minus OIDC, every HTTP route in RFC-0008 §5,
//! leases and the cleanup sweeper, and the session stream
//! ([`routes::stream`], with its live topology in [`hub`]), plus the
//! cursor-paged history and session list a controller reads it back
//! through ([`routes::history`], with its cursors in [`cursor`]). The
//! session runner, the web UI and encryption at rest are not here yet
//! and are deliberately absent rather than stubbed.
//!
//! Protocol types are imported from [`rebon_bridge`], never re-declared,
//! so a change to the wire contract breaks this build.

#![forbid(unsafe_code)]
// Auth and validation helpers return `Result<T, Response>`: the error
// *is* the prepared HTTP response, which is how a handler can bail out
// with the right status without inventing an error enum that would only
// be mapped straight back to a status. `Response` is a large type, so
// clippy flags the pattern — but these values are built once per failed
// request and returned immediately, never carried on a hot path, and
// boxing them would put a deref at every single early return.
#![allow(clippy::result_large_err)]

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::Path as StdPath,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use serde::Serialize;
use tokio::sync::Notify;
use tower_http::{limit::RequestBodyLimitLayer, trace::TraceLayer};
use tracing::error as log_error;
use url::Url;

pub mod auth;
pub mod cursor;
pub mod hub;
pub mod ids;
pub mod lease;
pub mod routes;
pub mod store;
pub mod work_secret;

use hub::{SessionHub, SessionHubs};
use ids::TokenDigest;
use rebon_bridge::session_stream::close_code;
use store::{work_state, Store};

/// Hard ceiling on a long poll, whatever the client asks for.
pub const MAX_POLL_WAIT: Duration = Duration::from_secs(60);

/// Hard ceiling on the replay a controller is sent when it attaches.
///
/// The replay is written to the socket before the connection starts
/// draining its live queue, so an unbounded N would be a way to make a
/// connect arbitrarily expensive and to overflow the attaching socket's
/// own outbound budget. Paging further back is a job for a history
/// endpoint, not for a bigger replay.
pub const MAX_SESSION_REPLAY_EVENTS: usize = 2_000;

/// Hard ceiling on the page size of the two paged read routes, whatever
/// the configuration says.
pub const MAX_PAGE_LIMIT: usize = 1_000;

/// Soft cap on the stored payload bytes one history page carries. A
/// page stops early once the next event would push it past this, but
/// always carries at least one event, so a single frame at the body
/// limit is still readable. It bounds the response a `limit` of
/// [`MAX_PAGE_LIMIT`] large frames would otherwise produce.
pub const MAX_EVENT_PAGE_BYTES: usize = 4 * 1024 * 1024;

/// Runtime configuration, entirely from `REBON_RC_*` environment
/// variables in production (see `main.rs`) and constructed directly by
/// tests.
#[derive(Clone, Debug)]
pub struct Config {
    /// Address to listen on.
    pub bind: SocketAddr,
    /// Public origin of the HTTP API, echoed in diagnostics.
    pub public_url: String,
    /// Public origin of the session WebSocket ingress. Embedded in the
    /// work secret so a worker knows where to attach the session stream.
    pub session_ingress_url: String,
    /// How long a lease survives without a heartbeat.
    pub lease_ttl: Duration,
    /// Lifetime of a device access token.
    pub access_token_ttl: Duration,
    /// Long-poll duration used when the client sends no `timeoutMs`.
    pub default_poll_wait: Duration,
    /// Upper bound on a long poll; clamped to [`MAX_POLL_WAIT`].
    pub max_poll_wait: Duration,
    /// Maximum request body, in bytes. Prompts are not 1 KiB, so this
    /// is far more generous than relay's limit. It is also the cap on a
    /// single session-stream frame: a prompt is the same size whichever
    /// surface it arrives on, so it is one limit rather than two.
    pub max_body_bytes: usize,
    /// How many persisted events a controller is replayed on attach.
    pub session_replay_events: usize,
    /// Page size of the paged read routes when the client sends no
    /// `limit`.
    pub page_limit_default: usize,
    /// Largest page the paged read routes return; a bigger `limit` is
    /// clamped to it. At most [`MAX_PAGE_LIMIT`].
    pub page_limit_max: usize,
    /// Burst size of the per-IP budget on credential-issuing routes.
    pub auth_rate_burst: u32,
    /// Sustained refill of that budget, in requests per minute.
    pub auth_rate_refill_per_minute: u32,
    /// One-time bootstrap token that mints the first account. Required
    /// until an account exists; ignored afterwards.
    pub bootstrap_token: Option<String>,
    /// Take the client IP for per-source limits from the rightmost
    /// `X-Forwarded-For` entry instead of the TCP peer. Enable only
    /// behind a proxy that overwrites the header.
    pub trust_forwarded_for: bool,
}

impl Config {
    /// Reject a configuration that would be unsafe or unusable before
    /// the listener is bound.
    pub fn validate(&self) -> Result<(), String> {
        validate_origin(&self.public_url, &["https"], "public URL")?;
        validate_origin(&self.session_ingress_url, &["wss"], "session ingress URL")?;
        if self.lease_ttl.is_zero() || self.access_token_ttl.is_zero() {
            return Err("lease and access-token lifetimes must be nonzero".into());
        }
        if self.default_poll_wait.is_zero() || self.max_poll_wait.is_zero() {
            return Err("poll durations must be nonzero".into());
        }
        if self.max_poll_wait > MAX_POLL_WAIT {
            return Err("maximum poll wait must not exceed 60 seconds".into());
        }
        if self.default_poll_wait > self.max_poll_wait {
            return Err("default poll wait must not exceed the maximum".into());
        }
        if self.max_body_bytes == 0 {
            return Err("body limit must be nonzero".into());
        }
        if self.session_replay_events == 0 {
            return Err("session replay must be at least one event".into());
        }
        if self.session_replay_events > MAX_SESSION_REPLAY_EVENTS {
            return Err(format!(
                "session replay must not exceed {MAX_SESSION_REPLAY_EVENTS} events"
            ));
        }
        if self.page_limit_default == 0 || self.page_limit_max == 0 {
            return Err("page limits must be nonzero".into());
        }
        if self.page_limit_max > MAX_PAGE_LIMIT {
            return Err(format!("page limit must not exceed {MAX_PAGE_LIMIT}"));
        }
        if self.page_limit_default > self.page_limit_max {
            return Err("default page limit must not exceed the maximum".into());
        }
        if self.auth_rate_burst == 0 || self.auth_rate_refill_per_minute == 0 {
            return Err("auth rate limits must be nonzero".into());
        }
        if let Some(token) = &self.bootstrap_token {
            if !ids::canonical_token(token) {
                return Err(
                    "bootstrap token must be a base64url-encoded 32-byte value (43 characters)"
                        .into(),
                );
            }
        }
        Ok(())
    }
}

/// Require an origin-only URL, permitting an insecure loopback scheme
/// in debug builds so local development does not need certificates.
fn validate_origin(value: &str, secure_schemes: &[&str], label: &str) -> Result<(), String> {
    let url = Url::parse(value).map_err(|error| format!("invalid {label}: {error}"))?;
    if url.cannot_be_a_base()
        || url.host_str().is_none()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(format!(
            "{label} must be an origin without credentials, path, query, or fragment"
        ));
    }
    if secure_schemes.contains(&url.scheme()) {
        return Ok(());
    }
    let insecure = matches!(url.scheme(), "http" | "ws");
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    });
    if cfg!(debug_assertions) && insecure && loopback {
        return Ok(());
    }
    Err(format!(
        "{label} must use {} (debug builds permit insecure loopback)",
        secure_schemes.join(" or ")
    ))
}

struct RateBucket {
    tokens: f64,
    updated: Instant,
}

/// Shared server state: configuration, the token HMAC key, the store,
/// the per-environment poll wakeups and the per-IP auth budget.
pub struct RcState {
    config: Config,
    hmac_key: [u8; 32],
    bootstrap_digest: Option<TokenDigest>,
    store: Store,
    /// One `Notify` per environment with a live poller, so enqueueing
    /// work wakes the poll immediately instead of waiting out its
    /// timeout. Entries whose last waiter has gone are swept at 1 Hz.
    waiters: Mutex<HashMap<String, Arc<Notify>>>,
    rate: Mutex<HashMap<IpAddr, RateBucket>>,
    /// Who is attached to which session stream right now. Purely
    /// in-memory: everything durable about a session is in the store.
    sessions: SessionHubs,
}

impl RcState {
    /// Open the persistent database at `database_path`.
    pub fn open(
        config: Config,
        database_path: impl AsRef<StdPath>,
        hmac_key: [u8; 32],
    ) -> rusqlite::Result<Self> {
        Self::from_store(config, hmac_key, Store::open(database_path)?)
    }

    /// Build state over a private in-memory database. Used by tests.
    pub fn in_memory(config: Config, hmac_key: [u8; 32]) -> rusqlite::Result<Self> {
        Self::from_store(config, hmac_key, Store::in_memory()?)
    }

    fn from_store(config: Config, hmac_key: [u8; 32], store: Store) -> rusqlite::Result<Self> {
        let bootstrap_digest = config.bootstrap_token.as_deref().map(|token| {
            ids::domain_digest(&hmac_key, auth::DOMAIN_BOOTSTRAP, &ids::token_bytes(token))
        });
        Ok(Self {
            config,
            hmac_key,
            bootstrap_digest,
            store,
            waiters: Mutex::new(HashMap::new()),
            rate: Mutex::new(HashMap::new()),
            sessions: SessionHubs::default(),
        })
    }

    /// Effective configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The persistence layer.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Digest `token` under the credential class identified by `domain`.
    pub(crate) fn digest(&self, domain: &[u8], token: &str) -> TokenDigest {
        let mut bytes = ids::token_bytes(token);
        let digest = ids::domain_digest(&self.hmac_key, domain, &bytes);
        bytes.fill(0);
        digest
    }

    /// Key the page cursors are tagged under (see [`cursor`]). The token
    /// key, used under its own domain separator, so rotating it
    /// invalidates outstanding cursors along with every credential.
    pub(crate) fn cursor_key(&self) -> &[u8; 32] {
        &self.hmac_key
    }

    pub(crate) fn bootstrap_digest(&self) -> Option<&TokenDigest> {
        self.bootstrap_digest.as_ref()
    }

    /// Wakeup handle for an environment's pollers, created on demand.
    pub(crate) fn waiter(&self, environment_id: &str) -> Arc<Notify> {
        let mut waiters = self.waiters.lock().expect("waiters lock");
        waiters
            .entry(environment_id.to_string())
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }

    /// Wake every poller waiting on an environment. All of them re-check
    /// the queue; the claim transaction decides which one wins.
    pub(crate) fn notify_environment(&self, environment_id: &str) {
        let waiter = self
            .waiters
            .lock()
            .expect("waiters lock")
            .get(environment_id)
            .cloned();
        if let Some(waiter) = waiter {
            waiter.notify_waiters();
        }
    }

    /// The live topology of `session_id`'s stream, created on the first
    /// attach.
    pub(crate) fn session_hub(&self, session_id: &str) -> Arc<SessionHub> {
        self.sessions.get_or_create(session_id)
    }

    /// The live topology of `session_id`'s stream, or `None` when nobody
    /// is attached. HTTP routes use this: they have something to
    /// announce, not a reason to create a hub.
    pub(crate) fn attached_session(&self, session_id: &str) -> Option<Arc<SessionHub>> {
        self.sessions.find(session_id)
    }

    /// Spend one unit of the per-IP budget that guards the routes which
    /// mint or exchange credentials. High-frequency routes (poll,
    /// heartbeat, ack) are deliberately not metered here: they are
    /// authenticated by an environment secret and are supposed to run hot.
    pub(crate) fn allow_auth_request(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let burst = f64::from(self.config.auth_rate_burst);
        let refill = f64::from(self.config.auth_rate_refill_per_minute) / 60.0;
        let mut rate = self.rate.lock().expect("rate lock");
        rate.retain(|_, bucket| now.duration_since(bucket.updated) < Duration::from_secs(300));
        if !rate.contains_key(&ip) && rate.len() >= 16_384 {
            return false;
        }
        let bucket = rate.entry(ip).or_insert(RateBucket {
            tokens: burst,
            updated: now,
        });
        bucket.tokens =
            burst.min(bucket.tokens + now.duration_since(bucket.updated).as_secs_f64() * refill);
        bucket.updated = now;
        if bucket.tokens < 1.0 {
            false
        } else {
            bucket.tokens -= 1.0;
            true
        }
    }

    /// Start the 1 Hz sweeper: lease expiry, access-token expiry, and
    /// the in-memory rate and waiter maps.
    pub fn start_cleanup(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                self.cleanup();
            }
        });
    }

    /// One sweep. Exposed so tests can drive it without waiting a second.
    pub fn cleanup(&self) {
        let now_unix = ids::now_unix();
        match self.store.expire_leases(now_unix) {
            Ok(environments) => {
                for environment_id in environments {
                    self.notify_environment(&environment_id);
                }
            }
            Err(error) => log_error!(error = %error, "failed to expire leases"),
        }
        if let Err(error) = self.store.expire_access_tokens(now_unix) {
            log_error!(error = %error, "failed to expire access tokens");
        }
        self.hang_up_on_workers_without_a_lease();
        self.sessions.prune();
        let now = Instant::now();
        self.rate
            .lock()
            .expect("rate lock")
            .retain(|_, bucket| now.duration_since(bucket.updated) < Duration::from_secs(300));
        // Drop wakeup handles nobody is holding: strong_count == 1 means
        // the map itself owns the only reference, so no poller can miss
        // a notification by having it swept out from under them.
        self.waiters
            .lock()
            .expect("waiters lock")
            .retain(|_, waiter| Arc::strong_count(waiter) > 1);
    }

    /// Close the socket of any worker whose work item stopped being
    /// leased to it.
    ///
    /// The lease is the thing that says which worker owns a session, and
    /// it can lapse in three ways this connection would not otherwise
    /// notice: the sweeper above expired it, a controller stopped the
    /// item, or the session was archived. Leaving the socket open would
    /// let a worker that no longer holds the session keep writing to its
    /// history — and once the item is re-leased, two workers would be
    /// writing at once. `auth::session` already refuses such a token at
    /// connect time; this is the same rule applied to a connection that
    /// was already up.
    fn hang_up_on_workers_without_a_lease(&self) {
        for (session_id, work_id) in self.sessions.attached_workers() {
            let held = match self.store.work_state(&work_id) {
                Ok(Some(state)) => state == work_state::LEASED || state == work_state::ACKED,
                Ok(None) => false,
                Err(error) => {
                    log_error!(error = %error, work = %work_id, "failed to read work state");
                    continue;
                }
            };
            if !held {
                if let Some(hub) = self.sessions.find(&session_id) {
                    hub.close_worker(close_code::LEASE_GONE);
                }
            }
        }
    }
}

/// Build the router. Every route is authenticated except `/healthz`.
pub fn app(state: Arc<RcState>) -> Router {
    let max_body_bytes = state.config.max_body_bytes;
    Router::new()
        .route("/healthz", get(routes::health::health))
        .route(
            "/v1/devices",
            post(routes::devices::issue_device).get(routes::devices::list_devices),
        )
        .route("/v1/devices/token", post(routes::devices::refresh_token))
        .route(
            "/v1/devices/{device_id}",
            delete(routes::devices::revoke_device),
        )
        .route(
            "/v1/environments",
            post(routes::environments::register).get(routes::environments::list),
        )
        .route(
            "/v1/environments/{environment_id}",
            delete(routes::environments::deregister),
        )
        .route(
            "/v1/environments/{environment_id}/projects",
            put(routes::environments::update_projects),
        )
        .route(
            "/v1/environments/{environment_id}/work",
            get(routes::work::poll).post(routes::work::enqueue),
        )
        .route(
            "/v1/environments/{environment_id}/work/{work_id}/ack",
            post(routes::work::acknowledge),
        )
        .route(
            "/v1/environments/{environment_id}/work/{work_id}/heartbeat",
            post(routes::work::heartbeat),
        )
        .route(
            "/v1/environments/{environment_id}/work/{work_id}/stop",
            post(routes::work::stop),
        )
        .route(
            "/v1/environments/{environment_id}/sessions/{session_id}/reconnect",
            post(routes::sessions::reconnect),
        )
        .route("/v1/sessions", get(routes::history::sessions))
        .route(
            "/v1/sessions/{session_id}/events",
            post(routes::sessions::events).get(routes::history::events),
        )
        .route(
            "/v1/sessions/{session_id}/archive",
            post(routes::sessions::archive),
        )
        .route(
            "/v1/sessions/{session_id}/stream",
            get(routes::stream::stream),
        )
        .layer(RequestBodyLimitLayer::new(max_body_bytes))
        .layer(TraceLayer::new_for_http().make_span_with(
            |request: &axum::http::Request<axum::body::Body>| {
                tracing::info_span!(
                    "http_request",
                    method = %request.method(),
                    path = request.uri().path()
                )
            },
        ))
        .layer(middleware::map_response(normalize_response))
        .with_state(state)
}

/// Collapse every failure into the same opaque body, so a caller cannot
/// distinguish "no such environment" from "not your environment" by
/// reading the payload. The status code is the only signal, and it is
/// documented in the README.
async fn normalize_response(response: Response) -> Response {
    let status = response.status();
    if status.is_client_error() || status.is_server_error() {
        error(status)
    } else {
        no_store(response)
    }
}

/// Stamp `Cache-Control: no-store`. Every RC response carries either a
/// credential, a work item or account metadata; none of it is cacheable.
pub(crate) fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The uniform error response.
pub(crate) fn error(status: StatusCode) -> Response {
    #[derive(Serialize)]
    struct ErrorBody {
        error: &'static str,
    }
    no_store(
        (
            status,
            Json(ErrorBody {
                error: "request rejected",
            }),
        )
            .into_response(),
    )
}

/// Run a blocking database closure off the async worker, mapping any
/// failure to an opaque 500 so a SQL error never reaches the client.
pub(crate) async fn db<T, F>(state: &Arc<RcState>, operation: F) -> Result<T, Response>
where
    F: FnOnce(&RcState) -> rusqlite::Result<T> + Send + 'static,
    T: Send + 'static,
{
    let state = state.clone();
    match tokio::task::spawn_blocking(move || operation(&state)).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(failure)) => {
            log_error!(error = %failure, "database error");
            Err(error(StatusCode::INTERNAL_SERVER_ERROR))
        }
        Err(failure) => {
            log_error!(error = %failure, "database task failed");
            Err(error(StatusCode::INTERNAL_SERVER_ERROR))
        }
    }
}

/// Resolve the client IP used for per-source limits. Behind a trusted
/// proxy the TCP peer is the proxy, so the real client is the rightmost
/// `X-Forwarded-For` entry — the one the immediate proxy appends.
pub(crate) fn client_ip(config: &Config, headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    if config.trust_forwarded_for {
        if let Some(forwarded) = headers
            .get(header::HeaderName::from_static("x-forwarded-for"))
            .and_then(|value| value.to_str().ok())
        {
            if let Some(entry) = forwarded.rsplit(',').next() {
                if let Ok(ip) = entry.trim().parse::<IpAddr>() {
                    return ip;
                }
            }
        }
    }
    peer.ip()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            bind: "127.0.0.1:0".parse().expect("address"),
            public_url: "https://rc.example.com".into(),
            session_ingress_url: "wss://rc.example.com".into(),
            lease_ttl: Duration::from_secs(90),
            access_token_ttl: Duration::from_secs(3_600),
            default_poll_wait: Duration::from_secs(25),
            max_poll_wait: Duration::from_secs(60),
            max_body_bytes: 262_144,
            session_replay_events: 200,
            page_limit_default: 100,
            page_limit_max: 500,
            auth_rate_burst: 30,
            auth_rate_refill_per_minute: 30,
            bootstrap_token: None,
            trust_forwarded_for: false,
        }
    }

    #[test]
    fn a_sound_configuration_validates() {
        config().validate().expect("valid");
    }

    #[test]
    fn urls_must_be_origins_with_the_right_scheme() {
        let mut with_path = config();
        with_path.public_url = "https://rc.example.com/api".into();
        assert!(with_path.validate().is_err());

        let mut http_ingress = config();
        http_ingress.session_ingress_url = "https://rc.example.com".into();
        assert!(http_ingress.validate().is_err());

        // Debug builds accept an insecure loopback origin so local runs
        // do not need certificates.
        let mut loopback = config();
        loopback.public_url = "http://127.0.0.1:8090".into();
        loopback.session_ingress_url = "ws://127.0.0.1:8090".into();
        assert_eq!(loopback.validate().is_ok(), cfg!(debug_assertions));
    }

    #[test]
    fn poll_waits_are_bounded() {
        let mut too_long = config();
        too_long.max_poll_wait = Duration::from_secs(120);
        assert!(too_long.validate().is_err());

        let mut inverted = config();
        inverted.default_poll_wait = Duration::from_secs(59);
        inverted.max_poll_wait = Duration::from_secs(30);
        assert!(inverted.validate().is_err());
    }

    #[test]
    fn page_limits_are_bounded() {
        let mut zero = config();
        zero.page_limit_default = 0;
        assert!(zero.validate().is_err());

        let mut inverted = config();
        inverted.page_limit_default = 200;
        inverted.page_limit_max = 100;
        assert!(inverted.validate().is_err());

        let mut too_big = config();
        too_big.page_limit_max = MAX_PAGE_LIMIT + 1;
        assert!(too_big.validate().is_err());
    }

    #[test]
    fn a_malformed_bootstrap_token_is_rejected_at_startup() {
        let mut bad = config();
        bad.bootstrap_token = Some("not-a-token".into());
        assert!(bad.validate().is_err());

        let mut good = config();
        good.bootstrap_token = Some(ids::generate_token());
        good.validate().expect("valid bootstrap token");
    }

    #[test]
    fn the_auth_budget_refills_and_runs_out() {
        let mut limited = config();
        limited.auth_rate_burst = 2;
        limited.auth_rate_refill_per_minute = 1;
        let state = RcState::in_memory(limited, [0u8; 32]).expect("state");
        let ip: IpAddr = "203.0.113.9".parse().expect("ip");
        assert!(state.allow_auth_request(ip));
        assert!(state.allow_auth_request(ip));
        assert!(!state.allow_auth_request(ip));
    }

    #[test]
    fn the_sweeper_drops_unheld_wakeup_handles() {
        let state = RcState::in_memory(config(), [0u8; 32]).expect("state");
        let held = state.waiter("env_held");
        drop(state.waiter("env_dropped"));
        state.cleanup();
        let waiters = state.waiters.lock().expect("waiters lock");
        assert!(waiters.contains_key("env_held"));
        assert!(!waiters.contains_key("env_dropped"));
        drop(held);
    }
}
