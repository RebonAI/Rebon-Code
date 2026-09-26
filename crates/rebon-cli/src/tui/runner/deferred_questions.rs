//! Answers to deferred `AskUserQuestion`s, delivered as user messages.
//!
//! The engine hands a deferred question's answer to a sink from a
//! background task (`rebon_core::deferred_question`). This terminal's sink
//! is an inbox the runner drains once per pass, beside the mid-turn queue
//! reconcile. An answer that arrives while a turn runs is queued like a
//! message typed mid-turn, so the turn steers it in, or the end-of-turn
//! drain sends it if the turn finishes first; one that arrives while the
//! session is idle starts the next turn. A bare dismissal while idle is
//! dropped: the user closed the dialog and said nothing, and whatever they
//! type next is what the model should hear.
//!
//! The dialog itself is the ordinary permission modal. A turn ending does
//! not clear it, and the broker's task owns the answer channel rather than
//! the turn, so a question can be answered after the turn that asked it is
//! over — or interrupted.

use std::sync::{Arc, Mutex};

use tokio::runtime::Handle;

use rebon_core::deferred_question::{DeferredQuestionAnswer, DeferredQuestionSink};

use crate::session::submit_payload::SubmitPayload;
use crate::tui::app::AppState;
use crate::tui::dispatch::enqueue_submit_payload_in_mode;
use crate::tui::wiring::TuiEngineSession;

use super::prompt_lifecycle::{local_turn_rejection, maybe_spawn_next_queued_prompt};
use super::ActivePrompt;

/// Where the broker's background tasks leave answers for the runner.
#[derive(Debug, Clone, Default)]
pub(crate) struct DeferredQuestionInbox(Arc<Mutex<Vec<DeferredQuestionAnswer>>>);

impl DeferredQuestionInbox {
    fn take(&self) -> Vec<DeferredQuestionAnswer> {
        std::mem::take(&mut *self.0.lock().expect("deferred question inbox poisoned"))
    }
}

impl DeferredQuestionSink for DeferredQuestionInbox {
    fn deliver(&self, answer: DeferredQuestionAnswer) {
        self.0
            .lock()
            .expect("deferred question inbox poisoned")
            .push(answer);
    }
}

