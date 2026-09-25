//! Walking one login through its phases, from a synchronous context.
//!
//! [`oauth`](crate::oauth) has the phases as free functions; this module is
//! the order they go in, what each result means, and what the wizard is told
//! after each one. Two front ends drive it — the terminal's pre-startup loop
//! and its in-session `/login` — and both used to carry their own copy of
//! that order.
//!
//! [`drive_account_login_blocking`] picks the flow a login uses: the ChatGPT
//! login's PKCE-and-loopback phases ([`drive_oauth_flow_blocking`]), or a
//! device code (show the code, poll until approved, finish).
//!
//! ## What the host supplies
//!
//! The collect phase has to watch for the user giving up while a background
//! thread waits on the loopback port, and to take a pasted callback when the
//! port was busy. Both are "what did the user just do", which is a terminal's
//! question, not this crate's: a host implements [`OAuthHost`] and this module
//! asks it. That keeps every decision — which phase runs, whether a listener
//! failure falls back to paste, whether an error is worth retrying — in one
//! place, without this crate learning what a key event is.
//!
//! ## Threading
//!
//! [`drive_oauth_flow_blocking`] blocks, and it is called from a thread that
//! is allowed to block. The listener runs on a thread of its own so the host
//! keeps getting asked for input while it waits. Only the token exchange is
//! async, and it is bridged with the caller's runtime handle.
//!
//! ## Coverage
//!
//! The phase order and the two fallbacks are pinned in this module's tests
//! with a scripted host. Live-network tests (a real token-exchange POST) are
//! not run: [`crate::oauth::flow`]'s tests pin the request body and
//! [`crate::oauth::listener`]'s pin the HTTP parser.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use tokio::runtime::Handle;

use rebon_config::account_login::{AccountFlow, AccountLoginSpec, AccountTokens};

use crate::dialog::{OnboardingDialogState, OnboardingStepTransition};
use crate::oauth::device::{self as oauth_device, DeviceFlowError, DeviceHttp, ReqwestDeviceHttp};
use crate::oauth::{
    flow as oauth_flow, listener as oauth_listener, CallbackResult, FlowError, ListenerError,
    OAuthChallenge,
};

/// What the host reports back when asked what the user did.
#[derive(Debug)]
pub enum DriveInput {
    /// Nothing happened within the host's poll window. Ask again.
    Idle,
    /// The user gave up. Tear the phase down and return
    /// [`Outcome::Cancelled`].
    Cancelled,
    /// The user submitted a callback URL or code in the paste view. The text
    /// is passed to [`crate::oauth::flow::parse_pasted_callback`] as typed.
    Submitted(String),
}

/// The terminal's half of the flow: paint the wizard, and say what the user
/// did next.
pub trait OAuthHost {
    /// Paint the wizard as it stands. Called after every phase transition so
    /// the new sub-view is on screen before the next blocking step starts.
    fn redraw(&mut self, state: &OnboardingDialogState) -> std::io::Result<()>;

    /// Wait a short while for the user to do something, then answer.
    ///
    /// Implementations must return within roughly a tick rather than blocking
    /// indefinitely: while the loopback listener runs, this is also the only
    /// thing keeping the loop responsive.
    ///
    /// `state` is passed mutably because a host that receives a paste feeds it
    /// straight into the wizard's text field, then answers [`DriveInput::Idle`]
    /// — the character landed, nothing was submitted.
    fn next_input(&mut self, state: &mut OnboardingDialogState) -> std::io::Result<DriveInput>;

    /// A device login is about to show `user_code`. A host that can put it
    /// on the clipboard does, and answers `true` so the view says so; the
    /// default offers nothing.
    fn offer_user_code(&mut self, _user_code: &str) -> bool {
        false
    }
}

/// What happened during one invocation of the driver. The caller maps this to
/// its own outcome (the startup loop exits vs. the runner re-enters its main
/// event pump).
#[derive(Debug)]
pub enum Outcome {
    /// Tokens persisted, provider upserted, wizard already notified via
    /// `report_oauth_success`.
    Success {
        transition: Option<OnboardingStepTransition>,
    },
    /// The user gave up in one of the sub-views. The wizard already has
    /// `oauth_view = None` and the listener has been torn down; the caller
    /// should fall back to rendering the login pane.
    Cancelled,
    /// The flow failed terminally. The wizard is in `OAuthView::Error` — the
    /// caller should keep it open so the user sees the error and can retry.
    Failed,
}

