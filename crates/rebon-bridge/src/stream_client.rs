//! A WebSocket client for the session stream.
//!
//! Behind the `ws` cargo feature, off by default, for the same reason the
//! HTTP client is behind `http`: most consumers want the frame envelope
//! in [`crate::session_stream`], which costs nothing, and not a
//! tungstenite + rustls build.
//!
//! ## Shape
//!
//! [`SessionStream::connect`] takes the `ingress_url` from a decoded
//! [`crate::work_secret::WorkSecret`] and a bearer credential — the
//! session token for a worker, a device access token for a controller —
//! and returns a connected stream. [`SessionStream::split`] hands back
//! the two halves a runner actually drives:
//!
//! * [`SessionStreamTx`] — cheap to clone, `Send + Sync`, and behind the
//!   object-safe [`SessionFrameSink`] so it can be shared as
//!   `Arc<dyn SessionFrameSink>` across whatever tasks produce frames.
//! * [`SessionStreamRx`] — the single reader. `recv` yields frames until
//!   the socket closes, then `None`; [`SessionStreamRx::close_reason`]
//!   says why. `recv_delivered` yields the same frames with the
//!   `event_id` RC stored each one under, for a reader that also pages
//!   history and has to drop the overlap.
//!
//! Pings from the server are answered by tungstenite as part of reading,
//! so the reader must keep being polled; a runner that stops reading
//! will be closed for missing its pong, which is the intended behaviour.
//!
//! ## Reconnecting is the caller's job
//!
//! This client never reconnects. What it does is say *why* the socket
//! ended, precisely enough to decide: [`CloseReason::Superseded`] and
//! [`CloseReason::LeaseGone`] mean stand down, [`CloseReason::Network`]
//! and [`CloseReason::Timeout`] mean try again. A handshake the server
//! refused is a [`SessionStreamError::Rejected`] carrying the status and
//! the same [`BridgeApiError`] classification the HTTP client uses.
//! [`SessionStreamError::is_lease_gone`] answers the one question a
//! worker most needs answered, whether the lease was lost before the
//! socket opened (409) or while it was open (close code 4409).

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{header::AUTHORIZATION, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::api_client::BridgeApiError;
use crate::session_stream::{close_code, DeliveredFrame, SessionFrame};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The raw message type [`SessionStreamTx::send_raw`] takes. Re-exported
/// only so a test outside this crate can build a message the envelope
/// would never produce without depending on this exact tungstenite.
#[doc(hidden)]
pub use tokio_tungstenite::tungstenite::Message as RawMessage;

/// Default cap on a single frame, matching RC's default
/// `REBON_RC_MAX_BODY_BYTES`: a prompt is the same size on either
/// transport.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 262_144;

/// How to reach one session's stream.
#[derive(Clone)]
pub struct SessionStreamOptions {
    /// `WorkSecret.ingress_url` — `wss://…/v1/sessions/{id}/stream`.
    pub ingress_url: String,
    /// Bearer credential: the session token for a worker, a device
    /// access token for a controller.
    pub token: String,
    /// Largest frame accepted from or sent to the server.
    pub max_frame_bytes: usize,
    /// How long the TCP connect, TLS and upgrade may take together.
    pub connect_timeout: Duration,
}

/// Redacted: `token` is a bearer credential.
impl std::fmt::Debug for SessionStreamOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionStreamOptions")
            .field("ingress_url", &self.ingress_url)
            .field("token", &"<redacted>")
            .field("max_frame_bytes", &self.max_frame_bytes)
            .field("connect_timeout", &self.connect_timeout)
            .finish()
    }
}

impl SessionStreamOptions {
    /// Options with the defaults: RC's frame cap and a 30 s connect.
    pub fn new(ingress_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            ingress_url: ingress_url.into(),
            token: token.into(),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            connect_timeout: Duration::from_secs(30),
        }
    }

    /// Reject options that cannot work before touching the network.
    pub fn validate(&self) -> Result<(), String> {
        if !(self.ingress_url.starts_with("ws://") || self.ingress_url.starts_with("wss://")) {
            return Err(format!(
                "ingress URL must be a ws(s) URL, got {:?}",
                self.ingress_url
            ));
        }
        if self.token.is_empty() {
            return Err("a bearer credential is required".into());
        }
        if self.max_frame_bytes == 0 {
            return Err("frame cap must be nonzero".into());
        }
        if self.connect_timeout.is_zero() {
            return Err("connect timeout must be nonzero".into());
        }
        Ok(())
    }
}

