//! The device-code login's IO: ask for a code, poll for the token, finish.
//!
//! The decisions — what an answer means, how long to wait — are
//! [`crate::onboarding::device_code`]'s; this module does the HTTP, reads the
//! JSON into the fields those decisions take, and sleeps. The transport is a
//! trait so the polling loop can be driven by a script in tests, and so a
//! caller that owns a tokio runtime (the terminal) and one that does not
//! (the desktop app) share the same loop.
//!
//! ## Threading
//!
//! Everything here blocks. [`poll_for_token`] runs for as long as the user
//! takes to approve, so callers put it on a thread of its own and stop it
//! through the `wait` callback — [`sleep_unless_cancelled`] turns a cancel
//! flag into one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rebon_config::account_login::{AccountFlow, AccountLoginSpec, AccountTokens};
use serde::Deserialize;

use crate::onboarding::device_code::{
    classify_poll, DeviceAuthorization, PollAnswer, PollFields, PollSchedule,
};

/// RFC 8628 §3.4.
pub const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Per-request ceiling for the two endpoints this flow calls.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a cancelled wait is noticed.
const CANCEL_SLICE: Duration = Duration::from_millis(100);

/// POSTs a form and hands back the status and body.
pub trait DeviceHttp: Send + Sync {
    /// POST `form` to `url` asking for JSON (`Accept: application/json` —
    /// GitHub answers form-encoded without it).
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, String), String>;
}

/// Why a device login stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceFlowError {
    /// A request did not complete, or the server refused to hand out a code.
    Request(String),
    /// The code ran out before the user approved.
    Expired,
    /// The user declined on the provider's page.
    Denied,
    /// The user gave up here.
    Cancelled,
    /// The server said something this flow cannot continue from, or the
    /// login could not be finished after approval.
    Failed(String),
}

impl std::fmt::Display for DeviceFlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(message) => write!(f, "sign-in request failed: {message}"),
            Self::Expired => write!(f, "the code expired before it was approved — start again"),
            Self::Denied => write!(f, "the sign-in was declined"),
            Self::Cancelled => write!(f, "sign-in cancelled"),
            Self::Failed(message) => write!(f, "sign-in failed: {message}"),
        }
    }
}

impl std::error::Error for DeviceFlowError {}

/// [`DeviceHttp`] over reqwest, on the caller's runtime or its own.
pub struct ReqwestDeviceHttp {
    client: reqwest::Client,
    runtime: RuntimeSlot,
}

enum RuntimeSlot {
    Borrowed(tokio::runtime::Handle),
    Owned(tokio::runtime::Runtime),
}

impl ReqwestDeviceHttp {
    /// Run requests on `handle`. Every call blocks on it, so it must be made
    /// from a thread outside that runtime's async context.
    pub fn with_handle(handle: tokio::runtime::Handle) -> Result<Self, DeviceFlowError> {
        Ok(Self {
            client: build_client()?,
            runtime: RuntimeSlot::Borrowed(handle),
        })
    }

    /// Run requests on a private runtime, for a caller with no reactor of
    /// its own (the GPUI app).
    pub fn with_own_runtime() -> Result<Self, DeviceFlowError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| DeviceFlowError::Request(format!("build runtime: {err}")))?;
        Ok(Self {
            client: build_client()?,
            runtime: RuntimeSlot::Owned(runtime),
        })
    }
}

fn build_client() -> Result<reqwest::Client, DeviceFlowError> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("Rebon/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| DeviceFlowError::Request(format!("build HTTP client: {err}")))
}

impl DeviceHttp for ReqwestDeviceHttp {
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, String), String> {
        let request = async {
            let response = self
                .client
                .post(url)
                .header("Accept", "application/json")
                .form(form)
                .send()
                .await
                .map_err(|err| err.to_string())?;
            let status = response.status().as_u16();
            let body = response.text().await.map_err(|err| err.to_string())?;
            Ok((status, body))
        };
        match &self.runtime {
            RuntimeSlot::Borrowed(handle) => handle.block_on(request),
            RuntimeSlot::Owned(runtime) => runtime.block_on(request),
        }
    }
}

