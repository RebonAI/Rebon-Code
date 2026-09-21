//! A real [`BridgeApiClient`] over HTTP.
//!
//! Behind the `http` cargo feature, which is off by default: a consumer that
//! only needs the trait and the pure protocol types has no business linking
//! `reqwest`.
//!
//! ## What the trait does not carry
//!
//! The nine trait methods pass ids and, in three places, a session token.
//! They never pass the *device* credential or the *environment* secret,
//! so this client holds both:
//!
//! | Method | Route | Credential |
//! |---|---|---|
//! | `register_bridge_environment` | `POST /v1/environments` | device access token |
//! | `reconnect_session` | `POST /v1/environments/{env}/sessions/{s}/reconnect` | device access token |
//! | `poll_for_work` / `poll_for_work_item` | `GET /v1/environments/{env}/work` | environment secret (trait argument) |
//! | `acknowledge_work` | `POST …/work/{id}/ack` | environment secret (**stored**) + session token in the body |
//! | `heartbeat_work` | `POST …/work/{id}/heartbeat` | environment secret (**stored**) + session token in the body |
//! | `stop_work` | `POST …/work/{id}/stop` | environment secret (**stored**), body `{"force":…}` |
//! | `deregister_environment` | `DELETE /v1/environments/{env}` | environment secret (**stored**) |
//! | `archive_session` | `POST /v1/sessions/{s}/archive` | environment secret (**stored**) |
//! | `send_permission_response_event` | `POST /v1/sessions/{s}/events` | session token (trait argument) |
//!
//! Routes the trait has no method for are **inherent** methods. The
//! controller-side ones a bridge never calls; a control surface does:
//!
//! | Method | Route | Credential |
//! |---|---|---|
//! | [`HttpBridgeApiClient::session_events`] | `GET /v1/sessions/{s}/events` | device access token |
//! | [`HttpBridgeApiClient::list_sessions`] | `GET /v1/sessions` | device access token |
//! | [`HttpBridgeApiClient::list_environments`] | `GET /v1/environments` | device access token |
//! | [`HttpBridgeApiClient::enqueue_work`] | `POST /v1/environments/{env}/work` | device access token |
//!
//! and one belongs to the environment:
//!
//! | Method | Route | Credential |
//! |---|---|---|
//! | [`HttpBridgeApiClient::update_projects`] | `PUT /v1/environments/{env}/projects` | environment secret (**stored**) |
//!
//! Binding a device comes before there is a client to hold its
//! credentials, so it is an associated function:
//!
//! | Function | Route | Credential |
//! |---|---|---|
//! | [`HttpBridgeApiClient::issue_device`] | `POST /v1/devices` | bootstrap token or device access token (argument) |
//!
//! `archive_session` and `deregister_environment` take no credential
//! argument at all, which makes reaching for the device token there the
//! easy mistake; both go through [`HttpBridgeApiClient::environment_secret`],
//! which a successful `register_bridge_environment` fills in.
//!
//! ## Statuses
//!
//! A long poll that times out answers **204**, which is not an error:
//! it becomes `Ok(None)`. Otherwise:
//!
//! | Status | [`BridgeApiError`] |
//! |---|---|
//! | 401 | `Unauthorized` |
//! | 400 / 403 / 404 / 409 / 413 (any other 4xx) | `Permanent` |
//! | 429, 5xx, transport errors, timeouts | `Transient` |
//! | a success body that will not parse | `Protocol` |
//!
//! A 401 on a device-scoped call is retried **once**, after exchanging
//! the refresh token at `POST /v1/devices/token`. A second 401 is
//! `Unauthorized`. Nothing else is ever retried here — backoff on a
//! `Transient` failure is the caller's loop to run.

use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, Method, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

use crate::api_client::{BridgeApiClient, BridgeApiError, BridgeApiResult, PollOptions};
use crate::config::{
    BridgeConfig, EnqueueWorkRequest, EnqueuedWork, HeartbeatOutcome, PermissionResponseEvent,
    RegisteredEnvironment, WorkItem, WorkResponse,
};
use crate::devices::{IssueDeviceRequest, IssuedDevice};
use crate::history::{PageRequest, SessionEventPage, SessionPage};
use crate::projects::{EnvironmentList, ProjectInfo, ProjectList};

/// Hard ceiling RC puts on a long poll (`REBON_RC_MAX_POLL_WAIT_SECONDS`
/// may not exceed this). Asking for more is not an error server-side —
/// it is silently clamped — but a client that believed its own number
/// would set a useless HTTP timeout, so the configuration rejects it.
pub const MAX_POLL_WAIT: Duration = Duration::from_secs(60);

/// Device credentials the trait signatures do not carry.
///
/// The access token authenticates the two controller-shaped routes; the
/// refresh token exists only to mint a new access token when the old one
/// expires. Without a refresh token an expired access token is simply
/// fatal.
#[derive(Clone)]
pub struct DeviceCredentials {
    /// Short-lived device access token (`Authorization: Bearer …`).
    pub access_token: String,
    /// Long-lived refresh token, exchanged at `POST /v1/devices/token`.
    pub refresh_token: Option<String>,
}

/// Redacted: these are bearer credentials, and a `{:?}` in a log line is
/// the cheapest way to leak one.
impl std::fmt::Debug for DeviceCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceCredentials")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl DeviceCredentials {
    /// Credentials that can refresh themselves.
    pub fn new(access_token: impl Into<String>, refresh_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            refresh_token: Some(refresh_token.into()),
        }
    }

    /// An access token with no way to renew itself.
    pub fn access_only(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            refresh_token: None,
        }
    }
}

/// Transport knobs the trait has no room for.
///
/// `poll_wait` is the long-poll duration sent as `timeoutMs`;
/// [`PollOptions`] carries only `reclaim_older_than_ms`, so the wait has
/// to come from here. The HTTP timeout for a poll is
/// `poll_wait + poll_timeout_margin` — it **must** exceed the wait, or
/// the client aborts its own long poll every single time and never sees
/// a 204.
#[derive(Debug, Clone)]
pub struct HttpClientConfig {
    /// Origin the RC API is served from, e.g. `https://rc.example.com`.
    /// A trailing slash is tolerated and trimmed.
    pub api_base_url: String,
    /// Long-poll duration, sent as `timeoutMs`.
    pub poll_wait: Duration,
    /// How much longer than `poll_wait` a poll's HTTP timeout runs.
    pub poll_timeout_margin: Duration,
    /// Timeout applied to every request that is not a long poll.
    pub request_timeout: Duration,
}

impl HttpClientConfig {
    /// Configuration with RC's own defaults: a 25 s long poll, a 10 s
    /// margin on top of it, and a 30 s timeout everywhere else.
    pub fn new(api_base_url: impl Into<String>) -> Self {
        Self {
            api_base_url: api_base_url.into(),
            poll_wait: Duration::from_secs(25),
            poll_timeout_margin: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
        }
    }

