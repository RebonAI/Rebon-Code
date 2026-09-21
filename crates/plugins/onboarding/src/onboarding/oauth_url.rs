//! OAuth authorize URL builder.
//!
//! Two things:
//!
//! * [`build_openai_authorize_url`] — the OpenAI Codex authorize URL.
//! * The pinned OpenAI endpoint + client constants it is built from.
//!
//! ## Pinned rules (each with a positive + negative test)
//!
//! 1. The query parameters appear in the **exact order**
//! [`build_openai_authorize_url`] appends them. The order matters for
//! tests AND for some
//! middleboxes that hash the URL.
//! 2. `code_challenge_method` is always `S256`. The `plain` variant
//! is forbidden.
//! 3. `scope` for the OpenAI flow is the literal string `"openid
//! profile email offline_access"`.
//! 4. The OpenAI redirect URI is fixed: `http://localhost:1455/auth/callback`.
//! There is no manual fallback.
//! 5. The state parameter MUST be non-empty. Building a URL with an
//! empty state returns [`AuthorizeUrlError::EmptyState`].
//! 6. The code-challenge MUST be non-empty. Building a URL with an
//! empty challenge returns [`AuthorizeUrlError::EmptyChallenge`].
//! 7. URL parameter values are percent-encoded for the unsafe
//! subset (`+`, ` `, `:`, `/`, `?`, `#`, `&`, `=`, `%`, plus
//! non-ASCII). The encoding is RFC 3986 form-data style.
//! 8. The OpenAI URL includes two extra params: `id_token_add_organizations=true`
//! and `codex_cli_simplified_flow=true`. Both are pinned in
//! [`build_openai_authorize_url`].

// Pinned OpenAI endpoint + client constants.
pub const OPENAI_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const OPENAI_REDIRECT_PORT: u16 = 1455;
pub const OPENAI_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const OPENAI_CALLBACK_PATH: &str = "/auth/callback";
pub const OPENAI_SCOPES: &str = "openid profile email offline_access";

/// Inputs to [`build_openai_authorize_url`].
#[derive(Debug, Clone)]
pub struct OpenAIAuthInputs<'a> {
    pub code_challenge: &'a str,
    pub state: &'a str,
}

/// Errors returned by the URL builders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizeUrlError {
    EmptyChallenge,
    EmptyState,
}

/// Build the OpenAI Codex authorize URL.
///
/// Query parameters are appended in the pinned order listed above.
pub fn build_openai_authorize_url(
    inputs: &OpenAIAuthInputs<'_>,
) -> Result<String, AuthorizeUrlError> {
    if inputs.code_challenge.is_empty() {
        return Err(AuthorizeUrlError::EmptyChallenge);
    }
    if inputs.state.is_empty() {
        return Err(AuthorizeUrlError::EmptyState);
    }
    let mut url = String::new();
    url.push_str(OPENAI_AUTHORIZE_URL);
    url.push('?');
    append_param(&mut url, "response_type", "code", true);
    append_param(&mut url, "client_id", OPENAI_CLIENT_ID, false);
    append_param(&mut url, "redirect_uri", OPENAI_REDIRECT_URI, false);
    append_param(&mut url, "scope", OPENAI_SCOPES, false);
    append_param(&mut url, "code_challenge", inputs.code_challenge, false);
    append_param(&mut url, "code_challenge_method", "S256", false);
    append_param(&mut url, "id_token_add_organizations", "true", false);
    append_param(&mut url, "codex_cli_simplified_flow", "true", false);
    append_param(&mut url, "state", inputs.state, false);
    Ok(url)
}

fn append_param(url: &mut String, key: &str, value: &str, is_first: bool) {
    if !is_first {
        url.push('&');
    }
    url.push_str(key);
    url.push('=');
    percent_encode_into(value, url);
}

/// Percent-encode a value for URL query parameters. Encodes the
/// "unsafe" subset per RFC 3986 application/x-www-form-urlencoded:
///
/// * Letters, digits, `-`, `_`, `.`, `~` pass through.
/// * Spaces become `%20`, never `+`.
/// * Everything else is `%XX`, using uppercase hex digits.
fn percent_encode_into(value: &str, out: &mut String) {
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(hex_uppercase_nibble(byte >> 4));
                out.push(hex_uppercase_nibble(byte & 0x0f));
            }
        }
    }
}

