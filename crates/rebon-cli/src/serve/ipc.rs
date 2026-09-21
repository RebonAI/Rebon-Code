//! The same ACP mux, over a local IPC endpoint.
//!
//! `rebon serve` puts a browser in front of the ACP server; a browser needs
//! HTTP, so the TCP listener stays. But a native client on this machine — the
//! desktop app, a TUI, an editor plugin, a script — has no use for the HTTP
//! shell around the socket, and no reason to make the agent reachable over the
//! loopback network to get at it. This is the other door: a Unix-domain socket
//! or a Windows named pipe, carrying the same ACP JSON-RPC the WebSocket
//! carries, spoken as newline-delimited JSON (ACP's native stdio framing).
//!
//! Every connection is one more [`super::mux::AcpMux`] client, exactly like a
//! tab: turns are visible across all of them, `session/update` is broadcast to
//! all of them, and a permission prompt can be answered by whichever gets to it
//! first. Nothing here knows how a turn runs.
//!
//! **The endpoint is not, by itself, the security boundary.** The Unix socket
//! is mode 0600, but a Windows named pipe created with the default security
//! descriptor grants read access to `Everyone` — enough for another local
//! account to sit and watch a session stream. So a client presents the run's
//! token as its first line before it becomes a mux client at all. That keeps
//! one rule for both platforms and makes the pipe's DACL a second line of
//! defence rather than the only one.
//!
//! The transport mechanics (stale-socket handling, `first_pipe_instance`,
//! `reject_remote_clients`) follow the local IPC in
//! `rebon-plugin-computer-use`'s `runtime::ipc`, which
//! solved the same problem for a different payload.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use super::ServeContext;

/// How long a connection has to present its token before it is dropped. A
/// client that opens the endpoint and says nothing must not hold a slot.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The endpoint used when `--ipc` is given without a path.
///
/// Keyed by the HTTP port so several servers can run at once without agreeing
/// on anything: the port is already how the user tells them apart, and it is
/// printed on stderr next to the URL.
pub fn default_endpoint(port: u16) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\.\pipe\rebon-serve-{port}"))
    }
    #[cfg(unix)]
    {
        rebon_session::config_home::default_config_home_dir()
            .join("run")
            .join(format!("serve-{port}.sock"))
    }
}

/// A bound IPC endpoint. Dropping it releases the endpoint's name.
pub struct IpcListener {
    inner: platform::Listener,
    endpoint: PathBuf,
}

/// Written by hand rather than derived: the endpoint is the whole of what
/// identifies a listener, and `platform::Listener` is a different type per
/// target, so deriving would make this impl's existence depend on which
/// platform you happened to compile on. That is exactly how it went missing
/// — the `#[cfg(unix)]` test below calls `unwrap_err()`, which needs `T:
/// Debug`, so the test only ever compiled on Windows and CI's macOS runner
/// was the first thing to try it.
impl std::fmt::Debug for IpcListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcListener")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl IpcListener {
    pub fn bind(endpoint: &Path) -> anyhow::Result<Self> {
        let inner = platform::Listener::bind(endpoint)
            .with_context(|| format!("failed to bind the IPC endpoint {}", endpoint.display()))?;
        Ok(Self {
            inner,
            endpoint: endpoint.to_path_buf(),
        })
    }

    pub async fn accept(&mut self) -> anyhow::Result<platform::Stream> {
        self.inner.accept().await
    }

    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }
}

impl Drop for IpcListener {
    fn drop(&mut self) {
        platform::cleanup(&self.endpoint);
    }
}

/// Accept connections until the endpoint fails. Each one runs as its own task,
/// so a client that stalls cannot hold up the next.
pub async fn accept_loop(mut listener: IpcListener, context: Arc<ServeContext>) {
    loop {
        match listener.accept().await {
            Ok(stream) => {
                let context = context.clone();
                tokio::spawn(async move {
                    if let Err(err) = serve_connection(stream, context).await {
                        tracing::debug!(error = %err, "rebon serve: IPC connection ended");
                    }
                });
            }
            Err(err) => {
                tracing::warn!(error = %err, "rebon serve: IPC accept failed; endpoint closed");
                return;
            }
        }
    }
}

