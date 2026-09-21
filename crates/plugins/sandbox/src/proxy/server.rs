//! The listeners — sandbox RFC §4.3, §7.
//!
//! Two protocols on two loopback ports, plus (on Unix) the same two on socket
//! files. The socket files are not a convenience: under bubblewrap's
//! `--unshare-net` the sandbox gets a *fresh* loopback, so the host's
//! `127.0.0.1:3128` is not reachable from inside it. A socket file crosses
//! the network namespace as an ordinary bind mount, and
//! [`crate::runtime::linux::bridge`] republishes it on the sandbox's
//! own loopback.
//!
//! ## Ports are ephemeral
//!
//! Bound on `127.0.0.1:0` and read back. Two Rebon sessions on one machine
//! must not fight over 3128, and a fixed port would also let anything else on
//! the box find the proxy by guessing.
//!
//! ## What a refusal looks like
//!
//! A blocked connection is answered with a 403 (or a SOCKS "not allowed by
//! ruleset" reply) rather than dropped. A dropped connection reaches the
//! model as a timeout, which reads like a broken network and is the sort of
//! thing it retries for a minute; a 403 whose body names the rule reaches it
//! as text it can act on.

use crate::proxy::policy::DomainPolicy;
use crate::proxy::{http, socks};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// Where a running proxy is listening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyEndpoints {
    pub http_port: u16,
    pub socks_port: u16,
    pub http_socket: Option<std::path::PathBuf>,
    pub socks_socket: Option<std::path::PathBuf>,
}

/// A running proxy. Dropping it stops accepting and closes the listeners.
pub struct ProxyHandle {
    endpoints: ProxyEndpoints,
    shutdown: watch::Sender<bool>,
}

impl ProxyHandle {
    pub fn endpoints(&self) -> &ProxyEndpoints {
        &self.endpoints
    }

    /// Stop accepting. Connections already established finish on their own —
    /// killing a download half way through to tidy up would lose the
    /// command's work for no security gain, since the decision was already
    /// made when the connection was opened.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("could not start the sandbox proxy: {0}")]
    Bind(#[source] std::io::Error),
}

/// Start the proxy for one session.
///
/// `socket_dir`, when given, is where the Unix socket files go — required on
/// Linux, ignored elsewhere. It should be a directory only this session can
/// write to; the sockets inherit its permissions.
pub async fn start(
    policy: DomainPolicy,
    socket_dir: Option<&std::path::Path>,
) -> Result<ProxyHandle, ProxyError> {
    let http = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(ProxyError::Bind)?;
    let socks = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(ProxyError::Bind)?;
    assemble(policy, socket_dir, http, socks)
}

/// Start the proxy from **synchronous** code running on a Tokio runtime.
///
/// The session is assembled in a constructor — `resolve_sandbox_policy` is
/// not async and cannot become async without turning every caller async — but
/// the ports have to be known before the first command is wrapped, because
/// they go into the environment the sandbox hands the child. Binding is the
/// only part that must finish first, and binding does not block; the accept
/// loops go on the runtime.
pub fn start_blocking(
    policy: DomainPolicy,
    socket_dir: Option<&std::path::Path>,
    handle: &tokio::runtime::Handle,
) -> Result<ProxyHandle, ProxyError> {
    // Both `from_std` and `tokio::spawn` need a runtime in scope on this
    // thread; the guard is what puts one there.
    let _guard = handle.enter();

    let http = std_listener()?;
    let socks = std_listener()?;
    assemble(policy, socket_dir, http, socks)
}

fn std_listener() -> Result<TcpListener, ProxyError> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(ProxyError::Bind)?;
    listener.set_nonblocking(true).map_err(ProxyError::Bind)?;
    TcpListener::from_std(listener).map_err(ProxyError::Bind)
}

fn assemble(
    policy: DomainPolicy,
    socket_dir: Option<&std::path::Path>,
    http_listener: TcpListener,
    socks_listener: TcpListener,
) -> Result<ProxyHandle, ProxyError> {
    let policy = Arc::new(policy);
    let (shutdown, watcher) = watch::channel(false);

    let http_port = http_listener.local_addr().map_err(ProxyError::Bind)?.port();
    let socks_port = socks_listener
        .local_addr()
        .map_err(ProxyError::Bind)?
        .port();

    spawn_tcp_acceptor(
        http_listener,
        policy.clone(),
        watcher.clone(),
        Protocol::Http,
    );
    spawn_tcp_acceptor(
        socks_listener,
        policy.clone(),
        watcher.clone(),
        Protocol::Socks,
    );

    let (http_socket, socks_socket) = unix::start_sockets(socket_dir, &policy, &watcher)?;

    Ok(ProxyHandle {
        endpoints: ProxyEndpoints {
            http_port,
            socks_port,
            http_socket,
            socks_socket,
        },
        shutdown,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Protocol {
    Http,
    Socks,
}

fn spawn_tcp_acceptor(
    listener: TcpListener,
    policy: Arc<DomainPolicy>,
    mut watcher: watch::Receiver<bool>,
    protocol: Protocol,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = watcher.changed() => {
                    if *watcher.borrow() {
                        return;
                    }
                }
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let policy = policy.clone();
                    tokio::spawn(async move {
                        // One bad client must not take the proxy down with
                        // it, and there is nobody to report to.
                        if let Err(error) = serve(stream, policy, protocol).await {
                            tracing::debug!(?protocol, "sandbox proxy connection ended: {error}");
                        }
                    });
                }
            }
        }
    });
}

