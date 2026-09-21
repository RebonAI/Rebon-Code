//! OpenAI Codex OAuth login flow.
//!
//! Orchestrates the three phases of a fresh OAuth login in-process:
//!
//! 1. **Prepare** — generate PKCE verifier + state, build the
//!    authorize URL via [`crate::onboarding::build_openai_authorize_url`],
//!    probe port 1455 to decide whether the auto-callback path is
//!    available or the paste-fallback path will be needed.
//! 2. **Collect** — either run the local [`crate::oauth::listener`] (the user
//!    lands back on `localhost:1455/auth/callback` after approving
//!    the consent screen) or parse a pasted redirect URL (browsers
//!    still show the `?code=…&state=…` in the URL bar even when
//!    nothing is listening on 1455).
//! 3. **Exchange + persist** — POST the code to
//!    `https://auth.openai.com/oauth/token` with the PKCE verifier,
//!    then write the tokens to `.credentials.json::openaiOAuth` and
//!    upsert the synthetic `openai` provider entry in `config.json`
//!    so the next session picks it up transparently via the
//!    [`OPENAI_OAUTH_TOKEN_SENTINEL`] indirection.
//!
//! Each phase is a free function (not a reducer) so the TUI dialog
//! can drive the state machine itself — that keeps the phases
//! individually testable and lets the dialog observe cancellation
//! between any two steps.
//!
//! ## Coverage matrix
//!
//! * `prepare_builds_valid_url_with_port_flag` — happy path, port
//!   probe reflects real availability.
//! * `prepare_marks_port_busy_when_1455_held` — the port-conflict
//!   branch that drives the paste fallback.
//! * `parse_pasted_callback_accepts_full_url` — user pastes the
//!   entire `http://localhost:1455/auth/callback?code=…&state=…`
//!   URL from the browser bar.
//! * `parse_pasted_callback_accepts_query_only` — user pastes just
//!   the query string.
//! * `parse_pasted_callback_accepts_code_hash_state` — user pastes
//!   `code#state`.
//! * `parse_pasted_callback_rejects_state_mismatch` — paste with
//!   wrong state → `StateMismatch`.
//! * `parse_pasted_callback_rejects_empty_input`.
//! * `exchange_token_request_body_is_pinned` — the form body sent
//!   to `OPENAI_TOKEN_URL` keeps the expected field order.
//!
//! Live-network tests (real POST to auth.openai.com) are not run
//! here — they live in the sibling [`crate::onboarding::token_response`]
//! tests plus manual QA.

use std::path::Path;
use std::time::Duration;

use crate::onboarding::{
    build_openai_authorize_url, code_challenge_s256, encode_state, encode_verifier,
    parse_callback_url, parse_pasted_code, CodeParseError, OPENAI_REDIRECT_PORT, OPENAI_TOKEN_URL,
};

use rebon_config::{
    self, config_home_dir, upsert_openai_oauth_provider_in, OpenAIOAuthTokens,
    OPENAI_OAUTH_PROVIDER_MODELS,
};

use crate::oauth::listener::{self, CallbackResult};

/// Max time we'll hold the listener open waiting for the browser
/// callback. The user might context-switch between "opening the
/// URL" and "clicking approve" — 5 minutes is the same cap the
/// same flow uses.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// Max wall-clock time for the token-exchange POST. Stricter than
/// the callback wait because this runs after the user has already
/// clicked approve — a hang here is a network issue, not a user
/// pacing issue.
pub const TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Opaque challenge carried between the prepare phase and the
/// exchange phase. The `verifier` MUST be preserved across the
/// browser-open boundary — that's what proves the token request
/// came from the same client that asked for the authorization
/// code.
#[derive(Debug, Clone)]
pub struct OAuthChallenge {
    pub authorize_url: String,
    pub state: String,
    pub verifier: String,
    /// `true` when the port probe said 1455 was free at prepare
    /// time — the caller should start the local listener via
    /// [`crate::oauth::listener::wait_for_callback_with_cancel`].
    /// `false` means the caller should show the paste UI.
    pub port_available: bool,
}

/// Errors surfaced by this module. Each variant is either a
/// user-facing stop (show the message in the dialog) or a
/// recoverable signal the dialog uses to pick a branch.
#[derive(Debug)]
pub enum FlowError {
    /// PKCE / authorize URL assembly failed. Should never happen —
    /// the URL builder only fails on empty challenge / state and
    /// we supply both.
    UrlBuild(String),
    /// The caller supplied an empty / whitespace paste.
    EmptyPaste,
    /// The pasted input did not match any of the accepted formats
    /// (full URL, query-only, `code#state`).
    ParsePaste(String),
    /// The pasted or listened-for `state` did not match the value
    /// we generated — CSRF guard or stale browser tab.
    StateMismatch { expected: String, actual: String },
    /// The token-exchange POST failed (non-200, network, or JSON
    /// parse).
    TokenExchange(String),
    /// Persisting credentials or upserting the provider entry
    /// failed. Split out from TokenExchange so the user sees
    /// "login worked but we couldn't save it" rather than
    /// "login failed".
    Persist(String),
}

