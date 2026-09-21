//! `rebon-rc-server` binary: configuration from `REBON_RC_*`, then serve.

#![forbid(unsafe_code)]

use std::{net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rebon_rc_server::{app, Config, RcState};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

type Failure = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() -> Result<(), Failure> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rebon_rc_server=info,tower_http=info".into()),
        )
        .init();

    let config = Config {
        bind: SocketAddr::from_str(
            &std::env::var("REBON_RC_BIND").unwrap_or_else(|_| "127.0.0.1:8090".into()),
        )?,
        public_url: std::env::var("REBON_RC_PUBLIC_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
        session_ingress_url: std::env::var("REBON_RC_SESSION_INGRESS_URL")
            .unwrap_or_else(|_| "ws://127.0.0.1:8090".into()),
        lease_ttl: env_duration("REBON_RC_LEASE_TTL_SECONDS", 90)?,
        access_token_ttl: env_duration("REBON_RC_ACCESS_TOKEN_TTL_SECONDS", 3_600)?,
        default_poll_wait: env_duration("REBON_RC_DEFAULT_POLL_WAIT_SECONDS", 25)?,
        max_poll_wait: env_duration("REBON_RC_MAX_POLL_WAIT_SECONDS", 60)?,
        max_body_bytes: env_usize("REBON_RC_MAX_BODY_BYTES", 262_144)?,
        session_replay_events: env_usize("REBON_RC_SESSION_REPLAY_EVENTS", 200)?,
        page_limit_default: env_usize("REBON_RC_PAGE_LIMIT_DEFAULT", 100)?,
        page_limit_max: env_usize("REBON_RC_PAGE_LIMIT_MAX", 500)?,
        auth_rate_burst: env_u32("REBON_RC_AUTH_RATE_BURST", 30)?,
        auth_rate_refill_per_minute: env_u32("REBON_RC_AUTH_RATE_REFILL_PER_MINUTE", 30)?,
        bootstrap_token: std::env::var("REBON_RC_BOOTSTRAP_TOKEN").ok(),
        trust_forwarded_for: env_bool("REBON_RC_TRUST_FORWARDED_FOR", false)?,
    };
    config.validate()?;
    let bind = config.bind;
    let database_path =
        std::env::var("REBON_RC_DATABASE_PATH").unwrap_or_else(|_| "rebon-rc.sqlite3".into());
    let hmac_key = token_hmac_key()?;
    let bootstrap_configured = config.bootstrap_token.is_some();
    let state = Arc::new(RcState::open(config, &database_path, hmac_key)?);

    // Without an account and without a bootstrap token there is no way
    // to obtain a credential, so every route would be unreachable.
    // Refusing to start says so now rather than at the first request.
    if !state.store().has_account()? && !bootstrap_configured {
        return Err("REBON_RC_BOOTSTRAP_TOKEN is required until the first account exists".into());
    }

    state.clone().start_cleanup();
    let listener = TcpListener::bind(bind).await?;
    info!(
        bind = %listener.local_addr()?,
        public_url = %state.config().public_url,
        "rc listening"
    );
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

fn env_usize(name: &str, default: usize) -> Result<usize, Failure> {
    Ok(std::env::var(name).map_or(Ok(default), |value| value.parse())?)
}

fn env_u32(name: &str, default: u32) -> Result<u32, Failure> {
    Ok(std::env::var(name).map_or(Ok(default), |value| value.parse())?)
}

fn env_duration(name: &str, default_seconds: u64) -> Result<Duration, Failure> {
    let seconds: u64 = std::env::var(name).map_or(Ok(default_seconds), |value| value.parse())?;
    Ok(Duration::from_secs(seconds))
}

fn env_bool(name: &str, default: bool) -> Result<bool, Failure> {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(format!("{name} must be a boolean (true/false)").into()),
        },
    }
}

/// Load the key every credential digest is derived from. Required: a
/// generated-at-startup key would invalidate every stored digest on
/// restart, logging out every device and bridge.
fn token_hmac_key() -> Result<[u8; 32], Failure> {
    let mut encoded = std::env::var("REBON_RC_TOKEN_HMAC_KEY")
        .map_err(|_| {
            "REBON_RC_TOKEN_HMAC_KEY is required and must be a base64url-encoded 32-byte key"
        })?
        .into_bytes();
    let decoded_result = URL_SAFE_NO_PAD.decode(&encoded);
    encoded.fill(0);
    let mut decoded = decoded_result?;
    if decoded.len() != 32 {
        decoded.fill(0);
        return Err("REBON_RC_TOKEN_HMAC_KEY must decode to exactly 32 bytes".into());
    }
    let key = decoded.as_slice().try_into()?;
    decoded.fill(0);
    Ok(key)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl-C handler")
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    info!("graceful shutdown requested");
}