/// Send a refusal and let the client finish talking before hanging up.
///
/// Closing with unread bytes still arriving is an *abortive* close: the OS
/// sends RST, and on Windows that discards whatever is already in the peer's
/// receive buffer — including the response just written. The client then sees
/// a connection reset instead of the 403 explaining which rule refused it,
/// which is the entire value of answering rather than dropping.
///
/// Bounded on both time and bytes: a client that keeps talking forever must
/// not hold the connection open, and the drain exists for politeness, not to
/// receive anything.
async fn respond<S>(client: &mut S, response: &[u8]) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    client.write_all(response).await?;
    client.flush().await?;

    let mut scratch = [0u8; 4096];
    let mut drained = 0usize;
    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
        while drained < 1024 * 1024 {
            match client.read(&mut scratch).await {
                Ok(0) | Err(_) => break,
                Ok(read) => drained += read,
            }
        }
    })
    .await;
    Ok(())
}

pub(crate) async fn serve<S>(
    stream: S,
    policy: Arc<DomainPolicy>,
    protocol: Protocol,
) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match protocol {
        Protocol::Http => serve_http(stream, policy).await,
        Protocol::Socks => serve_socks(stream, policy).await,
    }
}

async fn serve_http<S>(mut client: S, policy: Arc<DomainPolicy>) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut buffer = Vec::new();
    let head_length = loop {
        if let Some(end) = http::head_end(&buffer) {
            break end;
        }
        if buffer.len() > http::MAX_HEAD_BYTES {
            respond(
                &mut client,
                &http::bad_request_response("the request head is too large"),
            )
            .await?;
            return Ok(());
        }
        let mut chunk = [0u8; 4096];
        let read = client.read(&mut chunk).await?;
        if read == 0 {
            // The client hung up mid-head; nothing to answer.
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let destination = match http::parse(&buffer[..head_length]) {
        Ok(destination) => destination,
        Err(error) => {
            respond(&mut client, &http::bad_request_response(&error.to_string())).await?;
            return Ok(());
        }
    };

    let verdict = policy.decide(destination.host());
    if let Some(reason) = verdict.reason() {
        tracing::info!(host = destination.host(), "sandbox proxy refused: {reason}");
        respond(&mut client, &http::forbidden_response(reason)).await?;
        return Ok(());
    }

    let upstream = match TcpStream::connect((destination.host(), destination.port())).await {
        Ok(upstream) => upstream,
        Err(error) => {
            respond(
                &mut client,
                &http::bad_request_response(&format!(
                    "could not reach {}:{} — {error}",
                    destination.host(),
                    destination.port()
                )),
            )
            .await?;
            return Ok(());
        }
    };
    let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);

    match destination {
        http::Destination::Tunnel { .. } => {
            client.write_all(http::TUNNEL_ESTABLISHED).await?;
            // Anything the client sent after the head belongs to the tunnel.
            if buffer.len() > head_length {
                upstream_write.write_all(&buffer[head_length..]).await?;
            }
        }
        http::Destination::Forward { ref head, .. } => {
            upstream_write.write_all(head).await?;
            if buffer.len() > head_length {
                upstream_write.write_all(&buffer[head_length..]).await?;
            }
        }
    }

    let (mut client_read, mut client_write) = tokio::io::split(client);
    let to_upstream = tokio::io::copy(&mut client_read, &mut upstream_write);
    let to_client = tokio::io::copy(&mut upstream_read, &mut client_write);
    // Both directions end when either side closes; a half-open connection
    // that never finishes is the difference between a command that returns
    // and one that hangs until its timeout.
    tokio::select! {
        _ = to_upstream => {}
        _ = to_client => {}
    }
    Ok(())
}

