//! Authorization-code parsers.
//!
//! Two entry points:
//!
//! * [`parse_pasted_code`] — the manual paste path. The pasted value is
//! split on `#` into authorization code and state. Both halves must be
//! non-empty.
//! The empty/missing case maps to "Invalid code. Please make sure
//! the full code was copied".
//! * [`parse_callback_url`] — the automatic
//! callback path. The localhost server receives a callback URL
//! like `http://localhost:1234/callback?code=AUTH_CODE&state=STATE_VALUE`.
//! [`parse_callback_url`] matches the query parsing
//! the listener does.
//!
//! ## Pinned rules
//!
//! 1. **The pasted code separator is `#`** (a literal `#`, not `&` or
//! `?`). The parser splits the raw pasted value on literal `#`.
//! The first segment is the authorization code and the second segment
//! is state; both must be non-empty.
//! 2. **Both first and second segments must be non-empty.** A trailing
//! separator such as `"abc#"` leaves the second segment empty, so the
//! paste is rejected.
//! 3. **A bare paste with no `#`** (e.g. `"abc"`) has no second
//! segment, so the paste is rejected.
//! 4. **Trailing or leading whitespace is NOT stripped.** The parser
//! preserves whitespace in each segment.
//! 5. **Multiple `#` characters** use only the first two separated
//! segments: `"a#b#c"` yields authorization code `"a"` and state
//! `"b"`; later segments are ignored.
//! 6. **Callback URL parsing** extracts `code` and `state` query
//! parameters. Missing either is an error. The scheme, host, port, and path
//! are NOT validated here — the consumer's listener already did
//! that.

/// The error returned when a pasted code or callback URL fails to
/// parse. The message text is stable so the consumer can pin it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeParseError {
    /// The pasted value didn't contain a `#` separator, or one of
    /// the halves was empty. Its user-facing message is
    /// `"Invalid code. Please make sure the full code was copied"`.
    InvalidPastedCode,
    /// The callback URL had no `code=` query parameter.
    MissingCode,
    /// The callback URL had no `state=` query parameter.
    MissingState,
}

impl CodeParseError {
    /// The stable user-facing error message for this error.
    pub fn message(&self) -> &'static str {
        match self {
            Self::InvalidPastedCode => "Invalid code. Please make sure the full code was copied",
            Self::MissingCode => "Callback URL is missing the `code` parameter",
            Self::MissingState => "Callback URL is missing the `state` parameter",
        }
    }
}

/// Parsed result of a manually pasted `<authorization_code>#<state>`
/// blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPastedCode {
    pub authorization_code: String,
    pub state: String,
}

/// Parse a manually pasted authorization code.
///
/// The raw pasted value is split on literal `#`. The first segment is
/// the authorization code and the second segment is the state. Both
/// must be non-empty; missing, leading-empty, or trailing-empty
/// segments return [`CodeParseError::InvalidPastedCode`]. Whitespace is
/// preserved, and segments after the first two are ignored.
pub fn parse_pasted_code(value: &str) -> Result<ParsedPastedCode, CodeParseError> {
    let mut parts = value.split('#');
    let auth = parts.next().unwrap_or("");
    let state = parts.next().unwrap_or("");
    if auth.is_empty() || state.is_empty() {
        return Err(CodeParseError::InvalidPastedCode);
    }
    Ok(ParsedPastedCode {
        authorization_code: auth.to_string(),
        state: state.to_string(),
    })
}

/// Parsed result of a callback URL the localhost listener received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCallback {
    pub code: String,
    pub state: String,
}

/// Parse a callback URL like
/// `http://localhost:1234/callback?code=AUTH_CODE&state=STATE_VALUE`
/// (or any URL with a query string) into the `code` + `state` pair.
///
/// This is a hand-rolled query parser that handles percent-decoding
/// of the most common cases. The query parser is intentionally
/// minimal — the listener already validated the URL shape.
pub fn parse_callback_url(url: &str) -> Result<ParsedCallback, CodeParseError> {
    let query = match url.find('?') {
        Some(i) => &url[i + 1..],
        None => "",
    };
    // The fragment (#…) is NOT included in the query, so strip it.
    let query = match query.find('#') {
        Some(i) => &query[..i],
        None => query,
    };
    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.find('=') {
            Some(i) => (&pair[..i], &pair[i + 1..]),
            None => (pair, ""),
        };
        match k {
            "code" => code = Some(percent_decode(v)),
            "state" => state = Some(percent_decode(v)),
            _ => {}
        }
    }
    let code = code.ok_or(CodeParseError::MissingCode)?;
    let state = state.ok_or(CodeParseError::MissingState)?;
    if code.is_empty() {
        return Err(CodeParseError::MissingCode);
    }
    if state.is_empty() {
        return Err(CodeParseError::MissingState);
    }
    Ok(ParsedCallback { code, state })
}

