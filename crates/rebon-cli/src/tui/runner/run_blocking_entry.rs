//! Blocking TUI entry point that owns terminal setup and one-time AppState
//! initialization before handing control to the event loop.
//!
//! RFC-0004 §9: the first frame does not wait for the session. The runner
//! takes a [`SessionSlot`] — the session, or the channel it will arrive on
//! plus what the first frame needs to know — enters the terminal, and lets
//! the event loop draw. A session that was built before the runner started
//! is installed here, before the terminal, in the order it always was; one
//! that is still building is installed by the loop when it arrives, through
//! the same [`install_session`].

use rebon_design_system::theme::ThemeName;
use rebon_hooks::HookEventPayload;
use rebon_tui::RenderTheme;
use tokio::runtime::Handle;

use crate::session::transcript_replay::collect_auto_mode_allowed_ids;
use crate::tui::app::AppState;
use crate::tui::terminal::{TerminalGuard, TerminalUiSurface};
use crate::tui::wiring::TuiEngineSession;
use crate::ui_config::{ResolvedUiConfig, UiMode};

use super::agent_view::load_agent_view_grouping_preference;
use super::commands::{register_local_slash_commands, register_registered_skill_slash_commands};
use super::event_loop_entry::{
    event_loop, event_loop_with_mode, inline_event_loop, EventLoopOutcome,
};
use super::inline_banner::{emit_inline_startup_banner_from, startup_banner_for_slot};
use super::inline_commit_cursor::{prime_inline_scrollback_from_transcript, InlineRuntimeState};
use super::interrupt_flow::stop_session_tasks;
use super::onboarding_hooks::fire_onboarding_opened;
use super::permission_mode::{
    push_permission_mode_into_session, record_background_permission_mode_acceptance,
};
use super::prompt_history::spawn_input_history_load;
use super::prompt_lifecycle::sync_current_session_title;
use super::terminal_capabilities::{terminal_math_display_mode, terminal_supports_hyperlinks};
use super::title::apply_session_title_effect;
use super::transcript_messages::inject_system_message;
use super::transcript_replay::replay_transcript_entries_with_agent_tasks;
use super::ultraplan::restore_ultraplan_run_for_runtime;
use super::SessionSlot;
use crate::session::commands::effort::{load_persisted_effort_level, provider_kind_from_format};

fn active_ui_mode_after_initial_loop(
    startup_onboarding_present: bool,
    onboarding_completed: bool,
    configured_ui_mode: UiMode,
    selected_ui_mode: UiMode,
) -> UiMode {
    if startup_onboarding_present && onboarding_completed {
        selected_ui_mode
    } else {
        configured_ui_mode
    }
}

/// Where `--resume <session>` goes when the session lives in a job.
///
/// A session a worker has had is that worker's job's: `--resume` on it
/// means what `rebon attach` means — mirror the worker, or give the job a
/// worker and mirror that — not opening the transcript in this process
/// beside it. Returns the job to attach to; `None` means resume locally,
/// which is still what a session no job names gets.
///
/// `--local` asks for the transcript in this process and skips the
/// question: a job is not consulted at all. A worker still holding the
/// session's lock refuses the resume further down, as it always has.
pub(super) fn job_to_attach_for_resume(
    startup_resume: Option<&crate::tui::StartupResumeIntent>,
    local: bool,
) -> Option<String> {
    let crate::tui::StartupResumeIntent::Exact(session_id) = startup_resume? else {
        return None;
    };
    if local {
        return None;
    }
    crate::background::job_for_session(session_id).map(|job| job.identity.job_id)
}

/// What the runner decided before the session existed and the session has
/// to be told about when it is installed.
#[derive(Debug, Clone, Copy)]
pub(super) struct InstallOptions {
    pub math_rendering: crate::rebon_config::MathRenderingMode,
    /// The UI mode a first-run onboarding dialog opens with.
    pub ui_mode: UiMode,
    /// Shift+Tab was pressed before the session existed: the mode the app
    /// shows wins over the session's startup default.
    pub permission_mode_cycled: bool,
}

