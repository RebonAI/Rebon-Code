//! Transport-agnostic Computer Use IPC runtime.
//!
//! The wire protocol (newline-delimited JSON envelopes, token authentication,
//! bounded lines, per-request timeouts) is identical on every platform; only
//! the byte transport differs. Unix builds serve a mode-0600 Unix domain
//! socket, Windows builds serve a local-only named pipe whose name travels in
//! the same environment variable as the socket path.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, Semaphore};

use crate::runtime::{
    Action, ActionResponse, Backend, ComputerUseError, ErrorCode, Request, RequestEnvelope,
    ResponseEnvelope, MAX_LINE_BYTES, MAX_TYPE_CHARS, MAX_WAIT_MS, PROTOCOL_VERSION,
};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as transport;
#[cfg(windows)]
use windows as transport;

const IPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(70);
const IPC_READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONNECTIONS: usize = 8;

pub struct Runtime<B> {
    backend: Arc<Mutex<B>>,
    token: Arc<str>,
}

impl<B> Clone for Runtime<B> {
    fn clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
            token: Arc::clone(&self.token),
        }
    }
}

impl<B: Backend> Runtime<B> {
    pub fn new(token: impl Into<String>, backend: B) -> Result<Self, ComputerUseError> {
        let token = token.into();
        if token.is_empty() {
            return Err(ComputerUseError::new(
                ErrorCode::InvalidRequest,
                "authentication token must not be empty",
                false,
            ));
        }
        Ok(Self {
            backend: Arc::new(Mutex::new(backend)),
            token: Arc::from(token),
        })
    }

    async fn dispatch(&self, envelope: RequestEnvelope) -> ResponseEnvelope {
        if envelope.version != PROTOCOL_VERSION {
            return ResponseEnvelope::failure(
                envelope.id,
                ComputerUseError::new(
                    ErrorCode::ProtocolMismatch,
                    format!(
                        "protocol version {} is unsupported; expected {PROTOCOL_VERSION}",
                        envelope.version
                    ),
                    false,
                ),
            );
        }
        if !tokens_equal(envelope.token.as_bytes(), self.token.as_bytes()) {
            return ResponseEnvelope::failure(
                envelope.id,
                ComputerUseError::new(
                    ErrorCode::Unauthorized,
                    "invalid authentication token",
                    false,
                ),
            );
        }

        let mut backend = self.backend.lock().await;
        let result = match envelope.request {
            Request::Status => backend.status().map(|status| ActionResponse {
                status,
                screenshot: None,
            }),
            Request::Action {
                action,
                target_epoch,
            } => validate_action(&action).and_then(|()| {
                let selecting = matches!(&action, Action::Observe { target: Some(_) });
                if selecting {
                    if target_epoch.is_some() {
                        return Err(ComputerUseError::new(
                            ErrorCode::InvalidRequest,
                            "target selection must not include a target epoch",
                            false,
                        ));
                    }
                } else {
                    let status = backend.status()?;
                    if target_epoch != Some(status.target_epoch) || status.target.is_none() {
                        return Err(ComputerUseError::new(
                            ErrorCode::TargetInvalid,
                            "the Computer Use target changed; observe it again",
                            true,
                        ));
                    }
                }
                backend.execute(action)
            }),
        };
        match result {
            Ok(result) => ResponseEnvelope::success(envelope.id, result),
            Err(error) => ResponseEnvelope::failure(envelope.id, error),
        }
    }
}

fn validate_action(action: &Action) -> Result<(), ComputerUseError> {
    match action {
        Action::Type { text } if text.chars().count() > MAX_TYPE_CHARS => Err(
            ComputerUseError::new(ErrorCode::InvalidRequest, "text input is too long", false),
        ),
        Action::Wait { duration_ms } if *duration_ms > MAX_WAIT_MS => Err(ComputerUseError::new(
            ErrorCode::InvalidRequest,
            format!("wait duration exceeds {MAX_WAIT_MS}ms"),
            false,
        )),
        Action::Key { key, modifiers }
            if key.is_empty() || key.len() > 64 || modifiers.len() > 5 =>
        {
            Err(ComputerUseError::new(
                ErrorCode::InvalidRequest,
                "invalid key action",
                false,
            ))
        }
        _ => Ok(()),
    }
}

/// Whether a served endpoint currently exists at `path`.
///
/// On Unix this means a socket inode; on Windows, a named pipe with a
/// listening server instance. Neither probe connects to the endpoint.
pub fn endpoint_ready(path: &Path) -> bool {
    transport::endpoint_ready(path)
}

/// Serves authenticated, newline-delimited JSON on the platform's private
/// endpoint: a mode-0600 Unix socket, or a local-only Windows named pipe.
pub async fn serve<B: Backend>(
    endpoint: impl AsRef<Path>,
    token: impl Into<String>,
    backend: B,
) -> Result<(), ComputerUseError> {
    let mut listener = transport::Listener::bind(endpoint.as_ref())?;
    let runtime = Runtime::new(token, backend)?;
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    loop {
        let stream = listener.accept().await?;
        let permit = Arc::clone(&connections)
            .acquire_owned()
            .await
            .map_err(|_| service_closed())?;
        let runtime = runtime.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = serve_connection(stream, runtime).await;
        });
    }
}