/// Why a session stream ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    /// The server closed with 1000. The *socket* is done; whether the
    /// *session* is done is said by its frames and its work item, not by
    /// this code.
    Normal,
    /// [`close_code::SUPERSEDED`] — a newer worker connection took this
    /// session. Reconnecting would evict it.
    Superseded,
    /// [`close_code::LEASE_GONE`] — the work item stopped being leased to
    /// this worker. The session token is dead; stand down.
    LeaseGone,
    /// [`close_code::TIMEOUT`] — a missed pong or an idle socket.
    Timeout,
    /// [`close_code::RESOURCE_LIMIT`] — an oversized frame, or this peer
    /// fell behind its outbound queue.
    ResourceLimit,
    /// [`close_code::PROTOCOL_ERROR`] — the server could not route what
    /// it was sent. Reconnecting will send the same thing again.
    ProtocolError,
    /// A close code this build does not know.
    Other {
        /// The code as received.
        code: u16,
        /// The reason text as received.
        reason: String,
    },
    /// The socket ended without a close frame: a reset, a dropped
    /// connection, a proxy that gave up.
    Network,
}

impl CloseReason {
    /// Map a received close code to a reason. The reason *text* is only
    /// kept for codes this build does not know — for the rest the code is
    /// the contract and the text is decoration.
    pub fn classify(code: u16, reason: &str) -> Self {
        match code {
            1000 => Self::Normal,
            close_code::SUPERSEDED => Self::Superseded,
            close_code::LEASE_GONE => Self::LeaseGone,
            close_code::TIMEOUT => Self::Timeout,
            close_code::RESOURCE_LIMIT => Self::ResourceLimit,
            close_code::PROTOCOL_ERROR => Self::ProtocolError,
            other => Self::Other {
                code: other,
                reason: reason.to_string(),
            },
        }
    }

    /// The close code, when there was a close frame.
    pub fn code(&self) -> Option<u16> {
        match self {
            Self::Normal => Some(1000),
            Self::Superseded => Some(close_code::SUPERSEDED),
            Self::LeaseGone => Some(close_code::LEASE_GONE),
            Self::Timeout => Some(close_code::TIMEOUT),
            Self::ResourceLimit => Some(close_code::RESOURCE_LIMIT),
            Self::ProtocolError => Some(close_code::PROTOCOL_ERROR),
            Self::Other { code, .. } => Some(*code),
            Self::Network => None,
        }
    }

    /// Whether reconnecting **with the same credential** can help.
    ///
    /// `false` for the three reasons where it cannot: another worker owns
    /// the session, this worker's lease is gone, or what it sent is
    /// something the server will refuse again. An unknown code is
    /// treated as retryable — a newer server's code for a condition this
    /// build cannot name is more likely transient than terminal, and the
    /// caller still has the code to decide otherwise.
    pub fn is_reconnectable(&self) -> bool {
        !matches!(
            self,
            Self::Superseded | Self::LeaseGone | Self::ProtocolError
        )
    }
}

/// Failures from the session-stream client.
#[derive(Debug, thiserror::Error)]
pub enum SessionStreamError {
    /// The options could not work.
    #[error("invalid session stream options: {0}")]
    Config(String),
    /// The server answered the upgrade with an error status. `error` is
    /// classified exactly as the HTTP client classifies statuses — 401 is
    /// `Unauthorized`, every other 4xx is `Permanent`, 429 and 5xx are
    /// `Transient` — and `status` is kept alongside, because `Permanent`
    /// alone cannot tell a lost lease (409) from a wrong session (403).
    #[error("session stream rejected: {error}")]
    Rejected {
        /// The HTTP status the upgrade was refused with.
        status: u16,
        /// Its classification.
        error: BridgeApiError,
    },
    /// The connection could not be made, or broke mid-write.
    #[error("session stream transport failure: {0}")]
    Transport(String),
    /// A frame could not be encoded or decoded, or the server sent
    /// something that is not a text frame.
    #[error("session stream protocol failure: {0}")]
    Protocol(String),
    /// The stream has already closed.
    #[error("session stream closed: {0:?}")]
    Closed(CloseReason),
}

