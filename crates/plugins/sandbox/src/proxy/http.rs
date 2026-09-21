//! Reading just enough HTTP to know where a request is going.
//!
//! The proxy does **not** decrypt anything. A `CONNECT` request names its
//! target in the request line, and a plain request names it in the
//! absolute-form target or the `Host` header — which is all the domain
//! decision needs. Not terminating TLS is what keeps this from needing a CA
//! certificate the sandbox would have to trust, an installed root on the
//! user's machine, and (on macOS) a `trustd` hole in the seatbelt profile.
//!
//! Parsing is a pure function over the bytes of the request head so the
//! awkward cases — no `Host`, a port that is not a number, an absolute URI
//! with userinfo, a head that never terminates — are testable without a
//! socket.

/// Where a request wants to go, and how to forward it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// `CONNECT host:port` — after the 200, the proxy is a pipe.
    Tunnel { host: String, port: u16 },
    /// An ordinary request through the proxy. `head` is the rewritten
    /// request head to send upstream, already in origin form.
    Forward {
        host: String,
        port: u16,
        head: Vec<u8>,
    },
}

impl Destination {
    pub fn host(&self) -> &str {
        match self {
            Destination::Tunnel { host, .. } | Destination::Forward { host, .. } => host,
        }
    }

    pub fn port(&self) -> u16 {
        match self {
            Destination::Tunnel { port, .. } | Destination::Forward { port, .. } => *port,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error("the request head is not valid UTF-8")]
    NotUtf8,
    #[error("the request line is missing or malformed")]
    MalformedRequestLine,
    #[error("no host: neither the request target nor a Host header names one")]
    NoHost,
    #[error("`{0}` is not a usable port")]
    BadPort(String),
}

/// The largest request head the proxy will buffer.
///
/// A client that never sends `\r\n\r\n` would otherwise hold memory until it
/// gave up. 64 KiB is far past any real set of request headers.
pub const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Find the end of the request head, if the buffer contains one.
pub fn head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

/// Parse a complete request head.
pub fn parse(head: &[u8]) -> Result<Destination, RequestError> {
    let text = std::str::from_utf8(head).map_err(|_| RequestError::NotUtf8)?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or(RequestError::MalformedRequestLine)?;

    let mut parts = request_line.split(' ');
    let method = parts.next().ok_or(RequestError::MalformedRequestLine)?;
    let target = parts.next().ok_or(RequestError::MalformedRequestLine)?;
    let version = parts.next().unwrap_or("HTTP/1.1");
    if method.is_empty() || target.is_empty() {
        return Err(RequestError::MalformedRequestLine);
    }

    let headers: Vec<(&str, &str)> = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim(), value.trim()))
        .collect();
    let host_header = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("host"))
        .map(|(_, value)| *value);

    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_authority(target, 443)?;
        return Ok(Destination::Tunnel { host, port });
    }

    // Absolute form (`GET http://host/path`) is what a client sends *to a
    // proxy*; origin form with a Host header is the fallback for clients
    // that were pointed here as if it were the origin server.
    let (host, port, path) = match split_absolute_uri(target) {
        Some((host, port, path)) => (host, port, path),
        None => {
            let authority = host_header.ok_or(RequestError::NoHost)?;
            let (host, port) = split_authority(authority, 80)?;
            (host, port, target.to_string())
        }
    };

    Ok(Destination::Forward {
        host: host.clone(),
        port,
        head: rewrite_head(method, &path, version, &headers, &host, port),
    })
}

/// Rebuild the head in origin form for the upstream server.
///
/// Two headers are replaced rather than copied:
///
/// * `Connection: close`, and `Proxy-Connection` dropped. The proxy handles
///   exactly one request per connection and then pipes bytes, so a client
///   that kept the connection alive could send a *second* request — to a
///   different host — down a tunnel already authorised for the first. Closing
///   after one request is what makes the domain decision hold for everything
///   that travels on the connection.
/// * `Host`, set to the authority actually being dialled, so a mismatched
///   `Host` header cannot name one origin while the connection goes to
///   another.
fn rewrite_head(
    method: &str,
    path: &str,
    version: &str,
    headers: &[(&str, &str)],
    host: &str,
    port: u16,
) -> Vec<u8> {
    let path = if path.is_empty() { "/" } else { path };
    let mut head = format!("{method} {path} {version}\r\n");
    let authority = if port == 80 {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };
    head.push_str(&format!("Host: {authority}\r\n"));
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("host")
            || name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("proxy-connection")
            || name.eq_ignore_ascii_case("proxy-authorization")
            || name.eq_ignore_ascii_case("keep-alive")
        {
            continue;
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    head.into_bytes()
}

/// `scheme://[userinfo@]host[:port]/path` → the three pieces.
fn split_absolute_uri(target: &str) -> Option<(String, u16, String)> {
    let (scheme, rest) = target.split_once("://")?;
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], rest[index..].to_string()),
        None => (rest, "/".to_string()),
    };
    // Userinfo before an `@` is not part of the host. Reading it as one
    // would let `http://allowed.example.com@evil.test/` be checked against
    // the wrong name — the oldest URL-parsing trick there is.
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (host, port) = split_authority(authority, default_port).ok()?;
    Some((host, port, path))
}

