//! The terminal's shell around a session, and the mirror state it keeps.
//!
//! [`TuiEngineSession`] is what a terminal holds: an [`EngineSession`] plus the
//! handful of fields only something with a screen has. It is cli-local, because
//! only this binary has a terminal — but it is not a *drawing* concern, which is
//! why it lives here rather than under `crate::tui`. Nothing in this file
//! renders, reads a key, or names ratatui.
//!
//! The split it carries matters more than it looks. Everything the worker and
//! the IPC server need is on [`EngineSession`]; everything only a terminal has
//! is here. Keeping the second list short is what lets the first one leave
//! `rebon-cli` at all, so a field added here should be one no headless surface
//! could ever read.
//!
//! `crate::tui::wiring` re-exports the type, so the ~80 places that spell
//! `crate::tui::wiring::TuiEngineSession` keep resolving unchanged.

pub mod handover;
pub mod hosted_startup;
pub mod startup_gates;

use crate::session::EngineSession;

pub struct TuiEngineSession {
    /// Everything a session is without a terminal; see [`EngineSession`],
    /// which this derefs to.
    pub session: EngineSession,
    /// Optional stale-session warning computed for startup `--resume`.
    /// Injected into the transcript on boot to mirror `/resume`.
    pub resume_warning: Option<String>,
    /// UI mode used by the current TUI event loop and terminal surface.
    pub ui_mode: crate::ui_config::UiMode,
    /// Persisted UI mode selected for the next launch. This can differ from
    /// `ui_mode` after a Settings change that requires a restart.
    pub configured_ui_mode: crate::ui_config::UiMode,
    /// TUI-only formula display preference exposed as a synthetic Settings
    /// config option and mirrored into `AppState` for immediate rendering.
    pub math_rendering_mode: crate::rebon_config::MathRenderingMode,
    /// Cross-process background worker currently proxied by this terminal.
    ///
    /// Mirror state: it says what this screen is showing somebody else's
    /// session through. No headless surface has one — the worker *is* the
    /// thing being mirrored — which is why it sits here and not on
    /// [`EngineSession`].
    pub remote_background_attachment: Option<crate::background::RemoteBackgroundAttachment>,
    /// A `/hosted` handover in progress: this session was given to a worker
    /// and the terminal is waiting for that worker's IPC endpoint to appear
    /// so it can attach to it as a mirror. Mirror state for the same reason.
    pub pending_hosted_session: Option<crate::background::PendingHostedSession>,
    /// What this terminal was launched with; see
    /// [`crate::tui::wiring::TuiStartupParams`]. Named apart from
    /// [`EngineSession::startup`], which this derefs to: these are the
    /// terminal's launch flags, those are the session's.
    pub terminal_startup: crate::tui::wiring::TuiStartupParams,
}

/// The runtime half is reached through the shell.
///
/// This exists to carry one split, not as a design: the shell has a handful of
/// view fields and the session has eleven, and the tree reads the session's
/// from 1,788 places against 80 for the view ones. Spelling those 1,788 as
/// `session.session.x` would have made the move a diff nobody could review, so
/// they keep reading `session.x` and resolve through here.
///
/// View fields are named directly on the shell; session fields come through
/// this. Where a borrow of both at once conflicts, the session side is spelled
/// `session.session.x` explicitly. Turning every access explicit is the last
/// step of the thin-client refactor and needs the 210 `&TuiEngineSession`
/// parameters judged one by one first: each becomes a handle, a parameter
/// struct, or a view.
impl std::ops::Deref for TuiEngineSession {
    type Target = EngineSession;

    fn deref(&self) -> &EngineSession {
        &self.session
    }
}

impl std::ops::DerefMut for TuiEngineSession {
    fn deref_mut(&mut self) -> &mut EngineSession {
        &mut self.session
    }
}

impl TuiEngineSession {
    /// [`EngineSession::swap_runtime`], plus what the terminal has to forget
    /// when the runtime is genuinely replaced.
    ///
    /// Shadows the one reached through `Deref`, deliberately: a replaced
    /// runtime is a different session, and a mirror of the *old* session's
    /// worker still attached to this terminal would go on rendering another
    /// conversation's output. The engine half clears
    /// `attached_background_job_id` on the same `replaced`; these two are the
    /// terminal's half of that one act.
    pub fn swap_runtime(
        &mut self,
        session_id: &str,
        cwd: &str,
        active_prompt_running: bool,
    ) -> anyhow::Result<bool> {
        let replaced = self
            .session
            .swap_runtime(session_id, cwd, active_prompt_running)?;
        if replaced {
            self.remote_background_attachment = None;
            self.pending_hosted_session = None;
        }
        Ok(replaced)
    }

    /// This session's scratchpad, when this terminal is what hosts the
    /// session: read it before the session is let go (`swap_runtime` forgets
    /// the mirror state that decides it) and [`OwnedScratchpad::remove`] it
    /// after.
    ///
    /// `None` for a mirror, a handover in flight, or a session opened from a
    /// background job — in each of those a worker has, or can take back, the
    /// session, and the scratchpad is its working directory too.
    pub(crate) fn owned_scratchpad(&self) -> Option<OwnedScratchpad> {
        let hosted_elsewhere = self.remote_background_attachment.is_some()
            || self.pending_hosted_session.is_some()
            || self.attached_background_job_id.is_some();
        (!hosted_elsewhere).then(|| OwnedScratchpad {
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
        })
    }
}

/// A scratchpad whose session this terminal ended; see
/// [`TuiEngineSession::owned_scratchpad`].
pub(crate) struct OwnedScratchpad {
    cwd: String,
    session_id: String,
}

impl OwnedScratchpad {
    pub(crate) fn remove(self) {
        rebon_core::system_prompt::remove_scratchpad_for(&self.cwd, &self.session_id);
    }
}

/// Read a session command's inputs off a terminal's state.
///
/// A boundary adapter: it reads the terminal's `AppState` and hands back the
/// plain-data struct `rebon-session-runtime` takes. It sits here rather than
/// under `crate::tui` for the same reason the shell above does — reading a
/// screen's state is not drawing it, and the runtime crate must not learn what
/// an `AppState` is.
///
/// A free function rather than an inherent `impl`, because the type it builds
/// belongs to another crate now and only that crate may add inherent methods.
/// `ui_mode` comes separately because it lives on the shell, not on `AppState`.
pub(crate) fn session_command_inputs_from_app<'a>(
    app: &'a crate::tui::app::AppState,
    ui_mode: crate::ui_config::UiMode,
) -> crate::session::commands::SessionCommandInputs<'a> {
    crate::session::commands::SessionCommandInputs {
        rows: app.rebon_tui.transcript.rows(),
        permission_mode: app.permission_mode,
        usage: app.usage(),
        streaming_token_count: app.streaming_token_count,
        auto_mode_denials: &app.auto_mode_denials,
        auto_mode_verdicts: &app.auto_mode_verdicts,
        task_snapshots: app.task_snapshots(),
        session_title: app.session_title.as_deref(),
        ui_mode,
        vim_mode: app.vim_mode.map(|mode| mode.as_label()),
        ultraplan_phase: app
            .ultraplan_status
            .as_ref()
            .map(|status| format!("{:?}", status.phase)),
        update_notice: app.update_notice.as_ref(),
    }
}
