//! OAuth token-response parser.
//!
//! Covers:
//!
//! * [`TokenExchangeResponse`] — the shape of the JSON the token
//! endpoint returns.
//! * [`parse_token_exchange_response`] — the JSON parsing. The HTTP
//! POST itself is the consumer's job.
//!
//! ## Pinned rules
//!
//! 1. The token endpoint returns `expires_in` in **seconds**, not
//! milliseconds. To convert to a Unix timestamp ms, multiply by
//! 1000 and add to the current time. See
//! [`expires_at_from_response`].
//! 2. The JSON parser is hand-rolled (no serde dep). It supports the
//! minimum subset of JSON the token endpoint actually returns:
//! string values, integer values, optional missing fields. It is
//! NOT a general-purpose JSON parser.
//! 3. Required fields are `access_token` and `expires_in`. The
//! `refresh_token`, `scope`, and `id_token` fields are optional.

/// Parsed token-exchange response from the token endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenExchangeResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Lifetime in seconds, as the OAuth spec requires.
    pub expires_in: u64,
    pub scope: Option<String>,
    pub id_token: Option<String>,
}

/// Errors returned by [`parse_token_exchange_response`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenExchangeError {
    /// The body wasn't a JSON object (no `{` at the start).
    NotAnObject,
    /// The required `access_token` field was missing.
    MissingAccessToken,
    /// The required `expires_in` field was missing.
    MissingExpiresIn,
    /// A field had the wrong type (e.g. `expires_in` was a string).
    InvalidFieldType { field: &'static str },
    /// The JSON was syntactically invalid in a way the hand-rolled
    /// parser couldn't recover from.
    Malformed,
}

/// Parse the JSON body returned by the token endpoint.
///
/// This is a hand-rolled minimal JSON parser scoped to the shape the
/// token endpoint actually returns. The crate doesn't take a serde
/// dep so this is the bulk of the work.
pub fn parse_token_exchange_response(
    json: &str,
) -> Result<TokenExchangeResponse, TokenExchangeError> {
    let mut p = Parser::new(json);
    p.skip_whitespace();
    if p.peek() != Some(b'{') {
        return Err(TokenExchangeError::NotAnObject);
    }
    p.advance();
    let mut access_token: Option<String> = None;
    let mut refresh_token: Option<String> = None;
    let mut expires_in: Option<u64> = None;
    let mut scope: Option<String> = None;
    let mut id_token: Option<String> = None;
    loop {
        p.skip_whitespace();
        match p.peek() {
            Some(b'}') => {
                p.advance();
                break;
            }
            Some(b'"') => {
                let key = p.parse_string()?;
                p.skip_whitespace();
                if p.peek() != Some(b':') {
                    return Err(TokenExchangeError::Malformed);
                }
                p.advance();
                p.skip_whitespace();
                match key.as_str() {
                    "access_token" => {
                        access_token = Some(p.parse_string()?);
                    }
                    "refresh_token" => {
                        if p.peek_is_null() {
                            p.skip_null();
                            refresh_token = None;
                        } else {
                            refresh_token = Some(p.parse_string()?);
                        }
                    }
                    "expires_in" => {
                        let n =
                            p.parse_number_u64()
                                .ok_or(TokenExchangeError::InvalidFieldType {
                                    field: "expires_in",
                                })?;
                        expires_in = Some(n);
                    }
                    "scope" => {
                        if p.peek_is_null() {
                            p.skip_null();
                            scope = None;
                        } else {
                            scope = Some(p.parse_string()?);
                        }
                    }
                    "id_token" => {
                        if p.peek_is_null() {
                            p.skip_null();
                            id_token = None;
                        } else {
                            id_token = Some(p.parse_string()?);
                        }
                    }
                    _ => {
                        // Skip unknown values
                        p.skip_value()?;
                    }
                }
                p.skip_whitespace();
                if p.peek() == Some(b',') {
                    p.advance();
                }
            }
            _ => return Err(TokenExchangeError::Malformed),
        }
    }
    Ok(TokenExchangeResponse {
        access_token: access_token.ok_or(TokenExchangeError::MissingAccessToken)?,
        refresh_token,
        expires_in: expires_in.ok_or(TokenExchangeError::MissingExpiresIn)?,
        scope,
        id_token,
    })
}

