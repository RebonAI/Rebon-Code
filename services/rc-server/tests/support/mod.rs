//! Shared harness: bind a real listener, drive it with `reqwest`.
//!
//! Mirrors `relay-server`'s test style — no in-process service calls, no
//! mocked transport. What the tests exercise is the same axum stack an
//! operator runs.

#![allow(dead_code)]

use std::{net::SocketAddr, sync::Arc, time::Duration};

use rebon_bridge::config::BridgeConfig;
use rebon_rc_server::{app, ids, Config, RcState};
use reqwest::{Response, StatusCode};
use serde_json::Value;
use tokio::{net::TcpListener, task::JoinHandle};

/// A running server plus the handle its tests need.
pub struct Server {
    pub base: String,
    pub state: Arc<RcState>,
    pub bootstrap_token: String,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Knobs individual tests turn; everything else is the production default.
pub struct Options {
    pub lease_ttl: Duration,
    pub access_token_ttl: Duration,
    pub default_poll_wait: Duration,
    pub max_body_bytes: usize,
    pub session_replay_events: usize,
    pub page_limit_default: usize,
    pub page_limit_max: usize,
    pub auth_rate_burst: u32,
    /// Reuse a database file (and HMAC key) across two servers, to test
    /// that state survives a restart.
    pub database_path: Option<std::path::PathBuf>,
    pub hmac_key: [u8; 32],
    pub bootstrap_token: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            lease_ttl: Duration::from_secs(60),
            access_token_ttl: Duration::from_secs(3_600),
            // Short by default so a test that expects 204 does not sit
            // for 25 seconds; tests that care pass an explicit timeoutMs.
            default_poll_wait: Duration::from_millis(150),
            max_body_bytes: 262_144,
            session_replay_events: 200,
            page_limit_default: 100,
            page_limit_max: 500,
            auth_rate_burst: 10_000,
            database_path: None,
            hmac_key: [0u8; 32],
            bootstrap_token: None,
        }
    }
}

/// Start a server with the default options.
pub async fn server() -> Server {
    server_with(Options::default()).await
}

/// Start a server, overriding some options.
pub async fn server_with(options: Options) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let bootstrap_token = options
        .bootstrap_token
        .clone()
        .unwrap_or_else(ids::generate_token);
    let config = Config {
        bind: address,
        public_url: format!("http://{address}"),
        session_ingress_url: format!("ws://{address}"),
        lease_ttl: options.lease_ttl,
        access_token_ttl: options.access_token_ttl,
        default_poll_wait: options.default_poll_wait,
        max_poll_wait: Duration::from_secs(60),
        max_body_bytes: options.max_body_bytes,
        session_replay_events: options.session_replay_events,
        page_limit_default: options.page_limit_default,
        page_limit_max: options.page_limit_max,
        auth_rate_burst: options.auth_rate_burst,
        auth_rate_refill_per_minute: 60,
        bootstrap_token: Some(bootstrap_token.clone()),
        trust_forwarded_for: false,
    };
    config.validate().expect("test configuration is valid");
    let state = Arc::new(match &options.database_path {
        Some(path) => RcState::open(config, path, options.hmac_key).expect("open database"),
        None => RcState::in_memory(config, options.hmac_key).expect("open database"),
    });
    let serve_state = state.clone();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            app(serve_state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("server");
    });
    Server {
        base: format!("http://{address}"),
        state,
        bootstrap_token,
        task,
    }
}

/// Everything the bootstrap flow hands back.
#[derive(Debug, Clone)]
pub struct Device {
    pub account_id: String,
    pub device_id: String,
    pub refresh_token: String,
    pub access_token: String,
}

pub fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// Consume the bootstrap token: mint the account and its first device.
pub async fn bootstrap(server: &Server) -> Device {
    let response = client()
        .post(format!("{}/v1/devices", server.base))
        .bearer_auth(&server.bootstrap_token)
        .json(&serde_json::json!({"label": "first"}))
        .send()
        .await
        .expect("issue bootstrap device");
    assert_eq!(response.status(), StatusCode::CREATED);
    device_from(response).await
}