fn hex_uppercase_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'A' + n - 10) as char,
        _ => '?',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_openai_constants() {
        assert_eq!(
            OPENAI_AUTHORIZE_URL,
            "https://auth.openai.com/oauth/authorize"
        );
        assert_eq!(OPENAI_TOKEN_URL, "https://auth.openai.com/oauth/token");
        assert_eq!(OPENAI_CLIENT_ID, "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(OPENAI_REDIRECT_PORT, 1455);
        assert_eq!(OPENAI_REDIRECT_URI, "http://localhost:1455/auth/callback");
        assert_eq!(OPENAI_SCOPES, "openid profile email offline_access");
    }

    #[test]
    fn openai_url_basic() {
        let url = build_openai_authorize_url(&OpenAIAuthInputs {
            code_challenge: "CHAL",
            state: "STAT",
        })
        .unwrap();
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        assert!(url.contains("response_type=code&"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann&"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback&"));
        assert!(url.contains("scope=openid%20profile%20email%20offline_access&"));
        assert!(url.contains("code_challenge=CHAL&"));
        assert!(url.contains("code_challenge_method=S256&"));
        assert!(url.contains("id_token_add_organizations=true&"));
        assert!(url.contains("codex_cli_simplified_flow=true&"));
        assert!(url.contains("state=STAT"));
    }

    #[test]
    fn openai_url_param_order_is_pinned() {
        let url = build_openai_authorize_url(&OpenAIAuthInputs {
            code_challenge: "C",
            state: "S",
        })
        .unwrap();
        let q = url.split_once('?').unwrap().1;
        let mut iter = q.split('&').map(|p| p.split('=').next().unwrap());
        assert_eq!(iter.next(), Some("response_type"));
        assert_eq!(iter.next(), Some("client_id"));
        assert_eq!(iter.next(), Some("redirect_uri"));
        assert_eq!(iter.next(), Some("scope"));
        assert_eq!(iter.next(), Some("code_challenge"));
        assert_eq!(iter.next(), Some("code_challenge_method"));
        assert_eq!(iter.next(), Some("id_token_add_organizations"));
        assert_eq!(iter.next(), Some("codex_cli_simplified_flow"));
        assert_eq!(iter.next(), Some("state"));
    }

    #[test]
    fn openai_url_empty_challenge_errors() {
        let err = build_openai_authorize_url(&OpenAIAuthInputs {
            code_challenge: "",
            state: "S",
        })
        .unwrap_err();
        assert_eq!(err, AuthorizeUrlError::EmptyChallenge);
    }

    #[test]
    fn openai_url_empty_state_errors() {
        let err = build_openai_authorize_url(&OpenAIAuthInputs {
            code_challenge: "C",
            state: "",
        })
        .unwrap_err();
        assert_eq!(err, AuthorizeUrlError::EmptyState);
    }

    #[test]
    fn percent_encoding_passes_through_alphanumeric_and_unreserved() {
        let mut s = String::new();
        percent_encode_into("abcXYZ123-_.~", &mut s);
        assert_eq!(s, "abcXYZ123-_.~");
    }

    #[test]
    fn percent_encoding_encodes_space_as_percent_20() {
        let mut s = String::new();
        percent_encode_into("a b c", &mut s);
        assert_eq!(s, "a%20b%20c");
    }

    #[test]
    fn percent_encoding_encodes_colon_and_slash() {
        let mut s = String::new();
        percent_encode_into("http://x/y", &mut s);
        assert_eq!(s, "http%3A%2F%2Fx%2Fy");
    }

    #[test]
    fn percent_encoding_encodes_at_sign() {
        let mut s = String::new();
        percent_encode_into("u@e.com", &mut s);
        assert_eq!(s, "u%40e.com");
    }

    #[test]
    fn percent_encoding_handles_non_ascii() {
        let mut s = String::new();
        percent_encode_into("café", &mut s);
        assert_eq!(s, "caf%C3%A9");
    }
}
