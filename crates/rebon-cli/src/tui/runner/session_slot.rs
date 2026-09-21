//! The session an event loop works on — or, until it is built, the facts
//! the first frame needs and the channel the session arrives on.
//!
//! RFC-0004 §9. The first frame used to wait for `build_tui_session`: the
//! runner took a built `TuiEngineSession` by value, and the loop reads it
//! every frame. A loop that draws before the session exists needs three
//! things from somewhere else — what the status bar and inline banner
//! say (provider, model, cwd), where a prompt goes when Enter comes early
//! (the hosted job, if there is one), and a way to receive the session once
//! it is built. `SessionSlot` is that somewhere: the loop asks it for a
//! session and gets `None` until `install`, and asks it for the strings
//! either way.

use std::time::{Duration, Instant};

use rebon_permissions::PermissionMode;
use rebon_types::ReasoningEffort;

use crate::rebon_config::{ProviderFormat, RuntimeOverride};
use crate::tui::wiring::TuiEngineSession;

/// How long a session's worker may take to come up before the status bar
/// says so in words rather than a dot.
const HOSTED_WAIT_SLOW_HINT_AFTER: Duration = Duration::from_secs(3);

/// What the first frame knows about the session it is drawn for, before the
/// session exists. Resolved from the same config the build will read, so
/// the banner and status bar do not change when the session arrives except
/// where the build knows better (a provider default the registry fills in).
#[derive(Debug, Clone)]
pub(crate) struct StartupPreview {
    pub cwd: String,
    /// Known ahead of the build only when the session was named before it —
    /// the hosted default names the session to start its worker.
    pub session_id: Option<String>,
    /// The job whose worker is starting for this session; a prompt typed
    /// before the session exists goes to it directly.
    pub hosted_job_id: Option<String>,
    pub provider_name: String,
    pub model_name: String,
    pub provider_format: Option<ProviderFormat>,
    /// `[Fast]` in the banner: the toggle is on and the provider offers it.
    pub fast_mode: bool,
    pub effort_level: Option<ReasoningEffort>,
    pub permission_mode: Option<PermissionMode>,
    pub coordinator_mode: bool,
}

impl StartupPreview {
    /// The preview from what the build will resolve, minus the client:
    /// `resolved` is the provider the credentials gate already read from
    /// config (no second read, no network, no OAuth refresh).
    pub(crate) fn resolve(
        overrides: &RuntimeOverride,
        cwd: &str,
        hosted: Option<&super::HostedStartup>,
        resolved: Option<&crate::rebon_config::ResolvedProvider>,
    ) -> Self {
        let fast_toggle = overrides
            .fast_mode
            .unwrap_or_else(crate::rebon_config::saved_fast_mode_enabled);
        let (provider_name, model_name, provider_format, fast_mode) = match resolved {
            Some(resolved) => {
                let model = model_for_preview(overrides.model.as_deref(), &resolved.model, || {
                    crate::session::build::registry_default_model(overrides, resolved)
                });
                let fast =
                    fast_toggle && crate::tui::wiring::openai_service_tier_available(resolved);
                (resolved.name.clone(), model, Some(resolved.format), fast)
            }
            // No config, or one the build will refuse too: the env
            // fallback names itself `env`, and the error path shows the
            // real error instead of these strings.
            None => {
                let model = overrides
                    .model
                    .clone()
                    .or_else(crate::rebon_config::saved_user_model)
                    .unwrap_or_else(crate::tui::wiring::default_model);
                let fast = fast_toggle && crate::tui::wiring::env_openai_service_tier_available();
                (
                    String::from("env"),
                    model,
                    Some(crate::tui::wiring::fallback_provider_format_from_env()),
                    fast,
                )
            }
        };
        Self {
            cwd: cwd.to_string(),
            session_id: hosted.map(|hosted| hosted.session_id.clone()),
            hosted_job_id: hosted.map(|hosted| hosted.job_id.clone()),
            provider_name,
            model_name,
            provider_format,
            fast_mode,
            effort_level: overrides.effort_level,
            permission_mode: overrides
                .permission_mode
                .or_else(crate::rebon_config::saved_default_permission_mode),
            coordinator_mode: rebon_core::coordinator_mode::coordinator_mode_from_env_default(),
        }
    }