#[derive(Deserialize)]
struct DeviceAuthorizationWire {
    device_code: String,
    user_code: String,
    /// RFC 8628 spells it `verification_uri`; Google's older endpoint said
    /// `verification_url`.
    #[serde(alias = "verification_url")]
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

/// Read the device authorization endpoint's answer.
pub fn parse_device_authorization(body: &str) -> Result<DeviceAuthorization, String> {
    let wire: DeviceAuthorizationWire = serde_json::from_str(body)
        .map_err(|err| format!("unexpected device authorization response: {err}"))?;
    if wire.device_code.is_empty() || wire.user_code.is_empty() {
        return Err("the device authorization response carried no code".to_string());
    }
    if !wire.verification_uri.starts_with("https://") {
        return Err(format!(
            "refusing a verification page that is not https: {}",
            wire.verification_uri
        ));
    }
    Ok(DeviceAuthorization {
        device_code: wire.device_code,
        user_code: wire.user_code,
        verification_uri: wire.verification_uri,
        verification_uri_complete: wire
            .verification_uri_complete
            .filter(|uri| uri.starts_with("https://")),
        expires_in: Duration::from_secs(wire.expires_in),
        interval: Duration::from_secs(wire.interval.unwrap_or(0)),
    })
}

#[derive(Deserialize, Default)]
struct PollWire {
    error: Option<String>,
    error_description: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    interval: Option<u64>,
}

/// Read a token-endpoint answer into the fields the decision takes. A body
/// that is not JSON reads as no fields at all, which the decision turns into
/// a failure (or reads by status).
pub fn parse_poll_fields(body: &str) -> PollFields {
    let wire: PollWire = serde_json::from_str(body).unwrap_or_default();
    PollFields {
        error: wire.error,
        error_description: wire.error_description,
        access_token: wire.access_token,
        refresh_token: wire.refresh_token,
        expires_in: wire.expires_in,
        interval: wire.interval,
    }
}

fn device_authorization_url(spec: &AccountLoginSpec) -> Result<&'static str, DeviceFlowError> {
    match spec.flow {
        AccountFlow::DeviceCode {
            device_authorization_url,
        } => Ok(device_authorization_url),
        AccountFlow::PkceLoopback { .. } => Err(DeviceFlowError::Failed(format!(
            "{} does not sign in with a device code",
            spec.display_name
        ))),
    }
}

/// Ask for a user code. `client_id` is
/// [`rebon_config::account_login::account_client_id`]'s answer for `spec`.
pub fn request_device_authorization(
    http: &dyn DeviceHttp,
    spec: &AccountLoginSpec,
    client_id: &str,
) -> Result<DeviceAuthorization, DeviceFlowError> {
    let url = device_authorization_url(spec)?;
    let (status, body) = http
        .post_form(url, &[("client_id", client_id), ("scope", spec.scopes)])
        .map_err(DeviceFlowError::Request)?;
    if !(200..300).contains(&status) {
        return Err(DeviceFlowError::Request(format!("{url} answered {status}")));
    }
    parse_device_authorization(&body).map_err(DeviceFlowError::Request)
}

/// Poll the token endpoint until the user approves, declines, the code runs
/// out, or `wait` says to stop (it returns `false` when the wait was
/// cancelled). `client_id` must be the one the code was requested with.
/// `now_ms` stamps the token's expiry.
pub fn poll_for_token(
    http: &dyn DeviceHttp,
    spec: &AccountLoginSpec,
    client_id: &str,
    authorization: &DeviceAuthorization,
    wait: &mut dyn FnMut(Duration) -> bool,
    now_ms: &dyn Fn() -> u64,
) -> Result<AccountTokens, DeviceFlowError> {
    let mut schedule = PollSchedule::new(authorization);
    let form = [
        ("client_id", client_id),
        ("device_code", authorization.device_code.as_str()),
        ("grant_type", DEVICE_CODE_GRANT_TYPE),
    ];
    loop {
        if !wait(schedule.interval()) {
            return Err(DeviceFlowError::Cancelled);
        }
        let still_valid = schedule.waited();
        let (status, body) = http
            .post_form(spec.token_url, &form)
            .map_err(DeviceFlowError::Request)?;
        match classify_poll(status, parse_poll_fields(&body)) {
            PollAnswer::Pending => {}
            PollAnswer::SlowDown { server_interval } => schedule.slow_down(server_interval),
            PollAnswer::Expired => return Err(DeviceFlowError::Expired),
            PollAnswer::Denied => return Err(DeviceFlowError::Denied),
            PollAnswer::Failed(message) => return Err(DeviceFlowError::Failed(message)),
            PollAnswer::Granted {
                access_token,
                refresh_token,
                expires_in,
            } => {
                let expires_at =
                    expires_in.map(|secs| now_ms().saturating_add(secs.saturating_mul(1000)));
                return Ok(AccountTokens::new(access_token, refresh_token, expires_at));
            }
        }
        if !still_valid {
            return Err(DeviceFlowError::Expired);
        }
    }
}