    /// Reject a configuration that cannot work before any request is sent.
    pub fn validate(&self) -> Result<(), String> {
        let base = self.api_base_url.trim_end_matches('/');
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(format!(
                "api base URL must be an http(s) origin, got {:?}",
                self.api_base_url
            ));
        }
        if self.poll_wait.is_zero() {
            return Err("poll wait must be nonzero".into());
        }
        if self.poll_wait > MAX_POLL_WAIT {
            return Err("poll wait must not exceed 60 seconds".into());
        }
        if self.poll_timeout_margin.is_zero() {
            return Err(
                "poll timeout margin must be nonzero: the HTTP timeout has to outlast the wait"
                    .into(),
            );
        }
        if self.request_timeout.is_zero() {
            return Err("request timeout must be nonzero".into());
        }
        Ok(())
    }

    /// The HTTP timeout a long poll runs under. Always longer than
    /// [`Self::poll_wait`], which is the whole point of the margin.
    pub fn poll_timeout(&self) -> Duration {
        self.poll_wait + self.poll_timeout_margin
    }
}

/// Mutable credential state. Small enough to clone out of the lock so a
/// guard is never held across an `await`.
struct Credentials {
    access_token: String,
    refresh_token: Option<String>,
    environment_secret: Option<String>,
}

/// Redacted, for the same reason as [`DeviceCredentials`]: the client
/// itself is `Debug`, and printing it must not print its bearer tokens.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "environment_secret",
                &self.environment_secret.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// A [`BridgeApiClient`] over the bridge HTTP routes.
///
/// Object-safe, `Send + Sync`, and cheap to wrap in an `Arc`, so it
/// drops straight into `crate::runtime::start_bridge_runtime`.
#[derive(Debug)]
pub struct HttpBridgeApiClient {
    http: Client,
    config: HttpClientConfig,
    /// `api_base_url` with any trailing slash removed.
    base: String,
    credentials: RwLock<Credentials>,
    /// Serialises token refresh so a burst of 401s produces one exchange.
    refreshing: tokio::sync::Mutex<()>,
}

impl HttpBridgeApiClient {
    /// Build a client with its own `reqwest::Client`.
    pub fn new(config: HttpClientConfig, credentials: DeviceCredentials) -> BridgeApiResult<Self> {
        let http = Client::builder()
            .build()
            .map_err(|error| BridgeApiError::Other(format!("cannot build HTTP client: {error}")))?;
        Self::with_http_client(config, credentials, http)
    }

    /// Build a client over a caller-supplied `reqwest::Client` — for
    /// sharing a connection pool, or for pointing tests at a stub.
    ///
    /// Timeouts are applied per request rather than on the client, so a
    /// client configured with its own global timeout shorter than
    /// [`HttpClientConfig::poll_timeout`] would still strangle the poll.
    pub fn with_http_client(
        config: HttpClientConfig,
        credentials: DeviceCredentials,
        http: Client,
    ) -> BridgeApiResult<Self> {
        config.validate().map_err(|message| {
            BridgeApiError::Other(format!("invalid bridge client config: {message}"))
        })?;
        let base = config.api_base_url.trim_end_matches('/').to_string();
        Ok(Self {
            http,
            config,
            base,
            credentials: RwLock::new(Credentials {
                access_token: credentials.access_token,
                refresh_token: credentials.refresh_token,
                environment_secret: None,
            }),
            refreshing: tokio::sync::Mutex::new(()),
        })
    }

    /// `POST /v1/devices` — mint a device credential, authenticated by
    /// `bearer`: the one-time bootstrap token while the RC instance has no
    /// account, or an existing device's access token.
    ///
    /// Associated rather than a method: before this succeeds there are no
    /// device credentials to build a client around. Nothing is retried; a
    /// spent bootstrap token is `Unauthorized`, a label RC refuses is
    /// `Permanent`, and the per-IP budget running out is `Transient`.
    pub async fn issue_device(
        config: &HttpClientConfig,
        bearer: &str,
        request: &IssueDeviceRequest,
    ) -> BridgeApiResult<IssuedDevice> {
        let client = Self::new(config.clone(), DeviceCredentials::access_only(bearer))?;
        let body = serde_json::to_value(request).map_err(|error| {
            BridgeApiError::Protocol(format!("device request is not serializable: {error}"))
        })?;
        let call = Call::post(client.devices_url(), config.request_timeout).body(body);
        let response = client.send(&call, bearer).await?;
        let response = ensure_success(response, "issue device")?;
        json_body(response, "issued device").await
    }

    /// Exchange the refresh token for an access token now, rather than on
    /// the first 401.
    ///
    /// What a caller that was handed only a refresh token uses to prove it
    /// works before relying on it: an unknown or revoked device is
    /// `Unauthorized` here instead of on some later call.
    pub async fn refresh_now(&self) -> BridgeApiResult<()> {
        let stale = self.access_token();
        self.refresh_access_token(&stale).await
    }

    /// The transport configuration in force.
    pub fn config(&self) -> &HttpClientConfig {
        &self.config
    }

    /// The device access token currently in use.
    pub fn access_token(&self) -> String {
        self.read().access_token.clone()
    }

    /// The environment secret, once `register_bridge_environment` has
    /// returned one. The credential-free trait methods
    /// (`archive_session`, `deregister_environment`) use it.
    pub fn environment_secret(&self) -> Option<String> {
        self.read().environment_secret.clone()
    }

