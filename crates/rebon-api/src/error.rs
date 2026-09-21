//! Error types for model clients.
//!
//! Splits failures into transient and permanent classes, plus a
//! dedicated `Cancelled` variant for `AbortSignal`-driven cancels.

use std::time::Duration;

use thiserror::Error;

/// Metadata the retry middleware reads from transient / overloaded
/// errors to decide *how long* to wait before the next attempt.
#[derive(Debug, Clone, Default)]
pub struct RetryHint {
    /// HTTP status that triggered the error (e.g. 429, 503, 529).
    pub status: Option<u16>,
    /// Server-requested delay parsed from the `retry-after` header.
    pub retry_after: Option<Duration>,
}

/// Parse a `retry-after` header value into a [`Duration`].
///
/// Supports integer-seconds (`"5"`) only — HTTP-date format is rare
/// for API servers and not worth the extra dependency.
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?;
    let seconds: u64 = value.trim().parse().ok()?;
    Some(Duration::from_secs(seconds))
}

/// Parsed fields from a context window overflow error.
///
/// Some providers expose exact token counts, others only expose a
/// qualitative "context_length_exceeded" style error. The retry
/// middleware only needs to know whether the request exceeded the
/// window; exact counts are optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextOverflow {
    pub input_tokens: Option<u32>,
    pub max_tokens: Option<u32>,
    pub context_limit: Option<u32>,
}

/// Floor value for max_tokens when adjusting for context overflow.
pub const FLOOR_OUTPUT_TOKENS: u32 = 3000;

/// Try to parse a context overflow error message.
pub fn parse_context_overflow(message: &str) -> Option<ContextOverflow> {
    if message.contains("input length and `max_tokens` exceed context limit") {
        let re_part = message.rsplit("context limit:").next()?;
        let mut nums = re_part
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty());
        let input_tokens: u32 = nums.next()?.parse().ok()?;
        let max_tokens: u32 = nums.next()?.parse().ok()?;
        let context_limit: u32 = nums.next()?.parse().ok()?;
        return Some(ContextOverflow {
            input_tokens: Some(input_tokens),
            max_tokens: Some(max_tokens),
            context_limit: Some(context_limit),
        });
    }

    if message.contains("context_length_exceeded")
        || message.contains("context window exceeded")
        || message.contains("Your input exceeds the context window")
    {
        return Some(ContextOverflow {
            input_tokens: None,
            max_tokens: None,
            context_limit: None,
        });
    }

    None
}

/// Unified error type returned from every model client method.
#[derive(Debug, Error)]
pub enum ModelError {
    /// Authentication failure (401). The caller should refresh the
    /// API key / OAuth token and retry.
    #[error("model client unauthorized: {0}")]
    Unauthorized(String),
    /// The request was malformed or rejected. Retrying would not
    /// help.
    #[error("model client bad request: {0}")]
    BadRequest(String),
    /// The model provider returned a permanent failure (non-401 4xx,
    /// unrecoverable 5xx, content-policy refusal, …).
    #[error("model client permanent failure: {0}")]
    Permanent(String),
    /// The request failed transiently (network error, 429, 5xx the
    /// retry wrapper considers retryable). The caller may retry.
    #[error("model client transient failure: {message}")]
    Transient {
        message: String,
        /// Retry metadata the middleware can use to decide delay.
        hint: RetryHint,
    },
    /// The server returned 529 (overloaded). Separated from
    /// [`Transient`] so the retry middleware can apply special logic
    /// (consecutive-529 counter, foreground-only retry).
    #[error("model client overloaded: {message}")]
    Overloaded { message: String, hint: RetryHint },
    /// The provider response could not be parsed. Treated as
    /// permanent.
    #[error("model client protocol error: {0}")]
    Protocol(String),
    /// The operation was cancelled via `AbortSignal` (or its Rust
    /// equivalent).
    #[error("model client cancelled")]
    Cancelled,
    /// Low-level HTTP / transport error surfaced from the HTTP client.
    #[error("model client http error: {0}")]
    Http(String),
    /// I/O error reading the response stream.
    #[error("model client io error: {0}")]
    Io(String),
    /// Catch-all for implementation-specific errors that don't fit
    /// the buckets above.
    #[error("model client error: {0}")]
    Other(String),
}

impl ModelError {
    /// Ad-hoc constructor.
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    /// Convenience constructor for simple transient errors (no HTTP
    /// metadata). Keeps existing call sites compact.
    pub fn transient(msg: impl Into<String>) -> Self {
        Self::Transient {
            message: msg.into(),
            hint: RetryHint::default(),
        }
    }

    /// Transient error with full HTTP metadata for the retry
    /// middleware.
    pub fn transient_http(
        msg: impl Into<String>,
        status: u16,
        retry_after: Option<Duration>,
    ) -> Self {
        Self::Transient {
            message: msg.into(),
            hint: RetryHint {
                status: Some(status),
                retry_after,
            },
        }
    }