/// Issue an additional device using an existing access token.
pub async fn issue_device(server: &Server, access_token: &str, label: &str) -> Device {
    let response = client()
        .post(format!("{}/v1/devices", server.base))
        .bearer_auth(access_token)
        .json(&serde_json::json!({ "label": label }))
        .send()
        .await
        .expect("issue device");
    assert_eq!(response.status(), StatusCode::CREATED);
    device_from(response).await
}

async fn device_from(response: Response) -> Device {
    let body: Value = response.json().await.expect("device json");
    Device {
        account_id: body["account_id"].as_str().expect("account_id").to_string(),
        device_id: body["device_id"].as_str().expect("device_id").to_string(),
        refresh_token: body["refresh_token"]
            .as_str()
            .expect("refresh_token")
            .to_string(),
        access_token: body["access_token"]
            .as_str()
            .expect("access_token")
            .to_string(),
    }
}

/// A `BridgeConfig` with the fields RC actually stores filled in.
pub fn bridge_config(client_environment_id: &str) -> BridgeConfig {
    let mut config = BridgeConfig::minimal(
        "bridge-uuid-1",
        client_environment_id,
        "https://rc.example.com",
        "wss://rc.example.com",
    );
    config.dir = "/home/user/repo".to_string();
    config.machine_name = "workshop".to_string();
    config.branch = "main".to_string();
    config.git_repo_url = Some("git@github.com:user/repo.git".to_string());
    config.max_sessions = 4;
    config
}

/// Register an environment, returning `(environment_id, environment_secret)`.
pub async fn register(
    server: &Server,
    access_token: &str,
    config: &BridgeConfig,
) -> (String, String) {
    let response = client()
        .post(format!("{}/v1/environments", server.base))
        .bearer_auth(access_token)
        .json(config)
        .send()
        .await
        .expect("register environment");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("registration json");
    (
        body["environment_id"]
            .as_str()
            .expect("environment_id")
            .to_string(),
        body["environment_secret"]
            .as_str()
            .expect("environment_secret")
            .to_string(),
    )
}

/// Bootstrap a device and register one environment in one step.
pub async fn bootstrapped_environment(server: &Server) -> (Device, String, String) {
    let device = bootstrap(server).await;
    let (environment_id, secret) =
        register(server, &device.access_token, &bridge_config("client-env-1")).await;
    (device, environment_id, secret)
}

/// Queue session work and return its work id.
pub async fn enqueue(
    server: &Server,
    access_token: &str,
    environment_id: &str,
    prompt: &str,
) -> (String, String) {
    let response = client()
        .post(format!(
            "{}/v1/environments/{environment_id}/work",
            server.base
        ))
        .bearer_auth(access_token)
        .json(&serde_json::json!({"type": "session", "prompt": prompt}))
        .send()
        .await
        .expect("enqueue work");
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: Value = response.json().await.expect("enqueue json");
    (
        body["work_id"].as_str().expect("work_id").to_string(),
        body["session_id"].as_str().expect("session_id").to_string(),
    )
}

/// Long-poll once. Returns `None` on a 204.
pub async fn poll(
    server: &Server,
    environment_id: &str,
    secret: &str,
    timeout_ms: u64,
) -> Option<Value> {
    let response = client()
        .get(format!(
            "{}/v1/environments/{environment_id}/work?timeoutMs={timeout_ms}",
            server.base
        ))
        .bearer_auth(secret)
        .send()
        .await
        .expect("poll for work");
    match response.status() {
        StatusCode::OK => Some(response.json().await.expect("work json")),
        StatusCode::NO_CONTENT => None,
        other => panic!("unexpected poll status {other}"),
    }
}

/// Pull the session token out of an opaque `WorkResponse.secret`.
pub fn session_token_of(work: &Value) -> String {
    let encoded = work["secret"].as_str().expect("secret");
    let secret = rebon_rc_server::work_secret::WorkSecret::decode(encoded).expect("work secret");
    secret.session_token
}
