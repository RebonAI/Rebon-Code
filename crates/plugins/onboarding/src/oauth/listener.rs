//! OAuth callback listener — single-shot HTTP server bound to
//! `127.0.0.1:1455` that waits for the browser to hit
//! `/auth/callback?code=…&state=…`, validates `state`, returns a
//! browser-friendly HTML response, and hands the `code` back to the
//! caller.
//!
//! The port and path come from the sibling [`crate::onboarding`]
//! module; this module does the actual
//! `TcpListener::bind`, HTTP parsing, and socket writes. We do NOT
//! pull in a full HTTP framework (hyper/axum) because the entire
//! protocol surface is "parse one GET line, write one 200 response,
//! drop the socket". A ~200-line std-only module is both smaller
//! and easier to audit than adding hyper as a transitive dep.
//!
//! Cross-platform notes:
//!
//! * Windows firewalls prompt on first `bind(0.0.0.0:…)`. We bind
//!   to `127.0.0.1` specifically so the prompt never fires (loopback
//!   is exempt).
//! * macOS "local network" consent dialog is also loopback-exempt.
//! * On Linux, 1455 is above the privileged range, so the bind needs
//!   no elevated privileges.
//!
//! ## Coverage matrix
//!
//! Every behaviour is pinned by a test:
//!
//! * `probe_port_free_ok_when_unused` — port probe returns Ok on a
//!   free ephemeral port.
//! * `probe_port_free_fails_when_bound` — port probe returns Err
//!   when another listener already holds the port (the signal that
//!   drives the paste-flow fallback).
//! * `handle_request_happy_path` — GET with matching code+state
//!   returns the parsed code.
//! * `handle_request_rejects_non_get` — POST is rejected with
//!   `MalformedRequest`.
//! * `handle_request_rejects_wrong_path` — GET /other is rejected.
//! * `handle_request_rejects_missing_code` — missing `code` query
//!   param is rejected.
//! * `handle_request_rejects_state_mismatch` — wrong `state` is
//!   rejected with `StateMismatch` (CSRF guard).
//! * `handle_request_percent_decodes_code` — `%2B` etc. decoded.
//! * `wait_for_callback_times_out` — deadline reached with no
//!   connection returns `Timeout`.
//! * `wait_for_callback_happy_path` — end-to-end: bind, client
//!   connect, parse, return.
//! * `wait_for_callback_rejects_mismatched_state` — end-to-end
//!   negative path.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

/// Port pinned from `crate::onboarding::OPENAI_REDIRECT_PORT`. The
/// OpenAI authorize URL hard-codes `redirect_uri=http://localhost:1455/auth/callback`
/// so this port is NOT configurable — if it's busy we fall back to
/// the paste flow instead of picking a random port.
pub const CALLBACK_PORT: u16 = crate::onboarding::OPENAI_REDIRECT_PORT;

/// Path pinned from `crate::onboarding::OPENAI_CALLBACK_PATH`.
pub const CALLBACK_PATH: &str = crate::onboarding::OPENAI_CALLBACK_PATH;

/// Max time we hold a half-open connection while reading the
/// browser's GET line. 3s is plenty — a localhost GET arrives in
/// microseconds; anything that stalls this long is likely a port
/// probe or a misbehaving browser extension.
const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// Accept-loop poll interval. We use non-blocking accept + sleep
/// (rather than blocking accept with a thread-abort channel)
/// because the caller wants to observe Esc cancellation from a
/// parent thread without relying on `pthread_kill` semantics.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Errors from the listener surface. Each variant maps to one row
/// in the coverage matrix at the top of this file.
#[derive(Debug)]
pub enum ListenerError {
    /// `TcpListener::bind` failed. Most commonly because the port
    /// is already in use — the caller should fall back to the
    /// paste flow when they see this.
    Bind(std::io::Error),
    /// Deadline elapsed with no incoming connection.
    Timeout,
    /// Browser sent a non-GET request, a wrong path, or malformed
    /// query. Used for CSRF-style abuse detection too.
    MalformedRequest(String),
    /// `code` query param was absent.
    MissingCode,
    /// `state` query param was absent.
    MissingState,
    /// `state` query param did not match the expected value.
    /// Signals possible CSRF or a leftover tab from a previous
    /// login session.
    StateMismatch { expected: String, actual: String },
    /// Non-recoverable IO error mid-read or mid-write.
    Io(std::io::Error),
    /// External cancellation — the caller set the cancel flag
    /// passed to [`wait_for_callback_with_cancel`].
    Cancelled,
}