/// Everything the runner does with a session once it has one, in the order
/// it always did — whether the session was built before the first frame or
/// arrived after it. Returns whether a `--resume` transcript
/// was replayed into the view.
pub(super) fn install_session(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    startup_resume: Option<crate::tui::StartupResumeIntent>,
    options: InstallOptions,
) -> bool {
    let install_started = std::time::Instant::now();
    // The SessionStart hook runs on the runtime and its messages land in
    // the transcript when it is done (`drain_session_start_hook`). It used
    // to run here with `block_on` — 70–250 ms on the UI thread between the
    // first frame and the session — for effects that only ever fed the
    // view: what it says is shown, never sent with the first turn.
    {
        let (tx, rx) = std::sync::mpsc::channel();
        let model = session.model.name.clone();
        let hook_handle = handle.clone();
        let policy = session.engine_half.runtime.policy.clone();
        handle.spawn_blocking(move || {
            let started = std::time::Instant::now();
            let verdict = hook_handle.block_on(policy.emit(HookEventPayload::SessionStart {
                source: "tui".into(),
                model,
            }));
            let _ = tx.send((started.elapsed(), verdict));
        });
        session.session_start_hook = Some(rx);
    }

    app.coordinator_mode = session.engine_half.coordinator_mode_handle.get();
    app.sync_task_snapshots(session.engine_half.tasks.snapshots());
    #[cfg(test)]
    {
        app.tasks = session.engine_half.tasks.clone();
    }
    // The session's ledger, not this screen's: the turns counted in it may
    // have been run by a worker, and `/cost` must answer with what the
    // session cost rather than with what this terminal happened to watch.
    app.usage_ledger = session.engine_half.usage_ledger.clone();
    app.mid_turn_queued_submit_poller = Some(session.engine_half.runtime.mid_turn_queue.clone());
    session.math_rendering_mode = options.math_rendering;
    app.cwd = session.cwd.clone();
    sync_current_session_title(app, session);
    if let Some(intent) = startup_resume {
        app.resume_dialog = Some(match intent {
            crate::tui::StartupResumeIntent::Exact(session_id) => {
                crate::tui::resume_dialog::ResumeDialogState::open_exact(session_id)
            }
            crate::tui::StartupResumeIntent::ContinueLatest => {
                crate::tui::resume_dialog::ResumeDialogState::open_latest()
            }
        });
    }

    if session.terminal_startup.agent_view {
        crate::background::refresh_agent_view_supervisor_heartbeat();
        let store = crate::background::cli_default_store();
        let jobs = store.list_jobs().unwrap_or_default();
        let snapshots = app.task_snapshots();
        app.agent_view = Some(crate::tui::agent_view::AgentViewState::open_with_grouping(
            &store,
            jobs,
            &snapshots,
            session.terminal_startup.agent_view_cwd_scope.clone(),
            load_agent_view_grouping_preference(),
            session.attached_background_job_id.clone(),
        ));
    }

    // Wire the shared auto-mode handles. The engine's permission broker
    // already holds a `SharedDenialSink` over `session.engine_half.auto_mode_denials`
    // and a `PermissionModeProvider` over `session.engine_half.permission_mode_cell`
    // (installed during `build_tui_session`); here we make AppState
    // point at the same `Arc`s so `set_permission_mode` / the
    // `/permissions` command reach the same data.
    app.auto_mode_denials = session.engine_half.auto_mode_denials.clone();
    app.auto_mode_verdicts = session.engine_half.auto_mode_verdicts.clone();
    app.permission_mode_cell = session.engine_half.permission_mode_cell.clone();

    // The built-in slash commands were seeded before the first frame so the
    // picker activates on "/". That seed read the catalog before this session
    // booted the kernel, so it answered from the built-in fallback table and
    // carries none of the commands the `agents`, `tasks`, `updater` and
    // `profile` plugins register; this is the first moment the seat is live.
    // Catalog first, then the session's skills: a skill named after a command
    // is the one that gets dropped.
    register_local_slash_commands(&mut app.slash_commands);
    register_registered_skill_slash_commands(
        &mut app.slash_commands,
        &session.engine_half.skill_registry,
    );

    // Set provider kind for effort/thinking label distinction.
    app.effort_provider_kind = provider_kind_from_format(session.model.provider_format);
    if let Some(effort) = session.startup.effort_level {
        app.effort_level = Some(effort);
    }
    if options.permission_mode_cycled {
        push_permission_mode_into_session(app, session);
    } else if let Some(permission_mode) = session.startup.permission_mode {
        app.set_permission_mode(permission_mode);
        record_background_permission_mode_acceptance(permission_mode);
    } else {
        // Whatever the app shows already is what the session's cell should
        // hold: the two were separate objects until this line.
        let mode = app.permission_mode;
        app.set_permission_mode(mode);
    }

    // Auto-show onboarding on first run: setup has not been completed yet,
    // and the plugin that owns the wizard is loaded to show it.
    if crate::tui::onboarding_dialog::first_run_wizard_due() {
        app.onboarding_dialog = Some(
            rebon_plugin_onboarding::OnboardingDialogState::open_with_ui_mode(options.ui_mode),
        );
        if let Some(dialog) = app.onboarding_dialog.as_ref() {
            fire_onboarding_opened(session, handle, dialog, "first_run");
        }
        tracing::info!("rebon: first run detected, showing onboarding");
    }

    // A session whose worker was started at startup is already waiting for
    // it; attaching now would find a worker with no endpoint yet and refuse
    // to start another beside it.
    let startup_attach = session
        .attached_background_job_id
        .clone()
        .filter(|_| session.pending_hosted_session.is_none());
    if let Some(job_id) = startup_attach {
        match crate::background::attach_background_job(&job_id) {
            Ok(target) => {
                let attached = super::apply_background_attach_target(app, session, &target);
                if attached {
                    // The attach path already replayed the transcript from the
                    // session store; replaying the copy wiring loaded for
                    // `--resume` on top would duplicate every message and
                    // clobber the agent-task mapping.
                    session.loaded_transcript.clear();
                }
                if !attached {
                    session.attached_background_job_id = None;
                }
            }
            Err(err) => {
                session.attached_background_job_id = None;
                inject_system_message(
                    app,
                    "error",
                    &format!("Failed to attach background session {job_id}: {err}"),
                );
            }
        }
    }

    // Replay loaded transcript entries (from --resume) into the TUI.
    let had_loaded_transcript = !session.loaded_transcript.is_empty();
    if had_loaded_transcript {
        let count = session.loaded_transcript.len();
        app.auto_mode_allowed_tool_ids = collect_auto_mode_allowed_ids(&session.loaded_transcript);
        app.background_agent_tool_tasks = replay_transcript_entries_with_agent_tasks(
            &mut app.rebon_tui,
            std::mem::take(&mut session.loaded_transcript),
        );
        tracing::info!(entries = count, "rebon: replayed transcript into TUI");
        // The presentation store now owns the full replay. Release the duplicate
        // ACP raw vector; the engine rebuilds its bounded window from JSONL on
        // the first prompt if it has no matching cache yet.
        let _ = session
            .engine_half
            .handler
            .state()
            .release_transcript_residency(&session.session_id);
    }
    if let Some(mut run_state) = rebon_session::latest_active_run_for_session(
        &session.projects_root,
        &session.cwd,
        &session.session_id,
    ) {
        let outcome = restore_ultraplan_run_for_runtime(app, &mut run_state);
        run_state.prepare_for_persist();
        let preflight_error =
            crate::session::ultraplan_preflight::preflight_ultraplan_run(session, &mut run_state)
                .err();
        if let Err(err) =
            rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &run_state)
        {
            tracing::warn!(
                error = %err,
                run_id = %run_state.run_id,
                "failed to persist migrated or reconciled ultraplan run during startup restore"
            );
        }
        if let Some(diagnostic) = preflight_error {
            inject_system_message(
                app,
                "warning",
                &format!(
                    "Restored ultraplan run with degraded parent-session fallback: {diagnostic}"
                ),
            );
        }
        if outcome.restored {
            inject_system_message(
                app,
                "info",
                &format!(
                    "Restored active ultraplan run {} (phase {:?}, round {}).",
                    run_state.run_id, run_state.phase, run_state.round
                ),
            );
        }
    }
    if let Some(warning) = session.resume_warning.take() {
        inject_system_message(app, "info", &warning);
    }
    for notice in session.terminal_startup.notices.drain(..) {
        inject_system_message(app, "info", &notice);
    }
    tracing::info!(
        elapsed_ms = install_started.elapsed().as_millis() as u64,
        session_id = %session.session_id,
        "rebon startup: session installed"
    );
    had_loaded_transcript
}