impl SessionStreamError {
    /// Whether retrying the same operation later can help.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Rejected { error, .. } => error.is_transient(),
            Self::Transport(_) => true,
            Self::Closed(reason) => reason.is_reconnectable(),
            Self::Config(_) | Self::Protocol(_) => false,
        }
    }

    /// Whether this worker's lease is gone, however that was learned:
    /// refused at connect with 409, or closed mid-stream with
    /// [`close_code::LEASE_GONE`]. Either way the session token is dead
    /// and the session belongs to someone else now.
    pub fn is_lease_gone(&self) -> bool {
        matches!(
            self,
            Self::Rejected { status: 409, .. } | Self::Closed(CloseReason::LeaseGone)
        )
    }
}

/// Map an upgrade status the same way `http_client` maps a request's.
fn rejected(status: u16) -> SessionStreamError {
    let detail = format!("session stream upgrade: HTTP {status}");
    let error = match status {
        401 => BridgeApiError::Unauthorized(detail),
        429 | 500..=599 => BridgeApiError::Transient(detail),
        400..=499 => BridgeApiError::Permanent(detail),
        _ => BridgeApiError::Protocol(detail),
    };
    SessionStreamError::Rejected { status, error }
}

fn connect_error(error: WsError) -> SessionStreamError {
    match error {
        WsError::Http(response) => rejected(response.status().as_u16()),
        WsError::Url(error) => SessionStreamError::Config(error.to_string()),
        WsError::Io(_) | WsError::Tls(_) | WsError::ConnectionClosed | WsError::AlreadyClosed => {
            SessionStreamError::Transport(error.to_string())
        }
        other => SessionStreamError::Protocol(other.to_string()),
    }
}

/// A connected session stream.
#[derive(Debug)]
pub struct SessionStream {
    tx: SessionStreamTx,
    rx: SessionStreamRx,
}

impl SessionStream {
    /// Open the stream: upgrade `options.ingress_url` with
    /// `Authorization: Bearer <token>`.
    pub async fn connect(options: &SessionStreamOptions) -> Result<Self, SessionStreamError> {
        options.validate().map_err(SessionStreamError::Config)?;
        let mut request = options
            .ingress_url
            .as_str()
            .into_client_request()
            .map_err(|error| SessionStreamError::Config(error.to_string()))?;
        let bearer = HeaderValue::from_str(&format!("Bearer {}", options.token))
            .map_err(|_| SessionStreamError::Config("credential is not a valid header".into()))?;
        request.headers_mut().insert(AUTHORIZATION, bearer);

        let config = WebSocketConfig {
            max_message_size: Some(options.max_frame_bytes),
            max_frame_size: Some(options.max_frame_bytes),
            ..WebSocketConfig::default()
        };
        let connect = tokio_tungstenite::connect_async_with_config(request, Some(config), false);
        let (socket, _) = tokio::time::timeout(options.connect_timeout, connect)
            .await
            .map_err(|_| SessionStreamError::Transport("session stream connect timed out".into()))?
            .map_err(connect_error)?;

        let (sink, stream) = socket.split();
        Ok(Self {
            tx: SessionStreamTx {
                sink: Arc::new(Mutex::new(sink)),
                max_frame_bytes: options.max_frame_bytes,
            },
            rx: SessionStreamRx {
                stream,
                close: None,
            },
        })
    }

    /// Separate the writer from the reader.
    pub fn split(self) -> (SessionStreamTx, SessionStreamRx) {
        (self.tx, self.rx)
    }

    /// Send one frame. See [`SessionStreamTx::send`].
    pub async fn send(&self, frame: &SessionFrame) -> Result<(), SessionStreamError> {
        self.tx.send(frame).await
    }