/// Compute the absolute Unix-millis expiry from a parsed response.
///
/// `expires_in` is in seconds, so it is multiplied by 1000 and added
/// to `now_ms`.
pub fn expires_at_from_response(response: &TokenExchangeResponse, now_ms: u64) -> u64 {
    now_ms + response.expires_in * 1000
}

// Hand-rolled minimal JSON parser. Not general-purpose — only
// supports the subset of JSON the token endpoint actually returns.
struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            bytes: s.as_bytes(),
            pos: 0,
        }
    }
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }
    fn advance(&mut self) {
        self.pos += 1;
    }
    fn skip_whitespace(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.advance();
            } else {
                break;
            }
        }
    }
    fn peek_is_null(&self) -> bool {
        self.bytes.get(self.pos..self.pos + 4) == Some(b"null")
    }
    fn skip_null(&mut self) {
        if self.peek_is_null() {
            self.pos += 4;
        }
    }
    fn parse_string(&mut self) -> Result<String, TokenExchangeError> {
        if self.peek() != Some(b'"') {
            return Err(TokenExchangeError::Malformed);
        }
        self.advance();
        let mut out = String::new();
        while let Some(b) = self.peek() {
            match b {
                b'"' => {
                    self.advance();
                    return Ok(out);
                }
                b'\\' => {
                    self.advance();
                    let esc = self.peek().ok_or(TokenExchangeError::Malformed)?;
                    self.advance();
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'u' => {
                            // \uXXXX — only handles BMP characters
                            if self.pos + 4 > self.bytes.len() {
                                return Err(TokenExchangeError::Malformed);
                            }
                            let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
                                .map_err(|_| TokenExchangeError::Malformed)?;
                            let code = u32::from_str_radix(hex, 16)
                                .map_err(|_| TokenExchangeError::Malformed)?;
                            if let Some(c) = char::from_u32(code) {
                                out.push(c);
                            } else {
                                return Err(TokenExchangeError::Malformed);
                            }
                            self.pos += 4;
                        }
                        _ => return Err(TokenExchangeError::Malformed),
                    }
                }
                _ => {
                    out.push(b as char);
                    self.advance();
                }
            }
        }
        Err(TokenExchangeError::Malformed)
    }
    fn parse_number_u64(&mut self) -> Option<u64> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }
        let slice = std::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
        slice.parse::<u64>().ok()
    }
    fn skip_value(&mut self) -> Result<(), TokenExchangeError> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'"') => {
                self.parse_string()?;
                Ok(())
            }
            Some(b'{') => {
                let mut depth = 0;
                while let Some(b) = self.peek() {
                    if b == b'{' {
                        depth += 1;
                    } else if b == b'}' {
                        depth -= 1;
                        if depth == 0 {
                            self.advance();
                            return Ok(());
                        }
                    } else if b == b'"' {
                        self.parse_string()?;
                        continue;
                    }
                    self.advance();
                }
                Err(TokenExchangeError::Malformed)
            }
            Some(b'[') => {
                let mut depth = 0;
                while let Some(b) = self.peek() {
                    if b == b'[' {
                        depth += 1;
                    } else if b == b']' {
                        depth -= 1;
                        if depth == 0 {
                            self.advance();
                            return Ok(());
                        }
                    } else if b == b'"' {
                        self.parse_string()?;
                        continue;
                    }
                    self.advance();
                }
                Err(TokenExchangeError::Malformed)
            }
            Some(b) if b == b'-' || b.is_ascii_digit() => {
                while let Some(b) = self.peek() {
                    if b.is_ascii_digit()
                        || b == b'.'
                        || b == b'-'
                        || b == b'+'
                        || b == b'e'
                        || b == b'E'
                    {
                        self.advance();
                    } else {
                        break;
                    }
                }
                Ok(())
            }
            Some(b't') | Some(b'f') | Some(b'n') => {
                while let Some(b) = self.peek() {
                    if b.is_ascii_alphabetic() {
                        self.advance();
                    } else {
                        break;
                    }
                }
                Ok(())
            }
            _ => Err(TokenExchangeError::Malformed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_response() {
        let r =
            parse_token_exchange_response(r#"{"access_token":"AT","expires_in":3600}"#).unwrap();
        assert_eq!(r.access_token, "AT");
        assert_eq!(r.expires_in, 3600);
        assert_eq!(r.refresh_token, None);
        assert_eq!(r.scope, None);
    }

    #[test]
    fn parse_full_response() {
        let json = r#"{
            "access_token": "oai_at",
            "refresh_token": "oai_rt",
            "expires_in": 86400,
            "scope": "openid profile email offline_access",
            "id_token": "eyJhbGc.eyJzdWI.signature"
        }"#;
        let r = parse_token_exchange_response(json).unwrap();
        assert_eq!(r.access_token, "oai_at");
        assert_eq!(r.refresh_token.as_deref(), Some("oai_rt"));
        assert_eq!(r.expires_in, 86400);
        assert_eq!(
            r.scope.as_deref(),
            Some("openid profile email offline_access")
        );
        assert_eq!(r.id_token.as_deref(), Some("eyJhbGc.eyJzdWI.signature"));
    }

    #[test]
    fn parse_openai_response_with_id_token() {
        let json = r#"{
            "access_token": "oai_at",
            "refresh_token": "oai_rt",
            "expires_in": 3600,
            "id_token": "eyJhbGc.eyJzdWI.signature"
        }"#;
        let r = parse_token_exchange_response(json).unwrap();
        assert_eq!(r.id_token.as_deref(), Some("eyJhbGc.eyJzdWI.signature"));
    }

    #[test]
    fn parse_response_with_null_refresh_token() {
        let r = parse_token_exchange_response(
            r#"{"access_token":"AT","expires_in":3600,"refresh_token":null}"#,
        )
        .unwrap();
        assert_eq!(r.refresh_token, None);
    }

    #[test]
    fn parse_response_missing_access_token_errors() {
        let err = parse_token_exchange_response(r#"{"expires_in":3600}"#).unwrap_err();
        assert_eq!(err, TokenExchangeError::MissingAccessToken);
    }

    #[test]
    fn parse_response_missing_expires_in_errors() {
        let err = parse_token_exchange_response(r#"{"access_token":"AT"}"#).unwrap_err();
        assert_eq!(err, TokenExchangeError::MissingExpiresIn);
    }

    #[test]
    fn parse_response_not_an_object_errors() {
        let err = parse_token_exchange_response(r#"["not","an","object"]"#).unwrap_err();
        assert_eq!(err, TokenExchangeError::NotAnObject);
    }

    #[test]
    fn parse_response_with_unknown_fields_succeeds() {
        let json = r#"{
            "access_token": "AT",
            "expires_in": 3600,
            "irrelevant_field": "ignored",
            "another_one": 42
        }"#;
        let r = parse_token_exchange_response(json).unwrap();
        assert_eq!(r.access_token, "AT");
    }

    #[test]
    fn parse_response_with_escaped_string() {
        let json = r#"{"access_token":"a\nb\"c\\d","expires_in":1}"#;
        let r = parse_token_exchange_response(json).unwrap();
        assert_eq!(r.access_token, "a\nb\"c\\d");
    }

    #[test]
    fn parse_response_with_nested_unknown_object() {
        let json = r#"{
            "access_token": "AT",
            "expires_in": 1,
            "nested": {"foo": {"bar": [1,2,3]}, "baz": "qux"}
        }"#;
        let r = parse_token_exchange_response(json).unwrap();
        assert_eq!(r.access_token, "AT");
    }

    #[test]
    fn expires_at_multiplies_seconds_by_1000() {
        let r = TokenExchangeResponse {
            access_token: "AT".into(),
            refresh_token: None,
            expires_in: 3600,
            scope: None,
            id_token: None,
        };
        assert_eq!(expires_at_from_response(&r, 0), 3_600_000);
        assert_eq!(expires_at_from_response(&r, 1_000), 3_601_000);
    }
}