/// The SessionStart event's outcome, once it is in: its messages go into
/// the transcript, as they did when the install waited for it.
///
/// A subscriber that failed does not reach here — a notification event's
/// failure is logged where it happened and costs only that subscriber's
/// contribution.
pub(super) fn drain_session_start_hook(app: &mut AppState, session: &mut TuiEngineSession) {
    let Some(rx) = session.session_start_hook.as_ref() else {
        return;
    };
    let (elapsed, verdict) = match rx.try_recv() {
        Ok(landed) => landed,
        Err(std::sync::mpsc::TryRecvError::Empty) => return,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            session.session_start_hook = None;
            tracing::warn!("SessionStart hook task ended without reporting; continuing");
            return;
        }
    };
    session.session_start_hook = None;
    tracing::info!(
        elapsed_ms = elapsed.as_millis() as u64,
        "rebon startup: SessionStart hooks completed"
    );
    for message in rebon_core::hooks::apply_session_lifecycle_effects(verdict.effects()) {
        inject_system_message(app, "info", &message);
    }
}

pub fn run_blocking(
    mut slot: SessionSlot,
    handle: Handle,
    ui_config: ResolvedUiConfig,
    mut startup_resume: Option<crate::tui::StartupResumeIntent>,
) -> anyhow::Result<()> {
    // `rebon attach` already knows its job; `--resume` has to ask whether
    // the session it names lives in one.
    if let Some(session) = slot.session_mut() {
        if session.attached_background_job_id.is_none() {
            if let Some(job_id) =
                job_to_attach_for_resume(startup_resume.as_ref(), session.terminal_startup.local)
            {
                tracing::info!(
                    %job_id,
                    "rebon startup: the resumed session lives in a background job; attaching instead"
                );
                session.attached_background_job_id = Some(job_id);
                startup_resume = None;
            }
        }
    }
    let startup_resume_pending = startup_resume.is_some();
    let startup_started = std::time::Instant::now();
    tracing::info!(
        session_id = ?slot.session_id(),
        session_ready = slot.is_ready(),
        "rebon startup: tui runner started"
    );
    let RunnerStartup {
        surface,
        mut app,
        had_loaded_transcript,
    } = prepare_runner_app(&mut slot, &handle, &ui_config, startup_resume);

    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        "rebon startup: entering terminal surface"
    );
    let mut guard = match TerminalGuard::enter_surface(surface) {
        Ok(guard) => guard,
        Err(err) => {
            if let Some(session) = slot.session() {
                stop_session_tasks(session.engine_half.tasks.as_ref());
                session
                    .engine_half
                    .tasks
                    .close_owner_session(&session.session_id);
            }
            return Err(err.into());
        }
    };
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        "rebon startup: terminal surface entered"
    );

    // Spawn the async file scanner so the @ mention picker can show
    // file suggestions without blocking the event loop. The scanner
    // runs `git ls-files` on the tokio runtime and sends results
    // through a channel that the event loop drains each frame.
    let mut file_list_rx = crate::file_scanner::spawn_scanner(&handle, slot.cwd().to_string());
    let (file_list_tx, mut prefix_file_list_rx) = tokio::sync::mpsc::unbounded_channel();
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        "rebon startup: file scanner spawned"
    );

    // Derive the render theme from the user's saved preference (or
    // default to dark).
    let theme_name = crate::rebon_config::saved_theme()
        .and_then(|s| ThemeName::from_str(&s))
        .unwrap_or(ThemeName::Dark);
    rebon_design_system::theme::set_active_theme(theme_name);
    let mut theme = RenderTheme {
        supports_hyperlinks: terminal_supports_hyperlinks(),
        math_display: terminal_math_display_mode(ui_config.math_rendering, ui_config.mode),
        ..RenderTheme::from_theme_name(theme_name)
    };
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        "rebon startup: render theme ready"
    );
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        ui_mode = ?ui_config.mode,
        session_ready = slot.is_ready(),
        "rebon startup: entering event loop"
    );
    let mut inline_startup_banner_emitted = false;
    let startup_onboarding_present = app.onboarding_dialog.is_some();
    let mut result = (|| -> anyhow::Result<EventLoopOutcome> {
        if app.onboarding_dialog.is_some() {
            event_loop_with_mode(
                &mut guard,
                &mut app,
                &mut theme,
                &mut slot,
                &handle,
                &mut file_list_rx,
                &mut prefix_file_list_rx,
                file_list_tx.clone(),
                UiMode::Screen,
                true,
                false,
                None,
            )
        } else {
            match ui_config.mode {
                UiMode::Screen => event_loop(
                    &mut guard,
                    &mut app,
                    &mut theme,
                    &mut slot,
                    &handle,
                    &mut file_list_rx,
                    &mut prefix_file_list_rx,
                    file_list_tx.clone(),
                ),
                UiMode::Inline => {
                    // Skip the startup banner only when resuming with a non-empty
                    // loaded transcript. Startup notices (MCP warnings, lifecycle
                    // messages, resume warnings) are injected before the event loop
                    // and should not make a fresh inline session look replayed.
                    if !had_loaded_transcript && !startup_resume_pending {
                        let banner = startup_banner_for_slot(&slot, &app);
                        emit_inline_startup_banner_from(guard.terminal(), &banner)?;
                        inline_startup_banner_emitted = true;
                        tracing::info!("rebon startup: inline startup banner emitted");
                    }
                    let initial_inline_runtime = Some(if had_loaded_transcript {
                        // ratatui 0.29's insert_before scroll-region path pushes
                        // the entire above-viewport area into scrollback when the
                        // new chunk's total height exceeds viewport_top, which chops
                        // the banner mid-render. The replayed content already
                        // identifies the session; the banner adds nothing useful here.
                        prime_inline_scrollback_from_transcript(
                            guard.terminal(),
                            &mut app,
                            &theme,
                            ui_config.inline.viewport_height,
                        )?
                    } else {
                        InlineRuntimeState::with_initial_viewport_height(
                            ui_config.inline.viewport_height,
                        )
                    });
                    inline_event_loop(
                        &mut guard,
                        &mut app,
                        &mut theme,
                        &mut slot,
                        &handle,
                        &mut file_list_rx,
                        &mut prefix_file_list_rx,
                        file_list_tx.clone(),
                        initial_inline_runtime,
                    )
                }
            }
        }
    })();

    if matches!(surface, TerminalUiSurface::Inline { .. }) {
        if let Err(err) = guard.clear_inline_viewport_for_resume() {
            tracing::debug!(%err, "rebon-cli: failed to clear inline prompt before resume hint");
        }
    }
    drop(guard);

    let active_ui_mode = active_ui_mode_after_initial_loop(
        startup_onboarding_present,
        crate::rebon_config::has_completed_onboarding(),
        ui_config.mode,
        slot.session()
            .map(|session| session.ui_mode)
            .unwrap_or(ui_config.mode),
    );
    theme.math_display = terminal_math_display_mode(ui_config.math_rendering, active_ui_mode);

    while result.is_ok() && active_ui_mode == UiMode::Inline {
        match result.as_ref().unwrap() {
            EventLoopOutcome::InlineFullscreenRequested => {
                result = (|| -> anyhow::Result<EventLoopOutcome> {
                    let mut screen_guard = TerminalGuard::enter_surface(TerminalUiSurface::Screen)?;
                    event_loop_with_mode(
                        &mut screen_guard,
                        &mut app,
                        &mut theme,
                        &mut slot,
                        &handle,
                        &mut file_list_rx,
                        &mut prefix_file_list_rx,
                        file_list_tx.clone(),
                        UiMode::Screen,
                        false,
                        true,
                        None,
                    )
                })();
            }
            EventLoopOutcome::InlineFullscreenClosed => {
                result = (|| -> anyhow::Result<EventLoopOutcome> {
                    let mut inline_guard =
                        TerminalGuard::enter_surface(TerminalUiSurface::Inline {
                            height: ui_config.inline.viewport_height,
                        })?;
                    let has_replayed_transcript = !app.rebon_tui.transcript.rows().is_empty();
                    // Same banner-cut concern as the initial inline entry: skip
                    // the banner when there's already transcript content above to
                    // avoid ratatui's insert_before scroll-region from chopping it.
                    if !inline_startup_banner_emitted && !has_replayed_transcript {
                        let banner = startup_banner_for_slot(&slot, &app);
                        emit_inline_startup_banner_from(inline_guard.terminal(), &banner)?;
                        inline_startup_banner_emitted = true;
                    }
                    let initial_inline_runtime = Some(if has_replayed_transcript {
                        prime_inline_scrollback_from_transcript(
                            inline_guard.terminal(),
                            &mut app,
                            &theme,
                            ui_config.inline.viewport_height,
                        )?
                    } else {
                        InlineRuntimeState::with_initial_viewport_height(
                            ui_config.inline.viewport_height,
                        )
                    });
                    let outcome = inline_event_loop(
                        &mut inline_guard,
                        &mut app,
                        &mut theme,
                        &mut slot,
                        &handle,
                        &mut file_list_rx,
                        &mut prefix_file_list_rx,
                        file_list_tx.clone(),
                        initial_inline_runtime,
                    );
                    if let Err(err) = inline_guard.clear_inline_viewport_for_resume() {
                        tracing::debug!(%err, "rebon-cli: failed to clear inline prompt before resume hint");
                    }
                    outcome
                })();
            }
            EventLoopOutcome::Exit => break,
        }
    }

    finish_run(&mut app, slot, &handle, result, active_ui_mode)
}