    /// Receive one frame. See [`SessionStreamRx::recv`].
    pub async fn recv(&mut self) -> Option<Result<SessionFrame, SessionStreamError>> {
        self.rx.recv().await
    }

    /// Receive one frame with its event id. See
    /// [`SessionStreamRx::recv_delivered`].
    pub async fn recv_delivered(&mut self) -> Option<Result<DeliveredFrame, SessionStreamError>> {
        self.rx.recv_delivered().await
    }

    /// Why the stream ended, once it has.
    pub fn close_reason(&self) -> Option<&CloseReason> {
        self.rx.close_reason()
    }

    /// Close cleanly with 1000 and wait for the server to answer.
    pub async fn close(mut self) -> Result<(), SessionStreamError> {
        self.tx.close().await?;
        // Drain until the server's close arrives, so the reason is
        // recorded and the TCP connection ends in order.
        while self.rx.recv().await.is_some() {}
        Ok(())
    }
}

/// The writing half: shareable and object-safe.
#[async_trait]
pub trait SessionFrameSink: Send + Sync + std::fmt::Debug {
    /// Send one frame.
    async fn send_frame(&self, frame: &SessionFrame) -> Result<(), SessionStreamError>;
}

/// The writing half of a [`SessionStream`].
#[derive(Clone)]
pub struct SessionStreamTx {
    sink: Arc<Mutex<SplitSink<Socket, Message>>>,
    max_frame_bytes: usize,
}

impl std::fmt::Debug for SessionStreamTx {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionStreamTx")
            .field("max_frame_bytes", &self.max_frame_bytes)
            .finish_non_exhaustive()
    }
}

impl SessionStreamTx {
    /// Send one frame as a JSON text message.
    ///
    /// A frame over the cap is refused here rather than sent: the server
    /// would close the whole socket for it, and a runner is better placed
    /// to handle one failed send than a lost connection.
    pub async fn send(&self, frame: &SessionFrame) -> Result<(), SessionStreamError> {
        let text = serde_json::to_string(frame)
            .map_err(|error| SessionStreamError::Protocol(error.to_string()))?;
        if text.len() > self.max_frame_bytes {
            return Err(SessionStreamError::Protocol(format!(
                "{} frame is {} bytes, over the {} byte cap",
                frame.frame_type(),
                text.len(),
                self.max_frame_bytes
            )));
        }
        self.send_raw(Message::Text(text)).await
    }

    /// Send a message exactly as given. For tests that need to put
    /// something on the wire the envelope would never produce.
    #[doc(hidden)]
    pub async fn send_raw(&self, message: Message) -> Result<(), SessionStreamError> {
        self.sink
            .lock()
            .await
            .send(message)
            .await
            .map_err(|error| match error {
                WsError::ConnectionClosed | WsError::AlreadyClosed => {
                    SessionStreamError::Closed(CloseReason::Network)
                }
                other => SessionStreamError::Transport(other.to_string()),
            })
    }

    /// Start a clean close with 1000.
    pub async fn close(&self) -> Result<(), SessionStreamError> {
        self.send_raw(Message::Close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: Cow::Borrowed("normal"),
        })))
        .await
    }
}

#[async_trait]
impl SessionFrameSink for SessionStreamTx {
    async fn send_frame(&self, frame: &SessionFrame) -> Result<(), SessionStreamError> {
        self.send(frame).await
    }
}

/// The reading half of a [`SessionStream`].
#[derive(Debug)]
pub struct SessionStreamRx {
    stream: SplitStream<Socket>,
    close: Option<CloseReason>,
}

impl SessionStreamRx {
    /// The next frame, or `None` once the stream has ended.
    ///
    /// [`Self::recv_delivered`] without the event id.
    pub async fn recv(&mut self) -> Option<Result<SessionFrame, SessionStreamError>> {
        self.recv_delivered()
            .await
            .map(|delivered| delivered.map(|delivered| delivered.frame))
    }