    /// The preview a session already answers for itself — used when the
    /// slot starts out ready, so the accessors have one code path.
    fn from_session(session: &TuiEngineSession) -> Self {
        Self {
            cwd: session.cwd.clone(),
            session_id: Some(session.session_id.clone()),
            hosted_job_id: None,
            provider_name: session.model.provider_name.clone(),
            model_name: session.model.name.clone(),
            provider_format: Some(session.model.provider_format),
            fast_mode: session.model.service_tier_available && session.model.service_tier.is_fast(),
            effort_level: session.startup.effort_level,
            permission_mode: session.startup.permission_mode,
            coordinator_mode: session.engine_half.coordinator_mode_handle.get(),
        }
    }
}

/// The model the status bar shows before the build: what the build will
/// resolve, in its order — the `--model` override, the provider entry's own
/// model, and for an entry that names none the registry's default for it
/// (`resolve_runtime_model`), so the first frame does not show an empty
/// model the session then corrects.
fn model_for_preview(
    override_model: Option<&str>,
    provider_model: &str,
    registry_default: impl FnOnce() -> Option<String>,
) -> String {
    if let Some(model) = override_model {
        return model.to_string();
    }
    if !provider_model.trim().is_empty() {
        return provider_model.to_string();
    }
    registry_default().unwrap_or_default()
}

/// The channel a session still being built arrives on.
pub(crate) type SessionArrivalRx = std::sync::mpsc::Receiver<anyhow::Result<TuiEngineSession>>;

/// What the event loop holds instead of a session.
pub(crate) struct SessionSlot {
    session: Option<TuiEngineSession>,
    arrival: Option<SessionArrivalRx>,
    preview: StartupPreview,
    /// When the wait for the session (and its worker) began: the status
    /// bar's dot is timed from here until the session takes over.
    started_at: Instant,
    /// Enter was pressed before the session existed and the composer still
    /// holds the text; it is submitted once the session is installed.
    enter_deferred: bool,
    /// Shift+Tab was pressed before the session existed; the mode the app
    /// shows is the one to push into the session, not the startup default.
    permission_mode_cycled: bool,
    /// The math rendering mode the runner resolved from settings, applied to
    /// the session when it is installed.
    pub math_rendering_mode: crate::rebon_config::MathRenderingMode,
    /// The prompt history, loading on its own thread; merged into the app
    /// when it lands (`apply_loaded_input_history_if_idle`).
    pub input_history: Option<super::prompt_history::InputHistoryRx>,
}

impl SessionSlot {
    /// A slot that already holds its session.
    pub(crate) fn ready(session: TuiEngineSession) -> Self {
        let preview = StartupPreview::from_session(&session);
        Self {
            session: Some(session),
            arrival: None,
            preview,
            started_at: Instant::now(),
            enter_deferred: false,
            permission_mode_cycled: false,
            math_rendering_mode: crate::rebon_config::MathRenderingMode::default(),
            input_history: None,
        }
    }

    /// A slot whose session is being built elsewhere and will arrive on
    /// `arrival`.
    pub(crate) fn pending(arrival: SessionArrivalRx, preview: StartupPreview) -> Self {
        Self {
            session: None,
            arrival: Some(arrival),
            preview,
            started_at: Instant::now(),
            enter_deferred: false,
            permission_mode_cycled: false,
            math_rendering_mode: crate::rebon_config::MathRenderingMode::default(),
            input_history: None,
        }
    }

    pub(crate) fn session(&self) -> Option<&TuiEngineSession> {
        self.session.as_ref()
    }

    pub(crate) fn session_mut(&mut self) -> Option<&mut TuiEngineSession> {
        self.session.as_mut()
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.session.is_some()
    }

    /// The session, once the loop is over. `None` when it never arrived.
    pub(crate) fn into_session(self) -> Option<TuiEngineSession> {
        self.session
    }

    pub(crate) fn preview(&self) -> &StartupPreview {
        &self.preview
    }

    pub(crate) fn cwd(&self) -> &str {
        match &self.session {
            Some(session) => &session.cwd,
            None => &self.preview.cwd,
        }
    }