/// Drive the login `login_id` names (an id from
/// `rebon_config::account_login`'s table) from a synchronous context.
pub fn drive_account_login_blocking(
    state: &mut OnboardingDialogState,
    runtime: &Handle,
    host: &mut dyn OAuthHost,
    login_id: &str,
) -> anyhow::Result<Outcome> {
    let Some(spec) = rebon_config::account_login(login_id) else {
        state.report_oauth_error(format!("unknown account login `{login_id}`"), false);
        host.redraw(state)?;
        return Ok(Outcome::Failed);
    };
    state.oauth_login = spec.id;
    match spec.flow {
        AccountFlow::PkceLoopback { .. } => drive_oauth_flow_blocking(state, runtime, host),
        AccountFlow::DeviceCode { .. } => drive_device_flow_blocking(state, runtime, host, spec),
    }
}

/// Ask for a code, show it, poll until the user approves on the provider's
/// page, then finish the login. The poll runs on its own thread so the host
/// keeps being asked for input — Esc cancels it within one wait slice.
fn drive_device_flow_blocking(
    state: &mut OnboardingDialogState,
    runtime: &Handle,
    host: &mut dyn OAuthHost,
    spec: &'static AccountLoginSpec,
) -> anyhow::Result<Outcome> {
    let http: Arc<dyn DeviceHttp> = match ReqwestDeviceHttp::with_handle(runtime.clone()) {
        Ok(http) => Arc::new(http),
        Err(err) => {
            state.report_oauth_error(err.to_string(), true);
            host.redraw(state)?;
            return Ok(Outcome::Failed);
        }
    };
    let client_id =
        rebon_config::account_login::account_client_id(&rebon_config::config_home_dir(), spec);
    let mut finish = |tokens| {
        runtime
            .block_on(oauth_device::finish_device_login(spec, tokens))
            .map(|_| ())
    };
    drive_device_flow_with(state, host, spec, client_id, http, &mut finish)
}

/// [`drive_device_flow_blocking`] with the transport and the finishing step
/// handed in, so the phase order can be driven by a script.
fn drive_device_flow_with(
    state: &mut OnboardingDialogState,
    host: &mut dyn OAuthHost,
    spec: &'static AccountLoginSpec,
    client_id: String,
    http: Arc<dyn DeviceHttp>,
    finish: &mut dyn FnMut(AccountTokens) -> Result<(), DeviceFlowError>,
) -> anyhow::Result<Outcome> {
    let authorization = match oauth_device::request_device_authorization(&*http, spec, &client_id) {
        Ok(authorization) => authorization,
        Err(err) => {
            state.report_oauth_error(err.to_string(), true);
            host.redraw(state)?;
            return Ok(Outcome::Failed);
        }
    };

    let copied = host.offer_user_code(&authorization.user_code);
    state.report_oauth_device_code(
        authorization.verification_uri.clone(),
        authorization.user_code.clone(),
        copied,
    );
    host.redraw(state)?;
    let _ = oauth_flow::launch_browser(authorization.page_to_open());

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_thread = cancel.clone();
    let poller = thread::spawn(move || {
        let mut wait = oauth_device::sleep_unless_cancelled(&cancel_for_thread);
        oauth_device::poll_for_token(&*http, spec, &client_id, &authorization, &mut wait, &|| {
            rebon_types::wall_clock_ms()
        })
    });

    let polled = loop {
        if poller.is_finished() {
            break poller
                .join()
                .map_err(|_| anyhow::anyhow!("device login poll thread panicked"))?;
        }
        match host.next_input(state)? {
            DriveInput::Cancelled => {
                cancel.store(true, Ordering::SeqCst);
                // The poll notices within one wait slice; its answer no
                // longer matters.
                let _ = poller.join();
                state.clear_oauth_view();
                return Ok(Outcome::Cancelled);
            }
            DriveInput::Idle | DriveInput::Submitted(_) => {}
        }
    };

    let tokens = match polled {
        Ok(tokens) => tokens,
        Err(DeviceFlowError::Cancelled) => {
            state.clear_oauth_view();
            return Ok(Outcome::Cancelled);
        }
        Err(err) => {
            state.report_oauth_error(err.to_string(), true);
            host.redraw(state)?;
            return Ok(Outcome::Failed);
        }
    };

    state.report_oauth_exchanging();
    host.redraw(state)?;
    match finish(tokens) {
        Ok(()) => {
            let transition = state.report_oauth_success(crate::store::load_provider_snapshot());
            host.redraw(state)?;
            Ok(Outcome::Success { transition })
        }
        Err(err) => {
            state.report_oauth_error(err.to_string(), true);
            host.redraw(state)?;
            Ok(Outcome::Failed)
        }
    }
}

