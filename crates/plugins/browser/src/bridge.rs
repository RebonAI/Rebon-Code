use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use futures_util::{SinkExt, StreamExt};
use rebon_types::constant_time_eq;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{header, HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::Message;

pub const DEFAULT_BROWSER_PORT: u16 = 17_373;
pub const BROWSER_PROTOCOL_VERSION: u64 = 1;
pub const BROWSER_WEBSOCKET_PROTOCOL: &str = "rebon-browser-v1";

const PAIRING_CODE_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_PAIRING_ATTEMPTS: u8 = 5;
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct BrowserBridgeOptions {
    pub config_home: PathBuf,
    pub extension_dir: PathBuf,
    pub port: u16,
    pub request_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct BrowserBridge {
    inner: Arc<BridgeInner>,
}

#[derive(Debug)]
struct BridgeInner {
    options: BrowserBridgeOptions,
    state: Mutex<BridgeState>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    next_request_id: AtomicU64,
    next_connection_id: AtomicU64,
}

#[derive(Debug)]
struct BridgeState {
    stored_token: Option<String>,
    pairing: PairingCode,
    connection: Option<ExtensionConnection>,
}

#[derive(Debug)]
struct ExtensionConnection {
    id: u64,
    sender: mpsc::UnboundedSender<Message>,
    authenticated: bool,
    extension_version: Option<String>,
}

#[derive(Debug)]
struct PairingCode {
    code: String,
    expires_at: Instant,
    attempts: u8,
}

#[derive(Debug)]
enum PairAttempt {
    Paired(String),
    Rejected {
        message: &'static str,
        close_connection: bool,
    },
}

impl PairingCode {
    fn generate() -> anyhow::Result<Self> {
        let mut bytes = [0u8; 4];
        getrandom::getrandom(&mut bytes)
            .map_err(|err| anyhow!("could not generate browser pairing code: {err}"))?;
        let value = u32::from_le_bytes(bytes) % 1_000_000;
        Ok(Self {
            code: format!("{value:06}"),
            expires_at: Instant::now() + PAIRING_CODE_TTL,
            attempts: 0,
        })
    }

    fn time_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }

    fn attempts_exhausted(&self) -> bool {
        self.attempts >= MAX_PAIRING_ATTEMPTS
    }

    fn unavailable(&self) -> bool {
        self.time_expired() || self.attempts_exhausted()
    }
}

impl BrowserBridge {
    pub async fn start(mut options: BrowserBridgeOptions) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", options.port))
            .await
            .with_context(|| {
                format!(
                    "failed to bind Rebon browser bridge on 127.0.0.1:{}; another Rebon browser controller may already be running",
                    options.port
                )
            })?;
        options.port = listener.local_addr()?.port();
        let stored_token = load_token(&options.config_home)?;
        let bridge = Self {
            inner: Arc::new(BridgeInner {
                options,
                state: Mutex::new(BridgeState {
                    stored_token,
                    pairing: PairingCode::generate()?,
                    connection: None,
                }),
                pending: Mutex::new(HashMap::new()),
                next_request_id: AtomicU64::new(1),
                next_connection_id: AtomicU64::new(1),
            }),
        };
        let bridge_for_accept = bridge.clone();
        tokio::spawn(async move {
            bridge_for_accept.accept_loop(listener).await;
        });
        Ok(bridge)
    }

    pub async fn status(&self) -> anyhow::Result<Value> {
        let mut state = self.inner.state.lock().await;
        if state.pairing.unavailable() {
            state.pairing = PairingCode::generate()?;
        }
        let (socket_connected, authenticated, extension_version) = state
            .connection
            .as_ref()
            .map(|connection| {
                (
                    true,
                    connection.authenticated,
                    connection.extension_version.clone(),
                )
            })
            .unwrap_or((false, false, None));
        Ok(json!({
            "bridge": {
                "host": "127.0.0.1",
                "port": self.inner.options.port,
                "protocol": BROWSER_PROTOCOL_VERSION,
            },
            "extension_dir": self.inner.options.extension_dir,
            "socket_connected": socket_connected,
            "authenticated": authenticated,
            "paired": state.stored_token.is_some(),
            "pairing_code": if authenticated { None } else { Some(state.pairing.code.clone()) },
            "pairing_expires_in_seconds": if authenticated { None } else { Some(state.pairing.expires_at.saturating_duration_since(Instant::now()).as_secs()) },
            "extension_version": extension_version,
        }))
    }

    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let sender = {
            let state = self.inner.state.lock().await;
            let connection = state.connection.as_ref().ok_or_else(|| {
                anyhow!("Rebon browser extension is not connected; load the extension from `{}` and open its popup", self.inner.options.extension_dir.display())
            })?;
            if !connection.authenticated {
                return Err(anyhow!(
                    "Rebon browser extension is connected but not paired; run browser status and enter its pairing code in the extension popup"
                ));
            }
            connection.sender.clone()
        };

        let id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(id, response_tx);
        let message = json!({
            "type": "request",
            "protocol": BROWSER_PROTOCOL_VERSION,
            "id": id,
            "method": method,
            "params": params,
        });
        if sender.send(Message::Text(message.to_string())).is_err() {
            self.inner.pending.lock().await.remove(&id);
            return Err(anyhow!(
                "Rebon browser extension disconnected before the request was sent"
            ));
        }

        match timeout(self.inner.options.request_timeout, response_rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(message))) => Err(anyhow!(message)),
            Ok(Err(_)) => Err(anyhow!(
                "Rebon browser extension disconnected while handling `{method}`"
            )),
            Err(_) => {
                self.inner.pending.lock().await.remove(&id);
                Err(anyhow!(
                    "Rebon browser extension timed out while handling `{method}`"
                ))
            }
        }
    }

    async fn accept_loop(&self, listener: TcpListener) {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    if !peer.ip().is_loopback() {
                        tracing::warn!(%peer, "rejected non-loopback browser bridge connection");
                        continue;
                    }
                    let bridge = self.clone();
                    tokio::spawn(async move {
                        if let Err(err) = bridge.handle_connection(stream).await {
                            tracing::debug!(error = %err, "browser extension connection ended");
                        }
                    });
                }
                Err(err) => {
                    tracing::warn!(error = %err, "browser bridge accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    async fn handle_connection(&self, stream: TcpStream) -> anyhow::Result<()> {
        let websocket = tokio_tungstenite::accept_hdr_async(stream, validate_handshake)
            .await
            .context("browser extension WebSocket handshake failed")?;
        let connection_id = self
            .inner
            .next_connection_id
            .fetch_add(1, Ordering::Relaxed);
        let (mut writer, mut reader) = websocket.split();
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<Message>();

        let writer_task = tokio::spawn(async move {
            while let Some(message) = outgoing_rx.recv().await {
                if writer.send(message).await.is_err() {
                    break;
                }
            }
        });

        let hello = timeout(HELLO_TIMEOUT, reader.next())
            .await
            .map_err(|_| anyhow!("browser extension did not send hello in time"))?
            .ok_or_else(|| anyhow!("browser extension disconnected before hello"))??;
        let hello = parse_json_message(hello)?;
        if hello.get("type").and_then(Value::as_str) != Some("hello") {
            return Err(anyhow!("browser extension must send hello first"));
        }
        if hello.get("protocol").and_then(Value::as_u64) != Some(BROWSER_PROTOCOL_VERSION) {
            send_json(
                &outgoing_tx,
                json!({
                    "type": "protocol_error",
                    "expected": BROWSER_PROTOCOL_VERSION,
                }),
            )?;
            return Err(anyhow!("unsupported browser extension protocol version"));
        }

        let supplied_token = hello.get("token").and_then(Value::as_str);
        let extension_version = hello
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string);
        let authenticated = {
            let mut state = self.inner.state.lock().await;
            if state.connection.is_some() {
                send_json(
                    &outgoing_tx,
                    json!({
                        "type": "busy",
                        "message": "another Rebon browser extension connection is already active",
                    }),
                )?;
                return Err(anyhow!(
                    "another browser extension connection is already active"
                ));
            }
            let authenticated = match (&state.stored_token, supplied_token) {
                (Some(expected), Some(actual)) => constant_time_eq(expected, actual),
                _ => false,
            };
            state.connection = Some(ExtensionConnection {
                id: connection_id,
                sender: outgoing_tx.clone(),
                authenticated,
                extension_version,
            });
            authenticated
        };

        send_json(
            &outgoing_tx,
            json!({
                "type": "hello_ok",
                "protocol": BROWSER_PROTOCOL_VERSION,
                "authenticated": authenticated,
                "pairing_required": !authenticated,
            }),
        )?;

        let connection_result = async {
            while let Some(message) = reader.next().await {
                let message = message?;
                match message {
                    Message::Text(_) | Message::Binary(_) => {
                        let value = parse_json_message(message)?;
                        self.handle_extension_message(connection_id, &outgoing_tx, value)
                            .await?;
                    }
                    Message::Ping(payload) => {
                        let _ = outgoing_tx.send(Message::Pong(payload));
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => break,
                    Message::Frame(_) => {}
                }
            }
            Ok(())
        }
        .await;

        writer_task.abort();
        self.clear_connection(connection_id).await;
        connection_result
    }

    async fn handle_extension_message(
        &self,
        connection_id: u64,
        outgoing: &mpsc::UnboundedSender<Message>,
        message: Value,
    ) -> anyhow::Result<()> {
        match message.get("type").and_then(Value::as_str) {
            Some("pair") => {
                let supplied = message.get("code").and_then(Value::as_str).unwrap_or("");
                match self.pair_connection(connection_id, supplied).await? {
                    PairAttempt::Paired(token) => send_json(
                        outgoing,
                        json!({
                            "type": "pair_ok",
                            "token": token,
                        }),
                    )?,
                    PairAttempt::Rejected {
                        message,
                        close_connection,
                    } => {
                        send_json(
                            outgoing,
                            json!({
                                "type": "pair_error",
                                "message": message,
                            }),
                        )?;
                        if close_connection {
                            let _ = outgoing.send(Message::Close(None));
                        }
                    }
                }
            }
            Some("forget_pairing") => {
                self.forget_pairing(connection_id).await?;
                send_json(outgoing, json!({ "type": "pairing_forgotten" }))?;
            }
            Some("response") => {
                if !self.connection_authenticated(connection_id).await {
                    return Err(anyhow!("unauthenticated extension sent a response"));
                }
                let id = message
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow!("browser response is missing numeric id"))?;
                let response = if message.get("ok").and_then(Value::as_bool) == Some(true) {
                    Ok(message.get("result").cloned().unwrap_or(Value::Null))
                } else {
                    Err(message
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("browser action failed")
                        .to_string())
                };
                if let Some(pending) = self.inner.pending.lock().await.remove(&id) {
                    let _ = pending.send(response);
                }
            }
            Some("heartbeat") => {
                send_json(outgoing, json!({ "type": "heartbeat_ack" }))?;
            }
            Some("event") => {}
            Some(other) => return Err(anyhow!("unknown browser extension message `{other}`")),
            None => return Err(anyhow!("browser extension message is missing type")),
        }
        Ok(())
    }

    async fn pair_connection(
        &self,
        connection_id: u64,
        supplied: &str,
    ) -> anyhow::Result<PairAttempt> {
        let mut state = self.inner.state.lock().await;
        if state.pairing.time_expired() {
            return Ok(PairAttempt::Rejected {
                message: "pairing code expired; run browser status for a new code",
                close_connection: true,
            });
        }
        if state.pairing.attempts_exhausted() {
            return Ok(PairAttempt::Rejected {
                message: "too many invalid pairing attempts; run browser status for a new code",
                close_connection: true,
            });
        }

        state.pairing.attempts = state.pairing.attempts.saturating_add(1);
        if !constant_time_eq(&state.pairing.code, supplied) {
            let exhausted = state.pairing.attempts_exhausted();
            return Ok(PairAttempt::Rejected {
                message: if exhausted {
                    "too many invalid pairing attempts; run browser status for a new code"
                } else {
                    "invalid browser pairing code"
                },
                close_connection: exhausted,
            });
        }

        let token = rebon_types::secure_random_hex_token()
            .map_err(|err| anyhow!("could not generate browser auth token: {err}"))?;
        persist_token(&self.inner.options.config_home, &token)?;
        state.stored_token = Some(token.clone());
        state.pairing = PairingCode::generate()?;
        let connection = state
            .connection
            .as_mut()
            .filter(|connection| connection.id == connection_id)
            .ok_or_else(|| anyhow!("browser extension connection is no longer active"))?;
        connection.authenticated = true;
        Ok(PairAttempt::Paired(token))
    }

    async fn forget_pairing(&self, connection_id: u64) -> anyhow::Result<()> {
        let mut state = self.inner.state.lock().await;
        let connection = state
            .connection
            .as_mut()
            .filter(|connection| connection.id == connection_id)
            .ok_or_else(|| anyhow!("browser extension connection is no longer active"))?;
        if !connection.authenticated {
            return Err(anyhow!("browser extension is not authenticated"));
        }
        remove_token(&self.inner.options.config_home)?;
        connection.authenticated = false;
        state.stored_token = None;
        state.pairing = PairingCode::generate()?;
        Ok(())
    }

    async fn connection_authenticated(&self, connection_id: u64) -> bool {
        self.inner
            .state
            .lock()
            .await
            .connection
            .as_ref()
            .is_some_and(|connection| connection.id == connection_id && connection.authenticated)
    }

    async fn clear_connection(&self, connection_id: u64) {
        let cleared = {
            let mut state = self.inner.state.lock().await;
            if state
                .connection
                .as_ref()
                .is_some_and(|connection| connection.id == connection_id)
            {
                state.connection = None;
                true
            } else {
                false
            }
        };
        if cleared {
            let pending = std::mem::take(&mut *self.inner.pending.lock().await);
            for (_, sender) in pending {
                let _ = sender.send(Err("Rebon browser extension disconnected".to_string()));
            }
        }
    }
}