async fn serve_connection<S, B>(stream: S, runtime: Runtime<B>) -> Result<(), ComputerUseError>
where
    S: AsyncRead + AsyncWrite + Send + Unpin,
    B: Backend,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut line = Vec::new();
    loop {
        let bytes =
            tokio::time::timeout(IPC_READ_TIMEOUT, read_bounded_line(&mut reader, &mut line))
                .await
                .map_err(|_| {
                    ComputerUseError::new(
                        ErrorCode::InvalidRequest,
                        "Computer Use IPC request read timed out",
                        false,
                    )
                })??;
        if bytes == 0 {
            return Ok(());
        }
        let response = match serde_json::from_slice::<RequestEnvelope>(&line) {
            Ok(envelope) => runtime.dispatch(envelope).await,
            Err(error) => ResponseEnvelope::failure(
                0,
                ComputerUseError::new(
                    ErrorCode::InvalidRequest,
                    format!("invalid request JSON: {error}"),
                    false,
                ),
            ),
        };
        let mut encoded = serde_json::to_vec(&response).map_err(|error| {
            ComputerUseError::new(
                ErrorCode::Internal,
                format!("failed to encode response: {error}"),
                false,
            )
        })?;
        encoded.push(b'\n');
        tokio::time::timeout(IPC_READ_TIMEOUT, write_half.write_all(&encoded))
            .await
            .map_err(|_| {
                ComputerUseError::new(
                    ErrorCode::Internal,
                    "Computer Use IPC response write timed out",
                    true,
                )
            })?
            .map_err(io_error)?;
    }
}

async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> Result<usize, ComputerUseError> {
    line.clear();
    loop {
        let buffer = reader.fill_buf().await.map_err(io_error)?;
        if buffer.is_empty() {
            return Ok(line.len());
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        if line.len().saturating_add(consumed) > MAX_LINE_BYTES {
            return Err(ComputerUseError::new(
                ErrorCode::InvalidRequest,
                "request line is too large",
                false,
            ));
        }
        line.extend_from_slice(&buffer[..consumed]);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(line.len());
        }
    }
}

fn service_closed() -> ComputerUseError {
    ComputerUseError::new(
        ErrorCode::Internal,
        "Computer Use IPC service is shutting down",
        true,
    )
}

fn io_error(error: std::io::Error) -> ComputerUseError {
    ComputerUseError::new(ErrorCode::Internal, format!("IPC error: {error}"), true)
}

fn tokens_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(a ^ b);
    }
    difference == 0
}

/// Small request/response client. Each call gets a fresh connection, while a
/// server connection itself supports any number of newline-delimited requests.
pub struct Client {
    endpoint: PathBuf,
    token: String,
    next_id: AtomicU64,
}

