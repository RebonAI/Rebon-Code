//! Immediate `/compact`: run the compaction now, report what it kept.
//!
//! `/compact` used to be a promise. It set a one-shot flag on the context
//! budget, printed "old messages will be compacted on the next model
//! request", and did nothing else — the user had to send another prompt
//! before anything happened, and when they did, a 30-second summariser call
//! ran in front of their answer with only a spinner verb to show for it.
//!
//! Here the command runs the compaction the moment it is typed. The work is
//! a provider call that cannot be done on the event-loop thread, so it lives
//! on a worker thread (the same shape `resume_runtime` uses for its summary
//! worker) while [`crate::tui::compact_widget`] paints progress above the
//! prompt. When it lands, the compacted history is installed as the
//! session's replay baseline — so the *next* turn replays it with no extra
//! work — and the transcript gets a report of what survived.
//!
//! A turn already in flight keeps the old deferred behaviour: the engine's
//! in-turn manual-compact path owns the live `ContextManager` and would
//! fight an installed baseline for it, and it is already going to compact
//! within the same turn anyway.

use std::sync::mpsc;

use tokio::runtime::Handle;

use crate::tui::app::{AppState, CompactPhase, CompactRunState};
use crate::tui::wiring::TuiEngineSession;

use super::transcript_messages::inject_system_message;
use crate::session::compact::{
    answer_deferred_command, render_compact_report, CompactProgress, CompactWorker,
};

/// Outcome of asking for a compaction, for the caller to surface.
pub(crate) enum CompactStart {
    /// The run is under way; the report lands in the transcript.
    Started,
    /// Nothing was started, for the stated reason.
    Rejected(String),
}

/// Start an immediate compaction of the current session.
///
/// `respond_to_command_id` is set when the desktop app asked over the
/// foreground mailbox rather than the local user typing `/compact`: the
/// mailbox response is deferred until the run finishes so the app's command
/// card shows the real report instead of "started".
pub(crate) fn start_manual_compact(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    instructions: Option<String>,
    respond_to_command_id: Option<String>,
) -> CompactStart {
    if session.engine_half.compact_runtime.is_running() {
        return CompactStart::Rejected(
            "A compaction is already running for this session.".to_string(),
        );
    }

    app.compact_run = Some(CompactRunState {
        phase: CompactPhase::Reading,
        started_at: std::time::Instant::now(),
        messages_before: app.rebon_tui.transcript.rows().len(),
        respond_to_command_id,
        automatic: false,
        invalidated: false,
    });

    let projects_root = session.projects_root.clone();
    let cwd = session.cwd.clone();
    let session_id = session.session_id.clone();
    let worker_session_id = session_id.clone();
    let resume_replay = session.engine_half.resume_replay();
    let handle = handle.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let path = rebon_session::transcript_file_path(&projects_root, &cwd, &worker_session_id);
        let loaded = match rebon_session::load_transcript_from_file(&path) {
            Ok(Some(loaded)) => loaded,
            Ok(None) => {
                let _ = tx.send(CompactProgress::Done(Box::new(Err(
                    "This session has no transcript to compact yet.".to_string(),
                ))));
                return;
            }
            Err(err) => {
                let _ = tx.send(CompactProgress::Done(Box::new(Err(format!(
                    "Could not read this session's transcript: {err}"
                )))));
                return;
            }
        };
        if tx.send(CompactProgress::Summarizing).is_err() {
            return;
        }
        let result = handle.block_on(resume_replay.compact_session_now(
            &projects_root,
            &cwd,
            &worker_session_id,
            &loaded.messages,
            instructions.as_deref(),
        ));
        let _ = tx.send(CompactProgress::Done(Box::new(result)));
    });

    session.engine_half.compact_runtime.worker = Some(CompactWorker { session_id, rx });
    CompactStart::Started
}

pub(crate) fn invalidate_idle_compact(app: &mut AppState) {
    app.auto_compact_generation = app.auto_compact_generation.wrapping_add(1);
    if let Some(run) = app.compact_run.as_mut().filter(|run| run.automatic) {
        run.invalidated = true;
    }
}