    /// Install an environment secret without registering — for resuming
    /// against an environment whose secret was persisted elsewhere.
    pub fn set_environment_secret(&self, secret: impl Into<String>) {
        self.write().environment_secret = Some(secret.into());
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Credentials> {
        self.credentials
            .read()
            .expect("bridge credentials poisoned")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Credentials> {
        self.credentials
            .write()
            .expect("bridge credentials poisoned")
    }

    fn require_environment_secret(&self) -> BridgeApiResult<String> {
        self.environment_secret().ok_or_else(|| {
            BridgeApiError::Permanent(
                "no environment secret held: register_bridge_environment must succeed first".into(),
            )
        })
    }

    // ─── URLs ──────────────────────────────────────────────────────────

    /// `POST /v1/environments`, and the base of every environment route.
    fn environments_url(&self) -> String {
        format!("{}/v1/environments", self.base)
    }

    fn environment_url(&self, environment_id: &str) -> String {
        format!("{}/{}", self.environments_url(), segment(environment_id))
    }

    fn work_url(&self, environment_id: &str) -> String {
        format!("{}/work", self.environment_url(environment_id))
    }

    fn work_action_url(&self, environment_id: &str, work_id: &str, action: &str) -> String {
        format!(
            "{}/{}/{action}",
            self.work_url(environment_id),
            segment(work_id)
        )
    }

    fn projects_url(&self, environment_id: &str) -> String {
        format!("{}/projects", self.environment_url(environment_id))
    }

    fn reconnect_url(&self, environment_id: &str, session_id: &str) -> String {
        format!(
            "{}/sessions/{}/reconnect",
            self.environment_url(environment_id),
            segment(session_id)
        )
    }

    fn session_action_url(&self, session_id: &str, action: &str) -> String {
        format!("{}/v1/sessions/{}/{action}", self.base, segment(session_id))
    }

    fn sessions_url(&self) -> String {
        format!("{}/v1/sessions", self.base)
    }

    fn devices_url(&self) -> String {
        format!("{}/v1/devices", self.base)
    }

    fn device_token_url(&self) -> String {
        format!("{}/v1/devices/token", self.base)
    }

    // ─── Controller reads ──────────────────────────────────────────────

    /// `GET /v1/sessions/{s}/events` — one page of a session's persisted
    /// history, as the device.
    ///
    /// Pages walk backwards from the newest event; each page's events are
    /// oldest first. Pass the returned `next_cursor` back through
    /// [`PageRequest::after`] until it comes back `None`. A session on
    /// another account is `Permanent` (404), exactly like one that does
    /// not exist, and so is a cursor the server refuses (400).
    pub async fn session_events(
        &self,
        session_id: &str,
        page: &PageRequest,
    ) -> BridgeApiResult<SessionEventPage> {
        let call = page_query(
            Call::get(
                self.session_action_url(session_id, "events"),
                self.config.request_timeout,
            ),
            page,
        );
        let response = self.send_as_device(&call).await?;
        let response = ensure_success(response, "read session events")?;
        json_body(response, "session event page").await
    }

    /// `GET /v1/sessions` — one page of the account's sessions, most
    /// recent activity first, optionally only those on `environment_id`.
    ///
    /// A cursor is tied to the filter it was issued under: changing
    /// `environment_id` mid-walk is refused (400, `Permanent`).
    pub async fn list_sessions(
        &self,
        environment_id: Option<&str>,
        page: &PageRequest,
    ) -> BridgeApiResult<SessionPage> {
        let mut call = Call::get(self.sessions_url(), self.config.request_timeout);
        if let Some(environment_id) = environment_id {
            call = call.query("environment", environment_id);
        }
        let call = page_query(call, page);
        let response = self.send_as_device(&call).await?;
        let response = ensure_success(response, "list sessions")?;
        json_body(response, "session page").await
    }

    /// `GET /v1/environments` — the account's environments, each with
    /// the projects it currently advertises.
    pub async fn list_environments(&self) -> BridgeApiResult<EnvironmentList> {
        let call = Call::get(self.environments_url(), self.config.request_timeout);
        let response = self.send_as_device(&call).await?;
        let response = ensure_success(response, "list environments")?;
        json_body(response, "environment list").await
    }

    /// `POST /v1/environments/{env}/work` — queue work as a controller.
    ///
    /// A project the environment does not advertise, an ambiguous or
    /// missing one, or a resume target that is not a plain name is
    /// `Permanent` (400); a session that is not on this environment is
    /// `Permanent` (404).
    pub async fn enqueue_work(
        &self,
        environment_id: &str,
        request: &EnqueueWorkRequest,
    ) -> BridgeApiResult<EnqueuedWork> {
        let body = serde_json::to_value(request).map_err(|error| {
            BridgeApiError::Protocol(format!("enqueue request is not serializable: {error}"))
        })?;
        let call =
            Call::post(self.work_url(environment_id), self.config.request_timeout).body(body);
        let response = self.send_as_device(&call).await?;
        let response = ensure_success(response, "enqueue work")?;
        json_body(response, "enqueued work").await
    }

    // ─── Environment calls ─────────────────────────────────────────────

    /// `PUT /v1/environments/{env}/projects` — replace the projects this
    /// environment advertises, as the environment (the **stored**
    /// secret). Returns the list RC stored.
    ///
    /// The list is replaced wholesale. A list RC will not accept — a
    /// duplicate path, an empty path, too many entries — is `Permanent`
    /// (400).
    pub async fn update_projects(
        &self,
        environment_id: &str,
        projects: &[ProjectInfo],
    ) -> BridgeApiResult<ProjectList> {
        let secret = self.require_environment_secret()?;
        let body = serde_json::to_value(ProjectList {
            projects: projects.to_vec(),
        })
        .map_err(|error| {
            BridgeApiError::Protocol(format!("project list is not serializable: {error}"))
        })?;
        let call = Call::new(
            Method::PUT,
            self.projects_url(environment_id),
            self.config.request_timeout,
        )
        .body(body);
        let response = self.send(&call, &secret).await?;
        let response = ensure_success(response, "update projects")?;
        json_body(response, "project list").await
    }

    // ─── Transport ─────────────────────────────────────────────────────

    /// Send `call` with an explicit bearer credential — an environment
    /// secret or a session token, neither of which this client can renew.
    async fn send(&self, call: &Call, bearer: &str) -> BridgeApiResult<Response> {
        let mut request = self
            .http
            .request(call.method.clone(), &call.url)
            .bearer_auth(bearer)
            .timeout(call.timeout);
        if !call.query.is_empty() {
            request = request.query(&call.query);
        }
        if let Some(body) = &call.body {
            request = request.json(body);
        }
        request.send().await.map_err(transport_error)
    }

    /// Send `call` as the device, refreshing the access token and
    /// retrying **once** on a 401.
    async fn send_as_device(&self, call: &Call) -> BridgeApiResult<Response> {
        let token = self.access_token();
        let response = self.send(call, &token).await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        self.refresh_access_token(&token).await?;
        let refreshed = self.access_token();
        // A second 401 falls through to the ordinary status mapping,
        // which makes it `Unauthorized`. We do not refresh again.
        self.send(call, &refreshed).await
    }

    /// Exchange the refresh token for a new access token.
    ///
    /// `stale` is the access token whose 401 triggered this. If another
    /// task already replaced it while we waited for the lock, the
    /// exchange is skipped — a burst of concurrent 401s costs one
    /// round trip, not one per call.
    async fn refresh_access_token(&self, stale: &str) -> BridgeApiResult<()> {
        let Some(refresh_token) = self.read().refresh_token.clone() else {
            return Err(BridgeApiError::Unauthorized(
                "device access token rejected and no refresh token is held".into(),
            ));
        };
        let _guard = self.refreshing.lock().await;
        if self.access_token() != stale {
            return Ok(());
        }
        let call = Call::post(self.device_token_url(), self.config.request_timeout);
        let response = self.send(&call, &refresh_token).await?;
        let response = ensure_success(response, "refresh device access token")?;

        #[derive(Deserialize)]
        struct AccessToken {
            access_token: String,
        }
        let issued: AccessToken = json_body(response, "device access token").await?;
        self.write().access_token = issued.access_token;
        Ok(())
    }
}

/// One outbound request. A struct rather than six positional arguments,
/// because the refresh-and-retry path has to send the same call twice.
#[derive(Debug, Clone)]
struct Call {
    method: Method,
    url: String,
    query: Vec<(&'static str, String)>,
    body: Option<serde_json::Value>,
    timeout: Duration,
}

impl Call {
    fn new(method: Method, url: String, timeout: Duration) -> Self {
        Self {
            method,
            url,
            query: Vec::new(),
            body: None,
            timeout,
        }
    }

    fn get(url: String, timeout: Duration) -> Self {
        Self::new(Method::GET, url, timeout)
    }

    fn post(url: String, timeout: Duration) -> Self {
        Self::new(Method::POST, url, timeout)
    }

    fn delete(url: String, timeout: Duration) -> Self {
        Self::new(Method::DELETE, url, timeout)
    }

    fn query(mut self, name: &'static str, value: impl ToString) -> Self {
        self.query.push((name, value.to_string()));
        self
    }

    fn body(mut self, body: serde_json::Value) -> Self {
        self.body = Some(body);
        self
    }
}

/// Add a page request's `cursor` and `limit` to the query string.
fn page_query(mut call: Call, page: &PageRequest) -> Call {
    if let Some(cursor) = &page.cursor {
        call = call.query("cursor", cursor);
    }
    if let Some(limit) = page.limit {
        call = call.query("limit", limit);
    }
    call
}

/// Percent-encode one path segment.
///
/// RC ids are `env_…` / `wrk_…` / `sess_…` and need no encoding, but they
/// arrive here as plain `&str` from callers this crate does not control,
/// and a `/` or `?` slipping into a segment would silently address a
/// different route.
fn segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

/// Map a `reqwest` failure. Connection refused, DNS, a dropped socket
/// and a timeout are all worth retrying with backoff; a body that will
/// not decode is not.
fn transport_error(error: reqwest::Error) -> BridgeApiError {
    if error.is_decode() {
        return BridgeApiError::Protocol(format!("malformed response body: {error}"));
    }
    if error.is_timeout() {
        return BridgeApiError::Transient(format!("request timed out: {error}"));
    }
    BridgeApiError::Transient(format!("transport failure: {error}"))
}

/// The status → error table. Every non-2xx response goes through here.
fn status_error(status: StatusCode, context: &str) -> BridgeApiError {
    let code = status.as_u16();
    let detail = format!("{context}: HTTP {code}");
    match code {
        401 => BridgeApiError::Unauthorized(detail),
        429 => BridgeApiError::Transient(detail),
        500..=599 => BridgeApiError::Transient(detail),
        // 400, 403, 404, 409, 413 and every other 4xx: retrying the same
        // request would produce the same answer.
        400..=499 => BridgeApiError::Permanent(detail),
        // A 1xx or 3xx here means the peer is not the RC API we expect.
        _ => BridgeApiError::Protocol(detail),
    }
}

fn ensure_success(response: Response, context: &str) -> BridgeApiResult<Response> {
    let status = response.status();
    if status.is_success() {
        Ok(response)
    } else {
        Err(status_error(status, context))
    }
}

/// Parse a success body. A shape we cannot read is a protocol failure,
/// never a transient one: retrying will produce the same bytes.
async fn json_body<T: DeserializeOwned>(response: Response, context: &str) -> BridgeApiResult<T> {
    response
        .json::<T>()
        .await
        .map_err(|error| BridgeApiError::Protocol(format!("{context}: {error}")))
}

#[async_trait]
impl BridgeApiClient for HttpBridgeApiClient {
    /// `POST /v1/environments` — device access token.
    ///
    /// Stores the returned secret, which is what makes the
    /// credential-free `archive_session` and `deregister_environment`
    /// work later.
    async fn register_bridge_environment(
        &self,
        config: &BridgeConfig,
    ) -> BridgeApiResult<RegisteredEnvironment> {
        let body = serde_json::to_value(config).map_err(|error| {
            BridgeApiError::Protocol(format!("bridge config is not serializable: {error}"))
        })?;
        let call = Call::post(self.environments_url(), self.config.request_timeout).body(body);
        let response = self.send_as_device(&call).await?;
        let response = ensure_success(response, "register bridge environment")?;
        let registered: RegisteredEnvironment =
            json_body(response, "registered environment").await?;
        self.write().environment_secret = Some(registered.environment_secret.clone());
        Ok(registered)
    }

    /// `GET /v1/environments/{env}/work` — environment secret. The
    /// envelope of [`Self::poll_for_work_item`].
    async fn poll_for_work(
        &self,
        environment_id: &str,
        environment_secret: &str,
        options: PollOptions,
    ) -> BridgeApiResult<Option<WorkResponse>> {
        Ok(self
            .poll_for_work_item(environment_id, environment_secret, options)
            .await?
            .map(|item| item.response))
    }

    /// `GET /v1/environments/{env}/work` — environment secret.
    ///
    /// **204 is not an error.** The server answers 204 when the long poll
    /// runs out with an empty queue, and that is `Ok(None)`.
    async fn poll_for_work_item(
        &self,
        environment_id: &str,
        environment_secret: &str,
        options: PollOptions,
    ) -> BridgeApiResult<Option<WorkItem>> {
        let mut call = Call::get(self.work_url(environment_id), self.config.poll_timeout())
            .query("timeoutMs", self.config.poll_wait.as_millis());
        if let Some(reclaim) = options.reclaim_older_than_ms {
            call = call.query("reclaimOlderThanMs", reclaim);
        }
        let response = self.send(&call, environment_secret).await?;
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let response = ensure_success(response, "poll for work")?;
        Ok(Some(json_body(response, "work response").await?))
    }

    /// `POST …/work/{id}/ack` — environment secret, session token in the body.
    async fn acknowledge_work(
        &self,
        environment_id: &str,
        work_id: &str,
        session_token: &str,
    ) -> BridgeApiResult<()> {
        let secret = self.require_environment_secret()?;
        let call = Call::post(
            self.work_action_url(environment_id, work_id, "ack"),
            self.config.request_timeout,
        )
        .body(json!({ "session_token": session_token }));
        let response = self.send(&call, &secret).await?;
        ensure_success(response, "acknowledge work").map(|_| ())
    }

    /// `POST …/work/{id}/stop` — environment secret, body `{"force":…}`.
    async fn stop_work(
        &self,
        environment_id: &str,
        work_id: &str,
        force: bool,
    ) -> BridgeApiResult<()> {
        let secret = self.require_environment_secret()?;
        let call = Call::post(
            self.work_action_url(environment_id, work_id, "stop"),
            self.config.request_timeout,
        )
        .body(json!({ "force": force }));
        let response = self.send(&call, &secret).await?;
        ensure_success(response, "stop work").map(|_| ())
    }

    /// `DELETE /v1/environments/{env}` — the **stored** environment secret.
    async fn deregister_environment(&self, environment_id: &str) -> BridgeApiResult<()> {
        let secret = self.require_environment_secret()?;
        let call = Call::delete(
            self.environment_url(environment_id),
            self.config.request_timeout,
        );
        let response = self.send(&call, &secret).await?;
        ensure_success(response, "deregister environment").map(|_| ())
    }

    /// `POST /v1/sessions/{s}/events` — the session token the caller passes.
    async fn send_permission_response_event(
        &self,
        session_id: &str,
        event: &PermissionResponseEvent,
        session_token: &str,
    ) -> BridgeApiResult<()> {
        let body = serde_json::to_value(event).map_err(|error| {
            BridgeApiError::Protocol(format!("permission event is not serializable: {error}"))
        })?;
        let call = Call::post(
            self.session_action_url(session_id, "events"),
            self.config.request_timeout,
        )
        .body(body);
        let response = self.send(&call, session_token).await?;
        ensure_success(response, "send permission response event").map(|_| ())
    }

    /// `POST /v1/sessions/{s}/archive` — the **stored** environment secret.
    ///
    /// The trait passes no credential here, which is precisely why this
    /// must not quietly fall back to the device token.
    async fn archive_session(&self, session_id: &str) -> BridgeApiResult<()> {
        let secret = self.require_environment_secret()?;
        let call = Call::post(
            self.session_action_url(session_id, "archive"),
            self.config.request_timeout,
        );
        let response = self.send(&call, &secret).await?;
        ensure_success(response, "archive session").map(|_| ())
    }

    /// `POST /v1/environments/{env}/sessions/{s}/reconnect` — controller
    /// route, so the device access token.
    async fn reconnect_session(
        &self,
        environment_id: &str,
        session_id: &str,
    ) -> BridgeApiResult<()> {
        let call = Call::post(
            self.reconnect_url(environment_id, session_id),
            self.config.request_timeout,
        );
        let response = self.send_as_device(&call).await?;
        ensure_success(response, "reconnect session").map(|_| ())
    }

    /// `POST …/work/{id}/heartbeat` — environment secret, session token
    /// in the body. A lost lease comes back as 200 with
    /// `lease_extended: false`, not as an error.
    async fn heartbeat_work(
        &self,
        environment_id: &str,
        work_id: &str,
        session_token: &str,
    ) -> BridgeApiResult<HeartbeatOutcome> {
        let secret = self.require_environment_secret()?;
        let call = Call::post(
            self.work_action_url(environment_id, work_id, "heartbeat"),
            self.config.request_timeout,
        )
        .body(json!({ "session_token": session_token }));
        let response = self.send(&call, &secret).await?;
        let response = ensure_success(response, "heartbeat work")?;
        json_body(response, "heartbeat outcome").await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PermissionResponseBody;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // ─── A stub RC server on a real socket ────────────────────────────

    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        /// Request target: path plus query string, exactly as sent.
        target: String,
        authorization: Option<String>,
        body: String,
    }

    impl Seen {
        fn bearer(&self) -> &str {
            self.authorization
                .as_deref()
                .and_then(|value| value.strip_prefix("Bearer "))
                .unwrap_or("")
        }

        fn path(&self) -> &str {
            self.target.split('?').next().unwrap_or("")
        }
    }

    struct Stub {
        base: String,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl Stub {
        fn requests(&self) -> Vec<Seen> {
            self.seen.lock().expect("stub log").clone()
        }

        fn only(&self) -> Seen {
            let requests = self.requests();
            assert_eq!(requests.len(), 1, "{requests:#?}");
            requests[0].clone()
        }
    }

    /// Bind a listener and answer each request with the next scripted
    /// `(status, body)`. Exhausting the script answers 500, which fails a
    /// test loudly rather than hanging it.
    async fn stub(script: Vec<(u16, &str)>) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let queue: Arc<Mutex<VecDeque<(u16, String)>>> = Arc::new(Mutex::new(
            script
                .into_iter()
                .map(|(status, body)| (status, body.to_string()))
                .collect(),
        ));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let log = log.clone();
                let queue = queue.clone();
                tokio::spawn(async move { serve(socket, log, queue).await });
            }
        });
        Stub {
            base: format!("http://{address}"),
            seen,
        }
    }