impl std::fmt::Display for FlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UrlBuild(msg) => write!(f, "failed to build authorize URL: {msg}"),
            Self::EmptyPaste => write!(f, "no callback URL or code pasted"),
            Self::ParsePaste(msg) => write!(f, "could not parse pasted callback: {msg}"),
            Self::StateMismatch { expected, actual } => write!(
                f,
                "state mismatch: expected `{expected}`, got `{actual}` \
                 — retry login to get a fresh link"
            ),
            Self::TokenExchange(msg) => write!(f, "token exchange failed: {msg}"),
            Self::Persist(msg) => write!(f, "failed to persist credentials: {msg}"),
        }
    }
}

impl std::error::Error for FlowError {}

/// Build PKCE inputs and the authorize URL. The caller should show
/// the URL to the user (both as a clickable line and as a fallback
/// in case the browser launch below silently fails) and then open
/// the browser via [`launch_browser`].
pub fn prepare_openai_oauth() -> Result<OAuthChallenge, FlowError> {
    let verifier_bytes = random_32();
    let state_bytes = random_32();
    let verifier = encode_verifier(verifier_bytes);
    let state = encode_state(state_bytes);
    let challenge = code_challenge_s256(&verifier);

    let authorize_url = build_openai_authorize_url(&crate::onboarding::OpenAIAuthInputs {
        code_challenge: &challenge,
        state: &state,
    })
    .map_err(|err| FlowError::UrlBuild(format!("{err:?}")))?;

    let port_available = port_probe_ok(OPENAI_REDIRECT_PORT);

    Ok(OAuthChallenge {
        authorize_url,
        state,
        verifier,
        port_available,
    })
}

/// Open the URL in the OS default browser. Swallows errors
/// (returning `false`) because a failed launch is not fatal — the
/// dialog always displays the URL as text so the user can copy it
/// manually.
pub fn launch_browser(url: &str) -> bool {
    match webbrowser::open(url) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(error = %err, "oauth: browser launch failed, falling back to manual URL copy");
            false
        }
    }
}

/// Parse a pasted callback string. Accepts any of:
///
/// * A full URL: `http://localhost:1455/auth/callback?code=…&state=…`
///   (what the browser URL bar shows when the listener is down).
/// * A query string: `code=…&state=…` (what a user copies after
///   stripping the host by hand).
/// * `code#state` (the code and state joined by hand; OpenAI never
///   produces this form itself).
///
/// On success the returned `CallbackResult::state` is guaranteed to
/// equal `expected_state` — if it doesn't, `StateMismatch` is
/// returned instead.
pub fn parse_pasted_callback(
    input: &str,
    expected_state: &str,
) -> Result<CallbackResult, FlowError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(FlowError::EmptyPaste);
    }

    let (code, state) = if trimmed.contains("://") {
        // Full URL.
        let parsed = parse_callback_url(trimmed).map_err(map_parse_err)?;
        (parsed.code, parsed.state)
    } else if trimmed.contains('#') && !trimmed.contains('=') {
        // `code#state` — accepted only when no `=` is present;
        // anything with a `=` is likely a query string that happens
        // to contain a `#`.
        let parsed = parse_pasted_code(trimmed).map_err(map_parse_err)?;
        (parsed.authorization_code, parsed.state)
    } else if trimmed.contains('=') {
        // Bare query string — wrap in a fake URL and reuse
        // `parse_callback_url`.
        let synthetic = format!("http://x/?{}", trimmed.trim_start_matches('?'));
        let parsed = parse_callback_url(&synthetic).map_err(map_parse_err)?;
        (parsed.code, parsed.state)
    } else {
        return Err(FlowError::ParsePaste(
            "input must be a callback URL, `?code=…&state=…`, or `code#state`".into(),
        ));
    };

    if state != expected_state {
        return Err(FlowError::StateMismatch {
            expected: expected_state.to_string(),
            actual: state,
        });
    }
    Ok(CallbackResult { code, state })
}

fn map_parse_err(err: CodeParseError) -> FlowError {
    FlowError::ParsePaste(format!("{err:?}"))
}