pub(crate) fn maybe_start_idle_compact(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    idle: bool,
    idle_for: std::time::Duration,
) -> bool {
    let budget = &session.model.prune_level.budget;
    if !idle
        || idle_for < super::status_bar::NEW_SESSION_HINT_IDLE_DELAY
        || budget.last_input_tokens() < super::status_bar::NEW_SESSION_HINT_TOKENS_THRESHOLD
        || !budget.should_auto_compact_for_tokens(budget.auto_compact_threshold())
        || session.engine_half.runtime.session_agents.current_id() != "local"
        || session.engine_half.compact_runtime.is_running()
        || app.auto_compact_attempt.as_ref()
            == Some(&(session.session_id.clone(), app.auto_compact_generation))
    {
        return false;
    }
    if matches!(
        start_manual_compact(app, session, handle, None, None),
        CompactStart::Started
    ) {
        app.compact_run
            .as_mut()
            .expect("started compact run")
            .automatic = true;
        app.auto_compact_attempt = Some((session.session_id.clone(), app.auto_compact_generation));
        true
    } else {
        false
    }
}

/// Drain the compaction worker. Called once per frame from the event loop.
///
/// Takes the worker out of the session and only puts it back while the run
/// is still going, so the finishing path can borrow the rest of the session
/// (to install the summary and answer the mailbox) without fighting it.
pub(crate) fn sync_compact_run(app: &mut AppState, session: &mut TuiEngineSession) {
    let Some(worker) = session.engine_half.compact_runtime.worker.take() else {
        return;
    };

    let result = loop {
        match worker.rx.try_recv() {
            Ok(CompactProgress::Summarizing) => {
                if let Some(run) = app.compact_run.as_mut() {
                    run.phase = CompactPhase::Summarizing;
                }
            }
            Ok(CompactProgress::Done(result)) => break *result,
            Err(mpsc::TryRecvError::Empty) => {
                session.engine_half.compact_runtime.worker = Some(worker);
                return;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                break Err("Compaction stopped before returning a result.".to_string())
            }
        }
    };

    let worker_session_id = worker.session_id;
    let run = app.compact_run.take();
    let automatic = run.as_ref().is_some_and(|run| run.automatic);
    let invalidated = run.as_ref().is_some_and(|run| run.invalidated);
    let respond_to = run.and_then(|run| run.respond_to_command_id);
    if invalidated {
        return;
    }

    // A `/resume` or `/clear` mid-run points the TUI at a different session;
    // installing a baseline built from the old one would silently graft a
    // stranger's history onto it.
    if session.session_id != worker_session_id {
        tracing::info!(
            worker_session_id,
            current_session_id = session.session_id,
            "compact: discarding a result for a session that is no longer active"
        );
        answer_deferred_command(
            session,
            &worker_session_id,
            respond_to.as_deref(),
            Err("The session changed while the compaction was running.".to_string()),
        );
        return;
    }

    match result {
        Ok((_, report)) if automatic && !report.used_model => {
            session.model.prune_level.budget.record_compact_failure();
            crate::tui::update::seal_overlay_before_transcript_message(app);
            inject_system_message(app, "warning", "Idle auto-compaction could not summarize the conversation; the existing context was kept.");
            app.follow_transcript_tail = true;
        }
        Ok((prepared, report)) => {
            if automatic {
                let path = rebon_session::transcript_file_path(
                    &session.projects_root,
                    &session.cwd,
                    &worker_session_id,
                );
                match rebon_session::load_transcript_from_file(&path) {
                    Ok(Some(loaded)) if prepared.matches_transcript(&loaded.messages) => {}
                    Ok(_) => {
                        tracing::info!("idle compaction discarded because the transcript changed");
                        return;
                    }
                    Err(error) => {
                        session.model.prune_level.budget.record_compact_failure();
                        crate::tui::update::seal_overlay_before_transcript_message(app);
                        inject_system_message(
                            app,
                            "warning",
                            &format!("Could not verify idle compaction: {error}"),
                        );
                        app.follow_transcript_tail = true;
                        return;
                    }
                }
            }
            session
                .engine_half
                .resume_replay()
                .install_persistent_summary(
                    &session.projects_root,
                    &session.cwd,
                    worker_session_id.clone(),
                    prepared,
                );
            // Keep the status bar and `/context` honest immediately rather
            // than waiting for the next turn's usage report to arrive.
            session
                .model
                .prune_level
                .report_estimated_usage(report.tokens_after);
            session.model.prune_level.budget.record_compact_success();
            let text = render_compact_report(&report);
            let text = if automatic {
                format!("Automatically compacted after extended idle time to reduce context resent after cache expiry.\n{text}")
            } else {
                text
            };
            crate::tui::update::seal_overlay_before_transcript_message(app);
            inject_system_message(app, "compact", &text);
            app.follow_transcript_tail = true;
            answer_deferred_command(session, &worker_session_id, respond_to.as_deref(), Ok(text));
        }
        Err(message) => {
            if automatic {
                session.model.prune_level.budget.record_compact_failure();
            }
            let text = format!("Compaction failed: {message}");
            crate::tui::update::seal_overlay_before_transcript_message(app);
            inject_system_message(app, "warning", &text);
            app.follow_transcript_tail = true;
            answer_deferred_command(
                session,
                &worker_session_id,
                respond_to.as_deref(),
                Err(message),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::CompactPhase;

    #[tokio::test]
    async fn idle_auto_compact_respects_budget_and_attempts_once_per_turn() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let handle = tokio::runtime::Handle::current();
        let budget = session.model.prune_level.budget.clone();
        let threshold = super::super::status_bar::NEW_SESSION_HINT_TOKENS_THRESHOLD;
        let idle = super::super::status_bar::NEW_SESSION_HINT_IDLE_DELAY;
        budget.report_usage(threshold.saturating_sub(1));
        assert!(!maybe_start_idle_compact(
            &mut app,
            &mut session,
            &handle,
            true,
            idle
        ));
        budget.report_usage(threshold);
        assert!(!maybe_start_idle_compact(
            &mut app,
            &mut session,
            &handle,
            true,
            idle - std::time::Duration::from_secs(1)
        ));
        assert!(!maybe_start_idle_compact(
            &mut app,
            &mut session,
            &handle,
            false,
            idle
        ));
        budget.set_auto_compact_enabled(false);
        assert!(!maybe_start_idle_compact(
            &mut app,
            &mut session,
            &handle,
            true,
            idle
        ));
        budget.set_auto_compact_enabled(true);
        assert!(maybe_start_idle_compact(
            &mut app,
            &mut session,
            &handle,
            true,
            idle
        ));
        assert_eq!(crate::tui::compact_widget::desired_height(&app, 10), 1);
        for _ in 0..200 {
            sync_compact_run(&mut app, &mut session);
            if !session.engine_half.compact_runtime.is_running() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!session.engine_half.compact_runtime.is_running());
        assert_eq!(crate::tui::compact_widget::desired_height(&app, 10), 0);
        assert_eq!(budget.consecutive_compact_failures(), 1);
        assert!(!maybe_start_idle_compact(
            &mut app,
            &mut session,
            &handle,
            true,
            idle
        ));
        app.auto_compact_generation += 1;
        assert!(maybe_start_idle_compact(
            &mut app,
            &mut session,
            &handle,
            true,
            idle
        ));
    }

    #[tokio::test]
    async fn a_new_turn_invalidates_an_inflight_idle_compaction() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let handle = tokio::runtime::Handle::current();
        let budget = session.model.prune_level.budget.clone();
        let idle = super::super::status_bar::NEW_SESSION_HINT_IDLE_DELAY;
        budget.report_usage(super::super::status_bar::NEW_SESSION_HINT_TOKENS_THRESHOLD);
        assert!(maybe_start_idle_compact(
            &mut app,
            &mut session,
            &handle,
            true,
            idle
        ));
        assert_eq!(crate::tui::compact_widget::desired_height(&app, 10), 1);
        invalidate_idle_compact(&mut app);
        assert_eq!(crate::tui::compact_widget::desired_height(&app, 10), 0);
        for _ in 0..200 {
            sync_compact_run(&mut app, &mut session);
            if !session.engine_half.compact_runtime.is_running() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!session.engine_half.compact_runtime.is_running());
        assert!(app.compact_run.is_none());
        assert_eq!(budget.consecutive_compact_failures(), 0);
    }

    #[tokio::test]
    async fn starting_a_run_arms_the_progress_widget_and_marks_the_session_busy() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let handle = tokio::runtime::Handle::current();

        let started = start_manual_compact(&mut app, &mut session, &handle, None, None);

        assert!(matches!(started, CompactStart::Started));
        let run = app.compact_run.as_ref().expect("the widget needs a run");
        assert_eq!(run.phase, CompactPhase::Reading);
        assert!(run.respond_to_command_id.is_none());
        assert!(session.engine_half.compact_runtime.is_running());
    }

    /// Two concurrent compactions would install two baselines against the
    /// same transcript, and the loser would silently overwrite the winner.
    #[tokio::test]
    async fn a_second_run_is_refused_while_one_is_in_flight() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let handle = tokio::runtime::Handle::current();
        let _ = start_manual_compact(&mut app, &mut session, &handle, None, None);

        let second = start_manual_compact(&mut app, &mut session, &handle, None, None);

        match second {
            CompactStart::Rejected(reason) => {
                assert!(reason.contains("already running"), "{reason}")
            }
            CompactStart::Started => panic!("a second compaction must not start"),
        }
    }

    /// The test session points at a temp dir with no transcript, so the run
    /// fails fast. What matters is that the failure clears the widget and
    /// releases the session instead of leaving a bar on screen forever.
    #[tokio::test]
    async fn a_failed_run_clears_the_widget_and_reports_it_in_the_transcript() {
        let mut app = AppState::new();
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::AppendStreamingText("earlier reply".into()),
        );
        let mut session = super::super::test_support::make_test_tui_session();
        let handle = tokio::runtime::Handle::current();
        let _ = start_manual_compact(&mut app, &mut session, &handle, None, None);

        for _ in 0..200 {
            sync_compact_run(&mut app, &mut session);
            if !session.engine_half.compact_runtime.is_running() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert!(
            !session.engine_half.compact_runtime.is_running(),
            "the run never settled"
        );
        assert!(app.compact_run.is_none(), "the progress widget must clear");
        let rows = app.rebon_tui.transcript.rows();
        assert!(
            matches!(&rows[0], rebon_tui::Message::Assistant(_)),
            "{rows:?}"
        );
        assert!(
            matches!(&rows[1], rebon_tui::Message::System(_)),
            "{rows:?}"
        );
        let rendered = app
            .rebon_tui
            .transcript
            .rows()
            .iter()
            .filter_map(|row| match row {
                rebon_tui::Message::System(system) => system.content.clone(),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Compaction failed"), "{rendered}");
    }

    /// The desktop app blocks on the response sidecar for the length of the
    /// run. If a finished run failed to write one, the caller would sit out
    /// its whole timeout and then report a timeout instead of the reason.
    #[tokio::test]
    async fn a_deferred_run_answers_its_mailbox_command_when_it_settles() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let handle = tokio::runtime::Handle::current();
        let projects_root = session.projects_root.clone();
        let cwd = session.cwd.clone();
        let session_id = session.session_id.clone();
        let _ = start_manual_compact(
            &mut app,
            &mut session,
            &handle,
            None,
            Some("cmd-app-compact".into()),
        );

        for _ in 0..200 {
            sync_compact_run(&mut app, &mut session);
            if !session.engine_half.compact_runtime.is_running() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let response = rebon_session_host::read_foreground_command_response(
            &projects_root,
            &cwd,
            &session_id,
            "cmd-app-compact",
        )
        .expect("a deferred command must be answered");
        assert_eq!(response.command_id, "cmd-app-compact");
        assert!(
            response.error.is_some(),
            "the reason must reach the caller: {response:?}"
        );
    }
}