    /// The next frame and the `event_id` RC persisted it under, or `None`
    /// once the stream has ended.
    ///
    /// A controller that also reads [`crate::history`] pages drops any
    /// frame whose id it has already seen there. A frame RC never
    /// persisted (a `stream_error`) has no id.
    ///
    /// A text message that does not parse as a [`DeliveredFrame`] is an
    /// `Err` but does **not** end the stream — the caller decides whether
    /// one bad frame is fatal. A binary message is the same. Pings and
    /// pongs are consumed here and never surface.
    pub async fn recv_delivered(&mut self) -> Option<Result<DeliveredFrame, SessionStreamError>> {
        if self.close.is_some() {
            return None;
        }
        loop {
            let message = match self.stream.next().await {
                Some(Ok(message)) => message,
                Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) | None => {
                    self.close = Some(CloseReason::Network);
                    return None;
                }
                Some(Err(WsError::Capacity(error))) => {
                    // The server sent more than we agreed to accept.
                    // tungstenite has already failed the connection.
                    self.close = Some(CloseReason::ResourceLimit);
                    return Some(Err(SessionStreamError::Protocol(error.to_string())));
                }
                Some(Err(_)) => {
                    self.close = Some(CloseReason::Network);
                    return None;
                }
            };
            return match message {
                Message::Text(text) => Some(
                    serde_json::from_str::<DeliveredFrame>(&text)
                        .map_err(|error| SessionStreamError::Protocol(error.to_string())),
                ),
                Message::Binary(_) => Some(Err(SessionStreamError::Protocol(
                    "session stream frames are text, got binary".into(),
                ))),
                Message::Close(frame) => {
                    self.close = Some(match frame {
                        Some(frame) => CloseReason::classify(u16::from(frame.code), &frame.reason),
                        // A close frame with no code is still an
                        // orderly close.
                        None => CloseReason::Normal,
                    });
                    self.finish_close_handshake().await;
                    None
                }
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            };
        }
    }

    /// Why the stream ended, once `recv` has returned `None`.
    pub fn close_reason(&self) -> Option<&CloseReason> {
        self.close.as_ref()
    }

    /// Answer the peer's close.
    ///
    /// tungstenite queues the close reply when it reads the peer's close
    /// frame, but only writes it on the *next* read. Returning straight
    /// away would leave the reply unsent and the server waiting out its
    /// own deadline for an answer that never comes. Bounded, so a peer
    /// that never finishes the handshake cannot hang the caller.
    async fn finish_close_handshake(&mut self) {
        let drain = async { while let Some(Ok(_)) = self.stream.next().await {} };
        if tokio::time::timeout(CLOSE_HANDSHAKE_TIMEOUT, drain)
            .await
            .is_err()
        {
            tracing::debug!("session stream peer did not finish the close handshake");
        }
    }
}