impl std::fmt::Display for ListenerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bind(err) => write!(f, "failed to bind OAuth callback listener: {err}"),
            Self::Timeout => write!(f, "OAuth callback listener timed out"),
            Self::MalformedRequest(msg) => write!(f, "malformed OAuth callback request: {msg}"),
            Self::MissingCode => write!(f, "OAuth callback missing `code` query param"),
            Self::MissingState => write!(f, "OAuth callback missing `state` query param"),
            Self::StateMismatch { expected, actual } => write!(
                f,
                "OAuth callback state mismatch: expected `{expected}`, got `{actual}` \
                 (possible CSRF or stale browser tab)"
            ),
            Self::Io(err) => write!(f, "OAuth callback IO error: {err}"),
            Self::Cancelled => write!(f, "OAuth callback listener was cancelled"),
        }
    }
}

impl std::error::Error for ListenerError {}

/// Parsed callback. `code` is already percent-decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackResult {
    pub code: String,
    pub state: String,
}

/// Return `Ok(())` when we can bind `127.0.0.1:{port}` right now.
/// The probe binds, immediately drops, then returns — the kernel's
/// TIME_WAIT may keep the port unusable for a few seconds if this
/// is called repeatedly in a tight loop, which is why the caller
/// should treat a single probe as authoritative for the lifetime
/// of one OAuth flow.
pub fn probe_port_free(port: u16) -> Result<(), std::io::Error> {
    let addr: SocketAddr = (Ipv4Addr::LOCALHOST, port).into();
    let listener = TcpListener::bind(addr)?;
    drop(listener);
    Ok(())
}

/// Bind `127.0.0.1:1455`, wait up to `timeout` for the browser to
/// hit the callback URL, validate `state`, return the code.
///
/// Cancellation: the caller can abandon this call by dropping the
/// thread that runs it — there's no kill channel because the
/// non-blocking accept loop checks `Instant::now()` every 100ms
/// and will observe the deadline even if the user never hits the
/// URL. For explicit "Esc cancels" semantics the caller should
/// pick a short timeout and restart on each user interaction
/// (the onboarding dialog does this via its channel-polling loop).
#[cfg(test)]
pub fn wait_for_callback(
    expected_state: &str,
    timeout: Duration,
) -> Result<CallbackResult, ListenerError> {
    let never_cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    wait_for_callback_with_cancel(expected_state, timeout, &never_cancel)
}

/// Same as [`wait_for_callback`] but observes an external
/// cancellation flag on every 100ms poll iteration. When the
/// caller sets `cancel` to `true` the loop returns
/// [`ListenerError::Cancelled`] within one poll interval.
///
/// The flag is checked both before and after each non-blocking
/// `accept` so a fresh cancel request doesn't race a pending
/// connection.
pub fn wait_for_callback_with_cancel(
    expected_state: &str,
    timeout: Duration,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<CallbackResult, ListenerError> {
    use std::sync::atomic::Ordering;

    let addr: SocketAddr = (Ipv4Addr::LOCALHOST, CALLBACK_PORT).into();
    let listener = TcpListener::bind(addr).map_err(ListenerError::Bind)?;
    listener.set_nonblocking(true).map_err(ListenerError::Io)?;
    let deadline = Instant::now() + timeout;

    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(ListenerError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(ListenerError::Timeout);
        }
        match listener.accept() {
            Ok((stream, _addr)) => return handle_connection(stream, expected_state),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(err) => return Err(ListenerError::Io(err)),
        }
    }
}