/// POST the authorization code to `auth.openai.com/oauth/token`,
/// parse the tokens, write `.credentials.json::openaiOAuth`, and
/// upsert the synthetic `openai` provider entry so subsequent
/// sessions resolve it automatically.
///
/// Returns the fresh tokens on success. The tokens are the
/// caller's to display (access-token last 4, expiry time) if they
/// want a user-visible confirmation line.
pub async fn exchange_and_persist(
    code: &str,
    verifier: &str,
) -> Result<OpenAIOAuthTokens, FlowError> {
    let config_dir = config_home_dir();
    exchange_and_persist_in(&config_dir, code, verifier).await
}

/// [`exchange_and_persist`] for callers with no reactor of their own.
///
/// The TUI already owns a tokio runtime and bridges with its handle; the GPUI
/// app does not, and its executor is not tokio. Rather than make every such
/// caller stand up a runtime and get the "cannot block inside a runtime" rule
/// subtly wrong, own that here: a private current-thread runtime, built and
/// dropped around the one POST this flow makes.
///
/// Must be called from a plain worker thread. Calling it from inside a tokio
/// runtime would panic on the nested `block_on`, so runtime-owning callers use
/// the async form with their own handle instead.
pub fn exchange_and_persist_blocking(
    code: &str,
    verifier: &str,
) -> Result<OpenAIOAuthTokens, FlowError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| FlowError::TokenExchange(format!("build runtime: {err}")))?;
    runtime.block_on(exchange_and_persist(code, verifier))
}

pub async fn exchange_and_persist_in(
    config_dir: &Path,
    code: &str,
    verifier: &str,
) -> Result<OpenAIOAuthTokens, FlowError> {
    let tokens = exchange_code_for_tokens(code, verifier).await?;
    rebon_config::write_openai_oauth_tokens(config_dir, &tokens)
        .map_err(|err| FlowError::Persist(err.to_string()))?;
    let models: Vec<String> = OPENAI_OAUTH_PROVIDER_MODELS
        .iter()
        .map(|m| m.to_string())
        .collect();
    upsert_openai_oauth_provider_in(config_dir, &models)
        .map_err(|err| FlowError::Persist(err.to_string()))?;
    tracing::info!("oauth: OpenAI login complete, credentials persisted");
    Ok(tokens)
}

/// Issue the token-exchange POST and parse the response into an
/// [`OpenAIOAuthTokens`].
///
/// The form body order is pinned for deterministic token-exchange
/// requests.
async fn exchange_code_for_tokens(
    code: &str,
    verifier: &str,
) -> Result<OpenAIOAuthTokens, FlowError> {
    let http = reqwest::Client::builder()
        .timeout(TOKEN_EXCHANGE_TIMEOUT)
        .build()
        .map_err(|err| FlowError::TokenExchange(format!("build HTTP client: {err}")))?;

    let form = token_exchange_form(code, verifier);
    let response = http
        .post(OPENAI_TOKEN_URL)
        .form(&form)
        .send()
        .await
        .map_err(|err| FlowError::TokenExchange(format!("request: {err}")))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| FlowError::TokenExchange(format!("read body: {err}")))?;

    if !status.is_success() {
        return Err(FlowError::TokenExchange(format!(
            "OpenAI /oauth/token returned {status}: {body}"
        )));
    }

    let parsed = crate::onboarding::parse_token_exchange_response(&body)
        .map_err(|err| FlowError::TokenExchange(format!("parse response: {err:?}")))?;

    let expires_at = crate::onboarding::expires_at_from_response(&parsed, now_unix_ms());
    Ok(OpenAIOAuthTokens {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        expires_at: Some(expires_at),
    })
}

/// Build the form body for the token-exchange POST. Separated so
/// tests can pin the exact key/value order without a live network.
fn token_exchange_form<'a>(code: &'a str, verifier: &'a str) -> Vec<(&'static str, &'a str)> {
    vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", crate::onboarding::OPENAI_REDIRECT_URI),
        ("client_id", crate::onboarding::OPENAI_CLIENT_ID),
        ("code_verifier", verifier),
    ]
}

fn port_probe_ok(port: u16) -> bool {
    listener::probe_port_free(port).is_ok()
}

