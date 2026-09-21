//! Enter before the session exists.
//!
//! The first frame is drawn before `TuiEngineSession` is built, so the
//! composer accepts a prompt the session cannot yet run. Two answers:
//!
//! * A session that starts hosted already has a job, and the job is where
//!   a prompt goes while its worker comes up — the same durable pending
//!   prompt the handover path writes. The prompt is echoed at once and the
//!   worker claims it as it starts; the session, when it arrives, sees a
//!   transcript row it did not write, exactly as after `/hosted`.
//! * Anything else — a `--local` prompt, a slash command in either mode —
//!   waits for the session. The composer keeps its text (chips included),
//!   a line says so once, and the loop submits it the moment the session is
//!   installed, through the ordinary path, as if Enter had come then.

use crate::tui::app::AppState;
use crate::tui::dispatch::{commit_submit_payload_to_transcript, take_submit_payload_with_images};

use super::layout_and_scroll::repin_transcript_to_bottom;
use super::prompt_history::save_to_history_for_project_if_needed;
use super::transcript_messages::{inject_local_command_feedback, inject_system_message};
use super::SessionSlot;

/// What Enter did with the composer while there was no session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StartupSubmitOutcome {
    /// Nothing to submit.
    Empty,
    /// Queued on the hosted job and echoed.
    SentToJob,
    /// Left in the composer for the session to take.
    Deferred,
}

pub(super) fn submit_before_session(
    app: &mut AppState,
    text: String,
    slot: &mut SessionSlot,
) -> StartupSubmitOutcome {
    submit_before_session_in_store(
        app,
        text,
        slot,
        &crate::background::cli_default_store(),
        true,
    )
}