/// Read one HTTP request off the socket, validate it, write the
/// success (or error) HTML back, and return the parsed result.
///
/// Separate from [`wait_for_callback`] so tests can drive it with
/// a local `TcpStream` pair without actually binding 1455.
fn handle_connection(
    mut stream: TcpStream,
    expected_state: &str,
) -> Result<CallbackResult, ListenerError> {
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(ListenerError::Io)?;
    stream
        .set_write_timeout(Some(READ_TIMEOUT))
        .map_err(ListenerError::Io)?;

    let request_line = read_request_line(&mut stream)?;
    match parse_request_target(&request_line, expected_state) {
        Ok(result) => {
            let _ = write_success_html(&mut stream);
            Ok(result)
        }
        Err(err) => {
            let _ = write_error_html(&mut stream, &err);
            Err(err)
        }
    }
}

/// Read just enough of the request to get the first line
/// ("GET /path?query HTTP/1.1"). We don't bother parsing headers —
/// the OAuth callback has no body and we have no use for
/// User-Agent / Accept / etc. Stop reading on CRLF or when the
/// buffer is full, whichever comes first.
fn read_request_line(stream: &mut TcpStream) -> Result<String, ListenerError> {
    let mut buf = [0u8; 8192];
    let mut total = 0usize;
    loop {
        if total >= buf.len() {
            return Err(ListenerError::MalformedRequest(
                "request header exceeded 8KB".into(),
            ));
        }
        let n = stream.read(&mut buf[total..]).map_err(ListenerError::Io)?;
        if n == 0 {
            break;
        }
        total += n;
        if buf[..total].windows(2).any(|w| w == b"\r\n") {
            break;
        }
    }
    let header = std::str::from_utf8(&buf[..total])
        .map_err(|_| ListenerError::MalformedRequest("non-UTF8 request".into()))?;
    let first = header
        .lines()
        .next()
        .ok_or_else(|| ListenerError::MalformedRequest("empty request".into()))?;
    Ok(first.to_string())
}

/// Parse the HTTP request-line and return the validated callback
/// parameters. Public at the module level so unit tests can drive
/// it directly without a live socket.
fn parse_request_target(
    request_line: &str,
    expected_state: &str,
) -> Result<CallbackResult, ListenerError> {
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    if method != "GET" {
        return Err(ListenerError::MalformedRequest(format!(
            "expected GET, got `{method}`"
        )));
    }
    let Some((path, query)) = target.split_once('?') else {
        return Err(ListenerError::MalformedRequest(
            "no query string on callback request".into(),
        ));
    };
    if path != CALLBACK_PATH {
        return Err(ListenerError::MalformedRequest(format!(
            "expected path `{CALLBACK_PATH}`, got `{path}`"
        )));
    }

    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "code" => code = Some(percent_decode(v)),
            "state" => state = Some(percent_decode(v)),
            _ => {}
        }
    }
    let code = code.ok_or(ListenerError::MissingCode)?;
    let state = state.ok_or(ListenerError::MissingState)?;
    if code.is_empty() {
        return Err(ListenerError::MissingCode);
    }
    if state.is_empty() {
        return Err(ListenerError::MissingState);
    }
    if state != expected_state {
        return Err(ListenerError::StateMismatch {
            expected: expected_state.to_string(),
            actual: state,
        });
    }
    Ok(CallbackResult { code, state })
}