#[allow(clippy::result_large_err)]
fn validate_handshake(
    request: &Request,
    mut response: Response,
) -> Result<Response, ErrorResponse> {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !origin_allowed(origin) {
        return Err(reject_handshake(
            StatusCode::FORBIDDEN,
            "browser bridge accepts Chrome/Edge extension origins only",
        ));
    }
    let protocols = request
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !protocols
        .split(',')
        .map(str::trim)
        .any(|protocol| protocol == BROWSER_WEBSOCKET_PROTOCOL)
    {
        return Err(reject_handshake(
            StatusCode::BAD_REQUEST,
            "missing Rebon browser WebSocket protocol",
        ));
    }
    response.headers_mut().insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static(BROWSER_WEBSOCKET_PROTOCOL),
    );
    Ok(response)
}

fn reject_handshake(status: StatusCode, message: &str) -> ErrorResponse {
    tokio_tungstenite::tungstenite::http::Response::builder()
        .status(status)
        .body(Some(message.to_string()))
        .expect("valid WebSocket rejection response")
}

fn origin_allowed(origin: &str) -> bool {
    let Some(id) = origin.strip_prefix("chrome-extension://") else {
        return false;
    };
    id.len() == 32 && id.bytes().all(|byte| (b'a'..=b'p').contains(&byte))
}