/// What `prepare_runner_app` decided before the terminal was entered.
struct RunnerStartup {
    surface: TerminalUiSurface,
    app: AppState,
    /// Whether a `--resume` transcript was replayed into the view.
    had_loaded_transcript: bool,
}

/// Build the `AppState` the first frame is drawn from, and install a
/// session that is already built. Everything here happens before the
/// terminal surface is entered, in the order it always did.
fn prepare_runner_app(
    slot: &mut SessionSlot,
    handle: &Handle,
    ui_config: &ResolvedUiConfig,
    startup_resume: Option<crate::tui::StartupResumeIntent>,
) -> RunnerStartup {
    let startup_onboarding_active = crate::tui::onboarding_dialog::first_run_wizard_due();
    let surface = if startup_onboarding_active {
        TerminalUiSurface::Screen
    } else {
        match ui_config.mode {
            UiMode::Screen => TerminalUiSurface::Screen,
            UiMode::Inline => TerminalUiSurface::Inline {
                height: ui_config.inline.viewport_height,
            },
        }
    };
    let mut app = AppState::new_with_coordinator_mode(slot.coordinator_mode());
    slot.math_rendering_mode = ui_config.math_rendering;
    app.ui_mode = ui_config.mode;
    app.math_rendering_mode = ui_config.math_rendering;
    app.custom_status_line =
        crate::tui::runner::custom_status_line::CustomStatusLineState::configured(
            ui_config.status_line.clone(),
        );
    app.cwd = slot.cwd().to_string();
    // Populate built-in slash commands so the picker activates on "/".
    // The ACP server path populates these via session/new; the local
    // TUI path bypasses the server, so we seed them directly.
    app.slash_commands = rebon_slash_commands::for_surface(rebon_slash_commands::Surface::Tui);
    load_persisted_effort_level(&mut app.effort_level);
    // What the session will say about itself, said now so the first frame
    // and the inline banner do not change when it arrives.
    {
        let preview = slot.preview();
        if let Some(format) = preview.provider_format {
            app.effort_provider_kind = provider_kind_from_format(format);
        }
        if let Some(effort) = preview.effort_level {
            app.effort_level = Some(effort);
        }
        if let Some(mode) = preview.permission_mode {
            app.set_permission_mode(mode);
        }
    }

    // A session built before the runner is installed before the terminal
    // is entered, as it always was. One still building is installed by the
    // event loop when it arrives.
    let install_options = InstallOptions {
        math_rendering: ui_config.math_rendering,
        ui_mode: ui_config.mode,
        permission_mode_cycled: false,
    };
    let mut had_loaded_transcript = false;
    if let Some(session) = slot.session_mut() {
        had_loaded_transcript =
            install_session(&mut app, session, handle, startup_resume, install_options);
    }

    // The prompt history loads on its own thread and lands in the loop;
    // the first frame does not wait for a multi-megabyte file.
    slot.input_history = Some(spawn_input_history_load(
        slot.cwd().to_string(),
        slot.session_id().unwrap_or_default().to_string(),
    ));

    RunnerStartup {
        surface,
        app,
        had_loaded_transcript,
    }
}