/// Drive one IPC client: authenticate it, then splice it into the mux.
pub async fn serve_connection<S>(stream: S, context: Arc<ServeContext>) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(Box::pin(stream));
    let mut lines = BufReader::new(reader).lines();

    if let Err(err) = handshake(&mut lines, &context).await {
        // Say why before hanging up: a rejected client would otherwise see an
        // unexplained disconnect, and this line can never be mistaken for an
        // ACP message because the stream ends right after it.
        let refusal = serde_json::json!({ "error": err.to_string() });
        let _ = writer.write_all(format!("{refusal}\n").as_bytes()).await;
        let _ = writer.flush().await;
        return Err(err);
    }

    let (client, mut outbound) = context.mux.connect();
    tracing::info!(
        client,
        clients = context.mux.client_count(),
        "rebon serve: IPC client connected"
    );

    let writer_task = tokio::spawn(async move {
        while let Some(text) = outbound.recv().await {
            if writer.write_all(text.as_bytes()).await.is_err()
                || writer.write_all(b"\n").await.is_err()
                || writer.flush().await.is_err()
            {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });

    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let line = line.trim();
                if !line.is_empty() {
                    context.mux.on_client_message(client, line);
                }
            }
            Ok(None) => break,
            Err(err) => {
                tracing::debug!(client, error = %err, "rebon serve: IPC read failed");
                break;
            }
        }
    }

    context.mux.disconnect(client);
    writer_task.abort();
    tracing::info!(
        client,
        clients = context.mux.client_count(),
        "rebon serve: IPC client disconnected"
    );
    Ok(())
}

/// Read and check the opening `{"token": "..."}` line.
async fn handshake<R>(
    lines: &mut tokio::io::Lines<BufReader<R>>,
    context: &ServeContext,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
{
    let line = tokio::time::timeout(HANDSHAKE_TIMEOUT, lines.next_line())
        .await
        .map_err(|_| anyhow::anyhow!("no handshake within {HANDSHAKE_TIMEOUT:?}"))?
        .context("handshake read failed")?
        .ok_or_else(|| anyhow::anyhow!("closed before the handshake"))?;

    let presented = handshake_token(&line)
        .ok_or_else(|| anyhow::anyhow!("the first line must be a JSON object with a token"))?;

    if !context.guard.token_matches(&presented) {
        bail!("invalid token");
    }
    Ok(())
}

/// The token a handshake line presents, if it presents one.
///
/// Deliberately narrow: only an object with a string `token`. An ACP request
/// is not a handshake, so a client that skips authentication and opens with
/// `initialize` is refused rather than let through.
fn handshake_token(line: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    value
        .get("token")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[cfg(unix)]
mod platform {
    use std::path::Path;

    use anyhow::Context;
    use tokio::net::{UnixListener, UnixStream};

    pub type Stream = UnixStream;

    pub struct Listener(UnixListener);

    impl Listener {
        pub fn bind(path: &Path) -> anyhow::Result<Self> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "could not create the endpoint directory {}",
                        parent.display()
                    )
                })?;
            }
            remove_stale_socket(path)?;
            let listener = UnixListener::bind(path).context("bind failed")?;
            // Bound first, then narrowed: between the two the socket exists with
            // the process umask, which is why the parent directory is the one
            // under the user's own config home rather than a shared /tmp.
            set_owner_only(path)?;
            Ok(Self(listener))
        }

        pub async fn accept(&mut self) -> anyhow::Result<Stream> {
            let (stream, _) = self.0.accept().await.context("accept failed")?;
            Ok(stream)
        }
    }

    pub fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    /// Remove a socket left behind by a process that did not exit cleanly.
    /// Anything at that path which is *not* a socket is someone else's file:
    /// refuse rather than delete it.
    fn remove_stale_socket(path: &Path) -> anyhow::Result<()> {
        use std::os::unix::fs::FileTypeExt;

        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err).context("could not inspect the endpoint path"),
        };
        if !metadata.file_type().is_socket() {
            anyhow::bail!(
                "{} exists and is not a socket; refusing to replace it",
                path.display()
            );
        }
        std::fs::remove_file(path).context("could not remove the stale socket")
    }

    fn set_owner_only(path: &Path) -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .context("could not restrict the socket to its owner")
    }
}

