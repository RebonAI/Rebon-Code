use std::{net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rebon_relay_server::{app, Config, RelayState};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rebon_relay_server=info,tower_http=info".into()),
        )
        .init();

    let config = Config {
        bind: SocketAddr::from_str(
            &std::env::var("REBON_RELAY_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into()),
        )?,
        public_url: std::env::var("REBON_RELAY_PUBLIC_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8080".into()),
        pending_cap: env_usize("REBON_RELAY_PENDING_CAP", 10_000)?,
        trusted_cap: env_usize("REBON_RELAY_TRUSTED_CAP", 100_000)?,
        sockets_per_ip: env_usize("REBON_RELAY_SOCKETS_PER_IP", 32)?,
        trust_forwarded_for: env_bool("REBON_RELAY_TRUST_FORWARDED_FOR", false)?,
    };
    config.validate()?;
    let bind = config.bind;
    let database_path =
        std::env::var("REBON_RELAY_DATABASE_PATH").unwrap_or_else(|_| "rebon-relay.sqlite3".into());
    let hmac_key = token_hmac_key()?;
    let state = Arc::new(RelayState::open(config, &database_path, hmac_key)?);
    state.clone().start_cleanup();
    let listener = TcpListener::bind(bind).await?;
    info!(bind = %listener.local_addr()?, public_url = %state.config().public_url, "relay listening");
    axum::serve(
        listener,
        app(state.clone()).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(state))
    .await?;
    Ok(())
}

fn env_usize(name: &str, default: usize) -> Result<usize, Box<dyn std::error::Error>> {
    Ok(std::env::var(name).map_or(Ok(default), |value| value.parse())?)
}

fn env_bool(name: &str, default: bool) -> Result<bool, Box<dyn std::error::Error>> {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(format!("{name} must be a boolean (true/false)").into()),
        },
    }
}

fn token_hmac_key() -> Result<[u8; 32], Box<dyn std::error::Error>> {
    let mut encoded = std::env::var("REBON_RELAY_TOKEN_HMAC_KEY")
        .map_err(|_| {
            "REBON_RELAY_TOKEN_HMAC_KEY is required and must be a base64url-encoded 32-byte key"
        })?
        .into_bytes();
    let decoded_result = URL_SAFE_NO_PAD.decode(&encoded);
    encoded.fill(0);
    let mut decoded = decoded_result?;
    if decoded.len() != 32 {
        decoded.fill(0);
        return Err("REBON_RELAY_TOKEN_HMAC_KEY must decode to exactly 32 bytes".into());
    }
    let key = decoded.as_slice().try_into()?;
    decoded.fill(0);
    Ok(key)
}

async fn shutdown_signal(state: Arc<RelayState>) {
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
    state.shutdown();
    tokio::time::sleep(Duration::from_millis(10)).await;
}