impl Client {
    pub fn new(endpoint: impl Into<PathBuf>, token: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            token: token.into(),
            next_id: AtomicU64::new(1),
        }
    }

    pub async fn request(&self, request: Request) -> Result<ActionResponse, ComputerUseError> {
        tokio::time::timeout(IPC_REQUEST_TIMEOUT, self.request_inner(request))
            .await
            .map_err(|_| {
                ComputerUseError::new(
                    ErrorCode::Internal,
                    "Computer Use IPC request timed out",
                    true,
                )
            })?
    }

    async fn request_inner(&self, request: Request) -> Result<ActionResponse, ComputerUseError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let envelope = RequestEnvelope {
            version: PROTOCOL_VERSION,
            id,
            token: self.token.clone(),
            request,
        };
        let mut stream = transport::connect(&self.endpoint).await?;
        let mut request = serde_json::to_vec(&envelope).map_err(|error| {
            ComputerUseError::new(ErrorCode::Internal, error.to_string(), false)
        })?;
        request.push(b'\n');
        stream.write_all(&request).await.map_err(io_error)?;

        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .await
            .map_err(io_error)?;
        let response: ResponseEnvelope = serde_json::from_str(&response).map_err(|error| {
            ComputerUseError::new(
                ErrorCode::Internal,
                format!("invalid server response: {error}"),
                false,
            )
        })?;
        if response.id != id {
            return Err(ComputerUseError::new(
                ErrorCode::Internal,
                "server response ID did not match request",
                false,
            ));
        }
        match (response.result, response.error) {
            (Some(result), None) => Ok(result),
            (None, Some(error)) => Err(error),
            _ => Err(ComputerUseError::new(
                ErrorCode::Internal,
                "server response contained an invalid result/error combination",
                false,
            )),
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::runtime::{PermissionStatus, Rect, ServiceState, StatusResponse, TargetWindow};

    pub(crate) struct MockBackend {
        pub(crate) calls: usize,
        pub(crate) target_epoch: u64,
        pub(crate) active: bool,
    }

    impl Backend for MockBackend {
        fn status(&mut self) -> Result<StatusResponse, ComputerUseError> {
            self.calls += 1;
            Ok(StatusResponse {
                state: if self.active {
                    ServiceState::Active
                } else {
                    ServiceState::WaitingForTarget
                },
                permissions: PermissionStatus::default(),
                target_epoch: self.target_epoch,
                target: self.active.then(|| TargetWindow {
                    id: 7,
                    owner_pid: 42,
                    owner_name: "Target".into(),
                    title: None,
                    frame: Rect {
                        x: 0.0,
                        y: 0.0,
                        width: 800.0,
                        height: 600.0,
                    },
                }),
            })
        }
        fn execute(&mut self, _action: Action) -> Result<ActionResponse, ComputerUseError> {
            let status = self.status()?;
            Ok(ActionResponse {
                status,
                screenshot: None,
            })
        }
        fn request_permissions(&mut self) -> Result<StatusResponse, ComputerUseError> {
            self.status()
        }
        fn pause(&mut self) -> Result<(), ComputerUseError> {
            Ok(())
        }
        fn resume(&mut self) -> Result<(), ComputerUseError> {
            Ok(())
        }
        fn stop(&mut self) -> Result<(), ComputerUseError> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::MockBackend;
    use super::*;
    use crate::runtime::ServiceState;

    #[test]
    fn token_comparison_checks_content_and_length() {
        assert!(tokens_equal(b"secret", b"secret"));
        assert!(!tokens_equal(b"secret", b"Secret"));
        assert!(!tokens_equal(b"secret", b"secret-extra"));
        assert!(!tokens_equal(b"", b"x"));
    }

    #[tokio::test]
    async fn bounded_reader_rejects_oversized_lines_before_unbounded_growth() {
        let mut bytes = vec![b'a'; MAX_LINE_BYTES + 1];
        bytes.push(b'\n');
        let mut reader = BufReader::new(bytes.as_slice());
        let mut line = Vec::new();
        let error = read_bounded_line(&mut reader, &mut line).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(line.len() <= MAX_LINE_BYTES);
    }

    #[tokio::test]
    async fn runtime_rejects_bad_token_and_illegal_action() {
        let runtime = Runtime::new(
            "right",
            MockBackend {
                calls: 0,
                target_epoch: 0,
                active: false,
            },
        )
        .unwrap();
        let bad = runtime
            .dispatch(RequestEnvelope {
                version: PROTOCOL_VERSION,
                id: 1,
                token: "wrong".into(),
                request: Request::Status,
            })
            .await;
        assert_eq!(bad.error.unwrap().code, ErrorCode::Unauthorized);

        let invalid = runtime
            .dispatch(RequestEnvelope {
                version: PROTOCOL_VERSION,
                id: 2,
                token: "right".into(),
                request: Request::Action {
                    action: Action::Wait {
                        duration_ms: MAX_WAIT_MS + 1,
                    },
                    target_epoch: None,
                },
            })
            .await;
        assert_eq!(invalid.error.unwrap().code, ErrorCode::InvalidRequest);
    }

    #[tokio::test]
    async fn runtime_rejects_actions_from_an_old_target_epoch() {
        let runtime = Runtime::new(
            "right",
            MockBackend {
                calls: 0,
                target_epoch: 9,
                active: true,
            },
        )
        .unwrap();
        let stale = runtime
            .dispatch(RequestEnvelope {
                version: PROTOCOL_VERSION,
                id: 3,
                token: "right".into(),
                request: Request::Action {
                    action: Action::Observe { target: None },
                    target_epoch: Some(8),
                },
            })
            .await;
        assert_eq!(stale.error.unwrap().code, ErrorCode::TargetInvalid);

        let current = runtime
            .dispatch(RequestEnvelope {
                version: PROTOCOL_VERSION,
                id: 4,
                token: "right".into(),
                request: Request::Action {
                    action: Action::Observe { target: None },
                    target_epoch: Some(9),
                },
            })
            .await;
        assert!(current.error.is_none());
    }

    #[tokio::test]
    async fn endpoint_round_trips_through_platform_transport() {
        let (endpoint, _guard) = transport_test_endpoint();
        let server_endpoint = endpoint.clone();
        let server = tokio::spawn(async move {
            serve(
                server_endpoint,
                "secret",
                MockBackend {
                    calls: 0,
                    target_epoch: 0,
                    active: false,
                },
            )
            .await
        });
        for _ in 0..500 {
            if endpoint_ready(&endpoint) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(endpoint_ready(&endpoint));
        let response = Client::new(&endpoint, "secret")
            .request(Request::Status)
            .await
            .unwrap();
        assert_eq!(response.status.state, ServiceState::WaitingForTarget);
        server.abort();
    }

    #[cfg(unix)]
    fn transport_test_endpoint() -> (PathBuf, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime.sock");
        (path, directory)
    }

    #[cfg(windows)]
    fn transport_test_endpoint() -> (PathBuf, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let name = format!(
            r"\\.\pipe\rebon-cu-test-{}-{}",
            std::process::id(),
            directory
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .replace(['\\', '/'], "-")
        );
        (PathBuf::from(name), directory)
    }
}