/// Draw 32 random bytes with `getrandom`. Panics on failure — there is
/// no sensible recovery from "the kernel can't give us entropy" during a
/// login flow.
fn random_32() -> [u8; 32] {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("getrandom: kernel entropy source unavailable");
    buf
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, TcpListener};

    #[test]
    fn prepare_builds_valid_url() {
        let challenge = prepare_openai_oauth().expect("prepare should succeed");
        assert!(
            challenge
                .authorize_url
                .starts_with("https://auth.openai.com/oauth/authorize?"),
            "url = {}",
            challenge.authorize_url
        );
        assert!(challenge
            .authorize_url
            .contains("code_challenge_method=S256"));
        assert!(!challenge.state.is_empty());
        assert!(!challenge.verifier.is_empty());
        // The verifier and state MUST be drawn from independent
        // randomness — if they're ever equal it means the
        // `random_32` calls were merged.
        assert_ne!(challenge.state, challenge.verifier);
    }

    #[test]
    #[ignore = "races with other tests that transiently bind port 1455; \
                port-probe correctness is covered by \
                listener::probe_port_free_{ok_when_unused,fails_when_bound}"]
    fn prepare_marks_port_busy_when_1455_held() {
        // Hold the port while we probe. If the port was already
        // busy before this test started (e.g. another instance of
        // rebon is running) the assertion below still holds — the
        // probe reports false either way. We only assert the
        // "truly free port" direction is NOT incorrectly flagged
        // busy if we can bind it ourselves.
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, OPENAI_REDIRECT_PORT));
        match held {
            Ok(_listener) => {
                // We now hold the port; a fresh prepare must see it
                // busy.
                let challenge = prepare_openai_oauth().unwrap();
                assert!(!challenge.port_available, "expected port busy while held");
            }
            Err(_) => {
                // Port wasn't free to begin with — can't test the
                // busy-branch from this side. Just confirm the
                // probe surfaces the state without panicking.
                let challenge = prepare_openai_oauth().unwrap();
                assert!(!challenge.port_available);
            }
        }
    }

    #[test]
    fn parse_pasted_callback_accepts_full_url() {
        let result = parse_pasted_callback(
            "http://localhost:1455/auth/callback?code=ABC&state=XYZ",
            "XYZ",
        )
        .unwrap();
        assert_eq!(result.code, "ABC");
        assert_eq!(result.state, "XYZ");
    }

    #[test]
    fn parse_pasted_callback_accepts_url_with_extra_params() {
        let result = parse_pasted_callback(
            "http://localhost:1455/auth/callback?scope=openid&code=ABC&state=XYZ&foo=bar",
            "XYZ",
        )
        .unwrap();
        assert_eq!(result.code, "ABC");
    }

    #[test]
    fn parse_pasted_callback_accepts_query_only() {
        let result = parse_pasted_callback("code=ABC&state=XYZ", "XYZ").unwrap();
        assert_eq!(result.code, "ABC");
        assert_eq!(result.state, "XYZ");
    }

    #[test]
    fn parse_pasted_callback_accepts_question_prefix() {
        let result = parse_pasted_callback("?code=ABC&state=XYZ", "XYZ").unwrap();
        assert_eq!(result.code, "ABC");
    }

    #[test]
    fn parse_pasted_callback_accepts_code_hash_state() {
        let result = parse_pasted_callback("ABC#XYZ", "XYZ").unwrap();
        assert_eq!(result.code, "ABC");
        assert_eq!(result.state, "XYZ");
    }

    #[test]
    fn parse_pasted_callback_trims_whitespace() {
        let result = parse_pasted_callback("  code=ABC&state=XYZ  \n", "XYZ").unwrap();
        assert_eq!(result.code, "ABC");
    }

    #[test]
    fn parse_pasted_callback_rejects_state_mismatch() {
        let err = parse_pasted_callback("code=ABC&state=WRONG", "RIGHT").unwrap_err();
        assert!(matches!(
            err,
            FlowError::StateMismatch { ref expected, ref actual }
                if expected == "RIGHT" && actual == "WRONG"
        ));
    }

    #[test]
    fn parse_pasted_callback_rejects_empty_input() {
        let err = parse_pasted_callback("   ", "X").unwrap_err();
        assert!(matches!(err, FlowError::EmptyPaste));
    }

    #[test]
    fn parse_pasted_callback_rejects_unknown_format() {
        let err = parse_pasted_callback("just-a-string", "X").unwrap_err();
        assert!(matches!(err, FlowError::ParsePaste(_)));
    }

    #[test]
    fn parse_pasted_callback_rejects_malformed_url() {
        let err = parse_pasted_callback("http://localhost/cb", "X").unwrap_err();
        assert!(matches!(err, FlowError::ParsePaste(_)));
    }

    #[test]
    fn token_exchange_form_has_pinned_fields_in_order() {
        // Pinned against the runtime.
        let form = token_exchange_form("CODE", "VERIFIER");
        assert_eq!(
            form,
            vec![
                ("grant_type", "authorization_code"),
                ("code", "CODE"),
                ("redirect_uri", "http://localhost:1455/auth/callback"),
                ("client_id", "app_EMoamEEZ73f0CkXaXp7hrann"),
                ("code_verifier", "VERIFIER"),
            ]
        );
    }

    #[test]
    fn random_32_returns_distinct_bytes_across_calls() {
        // Overwhelmingly probable with 256 bits of entropy; a
        // failure here is a signal that the RNG is deterministic.
        let a = random_32();
        let b = random_32();
        assert_ne!(a, b);
    }
}