/// `host`, `host:port`, `[v6]`, or `[v6]:port`.
fn split_authority(authority: &str, default_port: u16) -> Result<(String, u16), RequestError> {
    let authority = authority.trim();
    if authority.is_empty() {
        return Err(RequestError::NoHost);
    }

    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or(RequestError::NoHost)?;
        if host.is_empty() {
            return Err(RequestError::NoHost);
        }
        let port = match tail.strip_prefix(':') {
            Some(port) => port
                .parse()
                .map_err(|_| RequestError::BadPort(port.to_string()))?,
            None => default_port,
        };
        return Ok((host.to_string(), port));
    }

    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => {
            let port = port
                .parse()
                .map_err(|_| RequestError::BadPort(port.to_string()))?;
            Ok((host.to_string(), port))
        }
        _ => Ok((authority.to_string(), default_port)),
    }
}

/// The refusal a client sees. Carries the reason in the body so it lands in
/// the command's own output, where the model will read it.
pub fn forbidden_response(reason: &str) -> Vec<u8> {
    let body = format!("Blocked by the Rebon sandbox: {reason}\n");
    format!(
        "HTTP/1.1 403 Forbidden\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

pub fn bad_request_response(reason: &str) -> Vec<u8> {
    let body = format!("Rebon sandbox proxy could not read the request: {reason}\n");
    format!(
        "HTTP/1.1 400 Bad Request\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

pub const TUNNEL_ESTABLISHED: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(head: &str) -> Result<Destination, RequestError> {
        parse(head.as_bytes())
    }

    fn head_of(destination: &Destination) -> String {
        match destination {
            Destination::Forward { head, .. } => String::from_utf8(head.clone()).unwrap(),
            other => panic!("expected a forward, got {other:?}"),
        }
    }

    #[test]
    fn a_connect_names_its_target() {
        let destination = parse_str("CONNECT example.com:443 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(
            destination,
            Destination::Tunnel {
                host: "example.com".into(),
                port: 443
            }
        );
    }

    #[test]
    fn a_connect_without_a_port_defaults_to_443() {
        let destination = parse_str("CONNECT example.com HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(destination.port(), 443);
    }

    #[test]
    fn a_connect_to_a_bracketed_ipv6_address_parses() {
        let destination = parse_str("CONNECT [2001:db8::1]:8443 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(destination.host(), "2001:db8::1");
        assert_eq!(destination.port(), 8443);
    }

    #[test]
    fn an_absolute_uri_gives_host_port_and_path() {
        let destination =
            parse_str("GET http://example.com:8080/a/b?c=d HTTP/1.1\r\nHost: example.com\r\n\r\n")
                .unwrap();
        assert_eq!(destination.host(), "example.com");
        assert_eq!(destination.port(), 8080);
        assert!(head_of(&destination).starts_with("GET /a/b?c=d HTTP/1.1\r\n"));
    }

    #[test]
    fn an_absolute_uri_with_no_path_becomes_a_slash() {
        let destination = parse_str("GET http://example.com HTTP/1.1\r\n\r\n").unwrap();
        assert!(head_of(&destination).starts_with("GET / HTTP/1.1\r\n"));
    }

    #[test]
    fn userinfo_before_an_at_sign_is_not_the_host() {
        // `http://allowed.example.com@evil.test/` reads as `allowed…` to
        // anyone splitting on the first `@`, and dials `evil.test`. The
        // domain check has to see the host that will actually be connected.
        let destination =
            parse_str("GET http://allowed.example.com@evil.test/x HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(destination.host(), "evil.test");
    }

    #[test]
    fn origin_form_falls_back_to_the_host_header() {
        let destination = parse_str("GET /a HTTP/1.1\r\nHost: example.com\r\n\r\n").unwrap();
        assert_eq!(destination.host(), "example.com");
        assert_eq!(destination.port(), 80);
    }

    #[test]
    fn origin_form_without_a_host_header_is_refused() {
        assert_eq!(
            parse_str("GET /a HTTP/1.1\r\n\r\n").unwrap_err(),
            RequestError::NoHost
        );
    }

    #[test]
    fn the_host_header_is_rewritten_to_the_authority_actually_dialled() {
        // A `Host` naming one origin while the connection goes to another is
        // how a request slips past a check made on the connection target.
        let destination =
            parse_str("GET http://real.example.com/x HTTP/1.1\r\nHost: lying.test\r\n\r\n")
                .unwrap();
        let head = head_of(&destination);
        assert!(head.contains("Host: real.example.com\r\n"), "{head}");
        assert!(!head.contains("lying.test"), "{head}");
    }

    #[test]
    fn a_non_default_port_stays_in_the_rewritten_host_header() {
        let destination = parse_str("GET http://example.com:8080/x HTTP/1.1\r\n\r\n").unwrap();
        assert!(head_of(&destination).contains("Host: example.com:8080\r\n"));
    }

    #[test]
    fn the_forwarded_head_closes_the_connection() {
        // One request per connection. Keeping it alive would let a client
        // send a second request, to a different host, down a connection
        // already authorised for the first.
        let destination = parse_str(
            "GET http://example.com/x HTTP/1.1\r\nConnection: keep-alive\r\n\
             Proxy-Connection: keep-alive\r\n\r\n",
        )
        .unwrap();
        let head = head_of(&destination);
        assert!(head.contains("Connection: close\r\n"), "{head}");
        assert!(!head.to_lowercase().contains("keep-alive"), "{head}");
        assert!(!head.to_lowercase().contains("proxy-connection"), "{head}");
    }

    #[test]
    fn hop_by_hop_and_proxy_credentials_do_not_reach_the_origin() {
        let destination = parse_str(
            "GET http://example.com/x HTTP/1.1\r\n\
             Proxy-Authorization: Basic c2VjcmV0\r\nAccept: */*\r\n\r\n",
        )
        .unwrap();
        let head = head_of(&destination);
        assert!(!head.contains("c2VjcmV0"), "{head}");
        assert!(head.contains("Accept: */*\r\n"), "{head}");
    }

    #[test]
    fn an_unparseable_port_is_an_error_rather_than_a_default() {
        // Falling back to 443 would connect somewhere the client did not
        // ask for.
        assert_eq!(
            parse_str("CONNECT example.com:https HTTP/1.1\r\n\r\n").unwrap_err(),
            RequestError::BadPort("https".into())
        );
    }

    #[test]
    fn a_malformed_request_line_is_refused() {
        assert_eq!(
            parse_str("GARBAGE\r\n\r\n").unwrap_err(),
            RequestError::MalformedRequestLine
        );
        assert_eq!(
            parse_str("\r\n\r\n").unwrap_err(),
            RequestError::MalformedRequestLine
        );
    }

    #[test]
    fn a_non_utf8_head_is_refused_rather_than_lossily_read() {
        assert_eq!(
            parse(&[0xff, 0xfe, b' ']).unwrap_err(),
            RequestError::NotUtf8
        );
    }

    #[test]
    fn an_unknown_scheme_falls_back_to_the_host_header() {
        let destination =
            parse_str("GET ftp://example.com/x HTTP/1.1\r\nHost: fallback.test\r\n\r\n").unwrap();
        assert_eq!(destination.host(), "fallback.test");
    }

    #[test]
    fn the_head_terminator_is_found_only_when_complete() {
        assert_eq!(head_end(b"GET / HTTP/1.1\r\n\r\n"), Some(18));
        assert_eq!(head_end(b"GET / HTTP/1.1\r\n"), None);
        assert_eq!(head_end(b""), None);
        assert_eq!(head_end(b"a\r\n\r\nbody"), Some(5));
    }

    #[test]
    fn method_matching_is_case_insensitive() {
        assert!(matches!(
            parse_str("connect example.com:443 HTTP/1.1\r\n\r\n").unwrap(),
            Destination::Tunnel { .. }
        ));
    }

    #[test]
    fn a_refusal_carries_its_reason_in_the_body() {
        // It ends up in the command's own output, which is the only place
        // the model will see it.
        let response = String::from_utf8(forbidden_response("evil.test is not allowed")).unwrap();
        assert!(response.starts_with("HTTP/1.1 403 Forbidden\r\n"));
        assert!(response.contains("evil.test is not allowed"));
        assert!(response.contains("Content-Length: "));
    }
}