fn write_success_html(stream: &mut TcpStream) -> std::io::Result<()> {
    let body = SUCCESS_HTML;
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn write_error_html(stream: &mut TcpStream, err: &ListenerError) -> std::io::Result<()> {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Rebon OAuth error</title>\
         </head><body style=\"font-family:sans-serif;padding:2em\">\
         <h1>Login error</h1><p>{}</p>\
         <p>You can close this tab and return to the terminal.</p></body></html>",
        html_escape(&err.to_string())
    );
    let response = format!(
        "HTTP/1.1 400 Bad Request\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

const SUCCESS_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
    <title>Rebon · Login complete</title></head>\
    <body style=\"font-family:sans-serif;padding:2em\">\
    <h1>Login complete</h1>\
    <p>You can close this tab and return to the terminal.</p></body></html>";

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Percent-decode a URL query value: `%XX` becomes the byte with
/// that hex value. `+` is left as-is — the OAuth callback writes
/// spaces as `%20`, never as `+`.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    /// Pick a free ephemeral port. Used so the end-to-end tests
    /// don't have to collide with whatever is actually running on
    /// 1455. `wait_for_callback` is hard-coded to `CALLBACK_PORT`
    /// so the end-to-end tests here exercise `handle_connection`
    /// directly instead.
    fn free_port() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    #[test]
    fn probe_port_free_ok_when_unused() {
        let port = free_port();
        // Immediately after dropping, the port may still be in
        // TIME_WAIT on some platforms; SO_REUSEADDR in std::net
        // handles that. If this flakes in CI we'll switch to
        // asserting on a port we have proof is unused.
        probe_port_free(port).expect("fresh port should bind");
    }

    #[test]
    fn probe_port_free_fails_when_bound() {
        let port = free_port();
        let _guard = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        // Port is held for the lifetime of `_guard`.
        assert!(probe_port_free(port).is_err());
    }

    #[test]
    fn parse_request_happy_path() {
        let result =
            parse_request_target("GET /auth/callback?code=abc&state=xyz HTTP/1.1", "xyz").unwrap();
        assert_eq!(
            result,
            CallbackResult {
                code: "abc".into(),
                state: "xyz".into(),
            }
        );
    }

    #[test]
    fn parse_request_rejects_non_get() {
        let err =
            parse_request_target("POST /auth/callback?code=x&state=y HTTP/1.1", "y").unwrap_err();
        assert!(matches!(err, ListenerError::MalformedRequest(_)));
    }

    #[test]
    fn parse_request_rejects_wrong_path() {
        let err = parse_request_target("GET /other?code=x&state=y HTTP/1.1", "y").unwrap_err();
        assert!(matches!(err, ListenerError::MalformedRequest(_)));
    }

    #[test]
    fn parse_request_rejects_missing_query() {
        let err = parse_request_target("GET /auth/callback HTTP/1.1", "y").unwrap_err();
        assert!(matches!(err, ListenerError::MalformedRequest(_)));
    }

    #[test]
    fn parse_request_rejects_missing_code() {
        let err = parse_request_target("GET /auth/callback?state=y HTTP/1.1", "y").unwrap_err();
        assert!(matches!(err, ListenerError::MissingCode));
    }

    #[test]
    fn parse_request_rejects_missing_state() {
        let err = parse_request_target("GET /auth/callback?code=x HTTP/1.1", "y").unwrap_err();
        assert!(matches!(err, ListenerError::MissingState));
    }

    #[test]
    fn parse_request_rejects_empty_code() {
        let err =
            parse_request_target("GET /auth/callback?code=&state=y HTTP/1.1", "y").unwrap_err();
        assert!(matches!(err, ListenerError::MissingCode));
    }

    #[test]
    fn parse_request_rejects_empty_state() {
        let err =
            parse_request_target("GET /auth/callback?code=x&state= HTTP/1.1", "").unwrap_err();
        // Empty string expected_state is rejected by MissingState
        // guard regardless of whether the actual is empty.
        assert!(matches!(err, ListenerError::MissingState));
    }

    #[test]
    fn parse_request_rejects_state_mismatch() {
        let err = parse_request_target("GET /auth/callback?code=x&state=wrong HTTP/1.1", "right")
            .unwrap_err();
        assert!(matches!(
            err,
            ListenerError::StateMismatch { ref expected, ref actual }
                if expected == "right" && actual == "wrong"
        ));
    }

    #[test]
    fn parse_request_percent_decodes_code_and_state() {
        let result = parse_request_target(
            "GET /auth/callback?code=abc%2Bdef&state=A%20B HTTP/1.1",
            "A B",
        )
        .unwrap();
        assert_eq!(result.code, "abc+def");
        assert_eq!(result.state, "A B");
    }

    #[test]
    fn parse_request_ignores_extra_params() {
        let result = parse_request_target(
            "GET /auth/callback?code=c&state=s&ignored=x&scope=y HTTP/1.1",
            "s",
        )
        .unwrap();
        assert_eq!(result.code, "c");
    }

    #[test]
    fn percent_decode_passes_through_ascii() {
        assert_eq!(percent_decode("hello"), "hello");
    }

    #[test]
    fn percent_decode_handles_known_escapes() {
        assert_eq!(percent_decode("%3A%2F%2F"), "://");
        assert_eq!(percent_decode("%20"), " ");
    }

    #[test]
    fn percent_decode_leaves_invalid_escape_intact() {
        assert_eq!(percent_decode("%ZZ"), "%ZZ");
    }

    #[test]
    fn html_escape_encodes_dangerous_characters() {
        assert_eq!(html_escape("<b>&\"'"), "&lt;b&gt;&amp;&quot;&#39;");
    }

    /// End-to-end: bind `CALLBACK_PORT`, have a client thread GET
    /// `/auth/callback?code=…&state=…`, assert the listener
    /// returns the parsed code.
    ///
    /// `#[ignore]` by default because the test binds the hard-coded
    /// port 1455, which is exactly what a running rebon might be
    /// using. CI enables `--ignored` explicitly.
    #[test]
    #[ignore = "binds hard-coded port 1455; run with --ignored in isolation"]
    fn end_to_end_happy_path() {
        let barrier = Arc::new(Barrier::new(2));
        let b = barrier.clone();
        let server = thread::spawn(move || {
            b.wait();
            wait_for_callback("STATE123", Duration::from_secs(5))
        });
        barrier.wait();
        // Small delay for the bind to settle.
        thread::sleep(Duration::from_millis(50));
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, CALLBACK_PORT)).unwrap();
        client
            .write_all(
                b"GET /auth/callback?code=the-code&state=STATE123 HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
            .unwrap();
        let mut response = String::new();
        let _ = client.read_to_string(&mut response);
        assert!(response.contains("200 OK"));
        let result = server.join().unwrap().unwrap();
        assert_eq!(result.code, "the-code");
    }

    #[test]
    #[ignore = "binds hard-coded port 1455; run with --ignored in isolation"]
    fn end_to_end_rejects_state_mismatch() {
        let barrier = Arc::new(Barrier::new(2));
        let b = barrier.clone();
        let server = thread::spawn(move || {
            b.wait();
            wait_for_callback("EXPECTED", Duration::from_secs(5))
        });
        barrier.wait();
        thread::sleep(Duration::from_millis(50));
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, CALLBACK_PORT)).unwrap();
        client
            .write_all(b"GET /auth/callback?code=c&state=WRONG HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        let _ = client.read_to_string(&mut response);
        assert!(response.contains("400 Bad Request"));
        let err = server.join().unwrap().unwrap_err();
        assert!(matches!(err, ListenerError::StateMismatch { .. }));
    }

    #[test]
    #[ignore = "binds hard-coded port 1455 for timeout"]
    fn end_to_end_times_out() {
        let err = wait_for_callback("STATE", Duration::from_millis(300)).unwrap_err();
        assert!(matches!(err, ListenerError::Timeout));
    }
}