    pub(crate) fn provider_name(&self) -> &str {
        match &self.session {
            Some(session) => &session.model.provider_name,
            None => &self.preview.provider_name,
        }
    }

    /// The model the session runs on. For a mirrored session that is the
    /// owner's answer once it has given one — this process resolved a model
    /// at start-up, but the turns run elsewhere, and `/model` in another
    /// client changes what the owner runs without touching this value.
    pub(crate) fn model_name(&self) -> &str {
        match &self.session {
            Some(session) => session
                .remote_background_attachment
                .as_ref()
                .and_then(|remote| remote.owner_model())
                .unwrap_or(&session.model.name),
            None => &self.preview.model_name,
        }
    }

    pub(crate) fn session_id(&self) -> Option<&str> {
        match &self.session {
            Some(session) => Some(&session.session_id),
            None => self.preview.session_id.as_deref(),
        }
    }

    pub(crate) fn coordinator_mode(&self) -> bool {
        match &self.session {
            Some(session) => session.engine_half.coordinator_mode_handle.get(),
            None => self.preview.coordinator_mode,
        }
    }

    /// The job a prompt goes to while there is no session: the worker
    /// started for this session before the build. `None` once the session
    /// is in (its own `pending_hosted_session` takes over) and for a local
    /// session.
    pub(crate) fn hosted_job_before_session(&self) -> Option<&str> {
        if self.session.is_some() {
            return None;
        }
        self.preview.hosted_job_id.as_deref()
    }

    /// The one visible sign that a session's worker is still coming up: a
    /// dot after the cwd, and only once it has taken a while, words. No
    /// dialog, no locked input, no row in the transcript — the session is
    /// usable now. Before the session exists the wait is the slot's; after,
    /// the session's own.
    pub(crate) fn hosted_wait_footer_hint(&self) -> Option<&'static str> {
        let waited = match &self.session {
            Some(session) => session
                .pending_hosted_session
                .as_ref()?
                .started_at
                .elapsed(),
            None => {
                self.preview.hosted_job_id.as_ref()?;
                self.started_at.elapsed()
            }
        };
        if waited >= HOSTED_WAIT_SLOW_HINT_AFTER {
            Some("· starting session host…")
        } else {
            Some("·")
        }
    }

    /// Enter came before the session: keep the composer as it is and
    /// remember to submit it once the session is in. Reports whether this
    /// press is the first, so the caller says so once.
    pub(crate) fn defer_enter(&mut self) -> bool {
        let first = !self.enter_deferred;
        self.enter_deferred = true;
        first
    }

    pub(crate) fn take_deferred_enter(&mut self) -> bool {
        std::mem::take(&mut self.enter_deferred)
    }

    pub(crate) fn note_permission_mode_cycled(&mut self) {
        self.permission_mode_cycled = true;
    }

    pub(crate) fn permission_mode_cycled(&self) -> bool {
        self.permission_mode_cycled
    }

    /// The session, if the build has delivered it since the last poll.
    ///
    /// A build that ended without sending (its task dropped the sender) is
    /// reported as an error rather than waited for forever.
    pub(crate) fn poll_arrival(&mut self) -> Option<anyhow::Result<TuiEngineSession>> {
        let arrival = self.arrival.as_ref()?;
        match arrival.try_recv() {
            Ok(result) => {
                self.arrival = None;
                Some(result)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.arrival = None;
                Some(Err(anyhow::anyhow!(
                    "the session build ended without producing a session"
                )))
            }
        }
    }

    /// Put the built session in. The preview keeps answering for anything
    /// the session does not (it never has to).
    pub(crate) fn install(&mut self, session: TuiEngineSession) {
        self.arrival = None;
        self.session = Some(session);
    }

    #[cfg(test)]
    pub(crate) fn pending_for_test(preview: StartupPreview) -> (Self, SessionArrivalSender) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Self::pending(rx, preview), tx)
    }
}

#[cfg(test)]
pub(crate) type SessionArrivalSender = std::sync::mpsc::Sender<anyhow::Result<TuiEngineSession>>;

#[cfg(test)]
impl StartupPreview {
    pub(crate) fn for_test(cwd: &str) -> Self {
        Self {
            cwd: cwd.to_string(),
            session_id: None,
            hosted_job_id: None,
            provider_name: "test".into(),
            model_name: "test-model".into(),
            provider_format: None,
            fast_mode: false,
            effort_level: None,
            permission_mode: None,
            coordinator_mode: false,
        }
    }