/// Close the session down and tell the user how to resume it. The
/// terminal guard is already dropped when this runs, so the hint prints
/// onto the restored shell.
fn finish_run(
    app: &mut AppState,
    slot: SessionSlot,
    handle: &Handle,
    result: anyhow::Result<EventLoopOutcome>,
    active_ui_mode: UiMode,
) -> anyhow::Result<()> {
    if let Err(err) = &result {
        tracing::error!(
            session_id = ?slot.session_id(),
            ui_mode = ?active_ui_mode,
            overlay_blocks = app.rebon_tui.overlay.blocks.len(),
            transcript_rows = app.rebon_tui.transcript.len(),
            error = %format!("{err:#}"),
            "rebon-cli: terminal UI event loop stopped unexpectedly"
        );
    }

    // Drop the terminal guard first to leave alt-screen and restore
    // the user's shell, then print the resume hint on normal stdout.
    // Read session_id AFTER event_loop because /resume may have
    // switched it. A session that never arrived (the loop ended before the
    // build did) has nothing to close; a hosted one still has an id to
    // resume, because its worker has it.
    let final_session_id = slot.session_id().map(str::to_string);
    let final_result = result.map(|_| ());
    if let Some(mut session) = slot.into_session() {
        stop_session_tasks(session.engine_half.tasks.as_ref());
        session
            .engine_half
            .tasks
            .close_owner_session(&session.session_id);
        {
            let verdict = handle.block_on(
                session
                    .engine_half
                    .runtime
                    .policy
                    .emit(HookEventPayload::SessionEnd {
                        reason: final_result
                            .as_ref()
                            .err()
                            .map(|err| err.to_string())
                            .or_else(|| Some("exit".to_string())),
                    }),
            );
            // The title effects are this screen's; everything else goes
            // through the one lifecycle projection the engine owns.
            let rest: Vec<rebon_hooks::HookEffect> = verdict
                .effects()
                .iter()
                .filter(|effect| !apply_session_title_effect(app, &mut session, handle, effect))
                .cloned()
                .collect();
            for message in rebon_core::hooks::apply_session_lifecycle_effects(&rest) {
                tracing::info!(message = %message, "SessionEnd hook message after TUI exit");
            }
        }
    }

    let leading_newline = if active_ui_mode == UiMode::Inline {
        ""
    } else {
        "\n"
    };
    match (&final_result, final_session_id) {
        (Err(_), Some(session_id)) => eprintln!(
            "{leading_newline}rebon: terminal UI stopped unexpectedly; terminal restoration was attempted.\n\
             To resume this conversation:\n  rebon --resume {session_id}\n"
        ),
        (Err(_), None) => eprintln!(
            "{leading_newline}rebon: terminal UI stopped unexpectedly; terminal restoration was attempted.\n"
        ),
        (Ok(()), Some(session_id)) => eprintln!(
            "{leading_newline}To resume this conversation:\n  rebon --resume {session_id}\n"
        ),
        (Ok(()), None) => {}
    }

    final_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_onboarding_uses_newly_selected_inline_mode() {
        assert_eq!(
            active_ui_mode_after_initial_loop(true, true, UiMode::Screen, UiMode::Inline,),
            UiMode::Inline
        );
    }

    #[test]
    fn regular_inline_overlay_keeps_configured_mode() {
        assert_eq!(
            active_ui_mode_after_initial_loop(false, true, UiMode::Inline, UiMode::Screen,),
            UiMode::Inline
        );
    }

    /// `--local --resume <id>` opens the transcript here: the jobs are not
    /// consulted, so a session that once lived in a worker is not sent
    /// back to one. The store is deliberately not reachable from this
    /// test — a `None` that came from looking and finding nothing would
    /// prove the wrong thing.
    #[test]
    fn a_local_resume_does_not_ask_the_jobs_where_the_session_lives() {
        let intent = crate::tui::StartupResumeIntent::Exact("sess-once-hosted".into());
        assert_eq!(job_to_attach_for_resume(Some(&intent), true), None);
        assert_eq!(
            job_to_attach_for_resume(
                Some(&crate::tui::StartupResumeIntent::ContinueLatest),
                false
            ),
            None,
            "the picker decides for --continue, not startup"
        );
    }

    #[test]
    fn incomplete_startup_onboarding_keeps_configured_mode() {
        assert_eq!(
            active_ui_mode_after_initial_loop(true, false, UiMode::Screen, UiMode::Inline,),
            UiMode::Screen
        );
    }

    /// Installing a session that arrived after the first frame swaps the
    /// data source under a view that is already on screen: the rows stay,
    /// the composer stays, and the app now reads the session's registries.
    #[test]
    fn installing_a_late_session_keeps_the_view_and_binds_the_session() {
        let _guard = crate::test_env::lock_env();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let mut session = super::super::test_support::make_test_tui_session();
        session.startup.effort_level = Some(rebon_types::ReasoningEffort::High);
        let mut app = AppState::new();
        app.input = "typed before the session".into();
        app.cursor_offset = app.input.len();
        inject_system_message(&mut app, "info", "drawn before the session");
        let rows_before = app.rebon_tui.transcript.len();
        assert!(!std::sync::Arc::ptr_eq(
            &app.tasks,
            &session.engine_half.tasks
        ));

        let replayed = install_session(
            &mut app,
            &mut session,
            &handle,
            None,
            InstallOptions {
                math_rendering: crate::rebon_config::MathRenderingMode::default(),
                ui_mode: UiMode::Inline,
                permission_mode_cycled: false,
            },
        );

        assert!(!replayed, "a fresh session has no transcript to replay");
        assert_eq!(
            app.rebon_tui.transcript.len(),
            rows_before,
            "nothing on screen is reset or replayed"
        );
        assert_eq!(app.input, "typed before the session");
        assert!(std::sync::Arc::ptr_eq(
            &app.tasks,
            &session.engine_half.tasks
        ));
        assert!(std::sync::Arc::ptr_eq(
            &app.permission_mode_cell,
            &session.engine_half.permission_mode_cell
        ));
        assert_eq!(app.effort_level, Some(rebon_types::ReasoningEffort::High));
        assert_eq!(
            session.math_rendering_mode,
            crate::rebon_config::MathRenderingMode::default()
        );
    }

    /// The picker is seeded before the session exists, and the session is
    /// what boots the kernel: at seed time the catalog answers from its
    /// built-in fallback table, without the commands a plugin registers.
    /// Installing the session is the first moment the seat can be read, so
    /// that is where the list is topped up.
    #[test]
    fn installing_the_session_tops_the_picker_up_from_the_command_seat() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let mut session = super::super::test_support::make_test_tui_session();
        let mut app = AppState::new();
        // Exactly what `run_blocking` writes before the kernel has booted:
        // `for_surface` falling back to the built-in table.
        app.slash_commands = rebon_slash_commands::builtin_command_table()
            .iter()
            .filter(|spec| spec.available_on(rebon_slash_commands::Surface::Tui))
            .map(rebon_slash_commands::CommandSpec::to_wire)
            .collect();
        for command in ["agents", "tasks", "update", "skills", "memory", "migrate"] {
            assert!(
                !app.slash_commands.iter().any(|c| c.name == command),
                "/{command} is a plugin's, so it is not in the fallback table"
            );
        }

        install_session(
            &mut app,
            &mut session,
            &handle,
            None,
            InstallOptions {
                math_rendering: crate::rebon_config::MathRenderingMode::default(),
                ui_mode: UiMode::Inline,
                permission_mode_cycled: false,
            },
        );

        let listed: Vec<&str> = app
            .slash_commands
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        for command in [
            "agents",
            "tasks",
            "update",
            "profile",
            "teams",
            "workflows",
            "skills",
            "memory",
            "migrate",
        ] {
            assert!(
                listed.contains(&command),
                "/{command} reaches the picker once the session has booted the kernel; listed: {listed:?}"
            );
        }
        assert_eq!(
            listed.iter().filter(|name| **name == "help").count(),
            1,
            "a command already in the list is not added twice"
        );
    }

    /// The SessionStart event's messages reach the transcript when it is
    /// done, not before the install — and a verdict with nothing in it adds
    /// nothing.
    #[test]
    fn session_start_hook_messages_land_when_the_hook_does() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let (tx, rx) = std::sync::mpsc::channel();
        session.session_start_hook = Some(rx);

        drain_session_start_hook(&mut app, &mut session);
        assert!(session.session_start_hook.is_some(), "nothing landed yet");
        assert!(app.rebon_tui.transcript.is_empty());

        tx.send((
            std::time::Duration::from_millis(3),
            rebon_core::policy_seat::Verdict::Modify {
                effects: vec![rebon_hooks::HookEffect::InjectContext {
                    text: "hook says hello".into(),
                }],
            },
        ))
        .unwrap();
        drain_session_start_hook(&mut app, &mut session);

        assert!(session.session_start_hook.is_none(), "drained once");
        let texts: Vec<String> = app
            .rebon_tui
            .transcript
            .rows()
            .iter()
            .filter_map(|row| match row {
                rebon_tui::Message::System(message) => message.content.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["hook says hello"]);

        let (tx, rx) = std::sync::mpsc::channel();
        session.session_start_hook = Some(rx);
        tx.send((
            std::time::Duration::ZERO,
            rebon_core::policy_seat::Verdict::Allow,
        ))
        .unwrap();
        drain_session_start_hook(&mut app, &mut session);
        assert!(session.session_start_hook.is_none());
        assert_eq!(
            app.rebon_tui.transcript.len(),
            1,
            "a verdict with nothing in it adds nothing"
        );
    }

    /// A permission mode reached with Shift+Tab before the session existed
    /// is what the session gets, not its startup default.
    #[test]
    fn a_mode_cycled_before_the_session_is_pushed_into_it() {
        let _guard = crate::test_env::lock_env();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let mut session = super::super::test_support::make_test_tui_session();
        session.startup.permission_mode = Some(rebon_permissions::PermissionMode::Default);
        let mut app = AppState::new();
        app.cycle_permission_mode();
        let cycled = app.permission_mode;
        assert_ne!(cycled, rebon_permissions::PermissionMode::Default);

        install_session(
            &mut app,
            &mut session,
            &handle,
            None,
            InstallOptions {
                math_rendering: crate::rebon_config::MathRenderingMode::default(),
                ui_mode: UiMode::Inline,
                permission_mode_cycled: true,
            },
        );

        assert_eq!(app.permission_mode, cycled);
        assert_eq!(
            *session.engine_half.permission_mode_cell.lock().unwrap(),
            cycled,
            "the session's cell agrees with the app"
        );
    }
}