    async fn serve(
        mut socket: tokio::net::TcpStream,
        log: Arc<Mutex<Vec<Seen>>>,
        queue: Arc<Mutex<VecDeque<(u16, String)>>>,
    ) {
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            // Read until the head is complete.
            let head_end = loop {
                if let Some(at) = find(&buffer, b"\r\n\r\n") {
                    break at;
                }
                let mut chunk = [0u8; 4096];
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            };
            let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
            let mut lines = head.split("\r\n");
            let mut request_line = lines.next().unwrap_or_default().split(' ');
            let method = request_line.next().unwrap_or_default().to_string();
            let target = request_line.next().unwrap_or_default().to_string();
            let mut authorization = None;
            let mut length = 0usize;
            for line in lines {
                let Some((name, value)) = line.split_once(':') else {
                    continue;
                };
                match name.trim().to_ascii_lowercase().as_str() {
                    "authorization" => authorization = Some(value.trim().to_string()),
                    "content-length" => length = value.trim().parse().unwrap_or(0),
                    _ => {}
                }
            }
            let body_at = head_end + 4;
            while buffer.len() < body_at + length {
                let mut chunk = [0u8; 4096];
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            }
            let body = String::from_utf8_lossy(&buffer[body_at..body_at + length]).to_string();
            log.lock().expect("stub log").push(Seen {
                method,
                target,
                authorization,
                body,
            });

            let (status, payload) = queue
                .lock()
                .expect("stub script")
                .pop_front()
                .unwrap_or((500, String::new()));
            let response = if status == 204 {
                "HTTP/1.1 204 No Content\r\n\r\n".to_string()
            } else {
                format!(
                    "HTTP/1.1 {status} Status\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{payload}",
                    payload.len()
                )
            };
            if socket.write_all(response.as_bytes()).await.is_err() {
                return;
            }
            buffer.drain(..body_at + length);
        }
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    // ─── Fixtures ─────────────────────────────────────────────────────