/// Drive the full flow from a synchronous context.
///
/// `runtime` is used to `block_on` the token-exchange POST — every other
/// phase is genuinely synchronous.
pub fn drive_oauth_flow_blocking(
    state: &mut OnboardingDialogState,
    runtime: &Handle,
    host: &mut dyn OAuthHost,
) -> anyhow::Result<Outcome> {
    // ── Phase 1: prepare ─────────────────────────────────────────
    let challenge = match oauth_flow::prepare_openai_oauth() {
        Ok(c) => c,
        Err(err) => {
            state.report_oauth_error(err.to_string(), false);
            host.redraw(state)?;
            return Ok(Outcome::Failed);
        }
    };

    state.report_oauth_prepared(challenge.authorize_url.clone(), challenge.port_available);
    host.redraw(state)?;

    // ── Phase 2a: browser launch (best-effort) ───────────────────
    let _ = oauth_flow::launch_browser(&challenge.authorize_url);

    // ── Phase 2b: collect ────────────────────────────────────────
    let callback = if challenge.port_available {
        state.report_oauth_listening();
        host.redraw(state)?;
        match collect_via_listener(state, &challenge, host)? {
            CollectOutcome::Got(cb) => cb,
            CollectOutcome::Cancelled => return Ok(Outcome::Cancelled),
            CollectOutcome::FellBackToPaste => match collect_via_paste(state, &challenge, host)? {
                CollectOutcome::Got(cb) => cb,
                CollectOutcome::Cancelled => return Ok(Outcome::Cancelled),
                CollectOutcome::FellBackToPaste => unreachable!(),
                CollectOutcome::Failed => return Ok(Outcome::Failed),
            },
            CollectOutcome::Failed => return Ok(Outcome::Failed),
        }
    } else {
        match collect_via_paste(state, &challenge, host)? {
            CollectOutcome::Got(cb) => cb,
            CollectOutcome::Cancelled => return Ok(Outcome::Cancelled),
            CollectOutcome::FellBackToPaste => unreachable!(),
            CollectOutcome::Failed => return Ok(Outcome::Failed),
        }
    };

    // ── Phase 3: exchange + persist ──────────────────────────────
    state.report_oauth_exchanging();
    host.redraw(state)?;

    let verifier = challenge.verifier.clone();
    let exchange_result = runtime
        .block_on(async move { oauth_flow::exchange_and_persist(&callback.code, &verifier).await });

    match exchange_result {
        Ok(_tokens) => {
            let transition = state.report_oauth_success(crate::store::load_provider_snapshot());
            host.redraw(state)?;
            Ok(Outcome::Success { transition })
        }
        Err(err) => {
            state.report_oauth_error(err.to_string(), err_is_retriable(&err));
            host.redraw(state)?;
            Ok(Outcome::Failed)
        }
    }
}

enum CollectOutcome {
    Got(CallbackResult),
    Cancelled,
    FellBackToPaste,
    Failed,
}