fn parse_json_message(message: Message) -> anyhow::Result<Value> {
    match message {
        Message::Text(text) => serde_json::from_str(&text).context("invalid browser JSON message"),
        Message::Binary(bytes) => {
            serde_json::from_slice(&bytes).context("invalid browser binary JSON message")
        }
        _ => Err(anyhow!("expected browser JSON message")),
    }
}

fn send_json(sender: &mpsc::UnboundedSender<Message>, value: Value) -> anyhow::Result<()> {
    sender
        .send(Message::Text(value.to_string()))
        .map_err(|_| anyhow!("browser extension disconnected"))
}

fn token_path(config_home: &Path) -> PathBuf {
    config_home.join("browser").join("pairing-token")
}

fn load_token(config_home: &Path) -> anyhow::Result<Option<String>> {
    let path = token_path(config_home);
    match fs::read_to_string(&path) {
        Ok(token) => {
            let token = token.trim().to_string();
            if token.is_empty() {
                Ok(None)
            } else {
                Ok(Some(token))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn persist_token(config_home: &Path, token: &str) -> anyhow::Result<()> {
    let path = token_path(config_home);
    let parent = path.parent().expect("browser token path has parent");
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    rebon_session::write_private_file_atomically(&path, token.as_bytes())
        .with_context(|| format!("failed to persist {}", path.display()))?;
    Ok(())
}

fn remove_token(config_home: &Path) -> anyhow::Result<()> {
    let path = token_path(config_home);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    #[tokio::test]
    async fn extension_pairs_and_answers_a_browser_request() {
        let directory = tempfile::tempdir().unwrap();
        let bridge = BrowserBridge::start(BrowserBridgeOptions {
            config_home: directory.path().join("config"),
            extension_dir: directory.path().join("extension"),
            port: 0,
            request_timeout: Duration::from_secs(2),
        })
        .await
        .unwrap();
        let initial_status = bridge.status().await.unwrap();
        let port = initial_status["bridge"]["port"].as_u64().unwrap();
        let pairing_code = initial_status["pairing_code"].as_str().unwrap().to_string();
        let invalid_code = if pairing_code == "999999" {
            "888888"
        } else {
            "999999"
        };

        let mut request = format!("ws://127.0.0.1:{port}")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("chrome-extension://abcdefghijklmnopabcdefghijklmnop"),
        );
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(BROWSER_WEBSOCKET_PROTOCOL),
        );
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        socket
            .send(Message::Text(
                json!({
                    "type": "hello",
                    "protocol": BROWSER_PROTOCOL_VERSION,
                    "version": "0.1.0"
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let hello = parse_json_message(socket.next().await.unwrap().unwrap()).unwrap();
        assert_eq!(hello["pairing_required"], true);

        socket
            .send(Message::Text(
                json!({ "type": "pair", "code": invalid_code }).to_string(),
            ))
            .await
            .unwrap();
        let rejected = parse_json_message(socket.next().await.unwrap().unwrap()).unwrap();
        assert_eq!(rejected["type"], "pair_error");

        socket
            .send(Message::Text(
                json!({ "type": "pair", "code": pairing_code }).to_string(),
            ))
            .await
            .unwrap();
        let paired = parse_json_message(socket.next().await.unwrap().unwrap()).unwrap();
        assert_eq!(paired["type"], "pair_ok");
        assert_eq!(paired["token"].as_str().unwrap().len(), 64);

        let request_task = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.request("observe", json!({})).await }
        });
        let request = parse_json_message(socket.next().await.unwrap().unwrap()).unwrap();
        assert_eq!(request["method"], "observe");
        socket
            .send(Message::Text(
                json!({
                    "type": "response",
                    "id": request["id"],
                    "ok": true,
                    "result": { "title": "Example" }
                })
                .to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(request_task.await.unwrap().unwrap()["title"], "Example");
    }

    #[test]
    fn only_chromium_extension_origins_are_allowed() {
        assert!(origin_allowed(
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop"
        ));
        assert!(!origin_allowed("http://127.0.0.1"));
        assert!(!origin_allowed("chrome-extension://abc/extra"));
        assert!(!origin_allowed("chrome-extension://ABC"));
        assert!(!origin_allowed("moz-extension://abc"));
    }

    #[test]
    fn pairing_codes_are_fixed_width_and_lock_after_attempt_limit() {
        let mut pairing = PairingCode::generate().unwrap();
        assert_eq!(pairing.code.len(), 6);
        assert!(pairing.code.bytes().all(|byte| byte.is_ascii_digit()));
        pairing.attempts = MAX_PAIRING_ATTEMPTS;
        assert!(pairing.attempts_exhausted());
        assert!(pairing.unavailable());
    }

    #[tokio::test]
    async fn pairing_attempt_limit_stays_locked_until_status_rotates_the_code() {
        let directory = tempfile::tempdir().unwrap();
        let bridge = BrowserBridge::start(BrowserBridgeOptions {
            config_home: directory.path().join("config"),
            extension_dir: directory.path().join("extension"),
            port: 0,
            request_timeout: Duration::from_secs(1),
        })
        .await
        .unwrap();

        for attempt in 1..=MAX_PAIRING_ATTEMPTS {
            let result = bridge.pair_connection(1, "not-a-code").await.unwrap();
            let PairAttempt::Rejected {
                close_connection, ..
            } = result
            else {
                panic!("invalid code unexpectedly paired");
            };
            assert_eq!(close_connection, attempt == MAX_PAIRING_ATTEMPTS);
        }
        let PairAttempt::Rejected {
            close_connection, ..
        } = bridge.pair_connection(1, "not-a-code").await.unwrap()
        else {
            panic!("locked pairing unexpectedly succeeded");
        };
        assert!(close_connection);

        bridge.status().await.unwrap();
        let state = bridge.inner.state.lock().await;
        assert_eq!(state.pairing.attempts, 0);
    }

    #[test]
    fn tokens_round_trip_and_remove() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(load_token(directory.path()).unwrap(), None);
        persist_token(directory.path(), "secret").unwrap();
        assert_eq!(
            load_token(directory.path()).unwrap().as_deref(),
            Some("secret")
        );
        persist_token(directory.path(), "replacement").unwrap();
        assert_eq!(
            load_token(directory.path()).unwrap().as_deref(),
            Some("replacement")
        );
        remove_token(directory.path()).unwrap();
        assert_eq!(load_token(directory.path()).unwrap(), None);
    }
}