pub(super) fn submit_before_session_in_store(
    app: &mut AppState,
    text: String,
    slot: &mut SessionSlot,
    store: &crate::background::BackgroundStore,
    start_supervisor: bool,
) -> StartupSubmitOutcome {
    if text.trim().is_empty() && app.pasted_contents.is_empty() {
        return StartupSubmitOutcome::Empty;
    }
    let is_command = text.trim_start().starts_with('/');
    let hosted = slot
        .hosted_job_before_session()
        .map(str::to_string)
        .zip(slot.session_id().map(str::to_string));
    let Some((job_id, session_id)) = hosted.filter(|_| !is_command) else {
        if slot.defer_enter() {
            let what = if is_command {
                "That command runs"
            } else {
                "Your prompt is sent"
            };
            inject_local_command_feedback(
                app,
                "startup",
                &format!("The session is still starting. {what} as soon as it is ready."),
            );
            app.follow_transcript_tail = true;
        }
        return StartupSubmitOutcome::Deferred;
    };

    let cwd = slot.cwd().to_string();
    save_to_history_for_project_if_needed(app, &cwd, &session_id, text.trim_end());
    let Some(mut submit) = take_submit_payload_with_images(app, &text, Vec::new()) else {
        return StartupSubmitOutcome::Empty;
    };
    let message = submit.prompt_text().to_string();
    let images = submit
        .image_pastes
        .iter()
        .cloned()
        .map(rebon_session_host::BackgroundImageAttachment::from_prompt_paste_content)
        .collect::<Vec<_>>();
    match rebon_session_host::reply_to_background_job_in_store_with_images(
        store,
        &job_id,
        message,
        images,
        start_supervisor,
        true,
        &crate::background::rebon_exe(),
    ) {
        Ok(()) => {
            // Echoed straight away: the prompt is durable now, and the
            // session will find the row already on screen.
            commit_submit_payload_to_transcript(app, &mut submit, &session_id);
            repin_transcript_to_bottom(app);
            tracing::info!(
                %job_id,
                chars = submit.text.len(),
                "rebon turn: prompt submitted before the session, queued on its job"
            );
            StartupSubmitOutcome::SentToJob
        }
        Err(err) => {
            app.input = text;
            app.cursor_offset = app.input.len();
            app.pasted_contents = submit.image_pastes;
            inject_system_message(
                app,
                "error",
                &format!(
                    "This session is starting in worker {job_id} and the prompt could not be queued for it: {err}"
                ),
            );
            app.follow_transcript_tail = true;
            StartupSubmitOutcome::Deferred
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::runner::StartupPreview;

    fn store_with_hosted_job(
        root: &std::path::Path,
    ) -> (crate::background::BackgroundStore, String, String) {
        use crate::background::RuntimeFieldsExt as _;
        let store = crate::background::BackgroundStore::new(root.join("jobs"));
        let cwd = root.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let session_id = "sess-early".to_string();
        let job = rebon_session_host::queue_session_for_worker(
            &store,
            &session_id,
            cwd.clone(),
            crate::background::BackgroundRuntimeFields::from_runtime_override(
                &crate::rebon_config::RuntimeOverride::default(),
            ),
            Some("early".into()),
            rebon_session_host::JobPlacement::Foreground,
        )
        .unwrap();
        (store, session_id, job.identity.job_id)
    }

    fn user_rows(app: &AppState) -> Vec<String> {
        app.rebon_tui
            .transcript
            .rows()
            .iter()
            .filter_map(|row| match row {
                rebon_tui::Message::User(user) => Some(
                    user.message
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            rebon_tui::UserContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                ),
                _ => None,
            })
            .collect()
    }

    fn system_rows(app: &AppState) -> Vec<String> {
        app.rebon_tui
            .transcript
            .rows()
            .iter()
            .filter_map(|row| match row {
                rebon_tui::Message::System(system) => system.content.clone(),
                _ => None,
            })
            .collect()
    }

    /// A prompt typed before the session exists lands on the hosted job as a
    /// pending prompt, is echoed in the transcript, and clears the composer —
    /// what the handover path does, without a session to do it with.
    #[test]
    fn a_prompt_typed_before_the_session_lands_on_the_job_and_is_echoed() {
        let _guard = crate::test_env::lock_env();
        let root = tempfile::tempdir().unwrap();
        let (store, session_id, job_id) = store_with_hosted_job(root.path());
        let cwd = root.path().join("proj").to_string_lossy().to_string();
        let (mut slot, _tx) = SessionSlot::pending_for_test(StartupPreview::hosted_for_test(
            &cwd,
            &session_id,
            &job_id,
        ));
        let mut app = AppState::new();
        app.cwd = cwd.clone();
        app.input = "hello worker".into();
        app.cursor_offset = app.input.len();

        let outcome = submit_before_session_in_store(
            &mut app,
            "hello worker".into(),
            &mut slot,
            &store,
            false,
        );

        assert_eq!(outcome, StartupSubmitOutcome::SentToJob);
        let state = store.read_state(&job_id).unwrap();
        assert_eq!(state.identity.pending_prompts.len(), 1);
        assert_eq!(state.identity.pending_prompts[0].text, "hello worker");
        assert!(
            !state.identity.resume_only,
            "the worker has something to run now"
        );
        assert_eq!(user_rows(&app), vec!["hello worker".to_string()]);
        assert!(app.input.is_empty(), "the composer is cleared");
        assert!(
            !slot.take_deferred_enter(),
            "nothing is left for the session to submit"
        );
    }

    /// A slash command typed before the session exists is not lost and not
    /// run: it stays in the composer, one visible line says the session is
    /// still starting, and the slot remembers to submit it on install.
    #[test]
    fn a_slash_command_typed_before_the_session_waits_in_the_composer_with_a_message() {
        let _guard = crate::test_env::lock_env();
        let root = tempfile::tempdir().unwrap();
        let (store, session_id, job_id) = store_with_hosted_job(root.path());
        let cwd = root.path().join("proj").to_string_lossy().to_string();
        let (mut slot, _tx) = SessionSlot::pending_for_test(StartupPreview::hosted_for_test(
            &cwd,
            &session_id,
            &job_id,
        ));
        let mut app = AppState::new();
        app.cwd = cwd;
        app.input = "/help".into();
        app.cursor_offset = app.input.len();

        let outcome =
            submit_before_session_in_store(&mut app, "/help".into(), &mut slot, &store, false);
        assert_eq!(outcome, StartupSubmitOutcome::Deferred);
        assert_eq!(app.input, "/help", "the composer keeps the command");
        let rows = system_rows(&app);
        assert_eq!(rows.len(), 1, "one line of feedback");
        assert!(rows[0].contains("still starting"));
        assert!(user_rows(&app).is_empty(), "nothing was echoed as a prompt");
        assert!(
            store
                .read_state(&job_id)
                .unwrap()
                .identity
                .pending_prompts
                .is_empty(),
            "a command is not a prompt for the worker"
        );

        // A second Enter does not repeat the line.
        let outcome =
            submit_before_session_in_store(&mut app, "/help".into(), &mut slot, &store, false);
        assert_eq!(outcome, StartupSubmitOutcome::Deferred);
        assert_eq!(app.rebon_tui.transcript.rows().len(), 1);
        assert!(
            slot.take_deferred_enter(),
            "the session submits it on install"
        );
    }

    /// A `--local` session has no job to send to: the prompt waits for the
    /// session the same way a command does.
    #[test]
    fn a_local_prompt_typed_before_the_session_waits_for_it() {
        let _guard = crate::test_env::lock_env();
        let root = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(root.path().join("jobs"));
        let (mut slot, _tx) = SessionSlot::pending_for_test(StartupPreview::for_test("."));
        let mut app = AppState::new();
        app.input = "do a thing".into();
        app.cursor_offset = app.input.len();

        let outcome =
            submit_before_session_in_store(&mut app, "do a thing".into(), &mut slot, &store, false);
        assert_eq!(outcome, StartupSubmitOutcome::Deferred);
        assert_eq!(app.input, "do a thing");
        assert!(system_rows(&app)[0].contains("Your prompt is sent"));
        assert!(slot.take_deferred_enter());
    }

    /// Empty Enter is not a decision, before the session as after.
    #[test]
    fn an_empty_enter_before_the_session_does_nothing() {
        let _guard = crate::test_env::lock_env();
        let root = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(root.path().join("jobs"));
        let (mut slot, _tx) = SessionSlot::pending_for_test(StartupPreview::for_test("."));
        let mut app = AppState::new();
        let outcome =
            submit_before_session_in_store(&mut app, "   ".into(), &mut slot, &store, false);
        assert_eq!(outcome, StartupSubmitOutcome::Empty);
        assert!(app.rebon_tui.transcript.rows().is_empty());
        assert!(!slot.take_deferred_enter());
    }
}