fn collect_via_listener(
    state: &mut OnboardingDialogState,
    challenge: &OAuthChallenge,
    host: &mut dyn OAuthHost,
) -> anyhow::Result<CollectOutcome> {
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_thread = cancel.clone();
    let expected_state = challenge.state.clone();

    let handle = thread::spawn(move || {
        oauth_listener::wait_for_callback_with_cancel(
            &expected_state,
            oauth_flow::CALLBACK_TIMEOUT,
            &cancel_for_thread,
        )
    });

    loop {
        if handle.is_finished() {
            let result = handle
                .join()
                .map_err(|_| anyhow::anyhow!("oauth listener thread panicked"))?;
            return Ok(match result {
                Ok(cb) => CollectOutcome::Got(cb),
                Err(ListenerError::Cancelled) => CollectOutcome::Cancelled,
                Err(ListenerError::StateMismatch { expected, actual }) => {
                    state.report_oauth_error(
                        format!("state mismatch: expected {expected}, got {actual} — retry login"),
                        true,
                    );
                    host.redraw(state)?;
                    CollectOutcome::Failed
                }
                Err(other) => {
                    // Bind / IO errors → fall back to paste path so the user
                    // still has a way to finish.
                    tracing::warn!(error = %other, "oauth listener failed, falling back to paste");
                    state.report_oauth_paste_required();
                    host.redraw(state)?;
                    CollectOutcome::FellBackToPaste
                }
            });
        }

        match host.next_input(state)? {
            DriveInput::Idle => {}
            DriveInput::Cancelled => {
                cancel.store(true, Ordering::SeqCst);
                // Wait for the listener to observe the flag and return, then
                // discard its result.
                let _ = handle.join();
                state.clear_oauth_view();
                return Ok(CollectOutcome::Cancelled);
            }
            // The paste view is not on screen yet, so nothing can be
            // submitted from it.
            DriveInput::Submitted(_) => {}
        }
    }
}

fn collect_via_paste(
    state: &mut OnboardingDialogState,
    challenge: &OAuthChallenge,
    host: &mut dyn OAuthHost,
) -> anyhow::Result<CollectOutcome> {
    // Make sure the paste view is on screen. The port-busy path set it before
    // calling; the listener-fallback path lands here with the view freshly
    // transitioned by `report_oauth_paste_required`.
    host.redraw(state)?;

    loop {
        match host.next_input(state)? {
            DriveInput::Idle => {}
            DriveInput::Cancelled => {
                state.clear_oauth_view();
                return Ok(CollectOutcome::Cancelled);
            }
            DriveInput::Submitted(text) => {
                match oauth_flow::parse_pasted_callback(&text, &challenge.state) {
                    Ok(cb) => return Ok(CollectOutcome::Got(cb)),
                    Err(err) => {
                        state.report_oauth_paste_error(err.to_string());
                        host.redraw(state)?;
                    }
                }
            }
        }
    }
}

