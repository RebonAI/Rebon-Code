//! Local TUI entrypoint — the default run mode of the `rebon` binary.
//!
//! Module layering:
//!
//! * [`crate::main`] — thin clap dispatcher.
//! * [`wiring`] — shared engine / model-client / publishers
//!   construction, used by both `--acp` and the local TUI path;
//!   [`crate::session::build::build_tui_session`] gives the runner a full harness
//!   session.
//! * `terminal` module — crossterm raw-mode / alt-screen lifecycle,
//!   panic-hook terminal restoration, RAII teardown guard.
//! * `app` / `runner` / `event` / `dispatch` modules — top-level
//!   `AppState`, main event loop, crossterm → `rebon_tui::promptinput`
//!   event translation, and dispatch of `*Plan` / `*Action` results
//!   onto `AppState`; the runner drives [`app::AppState`] through
//!   `rebon_tui::promptinput` + `rebon_tui::render_prompt_input` for the
//!   prompt surface.
//! * [`update`] — engine session update stream translation.
//! * Further modules cover the submit path, permissions + cancel,
//!   and the remaining UI surfaces.

use crate::rebon_config::RuntimeOverride;

pub mod agent_switcher;
pub mod agent_view;
pub mod ambiguous_width;
pub mod app;
pub mod at_mention_picker;
pub mod bridge_dialog;
pub mod clipboard_image;
pub mod compact_widget;
pub mod dialog_host;
pub mod dialog_support;
pub mod dispatch;
pub mod event;
pub mod external_editor;
pub mod global_search_dialog;
pub mod goal_confirm_dialog;
pub mod mcp_dialog;
pub mod onboarding_dialog;
pub mod permission_modal;
pub mod preflight;
pub mod prompt_tips;
pub mod resume_dialog;
pub mod rewind_dialog;
pub mod runner;
pub mod slash_picker;
pub mod spinner_verbs;
pub mod startup_dialog;
pub mod terminal;
pub mod ui_registry;
pub mod ultraplan_widget;
pub mod update;
pub mod wiring;

fn apply_saved_theme_for_startup_dialogs() {
    let theme_name = crate::rebon_config::saved_theme()
        .and_then(|theme| rebon_design_system::theme::ThemeName::from_str(&theme))
        .unwrap_or_default();
    rebon_design_system::theme::set_active_theme(theme_name);
}

/// How a startup resume request names the session to reopen.
///
/// `--resume <session-id>` carries the id ([`Self::Exact`]) and
/// `--continue` opens the latest stopped session in this directory
/// ([`Self::ContinueLatest`]). The runner turns either into the resume
/// dialog it opens before the first frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupResumeIntent {
    Exact(String),
    ContinueLatest,
}

/// Entry point for the local TUI run mode.
///
/// Three phases:
///
/// 1. **Async construction** via [`crate::session::build::build_tui_session`] —
///    builds the engine, model client, MCP runtime, policy store,
///    tool filter, sub-agent spawner, prompt executor, handler, and
///    the session update + permission request publishers. All of
///    these need the tokio runtime alive because MCP spawn and
///    model-client setup `.await` on reqwest / tokio::process.
/// 2. **Capture the tokio runtime handle** via
///    [`tokio::runtime::Handle::current()`]. We pass this
///    explicitly into the blocking task rather than relying on
///    `Handle::current()` from inside the blocking thread because
///    `spawn_blocking` workers are tokio-context-aware today but
///    there is no guarantee that will stay true across runtime
///    versions. Capturing explicitly also makes the dependency on
///    the runtime obvious at the call site.
/// 3. **Blocking event loop** via [`tokio::task::spawn_blocking`] —
///    crossterm's `event::poll` / `event::read` block the current
///    thread, so running them on a worker thread keeps the tokio
///    runtime responsive for the async submit path (every Enter
///    press spawns a `PromptExecutor::execute` future back onto the
///    captured handle).
///
/// Both the `TuiEngineSession` and the `Handle` are moved into the
/// blocking closure. Everything inside `TuiEngineSession`
/// (`Arc<...>`, tokio mpsc receivers, `DefaultHandler`) is `Send`,
/// and `Handle` is `Send + Clone`, so the `spawn_blocking` bound
/// is satisfied.
pub async fn run(overrides: RuntimeOverride) -> anyhow::Result<()> {
    run_with_startup_resume(overrides, None).await
}