    /// Overloaded (529) error.
    pub fn overloaded(msg: impl Into<String>, retry_after: Option<Duration>) -> Self {
        Self::Overloaded {
            message: msg.into(),
            hint: RetryHint {
                status: Some(529),
                retry_after,
            },
        }
    }

    /// True when the retry wrapper should back off and try again.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Transient { .. } | Self::Overloaded { .. } | Self::Http(_)
        )
    }

    /// True specifically for 529 / overloaded errors.
    pub fn is_overloaded(&self) -> bool {
        matches!(self, Self::Overloaded { .. })
    }

    /// True specifically for 429 rate-limit errors.
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, Self::Transient { hint, .. } if hint.status == Some(429))
    }

    /// True when the error is a context-overflow that can be retried
    /// with a smaller `max_tokens` or a session state reset.
    ///
    /// Checks `BadRequest` (HTTP path), `Permanent` (WS translator
    /// classifies `context_length_exceeded` as permanent), and `Http`
    /// (legacy classification) so that engine-level recovery works
    /// regardless of which transport surfaced the overflow.
    pub fn context_overflow(&self) -> Option<ContextOverflow> {
        match self {
            Self::BadRequest(msg) | Self::Permanent(msg) | Self::Http(msg) => {
                parse_context_overflow(msg)
            }
            _ => None,
        }
    }

    /// Server-requested retry delay, if any.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Transient { hint, .. } | Self::Overloaded { hint, .. } => hint.retry_after,
            _ => None,
        }
    }

    /// The HTTP status that produced this error, if known.
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::Transient { hint, .. } | Self::Overloaded { hint, .. } => hint.status,
            _ => None,
        }
    }
}

impl From<reqwest::Error> for ModelError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            Self::transient(format!("request timed out: {err}"))
        } else if err.is_connect() {
            Self::transient(format!("connect failed: {err}"))
        } else if let Some(status) = err.status() {
            if status.as_u16() == 401 {
                Self::Unauthorized(err.to_string())
            } else if status.is_client_error() {
                Self::BadRequest(err.to_string())
            } else if status.is_server_error() {
                Self::transient(err.to_string())
            } else {
                Self::Http(err.to_string())
            }
        } else {
            Self::Http(err.to_string())
        }
    }
}

impl From<serde_json::Error> for ModelError {
    fn from(err: serde_json::Error) -> Self {
        Self::Protocol(format!("json parse: {err}"))
    }
}

impl From<std::io::Error> for ModelError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

/// Strip anything credential-shaped out of a string bound for a log
/// file or the screen.
///
/// The log is the thing a user sends back with a bug report, so it has
/// to be safe to send without anyone reading it first. Provider errors
/// carry more than the failure: a transport error prints the URL it was
/// dialing (and a proxy or gateway base URL can carry the key in its
/// query string), and an error body can quote the `Authorization`
/// header back.
///
/// Shape-based rather than name-based on purpose. Keys are recognizable
/// by their prefixes across providers, and a redactor that only knew
/// the names Rebon uses today would leak the next provider's.
pub fn redact_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < text.len() {
        if !text.is_char_boundary(index) {
            index += 1;
            continue;
        }
        let rest = &text[index..];
        if let Some(token_len) = credential_prefix_len(rest) {
            out.push_str(REDACTED);
            index += token_len;
            continue;
        }
        if let Some((param_len, value_len)) = query_secret_len(rest) {
            out.push_str(&rest[..param_len]);
            out.push_str(REDACTED);
            index += param_len + value_len;
            continue;
        }
        let ch_len = rest.chars().next().map_or(1, char::len_utf8);
        out.push_str(&rest[..ch_len]);
        index += ch_len;
        let _ = bytes;
    }
    out
}

const REDACTED: &str = "[redacted]";

/// Key prefixes worth recognizing on sight. `sk-` covers OpenAI and
/// Anthropic (`sk-ant-`), `ghp_` / `github_pat_` a token pasted into a
/// header, `AIza` a Google key, `ya29.` a Google OAuth access token.
const CREDENTIAL_PREFIXES: [&str; 6] = ["sk-", "ghp_", "github_pat_", "AIza", "ya29.", "xoxb-"];

/// How long the credential starting at the front of `rest` is, or
/// `None` when nothing credential-shaped starts there.
///
/// A bare `Bearer ` is handled here too: the word is not the secret,
/// but everything up to the next whitespace after it is.
fn credential_prefix_len(rest: &str) -> Option<usize> {
    if let Some(after) = rest.strip_prefix("Bearer ") {
        let value_len = after
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .unwrap_or(after.len());
        if value_len > 0 {
            return Some("Bearer ".len() + value_len);
        }
    }
    let prefix = CREDENTIAL_PREFIXES
        .into_iter()
        .find(|prefix| rest.starts_with(prefix))?;
    let len = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
        .unwrap_or(rest.len());
    // A prefix on its own is a word, not a key: "sk-" in prose stays.
    (len > prefix.len() + 4).then_some(len)
}