/// Minimal percent-decoder for query-string values. Handles `%XX`
/// hex pairs and `+` → space. Anything else is passed through.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' && i + 2 < bytes.len() {
            let hi = hex_digit(bytes[i + 1]);
            let lo = hex_digit(bytes[i + 2]);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        if b == b'+' {
            out.push(b' ');
        } else {
            out.push(b);
        }
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_pasted_code() {
        let r = parse_pasted_code("abc123#state456").unwrap();
        assert_eq!(r.authorization_code, "abc123");
        assert_eq!(r.state, "state456");
    }

    #[test]
    fn parse_pasted_code_with_extra_hash_keeps_first_two_parts() {
        // Only the first two `#`-separated segments are significant;
        // later segments are ignored.
        let r = parse_pasted_code("abc#state#extra").unwrap();
        assert_eq!(r.authorization_code, "abc");
        assert_eq!(r.state, "state");
    }

    #[test]
    fn parse_pasted_code_rejects_no_separator() {
        let err = parse_pasted_code("abc123").unwrap_err();
        assert_eq!(err, CodeParseError::InvalidPastedCode);
    }

    #[test]
    fn parse_pasted_code_rejects_empty_state() {
        let err = parse_pasted_code("abc123#").unwrap_err();
        assert_eq!(err, CodeParseError::InvalidPastedCode);
    }

    #[test]
    fn parse_pasted_code_rejects_empty_code() {
        let err = parse_pasted_code("#state456").unwrap_err();
        assert_eq!(err, CodeParseError::InvalidPastedCode);
    }

    #[test]
    fn parse_pasted_code_rejects_empty_string() {
        let err = parse_pasted_code("").unwrap_err();
        assert_eq!(err, CodeParseError::InvalidPastedCode);
    }

    #[test]
    fn parse_pasted_code_rejects_only_separator() {
        let err = parse_pasted_code("#").unwrap_err();
        assert_eq!(err, CodeParseError::InvalidPastedCode);
    }

    #[test]
    fn parse_pasted_code_does_not_strip_whitespace() {
        // Pinned: the raw value goes to split as-is, so
        // leading whitespace becomes part of the auth
        // code.
        let r = parse_pasted_code(" abc#state").unwrap();
        assert_eq!(r.authorization_code, " abc");
    }

    #[test]
    fn parse_pasted_code_error_message_is_expected() {
        assert_eq!(
            CodeParseError::InvalidPastedCode.message(),
            "Invalid code. Please make sure the full code was copied"
        );
    }

    #[test]
    fn parse_callback_url_basic() {
        let r = parse_callback_url("http://localhost:1234/callback?code=ABC&state=XYZ").unwrap();
        assert_eq!(r.code, "ABC");
        assert_eq!(r.state, "XYZ");
    }

    #[test]
    fn parse_callback_url_with_other_params() {
        let r =
            parse_callback_url("http://localhost/cb?foo=bar&code=ABC&state=XYZ&baz=qux").unwrap();
        assert_eq!(r.code, "ABC");
        assert_eq!(r.state, "XYZ");
    }

    #[test]
    fn parse_callback_url_with_percent_encoding() {
        let r = parse_callback_url("http://localhost/cb?code=foo%20bar&state=a%2Bb").unwrap();
        assert_eq!(r.code, "foo bar");
        assert_eq!(r.state, "a+b");
    }

    #[test]
    fn parse_callback_url_with_plus_for_space() {
        let r = parse_callback_url("http://localhost/cb?code=foo+bar&state=baz").unwrap();
        assert_eq!(r.code, "foo bar");
    }

    #[test]
    fn parse_callback_url_missing_code() {
        let err = parse_callback_url("http://localhost/cb?state=XYZ").unwrap_err();
        assert_eq!(err, CodeParseError::MissingCode);
    }

    #[test]
    fn parse_callback_url_missing_state() {
        let err = parse_callback_url("http://localhost/cb?code=ABC").unwrap_err();
        assert_eq!(err, CodeParseError::MissingState);
    }

    #[test]
    fn parse_callback_url_no_query_string() {
        let err = parse_callback_url("http://localhost/cb").unwrap_err();
        assert_eq!(err, CodeParseError::MissingCode);
    }

    #[test]
    fn parse_callback_url_empty_code() {
        let err = parse_callback_url("http://localhost/cb?code=&state=XYZ").unwrap_err();
        assert_eq!(err, CodeParseError::MissingCode);
    }

    #[test]
    fn parse_callback_url_empty_state() {
        let err = parse_callback_url("http://localhost/cb?code=ABC&state=").unwrap_err();
        assert_eq!(err, CodeParseError::MissingState);
    }

    #[test]
    fn parse_callback_url_strips_fragment() {
        let r = parse_callback_url("http://localhost/cb?code=ABC&state=XYZ#frag").unwrap();
        assert_eq!(r.code, "ABC");
        assert_eq!(r.state, "XYZ");
    }

    #[test]
    fn parse_callback_url_handles_percent_encoded_uppercase() {
        let r = parse_callback_url("http://localhost/cb?code=A%2FB&state=C%3DD").unwrap();
        assert_eq!(r.code, "A/B");
        assert_eq!(r.state, "C=D");
    }

    #[test]
    fn parse_callback_url_handles_lowercase_hex() {
        let r = parse_callback_url("http://localhost/cb?code=A%2fB&state=C%3dD").unwrap();
        assert_eq!(r.code, "A/B");
        assert_eq!(r.state, "C=D");
    }

    #[test]
    fn parse_callback_url_invalid_percent_pair_passes_through() {
        // Pinned: a malformed percent-encoding (not three chars or
        // not hex) is passed through literally rather than throwing.
        let r = parse_callback_url("http://localhost/cb?code=A%ZZ&state=B").unwrap();
        assert_eq!(r.code, "A%ZZ");
    }
}