/// A `wait` for [`poll_for_token`] that sleeps in short slices and answers
/// `false` as soon as `cancel` is set.
pub fn sleep_unless_cancelled(cancel: &AtomicBool) -> impl FnMut(Duration) -> bool + '_ {
    move |duration| {
        let mut left = duration;
        while !left.is_zero() {
            if cancel.load(Ordering::SeqCst) {
                return false;
            }
            let slice = left.min(CANCEL_SLICE);
            std::thread::sleep(slice);
            left -= slice;
        }
        !cancel.load(Ordering::SeqCst)
    }
}

/// Finish an approved device login: exchange where the login needs it,
/// store the tokens, upsert and activate the provider entry.
pub async fn finish_device_login(
    spec: &AccountLoginSpec,
    tokens: AccountTokens,
) -> Result<AccountTokens, DeviceFlowError> {
    let config_dir = rebon_config::config_home_dir();
    rebon_config::account_login::complete_account_login(&config_dir, spec, tokens)
        .await
        .map_err(|err| DeviceFlowError::Failed(format!("{err:#}")))
}

/// [`finish_device_login`] on a private runtime, for a caller with no
/// reactor. Must not be called from inside a tokio runtime.
pub fn finish_device_login_blocking(
    spec: &AccountLoginSpec,
    tokens: AccountTokens,
) -> Result<AccountTokens, DeviceFlowError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| DeviceFlowError::Failed(format!("build runtime: {err}")))?
        .block_on(finish_device_login(spec, tokens))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Answers each POST from a script and records what was sent.
    struct ScriptedHttp {
        answers: Mutex<Vec<Result<(u16, String), String>>>,
        sent: Mutex<Vec<(String, Vec<(String, String)>)>>,
    }

    impl ScriptedHttp {
        fn new(answers: Vec<Result<(u16, &str), &str>>) -> Self {
            Self {
                answers: Mutex::new(
                    answers
                        .into_iter()
                        .map(|a| a.map(|(s, b)| (s, b.to_string())).map_err(str::to_string))
                        .collect(),
                ),
                sent: Mutex::new(Vec::new()),
            }
        }
    }

    impl DeviceHttp for ScriptedHttp {
        fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, String), String> {
            self.sent.lock().unwrap().push((
                url.to_string(),
                form.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ));
            let mut answers = self.answers.lock().unwrap();
            assert!(!answers.is_empty(), "polled past the script");
            answers.remove(0)
        }
    }

    fn copilot() -> &'static AccountLoginSpec {
        rebon_config::account_login(rebon_config::COPILOT_LOGIN_ID).unwrap()
    }

    fn authorization(interval: u64, expires_in: u64) -> DeviceAuthorization {
        DeviceAuthorization {
            device_code: "dev-code".into(),
            user_code: "ABCD-1234".into(),
            verification_uri: "https://github.com/login/device".into(),
            verification_uri_complete: None,
            expires_in: Duration::from_secs(expires_in),
            interval: Duration::from_secs(interval),
        }
    }

    /// Records each wait instead of sleeping.
    fn recorder(waits: &Mutex<Vec<u64>>) -> impl FnMut(Duration) -> bool + '_ {
        move |duration| {
            waits.lock().unwrap().push(duration.as_secs());
            true
        }
    }

    #[test]
    fn the_code_request_sends_the_client_id_and_scope_and_reads_the_code() {
        let http = ScriptedHttp::new(vec![Ok((
            200,
            r#"{"device_code":"d","user_code":"WDJB-MJHT","verification_uri":"https://github.com/login/device","expires_in":899,"interval":5}"#,
        ))]);
        let auth = request_device_authorization(&http, copilot(), copilot().client_id).unwrap();
        assert_eq!(auth.user_code, "WDJB-MJHT");
        assert_eq!(auth.interval, Duration::from_secs(5));
        assert_eq!(auth.expires_in, Duration::from_secs(899));
        let sent = http.sent.lock().unwrap();
        assert_eq!(sent[0].0, "https://github.com/login/device/code");
        assert_eq!(
            sent[0].1,
            vec![
                ("client_id".to_string(), copilot().client_id.to_string()),
                ("scope".to_string(), "read:user".to_string()),
            ]
        );
    }

    /// A configured client id is the one both requests carry: a code asked
    /// for under one id cannot be redeemed under another.
    #[test]
    fn an_overridden_client_id_is_sent_on_the_code_request_and_every_poll() {
        let http = ScriptedHttp::new(vec![
            Ok((
                200,
                r#"{"device_code":"d","user_code":"U","verification_uri":"https://github.com/login/device","expires_in":900,"interval":1}"#,
            )),
            Ok((200, r#"{"error":"authorization_pending"}"#)),
            Ok((200, r#"{"access_token":"gho"}"#)),
        ]);
        let auth = request_device_authorization(&http, copilot(), "Iv1.org").unwrap();
        let waits = Mutex::new(Vec::new());
        poll_for_token(
            &http,
            copilot(),
            "Iv1.org",
            &auth,
            &mut recorder(&waits),
            &|| 0,
        )
        .unwrap();
        let sent = http.sent.lock().unwrap();
        assert_eq!(sent.len(), 3);
        for (_, form) in sent.iter() {
            assert!(
                form.contains(&("client_id".to_string(), "Iv1.org".to_string())),
                "{form:?}"
            );
        }
    }

    #[test]
    fn a_refused_or_unreadable_code_request_is_a_request_error() {
        for answer in [
            Ok((400, r#"{"error":"unauthorized_client"}"#)),
            Ok((200, "device_code=d&user_code=x")),
            Err("connection reset"),
        ] {
            let http = ScriptedHttp::new(vec![answer]);
            assert!(matches!(
                request_device_authorization(&http, copilot(), copilot().client_id),
                Err(DeviceFlowError::Request(_))
            ));
        }
    }

    #[test]
    fn a_pkce_login_is_not_asked_for_a_device_code() {
        let http = ScriptedHttp::new(vec![]);
        let codex = rebon_config::account_login::codex_login();
        assert!(matches!(
            request_device_authorization(&http, codex, codex.client_id),
            Err(DeviceFlowError::Failed(_))
        ));
    }

    #[test]
    fn device_authorization_parsing_accepts_the_google_spelling_and_refuses_http() {
        let auth = parse_device_authorization(
            r#"{"device_code":"d","user_code":"u","verification_url":"https://example.com/device","expires_in":60}"#,
        )
        .unwrap();
        assert_eq!(auth.verification_uri, "https://example.com/device");
        assert_eq!(
            auth.interval,
            Duration::ZERO,
            "the schedule applies the default"
        );

        assert!(parse_device_authorization(
            r#"{"device_code":"d","user_code":"u","verification_uri":"http://example.com","expires_in":60}"#,
        )
        .is_err());
        assert!(parse_device_authorization(
            r#"{"device_code":"","user_code":"u","verification_uri":"https://example.com","expires_in":60}"#,
        )
        .is_err());
        let with_complete = parse_device_authorization(
            r#"{"device_code":"d","user_code":"u","verification_uri":"https://e.com","verification_uri_complete":"http://e.com/?c=u","expires_in":60}"#,
        )
        .unwrap();
        assert_eq!(with_complete.verification_uri_complete, None);
    }

    /// pending → slow_down → pending → granted: the waits follow the
    /// schedule and the token comes back with its expiry stamped.
    #[test]
    fn polling_walks_pending_and_slow_down_to_a_grant() {
        let http = ScriptedHttp::new(vec![
            Ok((200, r#"{"error":"authorization_pending"}"#)),
            Ok((200, r#"{"error":"slow_down","interval":10}"#)),
            Ok((400, r#"{"error":"authorization_pending"}"#)),
            Ok((
                200,
                r#"{"access_token":"gho_x","token_type":"bearer","scope":"read:user"}"#,
            )),
        ]);
        let waits = Mutex::new(Vec::new());
        let tokens = poll_for_token(
            &http,
            copilot(),
            copilot().client_id,
            &authorization(5, 900),
            &mut recorder(&waits),
            &|| 1_000,
        )
        .unwrap();
        assert_eq!(tokens.access_token, "gho_x");
        assert_eq!(tokens.expires_at, None);
        assert_eq!(*waits.lock().unwrap(), vec![5, 5, 10, 10]);
        let sent = http.sent.lock().unwrap();
        assert_eq!(sent[0].0, copilot().token_url);
        assert!(sent[0]
            .1
            .contains(&("grant_type".to_string(), DEVICE_CODE_GRANT_TYPE.to_string())));
        assert!(sent[0]
            .1
            .contains(&("device_code".to_string(), "dev-code".to_string())));
    }

    #[test]
    fn a_grant_with_a_lifetime_is_stamped_from_now() {
        let http = ScriptedHttp::new(vec![Ok((
            200,
            r#"{"access_token":"a","refresh_token":"r","expires_in":60}"#,
        ))]);
        let waits = Mutex::new(Vec::new());
        let tokens = poll_for_token(
            &http,
            copilot(),
            copilot().client_id,
            &authorization(1, 900),
            &mut recorder(&waits),
            &|| 5_000,
        )
        .unwrap();
        assert_eq!(tokens.refresh_token.as_deref(), Some("r"));
        assert_eq!(tokens.expires_at, Some(65_000));
    }

    #[test]
    fn expired_denied_and_unknown_answers_end_the_poll() {
        for (body, expected) in [
            (r#"{"error":"expired_token"}"#, DeviceFlowError::Expired),
            (r#"{"error":"access_denied"}"#, DeviceFlowError::Denied),
            (
                r#"{"error":"unsupported_grant_type"}"#,
                DeviceFlowError::Failed("unsupported_grant_type".into()),
            ),
        ] {
            let http = ScriptedHttp::new(vec![Ok((200, body))]);
            let waits = Mutex::new(Vec::new());
            let err = poll_for_token(
                &http,
                copilot(),
                copilot().client_id,
                &authorization(1, 900),
                &mut recorder(&waits),
                &|| 0,
            )
            .unwrap_err();
            assert_eq!(err, expected, "{body}");
        }
    }

    #[test]
    fn a_network_failure_mid_poll_ends_it_as_a_request_error() {
        let http = ScriptedHttp::new(vec![
            Ok((200, r#"{"error":"authorization_pending"}"#)),
            Err("timed out"),
        ]);
        let waits = Mutex::new(Vec::new());
        let err = poll_for_token(
            &http,
            copilot(),
            copilot().client_id,
            &authorization(1, 900),
            &mut recorder(&waits),
            &|| 0,
        )
        .unwrap_err();
        assert_eq!(err, DeviceFlowError::Request("timed out".into()));
    }

    /// Once the waits add up to the code's lifetime, a pending answer is
    /// the last one.
    #[test]
    fn the_poll_stops_when_the_code_runs_out() {
        let http = ScriptedHttp::new(vec![
            Ok((200, r#"{"error":"authorization_pending"}"#)),
            Ok((200, r#"{"error":"authorization_pending"}"#)),
        ]);
        let waits = Mutex::new(Vec::new());
        let err = poll_for_token(
            &http,
            copilot(),
            copilot().client_id,
            &authorization(5, 10),
            &mut recorder(&waits),
            &|| 0,
        )
        .unwrap_err();
        assert_eq!(err, DeviceFlowError::Expired);
        assert_eq!(http.sent.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_cancelled_wait_stops_before_the_next_request() {
        let http = ScriptedHttp::new(vec![Ok((200, r#"{"error":"authorization_pending"}"#))]);
        let mut calls = 0;
        let mut wait = |_duration: Duration| {
            calls += 1;
            calls == 1
        };
        let err = poll_for_token(
            &http,
            copilot(),
            copilot().client_id,
            &authorization(1, 900),
            &mut wait,
            &|| 0,
        )
        .unwrap_err();
        assert_eq!(err, DeviceFlowError::Cancelled);
        assert_eq!(http.sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn sleep_unless_cancelled_notices_the_flag() {
        let cancel = AtomicBool::new(false);
        let mut wait = sleep_unless_cancelled(&cancel);
        assert!(wait(Duration::from_millis(1)));
        cancel.store(true, Ordering::SeqCst);
        let started = std::time::Instant::now();
        assert!(!wait(Duration::from_secs(60)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_non_json_poll_body_reads_as_no_fields() {
        assert_eq!(parse_poll_fields("<html>"), PollFields::default());
    }
}