/// URL query parameters that name a secret. Returns the length of the
/// `name=` part and the length of the value that follows it.
fn query_secret_len(rest: &str) -> Option<(usize, usize)> {
    const SECRET_PARAMS: [&str; 5] = ["key=", "api_key=", "apikey=", "token=", "access_token="];
    let param = SECRET_PARAMS
        .into_iter()
        .filter(|param| {
            rest.get(..param.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(param))
                && rest.len() > param.len()
        })
        // Longest first so `api_key=` is not read as `key=`.
        .max_by_key(|param| param.len())?;
    let after = &rest[param.len()..];
    let value_len = after
        .find(|c: char| matches!(c, '&' | '#' | ' ' | '"' | '\'' | ')'))
        .unwrap_or(after.len());
    (value_len > 0).then_some((param.len(), value_len))
}

/// Result alias every model client method returns.
pub type ModelResult<T> = Result<T, ModelError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_takes_out_keys_bearer_tokens_and_query_secrets() {
        assert_eq!(
            redact_secrets("connect failed: x-api-key sk-ant-api03-AAAABBBBCCCC rejected"),
            "connect failed: x-api-key [redacted] rejected"
        );
        assert_eq!(
            redact_secrets("Authorization: Bearer ya29.a0AfB_abcdef"),
            "Authorization: [redacted]"
        );
        assert_eq!(
            redact_secrets("https://gw.example.com/v1/messages?api_key=abc123&beta=1"),
            "https://gw.example.com/v1/messages?api_key=[redacted]&beta=1"
        );
        assert_eq!(
            redact_secrets("https://gw.example.com/v1?key=abc123"),
            "https://gw.example.com/v1?key=[redacted]"
        );
    }

    #[test]
    fn redaction_leaves_ordinary_text_alone() {
        let plain = "connect failed: dns error for api.anthropic.com (os error 11001)";
        assert_eq!(redact_secrets(plain), plain);
        // A prefix that is only a word is not a key.
        assert_eq!(redact_secrets("the sk- prefix"), "the sk- prefix");
        // Nothing after `Bearer` is nothing to redact.
        assert_eq!(redact_secrets("Bearer "), "Bearer ");
    }

    #[test]
    fn redaction_is_utf8_safe() {
        let text = "连接失败：sk-ant-api03-AAAABBBBCCCC 不可用";
        assert_eq!(redact_secrets(text), "连接失败：[redacted] 不可用");
    }

    #[test]
    fn transient_flag_matches_variants() {
        assert!(ModelError::transient("hiccup").is_transient());
        assert!(ModelError::overloaded("busy", None).is_transient());
        assert!(ModelError::Http("bad gateway".into()).is_transient());
        assert!(!ModelError::Permanent("gone".into()).is_transient());
        assert!(!ModelError::Cancelled.is_transient());
    }

    #[test]
    fn overloaded_flag() {
        assert!(ModelError::overloaded("529", None).is_overloaded());
        assert!(!ModelError::transient("429").is_overloaded());
    }

    #[test]
    fn rate_limited_flag() {
        assert!(ModelError::transient_http("rate limit", 429, None).is_rate_limited());
        assert!(!ModelError::transient_http("server error", 503, None).is_rate_limited());
    }

    #[test]
    fn retry_after_extraction() {
        let err = ModelError::transient_http("slow down", 429, Some(Duration::from_secs(5)));
        assert_eq!(err.retry_after(), Some(Duration::from_secs(5)));
        assert_eq!(ModelError::transient("no hint").retry_after(), None);
    }

    #[test]
    fn context_overflow_parsing() {
        let msg =
            "400: input length and `max_tokens` exceed context limit: 188059 + 20000 > 200000";
        let parsed = parse_context_overflow(msg).unwrap();
        assert_eq!(parsed.input_tokens, Some(188059));
        assert_eq!(parsed.max_tokens, Some(20000));
        assert_eq!(parsed.context_limit, Some(200000));
    }

    #[test]
    fn context_overflow_returns_none_for_unrelated_messages() {
        assert!(parse_context_overflow("some other error").is_none());
    }

    #[test]
    fn context_overflow_via_bad_request_error() {
        let err = ModelError::BadRequest(
            "400: input length and `max_tokens` exceed context limit: 100000 + 50000 > 128000"
                .into(),
        );
        let overflow = err.context_overflow().unwrap();
        assert_eq!(overflow.input_tokens, Some(100000));
        assert_eq!(overflow.context_limit, Some(128000));
    }

    #[test]
    fn json_error_converts_to_protocol() {
        let err: ModelError = serde_json::from_str::<u32>("not a number")
            .err()
            .unwrap()
            .into();
        assert!(matches!(err, ModelError::Protocol(_)));
    }

    #[test]
    fn parse_retry_after_header() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "10".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(10)));

        let empty = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&empty), None);
    }
}