    fn config(base: &str) -> HttpClientConfig {
        let mut config = HttpClientConfig::new(base);
        config.poll_wait = Duration::from_millis(250);
        config.poll_timeout_margin = Duration::from_secs(5);
        config.request_timeout = Duration::from_secs(5);
        config
    }

    fn client(stub: &Stub) -> HttpBridgeApiClient {
        HttpBridgeApiClient::new(
            config(&stub.base),
            DeviceCredentials::new("access-1", "refresh-1"),
        )
        .expect("client")
    }

    /// A client that already holds an environment secret, as it would
    /// after a successful registration.
    fn registered_client(stub: &Stub) -> HttpBridgeApiClient {
        let client = client(stub);
        client.set_environment_secret("env-secret");
        client
    }

    const REGISTRATION: &str = r#"{"environment_id":"env_1","environment_secret":"env-secret"}"#;
    const WORK: &str = r#"{"id":"wrk_1","type":"work","environment_id":"env_1","state":"leased","data":{"type":"session","id":"sess_1"},"secret":"opaque","created_at":"2026-09-16T00:00:00.000Z"}"#;

    fn event() -> PermissionResponseEvent {
        PermissionResponseEvent::new(PermissionResponseBody::success(
            "req-1",
            json!({"behavior": "allow"}),
        ))
    }

    // ─── Configuration ────────────────────────────────────────────────

    #[test]
    fn the_poll_http_timeout_always_outlasts_the_poll_wait() {
        let config = HttpClientConfig::new("https://rc.example.com");
        assert!(config.poll_timeout() > config.poll_wait);
        config.validate().expect("default configuration is valid");
    }

    #[test]
    fn a_zero_margin_is_rejected_so_the_client_cannot_strangle_its_own_poll() {
        let mut config = HttpClientConfig::new("https://rc.example.com");
        config.poll_timeout_margin = Duration::ZERO;
        let message = config.validate().expect_err("rejected");
        assert!(message.contains("margin"), "{message}");

        let mut too_long = HttpClientConfig::new("https://rc.example.com");
        too_long.poll_wait = Duration::from_secs(61);
        assert!(too_long.validate().is_err());

        let mut not_http = HttpClientConfig::new("rc.example.com");
        not_http.poll_wait = Duration::from_secs(1);
        assert!(not_http.validate().is_err());
    }

    #[tokio::test]
    async fn an_invalid_configuration_never_produces_a_client() {
        let error = HttpBridgeApiClient::new(
            HttpClientConfig::new("nonsense"),
            DeviceCredentials::access_only("access-1"),
        )
        .expect_err("rejected");
        assert!(matches!(error, BridgeApiError::Other(_)), "{error:?}");
    }

    // ─── Devices ──────────────────────────────────────────────────────

