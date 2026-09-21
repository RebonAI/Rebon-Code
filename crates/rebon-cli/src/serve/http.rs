//! The little HTTP/1.1 `rebon serve` needs: a request head, a response, and
//! the checks that keep a loopback server from being driven by a page the
//! user did not open.
//!
//! There are three routes worth of HTTP here (a page, its assets, a few
//! `GET /api/*` reads) and one WebSocket; the conversation itself is ACP
//! over the socket. That is not enough surface to pull an HTTP framework
//! into the build for, and the one WebSocket dependency the workspace
//! already carries knows how to finish an upgrade on a raw stream.

use std::collections::HashMap;

use rebon_types::constant_time_eq;

/// A parsed request head. Everything the router needs, nothing the body
/// might carry — the only bodies accepted are discarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    /// Path without the query string.
    pub path: String,
    pub query: HashMap<String, String>,
    /// Header names lower-cased; values trimmed.
    pub headers: Vec<(String, String)>,
    /// Bytes the head occupies, terminator included.
    pub head_len: usize,
}

/// Why a request head could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadError {
    /// The terminator has not arrived yet.
    Incomplete,
    /// The head is not HTTP, or too large to be honest.
    Malformed(&'static str),
}

/// The most head this server reads. A browser request for a page is a few
/// hundred bytes; anything past this is not a browser talking to us.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;

impl RequestHead {
    pub fn parse(bytes: &[u8]) -> Result<Self, HeadError> {
        let Some(end) = find_terminator(bytes) else {
            return if bytes.len() >= MAX_HEAD_BYTES {
                Err(HeadError::Malformed("request head too large"))
            } else {
                Err(HeadError::Incomplete)
            };
        };
        let head_len = end + 4;
        let head = std::str::from_utf8(&bytes[..end])
            .map_err(|_| HeadError::Malformed("request head is not UTF-8"))?;
        let mut lines = head.split("\r\n");
        let request_line = lines
            .next()
            .ok_or(HeadError::Malformed("missing request line"))?;
        let mut parts = request_line.split(' ');
        let method = parts
            .next()
            .filter(|method| !method.is_empty())
            .ok_or(HeadError::Malformed("missing method"))?;
        let target = parts
            .next()
            .ok_or(HeadError::Malformed("missing request target"))?;
        let version = parts
            .next()
            .ok_or(HeadError::Malformed("missing HTTP version"))?;
        if !version.starts_with("HTTP/1.") {
            return Err(HeadError::Malformed("not HTTP/1.x"));
        }
        let (path, query) = match target.split_once('?') {
            Some((path, query)) => (path, parse_query(query)),
            None => (target, HashMap::new()),
        };
        if !path.starts_with('/') {
            return Err(HeadError::Malformed("request target is not a path"));
        }
        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let (name, value) = line
                .split_once(':')
                .ok_or(HeadError::Malformed("header line without a colon"))?;
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
        Ok(Self {
            method: method.to_string(),
            path: path.to_string(),
            query,
            headers,
            head_len,
        })
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn is_websocket_upgrade(&self) -> bool {
        self.header("upgrade")
            .map(|value| value.eq_ignore_ascii_case("websocket"))
            .unwrap_or(false)
            && self
                .header("connection")
                .map(|value| {
                    value
                        .split(',')
                        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
                })
                .unwrap_or(false)
    }

    pub fn content_length(&self) -> usize {
        self.header("content-length")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    }
}

fn find_terminator(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

/// `a=1&b=two%20words` → map. Repeated keys keep the last value.
pub fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (percent_decode(key), percent_decode(value)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

/// Percent-decoding with `+` as space, lossy on invalid UTF-8.
pub fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match hex_pair(bytes[i + 1], bytes[i + 2]) {
                Some(byte) => {
                    out.push(byte);
                    i += 2;
                }
                None => out.push(b'%'),
            },
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_pair(high: u8, low: u8) -> Option<u8> {
    let digit = |byte: u8| (byte as char).to_digit(16).map(|digit| digit as u8);
    Some(digit(high)? << 4 | digit(low)?)
}

/// A response, ready to be written.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type", content_type.to_string())],
            body: body.into(),
        }
    }