pub(super) fn drain_deferred_question_answers(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
) {
    // Every runtime the session installs — `/clear`, `/resume`, a provider
    // switch — brings its own broker. Following it here covers all of them
    // instead of each install site having to remember.
    let broker = &session.engine_half.runtime.permission_broker;
    if !broker.has_deferred_question_sink() {
        broker.set_deferred_question_sink(Some(Arc::new(app.deferred_question_inbox.clone())));
    }

    let answers = app.deferred_question_inbox.take();
    if answers.is_empty() {
        return;
    }
    let turn_running = active_prompt.is_some();
    let mut queued = false;
    for answer in answers {
        // The dialog stays on screen across `/clear` and session switches,
        // but its answer belongs to the conversation that asked.
        if answer.session_id != session.session_id {
            tracing::info!(
                asked_in = %answer.session_id,
                current = %session.session_id,
                tool_use_id = %answer.tool_use_id,
                "dropping a deferred question answer for a session that is no longer open"
            );
            continue;
        }
        if !turn_running && !answer.start_turn_if_idle {
            tracing::debug!(
                tool_use_id = %answer.tool_use_id,
                "dropping a bare dismissal while idle"
            );
            continue;
        }
        enqueue_submit_payload_in_mode(
            app,
            SubmitPayload {
                text: answer.display_text,
                model_text: Some(answer.model_text),
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
            "prompt".to_string(),
        );
        queued = true;
    }
    if queued
        && !turn_running
        && app.resume_dialog.is_none()
        && local_turn_rejection(app, session, None).is_none()
    {
        maybe_spawn_next_queued_prompt(app, session, handle, active_prompt);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rebon_core::deferred_question::{DeferredQuestionAnswer, DeferredQuestionSink};
    use rebon_types::ContentBlock;
    use tokio::runtime::{Builder, Runtime};

    use super::super::prompt_lifecycle::maybe_spawn_next_queued_prompt;
    use super::super::test_support::make_test_tui_session;
    use super::drain_deferred_question_answers;
    use crate::session::submit_payload::SubmitPayload;
    use crate::tui::app::AppState;
    use crate::tui::dispatch::{enqueue_submit_payload, queued_text};

    fn make_immediate_handle() -> (Runtime, tokio::runtime::Handle) {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let handle = runtime.handle().clone();
        (runtime, handle)
    }

    #[derive(Default)]
    struct RecordingPromptExecutor {
        requests: std::sync::Mutex<Vec<rebon_agent_core::PromptRequest>>,
    }

    #[async_trait::async_trait]
    impl rebon_agent_core::PromptExecutor for RecordingPromptExecutor {
        async fn execute(
            &self,
            request: rebon_agent_core::PromptRequest,
        ) -> Result<rebon_agent_core::PromptOutcome, rebon_agent_core::PromptExecutorError>
        {
            self.requests.lock().expect("requests lock").push(request);
            Ok(rebon_agent_core::PromptOutcome::end_turn())
        }
    }

    impl RecordingPromptExecutor {
        fn take_requests(&self) -> Vec<rebon_agent_core::PromptRequest> {
            std::mem::take(&mut *self.requests.lock().expect("requests lock"))
        }
    }

    /// A turn that never finishes: the running turn an answer arrives in.
    struct EndlessPromptExecutor;

    #[async_trait::async_trait]
    impl rebon_agent_core::PromptExecutor for EndlessPromptExecutor {
        async fn execute(
            &self,
            _request: rebon_agent_core::PromptRequest,
        ) -> Result<rebon_agent_core::PromptOutcome, rebon_agent_core::PromptExecutorError>
        {
            std::future::pending().await
        }
    }

    const DISPLAY: &str = "Answered questions:\n- Which database?\n  Answer: Postgres";

    fn answer(session_id: &str, start_turn_if_idle: bool) -> DeferredQuestionAnswer {
        DeferredQuestionAnswer {
            session_id: session_id.to_string(),
            tool_use_id: "toolu_q".to_string(),
            display_text: DISPLAY.to_string(),
            model_text: "<question-answer tool_use_id=\"toolu_q\">\nPostgres\n</question-answer>"
                .to_string(),
            start_turn_if_idle,
        }
    }

    #[test]
    fn the_first_pass_installs_the_inbox_on_the_sessions_broker() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let broker = session.engine_half.runtime.permission_broker.clone();
        assert!(!broker.has_deferred_question_sink());

        drain_deferred_question_answers(&mut app, &mut session, &handle, &mut None);

        assert!(broker.has_deferred_question_sink());
        runtime.shutdown_background();
    }

    #[test]
    fn an_answer_while_idle_starts_a_turn_with_the_tagged_model_text() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        app.deferred_question_inbox
            .deliver(answer(&session.session_id, true));
        let mut active_prompt = None;

        drain_deferred_question_answers(&mut app, &mut session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());

        assert!(active_prompt.is_some(), "an idle session starts a turn");
        assert!(app.queued_submit_payloads.is_empty());
        let rows = app.rebon_tui.transcript.rows();
        let rebon_tui::Message::User(user) = &rows[rows.len() - 1] else {
            panic!("the answer is a user row: {rows:?}");
        };
        assert!(matches!(
            &user.message.content[0],
            rebon_tui::UserContentBlock::Text(text) if text.text == DISPLAY
        ));
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            &requests[0].prompt[0],
            ContentBlock::Text(text) if text.text.starts_with("<question-answer tool_use_id=\"toolu_q\">")
        ));

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn an_answer_during_a_turn_is_queued_for_steering() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        session.set_test_executor(Arc::new(EndlessPromptExecutor));
        let poller = session.engine_half.runtime.mid_turn_queue.clone();
        app.mid_turn_queued_submit_poller = Some(poller.clone());
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "start working".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        let mut active_prompt = None;
        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());
        assert!(
            active_prompt.is_some() && app.is_loading,
            "a turn is running"
        );
        assert!(app.queued_submit_payloads.is_empty());

        app.deferred_question_inbox
            .deliver(answer(&session.session_id, true));
        drain_deferred_question_answers(&mut app, &mut session, &handle, &mut active_prompt);

        assert_eq!(app.queued_submit_payloads.len(), 1);
        assert_eq!(queued_text(&app.queued_commands[0]), Some(DISPLAY));
        assert_eq!(app.queued_commands[0].mode, "prompt");
        assert_eq!(
            app.queued_submit_payloads[0].model_text.as_deref(),
            Some("<question-answer tool_use_id=\"toolu_q\">\nPostgres\n</question-answer>")
        );
        assert_eq!(poller.pending_len(), 1, "offered to the running turn");

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn a_bare_dismissal_while_idle_is_dropped() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        app.deferred_question_inbox
            .deliver(answer(&session.session_id, false));
        let mut active_prompt = None;

        drain_deferred_question_answers(&mut app, &mut session, &handle, &mut active_prompt);

        assert!(active_prompt.is_none());
        assert!(app.queued_submit_payloads.is_empty());
        assert!(recorder.take_requests().is_empty());
        runtime.shutdown_background();
    }

    #[test]
    fn an_answer_for_another_session_is_dropped() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        app.deferred_question_inbox
            .deliver(answer("some-earlier-session", true));
        let mut active_prompt = None;

        drain_deferred_question_answers(&mut app, &mut session, &handle, &mut active_prompt);

        assert!(active_prompt.is_none());
        assert!(app.queued_submit_payloads.is_empty());
        assert!(recorder.take_requests().is_empty());
        runtime.shutdown_background();
    }
}