    pub(crate) fn hosted_for_test(cwd: &str, session_id: &str, job_id: &str) -> Self {
        Self {
            session_id: Some(session_id.to_string()),
            hosted_job_id: Some(job_id.to_string()),
            ..Self::for_test(cwd)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Before the session exists the slot answers from the preview; once it
    /// is in, from the session — and the hosted job stops being the slot's
    /// concern, because the session's own pending wait has it.
    #[test]
    fn the_slot_answers_from_the_preview_until_the_session_is_in() {
        let preview = StartupPreview::hosted_for_test("C:/proj", "sess-1", "bg-1");
        let (mut slot, _tx) = SessionSlot::pending_for_test(preview);
        assert!(!slot.is_ready());
        assert_eq!(slot.cwd(), "C:/proj");
        assert_eq!(slot.provider_name(), "test");
        assert_eq!(slot.model_name(), "test-model");
        assert_eq!(slot.session_id(), Some("sess-1"));
        assert_eq!(slot.hosted_job_before_session(), Some("bg-1"));
        assert_eq!(slot.hosted_wait_footer_hint(), Some("·"));

        let session = super::super::test_support::make_test_tui_session();
        let session_id = session.session_id.clone();
        slot.install(session);
        assert!(slot.is_ready());
        assert_eq!(slot.session_id(), Some(session_id.as_str()));
        assert_eq!(slot.hosted_job_before_session(), None);
        assert_eq!(
            slot.hosted_wait_footer_hint(),
            None,
            "a test session waits for no worker"
        );
    }

    /// The preview's model is the build's: override, then the entry's own,
    /// then the registry default an entry without one gets — asked for
    /// only in that last case.
    #[test]
    fn the_preview_model_is_resolved_the_way_the_build_resolves_it() {
        assert_eq!(
            model_for_preview(Some("gpt-x"), "entry-model", || panic!("not asked")),
            "gpt-x"
        );
        assert_eq!(
            model_for_preview(None, "entry-model", || panic!("not asked")),
            "entry-model"
        );
        assert_eq!(
            model_for_preview(None, "  ", || Some("registry-default".into())),
            "registry-default"
        );
        assert_eq!(model_for_preview(None, "", || None), "");
    }

    /// A local session in the making shows no dot: there is no worker to
    /// wait for, only a build.
    #[test]
    fn a_local_slot_shows_no_hosted_dot() {
        let (slot, _tx) = SessionSlot::pending_for_test(StartupPreview::for_test("."));
        assert_eq!(slot.hosted_wait_footer_hint(), None);
        assert_eq!(slot.hosted_job_before_session(), None);
    }

    /// The build's result reaches the loop through the slot, once; a build
    /// that drops its sender is an error, not a wait without end.
    #[test]
    fn the_arrival_is_polled_once_and_a_dropped_builder_is_an_error() {
        let (mut slot, tx) = SessionSlot::pending_for_test(StartupPreview::for_test("."));
        assert!(slot.poll_arrival().is_none(), "nothing sent yet");
        tx.send(Ok(super::super::test_support::make_test_tui_session()))
            .unwrap();
        let arrived = slot.poll_arrival().expect("the session arrived");
        assert!(arrived.is_ok());
        assert!(
            slot.poll_arrival().is_none(),
            "the channel is consumed with the arrival"
        );

        let (mut slot, tx) = SessionSlot::pending_for_test(StartupPreview::for_test("."));
        drop(tx);
        let arrived = slot.poll_arrival().expect("a dropped builder is reported");
        assert!(arrived.is_err());
    }

    /// The first early Enter is the one that gets told about; the composer
    /// is left alone either way, and the deferral is consumed once.
    #[test]
    fn an_early_enter_is_deferred_once() {
        let (mut slot, _tx) = SessionSlot::pending_for_test(StartupPreview::for_test("."));
        assert!(slot.defer_enter(), "the first press is announced");
        assert!(!slot.defer_enter(), "the second is not");
        assert!(slot.take_deferred_enter());
        assert!(!slot.take_deferred_enter());
    }
}