    /// Generic over the body rather than taking a `Value`, so a route can
    /// answer with the type that declares its shape instead of assembling
    /// one. A type that cannot serialise is a bug in the route, not
    /// something a client should see, so it becomes a 500 with no body
    /// detail rather than a panic.
    pub fn json<T: serde::Serialize>(status: u16, value: &T) -> Self {
        match serde_json::to_string(value) {
            Ok(body) => Self::new(status, "application/json; charset=utf-8", body),
            Err(_) => Self::error(500, "response could not be encoded"),
        }
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self::new(status, "text/plain; charset=utf-8", body.into())
    }

    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Self::json(status, &serde_json::json!({ "error": message.into() }))
    }

    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    /// The wire form. Every response closes the connection: one request
    /// per connection is all a page load costs, and it keeps the server
    /// out of the keep-alive business.
    pub fn to_bytes(&self) -> Vec<u8> {
        let reason = reason_phrase(self.status);
        let mut out = format!("HTTP/1.1 {} {reason}\r\n", self.status).into_bytes();
        for (name, value) in &self.headers {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        out.extend_from_slice(format!("Content-Length: {}\r\n", self.body.len()).as_bytes());
        out.extend_from_slice(b"Cache-Control: no-store\r\n");
        out.extend_from_slice(b"X-Content-Type-Options: nosniff\r\n");
        out.extend_from_slice(b"Connection: close\r\n\r\n");
        out.extend_from_slice(&self.body);
        out
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

/// The hosts a request may name. A loopback server answers requests for
/// the names that reach it; a request whose `Host` names anything else
/// came through a DNS rebind or a proxy the user did not set up, and gets
/// a refusal rather than a page.
#[derive(Debug, Clone)]
pub struct Guard {
    allowed_hosts: Vec<String>,
    token: String,
}

impl Guard {
    pub fn new(bind_host: &str, port: u16, token: impl Into<String>) -> Self {
        let mut allowed_hosts = vec![
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ];
        let bound = if bind_host.contains(':') && !bind_host.starts_with('[') {
            format!("[{bind_host}]:{port}")
        } else {
            format!("{bind_host}:{port}")
        };
        if !allowed_hosts.contains(&bound) {
            allowed_hosts.push(bound);
        }
        Self {
            allowed_hosts,
            token: token.into(),
        }
    }

    /// Only the tests read the list itself; the runtime asks
    /// [`Self::host_allowed`] instead of iterating it.
    #[cfg(test)]
    pub fn allowed_hosts(&self) -> &[String] {
        &self.allowed_hosts
    }

    pub fn host_allowed(&self, host: Option<&str>) -> bool {
        host.map(|host| {
            let host = host.trim().to_ascii_lowercase();
            self.allowed_hosts.iter().any(|allowed| *allowed == host)
        })
        .unwrap_or(false)
    }

    /// An `Origin`, when a browser sends one, must be one of our hosts.
    /// A missing origin is a non-browser client and is allowed through —
    /// it still needs the token.
    pub fn origin_allowed(&self, origin: Option<&str>) -> bool {
        match origin {
            None => true,
            Some(origin) => {
                let origin = origin.trim().to_ascii_lowercase();
                let host = origin
                    .strip_prefix("http://")
                    .or_else(|| origin.strip_prefix("https://"));
                host.map(|host| self.allowed_hosts.iter().any(|allowed| *allowed == host))
                    .unwrap_or(false)
            }
        }
    }

    /// Whether the request carries the token — `?token=` on the URL (the
    /// only place a browser's WebSocket can put it) or a bearer header.
    pub fn token_presented(&self, head: &RequestHead) -> bool {
        let presented = head.query.get("token").map(String::as_str).or_else(|| {
            head.header("authorization")
                .and_then(|value| value.strip_prefix("Bearer "))
                .map(str::trim)
        });
        presented
            .map(|presented| self.token_matches(presented))
            .unwrap_or(false)
    }

    /// Whether `presented` is this run's token. Compared in constant time: the
    /// IPC handshake lets a caller retry as fast as it likes.
    pub fn token_matches(&self, presented: &str) -> bool {
        constant_time_eq(presented, &self.token)
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_head_parses_into_method_path_query_and_headers() {
        let raw = b"GET /api/history?session=abc&cwd=C%3A%5Cwork+dir HTTP/1.1\r\nHost: 127.0.0.1:7700\r\nUpgrade: WebSocket\r\nConnection: keep-alive, Upgrade\r\n\r\nleftover";
        let head = RequestHead::parse(raw).unwrap();
        assert_eq!(head.method, "GET");
        assert_eq!(head.path, "/api/history");
        assert_eq!(head.query["session"], "abc");
        assert_eq!(head.query["cwd"], "C:\\work dir");
        assert_eq!(head.header("host"), Some("127.0.0.1:7700"));
        assert!(head.is_websocket_upgrade());
        assert_eq!(&raw[head.head_len..], b"leftover");
    }

    #[test]
    fn an_incomplete_head_asks_for_more_and_an_oversized_one_is_refused() {
        assert_eq!(
            RequestHead::parse(b"GET / HTTP/1.1\r\nHost: x"),
            Err(HeadError::Incomplete)
        );
        let huge = vec![b'a'; MAX_HEAD_BYTES];
        assert!(matches!(
            RequestHead::parse(&huge),
            Err(HeadError::Malformed(_))
        ));
        assert!(matches!(
            RequestHead::parse(b"NOPE\r\n\r\n"),
            Err(HeadError::Malformed(_))
        ));
        assert!(matches!(
            RequestHead::parse(b"GET / SPDY/3\r\n\r\n"),
            Err(HeadError::Malformed(_))
        ));
    }

    #[test]
    fn a_plain_request_is_not_an_upgrade() {
        let head =
            RequestHead::parse(b"GET /ws HTTP/1.1\r\nConnection: keep-alive\r\n\r\n").unwrap();
        assert!(!head.is_websocket_upgrade());
        assert_eq!(head.content_length(), 0);
    }

    #[test]
    fn responses_carry_length_and_close() {
        let bytes = Response::text(404, "nope").to_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(text.contains("Content-Length: 4\r\n"));
        assert!(text.contains("Connection: close\r\n\r\nnope"));
    }

    #[test]
    fn the_guard_accepts_our_hosts_and_refuses_others() {
        let guard = Guard::new("127.0.0.1", 7700, "secret");
        assert!(guard.host_allowed(Some("127.0.0.1:7700")));
        assert!(guard.host_allowed(Some("LOCALHOST:7700")));
        assert!(!guard.host_allowed(Some("evil.example:7700")));
        assert!(!guard.host_allowed(Some("127.0.0.1:7701")));
        assert!(!guard.host_allowed(None));

        assert!(guard.origin_allowed(None));
        assert!(guard.origin_allowed(Some("http://localhost:7700")));
        assert!(!guard.origin_allowed(Some("http://evil.example")));
        assert!(!guard.origin_allowed(Some("null")));

        let with_token =
            RequestHead::parse(b"GET /ws?token=secret HTTP/1.1\r\nHost: localhost:7700\r\n\r\n")
                .unwrap();
        assert!(guard.token_presented(&with_token));
        let bearer =
            RequestHead::parse(b"GET /api/info HTTP/1.1\r\nAuthorization: Bearer secret\r\n\r\n")
                .unwrap();
        assert!(guard.token_presented(&bearer));
        let wrong =
            RequestHead::parse(b"GET /ws?token=secre HTTP/1.1\r\nHost: localhost:7700\r\n\r\n")
                .unwrap();
        assert!(!guard.token_presented(&wrong));
        let none = RequestHead::parse(b"GET /ws HTTP/1.1\r\n\r\n").unwrap();
        assert!(!guard.token_presented(&none));
    }

    #[test]
    fn an_ipv6_bind_host_is_bracketed() {
        let guard = Guard::new("::1", 8080, "t");
        assert!(guard.host_allowed(Some("[::1]:8080")));
        assert_eq!(guard.allowed_hosts().len(), 3);
    }

    #[test]
    fn percent_decoding_handles_plus_and_bad_escapes() {
        assert_eq!(percent_decode("a+b%20c"), "a b c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%e4%b8%ad"), "中");
    }
}