/// How long [`SessionStreamRx`] waits for a close handshake to finish.
const CLOSE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};

    #[test]
    fn close_codes_classify_into_the_reasons_a_runner_acts_on() {
        assert_eq!(CloseReason::classify(1000, "normal"), CloseReason::Normal);
        assert_eq!(
            CloseReason::classify(close_code::SUPERSEDED, "superseded"),
            CloseReason::Superseded
        );
        assert_eq!(
            CloseReason::classify(close_code::LEASE_GONE, "lease_gone"),
            CloseReason::LeaseGone
        );
        assert_eq!(
            CloseReason::classify(close_code::TIMEOUT, "timeout"),
            CloseReason::Timeout
        );
        assert_eq!(
            CloseReason::classify(close_code::RESOURCE_LIMIT, "resource_limit"),
            CloseReason::ResourceLimit
        );
        assert_eq!(
            CloseReason::classify(close_code::PROTOCOL_ERROR, "protocol_error"),
            CloseReason::ProtocolError
        );
        assert_eq!(
            CloseReason::classify(4999, "from the future"),
            CloseReason::Other {
                code: 4999,
                reason: "from the future".into()
            }
        );
    }

    #[test]
    fn the_code_round_trips_through_classification() {
        for code in [
            1000,
            close_code::SUPERSEDED,
            close_code::LEASE_GONE,
            close_code::TIMEOUT,
            close_code::RESOURCE_LIMIT,
            close_code::PROTOCOL_ERROR,
            4999,
        ] {
            assert_eq!(CloseReason::classify(code, "").code(), Some(code));
        }
        assert_eq!(CloseReason::Network.code(), None);
    }

    #[test]
    fn superseded_lease_gone_and_protocol_errors_are_terminal() {
        let terminal = [
            CloseReason::Superseded,
            CloseReason::LeaseGone,
            CloseReason::ProtocolError,
        ];
        let retryable = [
            CloseReason::Normal,
            CloseReason::Timeout,
            CloseReason::ResourceLimit,
            CloseReason::Network,
            CloseReason::Other {
                code: 4999,
                reason: String::new(),
            },
        ];
        for reason in terminal {
            assert!(!reason.is_reconnectable(), "{reason:?}");
            assert!(!SessionStreamError::Closed(reason).is_transient());
        }
        for reason in retryable {
            assert!(reason.is_reconnectable(), "{reason:?}");
            assert!(SessionStreamError::Closed(reason).is_transient());
        }
    }

    #[test]
    fn a_refused_upgrade_is_classified_like_an_http_status() {
        let classify = |status| match rejected(status) {
            SessionStreamError::Rejected {
                status: kept,
                error,
            } => {
                assert_eq!(kept, status, "the status is carried through");
                match error {
                    BridgeApiError::Unauthorized(_) => "unauthorized",
                    BridgeApiError::Permanent(_) => "permanent",
                    BridgeApiError::Transient(_) => "transient",
                    BridgeApiError::Protocol(_) => "protocol",
                    other => panic!("unexpected {other:?}"),
                }
            }
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(classify(401), "unauthorized");
        assert_eq!(classify(403), "permanent");
        assert_eq!(classify(404), "permanent");
        assert_eq!(classify(409), "permanent");
        assert_eq!(classify(429), "transient");
        assert_eq!(classify(503), "transient");
        assert_eq!(classify(302), "protocol");
        assert!(!rejected(409).is_transient());
        assert!(rejected(503).is_transient());
    }

    #[test]
    fn a_lost_lease_reads_the_same_at_connect_and_mid_stream() {
        // `Permanent` alone would lump 409 in with 403 and 404; the
        // status is what lets a runner stand down for the right reason.
        assert!(rejected(409).is_lease_gone());
        assert!(SessionStreamError::Closed(CloseReason::LeaseGone).is_lease_gone());
        for other in [
            rejected(401),
            rejected(403),
            rejected(404),
            SessionStreamError::Closed(CloseReason::Superseded),
            SessionStreamError::Closed(CloseReason::Network),
            SessionStreamError::Transport("reset".into()),
        ] {
            assert!(!other.is_lease_gone(), "{other:?}");
        }
    }

    #[test]
    fn options_are_validated_before_the_network() {
        SessionStreamOptions::new("wss://rc.example.com/v1/sessions/s/stream", "t")
            .validate()
            .expect("valid");
        assert!(SessionStreamOptions::new("https://rc.example.com", "t")
            .validate()
            .is_err());
        assert!(SessionStreamOptions::new("ws://127.0.0.1:1", "")
            .validate()
            .is_err());
        let mut zero = SessionStreamOptions::new("ws://127.0.0.1:1", "t");
        zero.max_frame_bytes = 0;
        assert!(zero.validate().is_err());
    }

    #[test]
    fn debug_output_never_carries_the_credential() {
        let options = SessionStreamOptions::new("wss://rc.example.com", "hunter2");
        let rendered = format!("{options:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    /// Join a stub server task, failing the test instead of hanging it
    /// when a close handshake never completes.
    async fn joined<T>(server: tokio::task::JoinHandle<T>) -> T {
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("the stub server finished; a close handshake did not stall")
            .expect("server task")
    }

    /// A one-shot WebSocket server that records the bearer it was given,
    /// echoes one frame back, then closes with `close_code`.
    // The header callback's `Result<Response, ErrorResponse>` is
    // tungstenite's `Callback` contract, not a type this crate chose.
    #[allow(clippy::result_large_err)]
    async fn echo_then_close(close_with: u16) -> (String, tokio::task::JoinHandle<Option<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            let mut bearer = None;
            let callback =
                |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
                    bearer = request
                        .headers()
                        .get(AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    Ok(response)
                };
            let mut socket = tokio_tungstenite::accept_hdr_async(tcp, callback)
                .await
                .expect("handshake");
            if let Some(Ok(message)) = socket.next().await {
                socket.send(message).await.expect("echo");
            }
            socket
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::from(close_with),
                    reason: Cow::Borrowed("bye"),
                })))
                .await
                .expect("close");
            // Let the client's close answer arrive.
            while let Some(Ok(_)) = socket.next().await {}
            bearer
        });
        (format!("ws://{address}/v1/sessions/sess_1/stream"), task)
    }

    #[tokio::test]
    async fn a_connected_stream_sends_the_bearer_and_reports_the_close_reason() {
        let (url, server) = echo_then_close(close_code::SUPERSEDED).await;
        let stream = SessionStream::connect(&SessionStreamOptions::new(url, "session-token"))
            .await
            .expect("connect");
        let (tx, mut rx) = stream.split();
        let sink: Arc<dyn SessionFrameSink> = Arc::new(tx);
        sink.send_frame(&SessionFrame::prompt("hello"))
            .await
            .expect("send");

        let echoed = rx.recv().await.expect("a frame").expect("parses");
        assert_eq!(echoed, SessionFrame::prompt("hello"));
        assert!(rx.recv().await.is_none(), "the server closed");
        assert_eq!(rx.close_reason(), Some(&CloseReason::Superseded));
        assert!(rx.recv().await.is_none(), "stays closed");

        // The close handshake completed from our side, so the server's
        // drain loop ends without the socket being dropped first.
        assert_eq!(
            joined(server).await.as_deref(),
            Some("Bearer session-token")
        );
    }

    #[tokio::test]
    async fn an_oversized_frame_is_refused_before_it_reaches_the_wire() {
        let (url, server) = echo_then_close(1000).await;
        let mut options = SessionStreamOptions::new(url, "t");
        options.max_frame_bytes = 64;
        let stream = SessionStream::connect(&options).await.expect("connect");
        let error = stream
            .send(&SessionFrame::prompt("x".repeat(128)))
            .await
            .expect_err("over the cap");
        assert!(
            matches!(error, SessionStreamError::Protocol(_)),
            "{error:?}"
        );
        // Still usable afterwards.
        stream
            .send(&SessionFrame::Cancel)
            .await
            .expect("a small frame still goes");
        stream.close().await.expect("close");
        joined(server).await;
    }

    #[tokio::test]
    async fn an_unreachable_ingress_is_a_transient_transport_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        drop(listener);
        let error = SessionStream::connect(&SessionStreamOptions::new(
            format!("ws://{address}/v1/sessions/s/stream"),
            "t",
        ))
        .await
        .expect_err("nothing is listening");
        assert!(
            matches!(error, SessionStreamError::Transport(_)),
            "{error:?}"
        );
        assert!(error.is_transient());
    }

    #[tokio::test]
    async fn a_refused_upgrade_surfaces_its_status() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut tcp, _) = listener.accept().await.expect("accept");
            let mut buffer = [0u8; 4096];
            let _ = tcp.read(&mut buffer).await;
            tcp.write_all(b"HTTP/1.1 409 Conflict\r\ncontent-length: 0\r\n\r\n")
                .await
                .expect("respond");
        });
        let error = SessionStream::connect(&SessionStreamOptions::new(
            format!("ws://{address}/v1/sessions/s/stream"),
            "t",
        ))
        .await
        .expect_err("refused");
        assert!(
            matches!(
                error,
                SessionStreamError::Rejected {
                    status: 409,
                    error: BridgeApiError::Permanent(_)
                }
            ),
            "{error:?}"
        );
        assert!(!error.is_transient());
        assert!(error.is_lease_gone());
        joined(server).await;
    }
}