    const ISSUED: &str = r#"{"account_id":"acct_1","device_id":"dev_1","refresh_token":"refresh-9","access_token":"access-9","access_expires_at":"2026-09-16T01:00:00Z"}"#;

    #[tokio::test]
    async fn issuing_a_device_sends_the_given_bearer_and_label() {
        let stub = stub(vec![(201, ISSUED), (401, "")]).await;
        let issued = HttpBridgeApiClient::issue_device(
            &config(&stub.base),
            "bootstrap-token",
            &IssueDeviceRequest {
                label: Some("workshop".into()),
            },
        )
        .await
        .expect("issued");
        assert_eq!(issued.device_id, "dev_1");
        assert_eq!(issued.refresh_token, "refresh-9");
        let seen = stub.only();
        assert_eq!(seen.method, "POST");
        assert_eq!(seen.path(), "/v1/devices");
        assert_eq!(seen.bearer(), "bootstrap-token");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&seen.body).unwrap(),
            json!({"label": "workshop"})
        );

        // A spent bootstrap token is not retried through a refresh.
        let error = HttpBridgeApiClient::issue_device(
            &config(&stub.base),
            "bootstrap-token",
            &IssueDeviceRequest::default(),
        )
        .await
        .expect_err("refused");
        assert!(
            matches!(error, BridgeApiError::Unauthorized(_)),
            "{error:?}"
        );
        assert_eq!(stub.requests().len(), 2);
    }

    #[tokio::test]
    async fn refreshing_now_exchanges_the_refresh_token_once() {
        let stub = stub(vec![
            (
                200,
                r#"{"access_token":"access-2","access_expires_at":"x"}"#,
            ),
            (401, ""),
        ])
        .await;
        let client =
            HttpBridgeApiClient::new(config(&stub.base), DeviceCredentials::new("", "refresh-1"))
                .expect("client");
        client.refresh_now().await.expect("refreshed");
        assert_eq!(client.access_token(), "access-2");
        let seen = stub.only();
        assert_eq!(seen.path(), "/v1/devices/token");
        assert_eq!(seen.bearer(), "refresh-1");

        let error = client.refresh_now().await.expect_err("revoked");
        assert!(
            matches!(error, BridgeApiError::Unauthorized(_)),
            "{error:?}"
        );

        let no_refresh = HttpBridgeApiClient::new(
            config(&stub.base),
            DeviceCredentials::access_only("access-1"),
        )
        .expect("client");
        assert!(matches!(
            no_refresh.refresh_now().await,
            Err(BridgeApiError::Unauthorized(_))
        ));
    }

    // ─── URL building ─────────────────────────────────────────────────

    #[tokio::test]
    async fn every_route_lands_on_its_rfc_path() {
        let stub = stub(vec![
            (200, REGISTRATION),
            (200, WORK),
            (204, ""),
            (200, r#"{"lease_extended":true,"state":"running"}"#),
            (204, ""),
            (204, ""),
            (204, ""),
            (202, r#"{"work_id":"wrk_2","session_id":"sess_1"}"#),
            (204, ""),
        ])
        .await;
        let client = client(&stub);
        let config = BridgeConfig::minimal("bridge-1", "client-env-1", "https://api", "wss://sess");

        client
            .register_bridge_environment(&config)
            .await
            .expect("register");
        client
            .poll_for_work("env_1", "env-secret", PollOptions::default())
            .await
            .expect("poll");
        client
            .acknowledge_work("env_1", "wrk_1", "sess-token")
            .await
            .expect("ack");
        client
            .heartbeat_work("env_1", "wrk_1", "sess-token")
            .await
            .expect("heartbeat");
        client
            .stop_work("env_1", "wrk_1", true)
            .await
            .expect("stop");
        client
            .send_permission_response_event("sess_1", &event(), "sess-token")
            .await
            .expect("event");
        client.archive_session("sess_1").await.expect("archive");
        client
            .reconnect_session("env_1", "sess_1")
            .await
            .expect("reconnect");
        client
            .deregister_environment("env_1")
            .await
            .expect("deregister");

        let seen = stub.requests();
        let routes: Vec<(&str, &str)> = seen
            .iter()
            .map(|request| (request.method.as_str(), request.path()))
            .collect();
        assert_eq!(
            routes,
            vec![
                ("POST", "/v1/environments"),
                ("GET", "/v1/environments/env_1/work"),
                ("POST", "/v1/environments/env_1/work/wrk_1/ack"),
                ("POST", "/v1/environments/env_1/work/wrk_1/heartbeat"),
                ("POST", "/v1/environments/env_1/work/wrk_1/stop"),
                ("POST", "/v1/sessions/sess_1/events"),
                ("POST", "/v1/sessions/sess_1/archive"),
                ("POST", "/v1/environments/env_1/sessions/sess_1/reconnect"),
                ("DELETE", "/v1/environments/env_1"),
            ]
        );
    }

    #[tokio::test]
    async fn a_trailing_slash_on_the_base_url_does_not_double_up() {
        let stub = stub(vec![(204, "")]).await;
        let mut settings = config(&format!("{}/", stub.base));
        settings.poll_wait = Duration::from_millis(50);
        let client = HttpBridgeApiClient::new(settings, DeviceCredentials::access_only("access-1"))
            .expect("client");
        client
            .poll_for_work("env_1", "env-secret", PollOptions::default())
            .await
            .expect("poll");
        assert_eq!(stub.only().path(), "/v1/environments/env_1/work");
    }

    #[tokio::test]
    async fn a_path_segment_that_looks_like_a_route_is_percent_encoded() {
        let stub = stub(vec![(204, "")]).await;
        let client = registered_client(&stub);
        client
            .archive_session("sess_1/../../devices")
            .await
            .expect("archive");
        assert_eq!(
            stub.only().path(),
            "/v1/sessions/sess_1%2F..%2F..%2Fdevices/archive"
        );
    }

    #[tokio::test]
    async fn the_poll_sends_its_wait_and_the_reclaim_hint() {
        let stub = stub(vec![(204, ""), (204, "")]).await;
        let client = client(&stub);
        client
            .poll_for_work("env_1", "env-secret", PollOptions::default())
            .await
            .expect("poll");
        client
            .poll_for_work(
                "env_1",
                "env-secret",
                PollOptions {
                    reclaim_older_than_ms: Some(30_000),
                },
            )
            .await
            .expect("poll");

        let seen = stub.requests();
        assert_eq!(
            seen[0].target, "/v1/environments/env_1/work?timeoutMs=250",
            "the wait comes from client config, not PollOptions"
        );
        assert_eq!(
            seen[1].target,
            "/v1/environments/env_1/work?timeoutMs=250&reclaimOlderThanMs=30000"
        );
    }

    #[tokio::test]
    async fn the_controller_reads_land_on_their_routes_as_the_device() {
        let stub = stub(vec![
            (200, r#"{"events":[]}"#),
            (
                200,
                r#"{"events":[{"event_id":3,"kind":"session_state","payload":{"type":"session_state","state":"idle"},"created_at":"2026-09-16T00:00:00.000Z"}],"next_cursor":"c2"}"#,
            ),
            (200, r#"{"sessions":[]}"#),
            (200, r#"{"sessions":[],"next_cursor":"c3"}"#),
        ])
        .await;
        // Holding an environment secret must not change which credential
        // these present: they are controller routes.
        let client = registered_client(&stub);

        let first = client
            .session_events("sess_1", &PageRequest::first())
            .await
            .expect("first page");
        assert!(first.events.is_empty() && first.next_cursor.is_none());
        let next = client
            .session_events("sess_1", &PageRequest::after("c1/+=").with_limit(20))
            .await
            .expect("next page");
        assert_eq!(next.next_cursor.as_deref(), Some("c2"));
        assert_eq!(next.events[0].event_id, 3);
        client
            .list_sessions(None, &PageRequest::first())
            .await
            .expect("list");
        let filtered = client
            .list_sessions(Some("env_1"), &PageRequest::after("c2").with_limit(5))
            .await
            .expect("filtered list");
        assert_eq!(filtered.next_cursor.as_deref(), Some("c3"));

        let seen = stub.requests();
        let targets: Vec<(&str, &str, &str)> = seen
            .iter()
            .map(|request| {
                (
                    request.method.as_str(),
                    request.target.as_str(),
                    request.bearer(),
                )
            })
            .collect();
        assert_eq!(
            targets,
            vec![
                ("GET", "/v1/sessions/sess_1/events", "access-1"),
                (
                    "GET",
                    "/v1/sessions/sess_1/events?cursor=c1%2F%2B%3D&limit=20",
                    "access-1"
                ),
                ("GET", "/v1/sessions", "access-1"),
                (
                    "GET",
                    "/v1/sessions?environment=env_1&cursor=c2&limit=5",
                    "access-1"
                ),
            ]
        );
    }

    #[tokio::test]
    async fn a_controller_read_refreshes_on_401_and_maps_400_to_permanent() {
        let stub = stub(vec![
            (401, r#"{"error":"request rejected"}"#),
            (
                200,
                r#"{"access_token":"access-2","access_expires_at":"2026-09-16T01:00:00Z"}"#,
            ),
            (200, r#"{"sessions":[]}"#),
            (400, r#"{"error":"request rejected"}"#),
        ])
        .await;
        let client = client(&stub);
        client
            .list_sessions(None, &PageRequest::first())
            .await
            .expect("list after refresh");
        assert_eq!(client.access_token(), "access-2");
        let error = client
            .session_events("sess_1", &PageRequest::after("garbled"))
            .await
            .expect_err("rejected cursor");
        assert!(matches!(error, BridgeApiError::Permanent(_)), "{error:?}");
    }

    // ─── Credential selection ─────────────────────────────────────────

    #[tokio::test]
    async fn each_route_presents_the_credential_its_server_side_demands() {
        let stub = stub(vec![
            (200, REGISTRATION),
            (204, ""),
            (204, ""),
            (204, ""),
            (204, ""),
            (204, ""),
            (204, ""),
            (204, ""),
        ])
        .await;
        let client = client(&stub);
        let config = BridgeConfig::minimal("bridge-1", "client-env-1", "https://api", "wss://sess");

        // Registration hands back the environment secret the
        // credential-free methods below rely on.
        client
            .register_bridge_environment(&config)
            .await
            .expect("register");
        assert_eq!(client.environment_secret().as_deref(), Some("env-secret"));

        client
            .poll_for_work("env_1", "polled-secret", PollOptions::default())
            .await
            .expect("poll");
        client
            .acknowledge_work("env_1", "wrk_1", "sess-token")
            .await
            .expect("ack");
        client
            .stop_work("env_1", "wrk_1", false)
            .await
            .expect("stop");
        client
            .send_permission_response_event("sess_1", &event(), "sess-token")
            .await
            .expect("event");
        client.archive_session("sess_1").await.expect("archive");
        client
            .reconnect_session("env_1", "sess_1")
            .await
            .expect("reconnect");
        client
            .deregister_environment("env_1")
            .await
            .expect("deregister");

        let seen = stub.requests();
        let bearers: Vec<&str> = seen.iter().map(Seen::bearer).collect();
        assert_eq!(
            bearers,
            vec![
                "access-1",      // register            — device access
                "polled-secret", // poll                — the trait's argument
                "env-secret",    // ack                 — stored secret
                "env-secret",    // stop                — stored secret
                "sess-token",    // events              — the trait's argument
                "env-secret",    // archive             — stored secret, NOT the device token
                "access-1",      // reconnect           — device access
                "env-secret",    // deregister          — stored secret, NOT the device token
            ]
        );
    }

    #[tokio::test]
    async fn ack_and_heartbeat_put_the_session_token_in_the_body_and_stop_sends_force() {
        let stub = stub(vec![
            (204, ""),
            (200, r#"{"lease_extended":false,"state":"stopped"}"#),
            (204, ""),
        ])
        .await;
        let client = registered_client(&stub);
        client
            .acknowledge_work("env_1", "wrk_1", "sess-token")
            .await
            .expect("ack");
        let outcome = client
            .heartbeat_work("env_1", "wrk_1", "sess-token")
            .await
            .expect("heartbeat");
        client
            .stop_work("env_1", "wrk_1", true)
            .await
            .expect("stop");

        assert!(!outcome.lease_extended);
        assert_eq!(outcome.state, "stopped");
        let seen = stub.requests();
        assert_eq!(seen[0].body, r#"{"session_token":"sess-token"}"#);
        assert_eq!(seen[1].body, r#"{"session_token":"sess-token"}"#);
        assert_eq!(seen[2].body, r#"{"force":true}"#);
    }

    #[tokio::test]
    async fn the_credential_free_methods_refuse_to_fall_back_to_the_device_token() {
        let stub = stub(vec![]).await;
        let client = client(&stub);

        for error in [
            client.archive_session("sess_1").await.expect_err("archive"),
            client
                .deregister_environment("env_1")
                .await
                .expect_err("deregister"),
            client
                .acknowledge_work("env_1", "wrk_1", "t")
                .await
                .expect_err("ack"),
            client
                .heartbeat_work("env_1", "wrk_1", "t")
                .await
                .expect_err("heartbeat"),
            client
                .stop_work("env_1", "wrk_1", false)
                .await
                .expect_err("stop"),
        ] {
            assert!(matches!(error, BridgeApiError::Permanent(_)), "{error:?}");
        }
        assert!(
            stub.requests().is_empty(),
            "nothing should reach the network without an environment secret"
        );
    }

    // ─── 204 and the error table ──────────────────────────────────────

    #[tokio::test]
    async fn a_timed_out_long_poll_is_not_an_error() {
        let stub = stub(vec![(204, "")]).await;
        let client = client(&stub);
        let outcome = client
            .poll_for_work("env_1", "env-secret", PollOptions::default())
            .await
            .expect("204 maps to Ok(None)");
        assert!(outcome.is_none());
    }

    #[tokio::test]
    async fn a_poll_that_returns_work_decodes_it() {
        let stub = stub(vec![(200, WORK)]).await;
        let client = client(&stub);
        let work = client
            .poll_for_work("env_1", "env-secret", PollOptions::default())
            .await
            .expect("poll")
            .expect("work");
        assert_eq!(work.id, "wrk_1");
        assert_eq!(work.data.id, "sess_1");
    }

    #[tokio::test]
    async fn the_status_table_holds() {
        for (status, expected) in [
            (400u16, "permanent"),
            (403, "permanent"),
            (404, "permanent"),
            (409, "permanent"),
            (413, "permanent"),
            (429, "transient"),
            (500, "transient"),
            (502, "transient"),
            (503, "transient"),
        ] {
            let stub = stub(vec![(status, r#"{"error":"request rejected"}"#)]).await;
            let client = registered_client(&stub);
            let error = client
                .archive_session("sess_1")
                .await
                .expect_err("rejected");
            let actual = match error {
                BridgeApiError::Permanent(_) => "permanent",
                BridgeApiError::Transient(_) => "transient",
                BridgeApiError::Unauthorized(_) => "unauthorized",
                other => panic!("unexpected {other:?} for {status}"),
            };
            assert_eq!(actual, expected, "status {status}");
        }
    }

    #[tokio::test]
    async fn a_401_on_a_non_device_route_is_unauthorized_without_a_refresh() {
        let stub = stub(vec![(401, r#"{"error":"request rejected"}"#)]).await;
        let client = registered_client(&stub);
        let error = client
            .archive_session("sess_1")
            .await
            .expect_err("rejected");
        assert!(
            matches!(error, BridgeApiError::Unauthorized(_)),
            "{error:?}"
        );
        assert_eq!(
            stub.requests().len(),
            1,
            "no refresh on an environment route"
        );
    }

    #[tokio::test]
    async fn a_success_body_that_will_not_parse_is_a_protocol_error() {
        let stub = stub(vec![(200, "not json at all")]).await;
        let client = client(&stub);
        let config = BridgeConfig::minimal("bridge-1", "client-env-1", "https://api", "wss://sess");
        let error = client
            .register_bridge_environment(&config)
            .await
            .expect_err("rejected");
        assert!(matches!(error, BridgeApiError::Protocol(_)), "{error:?}");
    }

    #[tokio::test]
    async fn an_unreachable_server_is_transient() {
        // Bind and immediately drop, so the port is almost certainly free.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        drop(listener);
        let client = HttpBridgeApiClient::new(
            config(&format!("http://{address}")),
            DeviceCredentials::access_only("access-1"),
        )
        .expect("client");
        let error = client
            .poll_for_work("env_1", "env-secret", PollOptions::default())
            .await
            .expect_err("rejected");
        assert!(matches!(error, BridgeApiError::Transient(_)), "{error:?}");
        assert!(error.is_transient());
    }

    // ─── Refresh and retry ────────────────────────────────────────────

    #[tokio::test]
    async fn a_401_on_a_device_route_refreshes_once_and_retries() {
        let stub = stub(vec![
            (401, r#"{"error":"request rejected"}"#),
            (
                200,
                r#"{"access_token":"access-2","access_expires_at":"2026-09-16T01:00:00Z"}"#,
            ),
            (200, REGISTRATION),
        ])
        .await;
        let client = client(&stub);
        let config = BridgeConfig::minimal("bridge-1", "client-env-1", "https://api", "wss://sess");
        let registered = client
            .register_bridge_environment(&config)
            .await
            .expect("register after refresh");
        assert_eq!(registered.environment_id, "env_1");
        assert_eq!(client.access_token(), "access-2");

        let seen = stub.requests();
        assert_eq!(seen.len(), 3);
        assert_eq!(
            (seen[0].path(), seen[0].bearer()),
            ("/v1/environments", "access-1")
        );
        assert_eq!(
            (seen[1].method.as_str(), seen[1].path(), seen[1].bearer()),
            ("POST", "/v1/devices/token", "refresh-1"),
        );
        assert_eq!(
            (seen[2].path(), seen[2].bearer()),
            ("/v1/environments", "access-2")
        );
    }

    #[tokio::test]
    async fn a_second_401_is_unauthorized_and_is_not_refreshed_again() {
        let stub = stub(vec![
            (401, r#"{"error":"request rejected"}"#),
            (
                200,
                r#"{"access_token":"access-2","access_expires_at":"2026-09-16T01:00:00Z"}"#,
            ),
            (401, r#"{"error":"request rejected"}"#),
        ])
        .await;
        let client = client(&stub);
        let error = client
            .reconnect_session("env_1", "sess_1")
            .await
            .expect_err("rejected");
        assert!(
            matches!(error, BridgeApiError::Unauthorized(_)),
            "{error:?}"
        );
        assert_eq!(stub.requests().len(), 3, "exactly one refresh, one retry");
    }

    #[tokio::test]
    async fn without_a_refresh_token_a_401_is_simply_unauthorized() {
        let stub = stub(vec![(401, r#"{"error":"request rejected"}"#)]).await;
        let client = HttpBridgeApiClient::new(
            config(&stub.base),
            DeviceCredentials::access_only("access-1"),
        )
        .expect("client");
        let error = client
            .reconnect_session("env_1", "sess_1")
            .await
            .expect_err("rejected");
        assert!(
            matches!(error, BridgeApiError::Unauthorized(_)),
            "{error:?}"
        );
        assert_eq!(stub.requests().len(), 1, "no token exchange was attempted");
    }

    #[tokio::test]
    async fn a_refresh_that_is_itself_rejected_surfaces_as_unauthorized() {
        let stub = stub(vec![
            (401, r#"{"error":"request rejected"}"#),
            (401, r#"{"error":"request rejected"}"#),
        ])
        .await;
        let client = client(&stub);
        let error = client
            .reconnect_session("env_1", "sess_1")
            .await
            .expect_err("rejected");
        assert!(
            matches!(error, BridgeApiError::Unauthorized(_)),
            "{error:?}"
        );
        assert_eq!(stub.requests().len(), 2);
    }

    #[tokio::test]
    async fn a_rate_limited_refresh_is_transient_not_fatal() {
        let stub = stub(vec![
            (401, r#"{"error":"request rejected"}"#),
            (429, r#"{"error":"request rejected"}"#),
        ])
        .await;
        let client = client(&stub);
        let error = client
            .reconnect_session("env_1", "sess_1")
            .await
            .expect_err("rejected");
        assert!(matches!(error, BridgeApiError::Transient(_)), "{error:?}");
    }

    // ─── Object safety ────────────────────────────────────────────────

    #[tokio::test]
    async fn debug_output_never_carries_a_credential() {
        let stub = stub(vec![(200, REGISTRATION)]).await;
        let client = client(&stub);
        let config = BridgeConfig::minimal("bridge-1", "client-env-1", "https://api", "wss://sess");
        client
            .register_bridge_environment(&config)
            .await
            .expect("register");
        let rendered = format!("{client:?}");
        for secret in ["access-1", "refresh-1", "env-secret"] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
        assert!(rendered.contains("<redacted>"), "{rendered}");
        let credentials = format!("{:?}", DeviceCredentials::new("hunter2", "hunter3"));
        assert!(!credentials.contains("hunter"), "{credentials}");
    }

    #[tokio::test]
    async fn the_client_is_usable_as_a_trait_object() {
        let stub = stub(vec![(200, REGISTRATION)]).await;
        let client: std::sync::Arc<dyn BridgeApiClient> = std::sync::Arc::new(client(&stub));
        let config = BridgeConfig::minimal("bridge-1", "client-env-1", "https://api", "wss://sess");
        let registered = client
            .register_bridge_environment(&config)
            .await
            .expect("register");
        assert_eq!(registered.environment_secret, "env-secret");
    }
}