#[cfg(windows)]
mod platform {
    use std::path::Path;

    use anyhow::Context;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    pub type Stream = NamedPipeServer;

    const PIPE_PREFIX: &str = r"\\.\pipe\";

    pub struct Listener {
        name: String,
        /// The instance the next `accept` will hand out. One is always bound so
        /// the pipe name never blinks out of existence between connections.
        next: Option<NamedPipeServer>,
    }

    impl Listener {
        pub fn bind(path: &Path) -> anyhow::Result<Self> {
            let name = pipe_name(path);
            let first = ServerOptions::new()
                // Fail loudly if something already owns this name instead of
                // silently splitting clients between two servers.
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)
                .context("create failed")?;
            Ok(Self {
                name,
                next: Some(first),
            })
        }

        pub async fn accept(&mut self) -> anyhow::Result<Stream> {
            let server = match self.next.take() {
                Some(server) => server,
                None => self.new_instance().context("create failed")?,
            };
            server.connect().await.context("connect failed")?;
            self.next = self.new_instance().ok();
            Ok(server)
        }

        fn new_instance(&self) -> std::io::Result<NamedPipeServer> {
            ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.name)
        }
    }

    /// A named pipe disappears with the process that owns it.
    pub fn cleanup(_path: &Path) {}

    /// Accept both a full `\\.\pipe\name` and a bare name.
    pub(super) fn pipe_name(path: &Path) -> String {
        let raw = path.to_string_lossy().into_owned();
        if raw.starts_with(PIPE_PREFIX) {
            raw
        } else {
            format!("{PIPE_PREFIX}{}", raw.trim_start_matches('\\'))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handshake_line_must_carry_a_string_token() {
        let token = |line| handshake_token(line);
        assert_eq!(token(r#"{"token":"abc"}"#).as_deref(), Some("abc"));
        assert_eq!(token(r#"{"token":"abc","v":1}"#).as_deref(), Some("abc"));
        assert!(token(r#"{"token":42}"#).is_none());
        assert!(token(r#"{"nope":"abc"}"#).is_none());
        assert!(token("not json").is_none());
        assert!(token("").is_none());
        assert!(token(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#).is_none());
    }

    #[test]
    fn the_default_endpoint_is_keyed_by_port() {
        let a = default_endpoint(7700);
        let b = default_endpoint(7701);
        assert_ne!(a, b);
        assert!(a.to_string_lossy().contains("7700"));
    }

    #[cfg(windows)]
    #[test]
    fn the_default_windows_endpoint_is_a_pipe_name() {
        let endpoint = default_endpoint(7700);
        assert!(endpoint.to_string_lossy().starts_with(r"\\.\pipe\"));
    }

    #[cfg(unix)]
    #[test]
    fn the_default_unix_endpoint_is_a_socket_under_the_config_home() {
        let endpoint = default_endpoint(7700);
        assert_eq!(endpoint.extension().unwrap(), "sock");
        assert!(endpoint.parent().unwrap().ends_with("run"));
    }

    #[cfg(windows)]
    #[test]
    fn a_bare_pipe_name_is_prefixed_and_a_full_one_is_left_alone() {
        assert_eq!(
            super::platform::pipe_name(Path::new("rebon-test")),
            r"\\.\pipe\rebon-test"
        );
        assert_eq!(
            super::platform::pipe_name(Path::new(r"\\.\pipe\rebon-test")),
            r"\\.\pipe\rebon-test"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_bound_socket_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("serve.sock");
        let listener = IpcListener::bind(&endpoint).unwrap();

        let mode = std::fs::metadata(&endpoint).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        drop(listener);
        assert!(!endpoint.exists(), "dropping the listener frees the name");
    }

    #[cfg(unix)]
    #[test]
    fn binding_refuses_to_delete_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("not-a-socket");
        std::fs::write(&endpoint, b"important").unwrap();

        let err = IpcListener::bind(&endpoint).unwrap_err();

        assert!(format!("{err:#}").contains("not a socket"));
        assert_eq!(std::fs::read(&endpoint).unwrap(), b"important");
    }
}