/// [`run`] with an explicit startup resume request: the intent decides
/// which session the resume dialog opens, and suppresses any `--resume`
/// carried in the runtime overrides.
pub async fn run_with_startup_resume(
    mut overrides: RuntimeOverride,
    startup_resume: Option<StartupResumeIntent>,
) -> anyhow::Result<()> {
    if startup_resume.is_some() {
        overrides.resume = None;
    }
    let startup_started = std::time::Instant::now();
    tracing::info!("rebon startup: tui preflight started");
    apply_saved_theme_for_startup_dialogs();
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        "rebon startup: theme applied"
    );

    use crate::session_shell::startup_gates as gates;
    gates::run_development_channel_gate(&mut overrides, startup_started)?;
    let config_dir = gates::run_config_file_gate(startup_started)?;

    let cwd = std::env::current_dir()
        .map_err(|err| anyhow::anyhow!("failed to read the current directory at startup: {err}"))?;
    let mut ui_config =
        gates::run_settings_and_mcp_gates(&mut overrides, &config_dir, &cwd, startup_started)?;
    gates::run_trust_gate(&cwd, startup_started)?;
    gates::run_kernel_plugin_runtime_gate(startup_started).await?;

    let startup_provider =
        gates::run_credentials_gate(&mut overrides, &mut ui_config, &config_dir, startup_started)
            .await?;

    let host = crate::session_shell::hosted_startup::decide_session_host(
        &mut overrides,
        &cwd,
        startup_resume.is_some(),
        startup_started,
    );
    let draw_before_session = host.draw_before_session;
    let hosted_startup = host.hosted;

    let handle = tokio::runtime::Handle::current();
    let hosted_job_id = hosted_startup.as_ref().map(|hosted| hosted.job_id.clone());
    let build_started = std::time::Instant::now();
    // Read off the run's arguments before the builder takes them.
    let tui_ui_mode = overrides.ui_mode.unwrap_or_default();
    let tui_startup = wiring::TuiStartupParams::from_overrides(&overrides);
    let finish_session = move |mut session: wiring::TuiEngineSession| {
        if let Some(job_id) = hosted_job_id {
            session.pending_hosted_session =
                Some(crate::background::PendingHostedSession::startup(job_id));
        }
        tracing::info!(
            elapsed_ms = startup_started.elapsed().as_millis() as u64,
            build_ms = build_started.elapsed().as_millis() as u64,
            session_id = %session.session_id,
            mirror = session.pending_hosted_session.is_some(),
            "rebon startup: tui session built"
        );
        session
    };

    if draw_before_session {
        // The runner enters the terminal and draws while the session builds
        // on this task; the build's result reaches the loop through the
        // slot. A loop that ends first (Ctrl+C twice, or an error) drops
        // the receiver, and the session is dropped here with its lock.
        let preview = runner::StartupPreview::resolve(
            &overrides,
            &cwd.to_string_lossy(),
            hosted_startup.as_ref(),
            startup_provider.resolved(),
        );
        let (session_tx, session_rx) = std::sync::mpsc::channel();
        let slot = runner::SessionSlot::pending(session_rx, preview);
        tracing::info!(
            elapsed_ms = startup_started.elapsed().as_millis() as u64,
            "rebon startup: runner started before the session"
        );
        let loop_task = tokio::task::spawn_blocking(move || {
            runner::run_blocking(slot, handle, ui_config, startup_resume)
        });
        let built = crate::session::build::build_tui_session(overrides)
            .await
            .map(|build| finish_session(wiring::into_tui_session(build, tui_ui_mode, tui_startup)));
        // Cloned out before the session moves into the runner: the pool's
        // agent processes outlive the run loop, and their deliberate stop
        // belongs after it returns (kill-on-drop stays the backstop).
        let acp_subagent_pool = built
            .as_ref()
            .ok()
            .and_then(|session| session.engine_half.runtime.acp_subagent_pool.clone());
        if let Err(unsent) = session_tx.send(built) {
            tracing::info!(
                built = unsent.0.is_ok(),
                "rebon startup: the runner ended before the session was built"
            );
        }
        let result = match loop_task.await {
            Ok(result) => result,
            Err(err) => Err(tui_worker_join_error(err)),
        };
        if let Some(pool) = acp_subagent_pool {
            pool.shutdown().await;
        }
        return result;
    }

    let session = finish_session(wiring::into_tui_session(
        crate::session::build::build_tui_session(overrides).await?,
        tui_ui_mode,
        tui_startup,
    ));
    // Cloned out before the session moves into the runner: the pool's
    // agent processes outlive the run loop, and their deliberate stop
    // belongs after it returns (kill-on-drop stays the backstop).
    let acp_subagent_pool = session.engine_half.runtime.acp_subagent_pool.clone();
    let slot = runner::SessionSlot::ready(session);
    let result = match tokio::task::spawn_blocking(move || {
        runner::run_blocking(slot, handle, ui_config, startup_resume)
    })
    .await
    {
        Ok(result) => result,
        Err(err) => Err(tui_worker_join_error(err)),
    };
    if let Some(pool) = acp_subagent_pool {
        pool.shutdown().await;
    }
    result
}

fn tui_worker_join_error(err: tokio::task::JoinError) -> anyhow::Error {
    if err.is_panic() {
        anyhow::anyhow!("TUI worker panicked; panic-hook terminal restoration was attempted: {err}")
    } else if err.is_cancelled() {
        anyhow::anyhow!("TUI worker was cancelled: {err}")
    } else {
        anyhow::anyhow!("TUI worker failed to join: {err}")
    }
}

#[cfg(test)]
mod tests {
    use super::tui_worker_join_error;

    #[tokio::test]
    async fn tui_worker_panic_is_reported_as_fatal_after_restoration_attempt() {
        let join_error = tokio::spawn(async { panic!("worker panic") })
            .await
            .expect_err("task should panic");
        let error = tui_worker_join_error(join_error);

        assert!(error.to_string().contains("TUI worker panicked"));
        assert!(error
            .to_string()
            .contains("terminal restoration was attempted"));
    }

    #[tokio::test]
    async fn tui_worker_cancellation_is_reported_separately() {
        let task = tokio::spawn(std::future::pending::<()>());
        task.abort();
        let join_error = task.await.expect_err("task should be cancelled");
        let error = tui_worker_join_error(join_error);

        assert!(error.to_string().contains("TUI worker was cancelled"));
        assert!(!error.to_string().contains("panicked"));
    }
}