async fn serve_socks<S>(mut client: S, policy: Arc<DomainPolicy>) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut header = [0u8; 2];
    client.read_exact(&mut header).await?;
    let count = header[1] as usize;
    let mut methods = vec![0u8; count];
    client.read_exact(&mut methods).await?;

    let mut greeting = header.to_vec();
    greeting.extend_from_slice(&methods);
    if socks::parse_greeting(&greeting).is_err() {
        client
            .write_all(&[socks::VERSION, socks::METHOD_UNACCEPTABLE])
            .await?;
        return Ok(());
    }
    client
        .write_all(&[socks::VERSION, socks::METHOD_NONE])
        .await?;

    let mut prefix = [0u8; 4];
    client.read_exact(&mut prefix).await?;
    // The domain form's length byte has to be read before the rest can be
    // sized, and it is the one field a client controls the length with.
    let first = if prefix[3] == 0x03 {
        let mut length = [0u8; 1];
        client.read_exact(&mut length).await?;
        Some(length[0])
    } else {
        None
    };
    let Ok(remaining) = socks::address_length(prefix[3], first) else {
        client
            .write_all(&socks::reply(socks::REPLY_COMMAND_NOT_SUPPORTED))
            .await?;
        return Ok(());
    };
    let already_read = usize::from(first.is_some());
    let mut rest = vec![0u8; remaining - already_read];
    client.read_exact(&mut rest).await?;

    let mut request_bytes = prefix.to_vec();
    if let Some(length) = first {
        request_bytes.push(length);
    }
    request_bytes.extend_from_slice(&rest);

    let request = match socks::parse_request(&request_bytes) {
        Ok(request) => request,
        Err(_) => {
            client
                .write_all(&socks::reply(socks::REPLY_COMMAND_NOT_SUPPORTED))
                .await?;
            return Ok(());
        }
    };

    let verdict = policy.decide(&request.host);
    if let Some(reason) = verdict.reason() {
        tracing::info!(host = %request.host, "sandbox proxy refused: {reason}");
        client
            .write_all(&socks::reply(socks::REPLY_NOT_ALLOWED))
            .await?;
        return Ok(());
    }

    let upstream = match TcpStream::connect((request.host.as_str(), request.port)).await {
        Ok(upstream) => upstream,
        Err(_) => {
            client
                .write_all(&socks::reply(socks::REPLY_HOST_UNREACHABLE))
                .await?;
            return Ok(());
        }
    };
    client
        .write_all(&socks::reply(socks::REPLY_SUCCESS))
        .await?;

    let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);
    let (mut client_read, mut client_write) = tokio::io::split(client);
    tokio::select! {
        _ = tokio::io::copy(&mut client_read, &mut upstream_write) => {}
        _ = tokio::io::copy(&mut upstream_read, &mut client_write) => {}
    }
    Ok(())
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::path::{Path, PathBuf};

    pub(super) fn start_sockets(
        socket_dir: Option<&Path>,
        policy: &Arc<DomainPolicy>,
        watcher: &watch::Receiver<bool>,
    ) -> Result<(Option<PathBuf>, Option<PathBuf>), ProxyError> {
        let Some(directory) = socket_dir else {
            return Ok((None, None));
        };
        std::fs::create_dir_all(directory).map_err(ProxyError::Bind)?;

        let http_path = directory.join("http-proxy.sock");
        let socks_path = directory.join("socks-proxy.sock");
        // A leftover file from a session that did not shut down cleanly
        // makes `bind` fail with EADDRINUSE; nothing else owns these names.
        let _ = std::fs::remove_file(&http_path);
        let _ = std::fs::remove_file(&socks_path);

        spawn(&http_path, policy.clone(), watcher.clone(), Protocol::Http)?;
        spawn(
            &socks_path,
            policy.clone(),
            watcher.clone(),
            Protocol::Socks,
        )?;
        Ok((Some(http_path), Some(socks_path)))
    }

    fn spawn(
        path: &Path,
        policy: Arc<DomainPolicy>,
        mut watcher: watch::Receiver<bool>,
        protocol: Protocol,
    ) -> Result<(), ProxyError> {
        let listener = tokio::net::UnixListener::bind(path).map_err(ProxyError::Bind)?;
        let path = path.to_path_buf();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = watcher.changed() => {
                        if *watcher.borrow() {
                            // The socket file outlives the listener unless
                            // it is removed, and the next session would then
                            // fail to bind its own.
                            let _ = std::fs::remove_file(&path);
                            return;
                        }
                    }
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { continue };
                        let policy = policy.clone();
                        tokio::spawn(async move {
                            if let Err(error) = serve(stream, policy, protocol).await {
                                tracing::debug!(?protocol, "sandbox proxy connection ended: {error}");
                            }
                        });
                    }
                }
            }
        });
        Ok(())
    }
}

#[cfg(not(unix))]
mod unix {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Windows and macOS reach the proxy over loopback, which the sandbox
    /// shares with the host — there is no network namespace in the way, so
    /// there is nothing for a socket file to solve.
    pub(super) fn start_sockets(
        _socket_dir: Option<&Path>,
        _policy: &Arc<DomainPolicy>,
        _watcher: &watch::Receiver<bool>,
    ) -> Result<(Option<PathBuf>, Option<PathBuf>), ProxyError> {
        Ok((None, None))
    }
}