/// Everything but a failed write is worth offering a retry for: a persist
/// error means the tokens are already gone, so re-running the flow would ask
/// the user to log in again for nothing.
pub fn err_is_retriable(err: &FlowError) -> bool {
    !matches!(err, FlowError::Persist(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn err_is_retriable_matches_expected_variants() {
        assert!(err_is_retriable(&FlowError::EmptyPaste));
        assert!(err_is_retriable(&FlowError::ParsePaste("x".into())));
        assert!(err_is_retriable(&FlowError::TokenExchange("x".into())));
        assert!(!err_is_retriable(&FlowError::Persist("x".into())));
    }

    /// A host that answers from a script and records every repaint, so a test
    /// can assert on the order of phases without a terminal.
    struct ScriptedHost {
        inputs: Vec<DriveInput>,
        redraws: usize,
    }

    impl OAuthHost for ScriptedHost {
        fn redraw(&mut self, _state: &OnboardingDialogState) -> std::io::Result<()> {
            self.redraws += 1;
            Ok(())
        }

        fn next_input(
            &mut self,
            _state: &mut OnboardingDialogState,
        ) -> std::io::Result<DriveInput> {
            Ok(if self.inputs.is_empty() {
                DriveInput::Cancelled
            } else {
                self.inputs.remove(0)
            })
        }
    }

    /// The paste collector hands the host's text to the parser as typed, and
    /// a rejected paste keeps the view open rather than failing the flow —
    /// the user gets to try again with the rest of the URL.
    #[test]
    fn a_rejected_paste_keeps_the_paste_view_open_and_asks_again() {
        let mut state = OnboardingDialogState::open_for_login_pane_from(
            crate::dialog::state::OnboardingOpenInputs::default(),
        );
        state.report_oauth_prepared("https://example.invalid/authorize".into(), false);
        let challenge = OAuthChallenge {
            authorize_url: "https://example.invalid/authorize".into(),
            state: "expected-state".into(),
            verifier: "verifier".into(),
            port_available: false,
        };
        let mut host = ScriptedHost {
            inputs: vec![
                DriveInput::Submitted("not-a-callback".into()),
                DriveInput::Cancelled,
            ],
            redraws: 0,
        };

        let outcome = collect_via_paste(&mut state, &challenge, &mut host).expect("no io error");

        assert!(matches!(outcome, CollectOutcome::Cancelled));
        assert!(
            host.redraws >= 2,
            "the opening paint plus one after the rejected paste"
        );
        assert!(
            state.oauth_view.is_none(),
            "giving up clears the sub-view so the login pane renders again"
        );
    }

    /// Answers each POST from a script, and fails the test past its end.
    struct ScriptedHttp(std::sync::Mutex<Vec<(u16, &'static str)>>);

    impl DeviceHttp for ScriptedHttp {
        fn post_form(&self, _url: &str, _form: &[(&str, &str)]) -> Result<(u16, String), String> {
            let mut answers = self.0.lock().unwrap();
            if answers.is_empty() {
                return Err("script exhausted".into());
            }
            let (status, body) = answers.remove(0);
            Ok((status, body.to_string()))
        }
    }

    const CODE: (u16, &str) = (
        200,
        r#"{"device_code":"d","user_code":"WDJB-MJHT","verification_uri":"https://github.com/login/device","expires_in":900,"interval":1}"#,
    );

    fn copilot() -> &'static AccountLoginSpec {
        rebon_config::account_login(rebon_config::COPILOT_LOGIN_ID).unwrap()
    }

    fn login_pane() -> OnboardingDialogState {
        let mut state = OnboardingDialogState::open_for_login_pane_from(
            crate::dialog::state::OnboardingOpenInputs::default(),
        );
        state.oauth_login = copilot().id;
        state
    }

    /// Records what the host was shown and offers to copy the code.
    struct DeviceHost {
        inputs: Vec<DriveInput>,
        views: Vec<Option<crate::dialog::OAuthView>>,
        offered: Vec<String>,
    }

    impl OAuthHost for DeviceHost {
        fn redraw(&mut self, state: &OnboardingDialogState) -> std::io::Result<()> {
            self.views.push(state.oauth_view.clone());
            Ok(())
        }

        fn next_input(
            &mut self,
            _state: &mut OnboardingDialogState,
        ) -> std::io::Result<DriveInput> {
            std::thread::sleep(std::time::Duration::from_millis(20));
            Ok(if self.inputs.is_empty() {
                DriveInput::Idle
            } else {
                self.inputs.remove(0)
            })
        }

        fn offer_user_code(&mut self, user_code: &str) -> bool {
            self.offered.push(user_code.to_string());
            true
        }
    }

    /// Code shown (and offered to the clipboard), approval polled, the
    /// login finished with the granted token, success reported.
    #[test]
    fn a_device_login_shows_the_code_then_finishes_with_the_grant() {
        let mut state = login_pane();
        let mut host = DeviceHost {
            inputs: Vec::new(),
            views: Vec::new(),
            offered: Vec::new(),
        };
        let http = Arc::new(ScriptedHttp(std::sync::Mutex::new(vec![
            CODE,
            (200, r#"{"access_token":"gho_granted"}"#),
        ])));
        let mut finished = Vec::new();
        let mut finish = |tokens: AccountTokens| {
            finished.push(tokens.access_token);
            Ok(())
        };

        let outcome = drive_device_flow_with(
            &mut state,
            &mut host,
            copilot(),
            copilot().client_id.to_string(),
            http,
            &mut finish,
        )
        .unwrap();

        assert!(matches!(outcome, Outcome::Success { .. }));
        assert_eq!(finished, vec!["gho_granted".to_string()]);
        assert_eq!(host.offered, vec!["WDJB-MJHT".to_string()]);
        assert!(host.views.iter().any(|view| matches!(
            view,
            Some(crate::dialog::OAuthView::DeviceCode { user_code, copied: true, .. })
                if user_code == "WDJB-MJHT"
        )));
        assert!(state.oauth_view.is_none());
        assert_eq!(
            state.add_provider_status,
            crate::dialog::PanelStatus::Ok("GitHub Copilot account connected.".into())
        );
    }

    /// Esc while waiting stops the poll and leaves no error behind; nothing
    /// is finished.
    #[test]
    fn cancelling_a_device_login_stops_the_poll_without_finishing() {
        let mut state = login_pane();
        let mut host = DeviceHost {
            inputs: vec![DriveInput::Idle, DriveInput::Cancelled],
            views: Vec::new(),
            offered: Vec::new(),
        };
        let http = Arc::new(ScriptedHttp(std::sync::Mutex::new(vec![CODE])));
        let mut finish = |_tokens: AccountTokens| -> Result<(), DeviceFlowError> {
            panic!("a cancelled login must not finish")
        };

        let started = std::time::Instant::now();
        let outcome = drive_device_flow_with(
            &mut state,
            &mut host,
            copilot(),
            copilot().client_id.to_string(),
            http,
            &mut finish,
        )
        .unwrap();

        assert!(matches!(outcome, Outcome::Cancelled));
        assert!(state.oauth_view.is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    /// A declined grant, a refused code request and a failed finish all
    /// land in the retryable error view.
    #[test]
    fn device_login_failures_offer_a_retry() {
        for (answers, finish_fails) in [
            (vec![CODE, (200, r#"{"error":"access_denied"}"#)], false),
            (vec![(403, "{}")], false),
            (vec![CODE, (200, r#"{"access_token":"g"}"#)], true),
        ] {
            let mut state = login_pane();
            let mut host = DeviceHost {
                inputs: Vec::new(),
                views: Vec::new(),
                offered: Vec::new(),
            };
            let http = Arc::new(ScriptedHttp(std::sync::Mutex::new(answers)));
            let mut finish = |_tokens: AccountTokens| {
                if finish_fails {
                    Err(DeviceFlowError::Failed("no subscription".into()))
                } else {
                    Ok(())
                }
            };
            let outcome = drive_device_flow_with(
                &mut state,
                &mut host,
                copilot(),
                copilot().client_id.to_string(),
                http,
                &mut finish,
            )
            .unwrap();
            assert!(matches!(outcome, Outcome::Failed));
            assert!(matches!(
                state.oauth_view,
                Some(crate::dialog::OAuthView::Error {
                    can_retry: true,
                    ..
                })
            ));
        }
    }

    #[test]
    fn an_unknown_login_fails_without_retry() {
        let mut state = login_pane();
        let mut host = ScriptedHost {
            inputs: Vec::new(),
            redraws: 0,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let outcome =
            drive_account_login_blocking(&mut state, runtime.handle(), &mut host, "gemini")
                .unwrap();
        assert!(matches!(outcome, Outcome::Failed));
        assert!(matches!(
            state.oauth_view,
            Some(crate::dialog::OAuthView::Error {
                can_retry: false,
                ..
            })
        ));
    }

    /// Picking a row asks for the right flow: the ChatGPT row keeps its own
    /// outcome, and a retry after an error starts the same login again.
    #[test]
    fn each_login_row_starts_its_own_flow() {
        use crate::dialog::OnboardingDialogOutcome;
        assert_eq!(
            crate::login_outcome("openai"),
            Some(OnboardingDialogOutcome::StartOpenAIOAuth)
        );
        assert_eq!(
            crate::login_outcome("copilot"),
            Some(OnboardingDialogOutcome::StartAccountLogin("copilot"))
        );
        assert_eq!(crate::login_outcome("gemini"), None);
        let state = login_pane();
        assert_eq!(
            state.oauth_login_outcome(),
            OnboardingDialogOutcome::StartAccountLogin("copilot")
        );
        let fresh = OnboardingDialogState::open_for_login_pane_from(
            crate::dialog::state::OnboardingOpenInputs::default(),
        );
        assert_eq!(
            fresh.oauth_login_outcome(),
            OnboardingDialogOutcome::StartOpenAIOAuth
        );
    }

    /// Cancelling while the paste view waits is the user's decision, not an
    /// error: the flow ends without an error view.
    #[test]
    fn cancelling_the_paste_view_ends_the_collect_phase() {
        let mut state = OnboardingDialogState::open_for_login_pane_from(
            crate::dialog::state::OnboardingOpenInputs::default(),
        );
        let challenge = OAuthChallenge {
            authorize_url: "https://example.invalid/authorize".into(),
            state: "expected-state".into(),
            verifier: "verifier".into(),
            port_available: false,
        };
        let mut host = ScriptedHost {
            inputs: vec![DriveInput::Idle, DriveInput::Cancelled],
            redraws: 0,
        };

        let outcome = collect_via_paste(&mut state, &challenge, &mut host).expect("no io error");

        assert!(matches!(outcome, CollectOutcome::Cancelled));
        assert!(state.oauth_view.is_none());
    }
}
