//! Idle, isolated model requests for the next user prompt.
use crate::tui::{app::AppState, wiring::TuiEngineSession};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

const SYSTEM: &str = "Suggest the next message the user might want to send in this coding conversation. Write a concise, useful request in the user's language, as if written by the user. Return only the suggested user message, without quotation marks or explanation. Do not answer the user.";
const CACHE_LIMIT: usize = 16;
const HISTORY_LIMIT: usize = 8;
const PROMPT_CHARS: usize = 2000;
const ANSWER_CHARS: usize = 4000;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Key {
    session: String,
    provider: String,
    model: String,
    client_id: u64,
    context: String,
    transcript: [u8; 32],
}

#[derive(Default)]
pub(super) struct SuggestionRuntime {
    sequence: u64,
    turn: u64,
    current: Option<Key>,
    cache: HashMap<Key, String>,
    cache_order: VecDeque<Key>,
    pending: Option<tokio::task::JoinHandle<()>>,
    tx: Option<UnboundedSender<(u64, Key, Option<String>)>>,
    rx: Option<UnboundedReceiver<(u64, Key, Option<String>)>>,
}

impl SuggestionRuntime {
    fn invalidate(&mut self, app: &mut AppState) {
        self.sequence = self.sequence.wrapping_add(1);
        self.current = None;
        app.prompt_suggestion_text = None;
        if let Some(pending) = self.pending.take() {
            pending.abort();
        }
    }

    pub fn sync(
        &mut self,
        app: &mut AppState,
        session: &TuiEngineSession,
        handle: &Handle,
        idle: bool,
    ) {
        if !idle {
            if self.current.is_some() {
                self.invalidate(app);
            }
            app.prompt_suggestion_text = None;
            return;
        }
        app.prompt_suggestion_session = Some(session.session_id.clone());
        if self.turn != app.prompt_suggestion_turn {
            self.turn = app.prompt_suggestion_turn;
            self.invalidate(app);
        }
        let (runtime, parent) = session.model.runtime_model.snapshot();
        let model = if runtime.title_model.trim().is_empty() {
            &runtime.model
        } else {
            &runtime.title_model
        };
        if model.trim().is_empty() {
            self.invalidate(app);
            return;
        }
        let path = rebon_session::transcript_file_path(
            &session.engine_half.runtime.projects_root,
            &session.cwd,
            &session.session_id,
        );
        // The complete transcript, not the truncated model prompt or file metadata,
        // identifies the context. This also detects same-size rewrites.
        let Ok(bytes) = std::fs::read(&path) else {
            self.invalidate(app);
            return;
        };
        let fingerprint: [u8; 32] = Sha256::digest(&bytes).into();
        let existing = self
            .current
            .as_ref()
            .filter(|key| {
                key.transcript == fingerprint
                    && key.session == session.session_id
                    && key.provider == runtime.provider_name
                    && key.model == *model
                    && key.client_id == parent.id().get()
            })
            .cloned();
        let key = if let Some(key) = existing {
            key
        } else {
            let Ok(history) = rebon_session::load_session_history(&path) else {
                self.invalidate(app);
                return;
            };
            let Ok(Some(answer)) = rebon_session::last_turn_assistant_text(&path) else {
                self.invalidate(app);
                return;
            };
            if std::fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
                self.invalidate(app);
                return;
            }
            let context = build_context(history.turns.iter().map(|t| t.prompt.as_str()), &answer);
            Key {
                session: session.session_id.clone(),
                provider: runtime.provider_name.clone(),
                model: model.clone(),
                client_id: parent.id().get(),
                context,
                transcript: fingerprint,
            }
        };
        if self.current.as_ref() != Some(&key) {
            self.invalidate(app);
            self.current = Some(key.clone());
            if let Some(result) = self.cache.get(&key) {
                app.prompt_suggestion_text = Some(result.clone());
            } else {
                let child = parent.fork_for_sub_agent(None);
                if child.owns_session_state() {
                    let tx = self
                        .tx
                        .get_or_insert_with(|| {
                            let (tx, rx) = mpsc::unbounded_channel();
                            self.rx = Some(rx);
                            tx
                        })
                        .clone();
                    let sequence = self.sequence;
                    self.pending = Some(handle.spawn(async move {
                        let text = child
                            .client()
                            .create_message(build_request(&key))
                            .await
                            .ok()
                            .and_then(|message| {
                                let text =
                                    message.text().trim().chars().take(1000).collect::<String>();
                                (!text.is_empty()).then_some(text)
                            });
                        let _ = tx.send((sequence, key, text));
                    }));
                }
            }
        }
        let mut results = Vec::new();
        if let Some(rx) = self.rx.as_mut() {
            while let Ok(result) = rx.try_recv() {
                results.push(result);
            }
        }
        for (sequence, key, result) in results {
            self.accept_result(app, sequence, key, result);
        }
    }

    fn accept_result(
        &mut self,
        app: &mut AppState,
        sequence: u64,
        key: Key,
        result: Option<String>,
    ) {
        if sequence == self.sequence && self.current.as_ref() == Some(&key) {
            self.pending = None;
            // A failure is suppressed for this idle interval by `current`, but
            // is not cached: leaving and returning to idle permits a retry.
            if let Some(text) = result.as_ref() {
                if self.cache.len() >= CACHE_LIMIT {
                    if let Some(old) = self.cache_order.pop_front() {
                        self.cache.remove(&old);
                    }
                }
                self.cache_order.push_back(key.clone());
                self.cache.insert(key, text.clone());
            }
            app.prompt_suggestion_text = result;
        }
    }
}

fn build_request(key: &Key) -> rebon_api::CreateMessageRequest {
    let mut request =
        rebon_api::CreateMessageRequest::simple(&key.model, &key.context).with_system(SYSTEM);
    request.stream = false;
    request.max_tokens = 128;
    request.thinking = Some(rebon_api::ThinkingConfig::Disabled);
    request
}

fn build_context<'a>(prompts: impl Iterator<Item = &'a str>, answer: &str) -> String {
    let mut history = VecDeque::new();
    for prompt in prompts {
        if history.len() == HISTORY_LIMIT {
            history.pop_front();
        }
        history.push_back(format!(
            "User: {}",
            prompt.chars().take(PROMPT_CHARS).collect::<String>()
        ));
    }
    let answer: String = answer.chars().take(ANSWER_CHARS).collect();
    format!(
        "{}\n\nFinal assistant response:\n{answer}\n\nGenerate the next user suggestion.",
        history.into_iter().collect::<Vec<_>>().join("\n\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_api::ModelClient;
    use std::sync::Arc;

    #[derive(Clone)]
    struct ForkableMock(rebon_api::MockModelClient);

    #[async_trait::async_trait]
    impl ModelClient for ForkableMock {
        fn provider_name(&self) -> &'static str {
            "mock"
        }
        fn fork_for_sub_agent(&self) -> Option<Arc<dyn ModelClient>> {
            Some(Arc::new(self.clone()))
        }
        async fn create_message_stream(
            &self,
            request: rebon_api::CreateMessageRequest,
        ) -> rebon_api::ModelResult<rebon_api::StreamEventStream> {
            self.0.create_message_stream(request).await
        }
    }

    fn transcript(session: &TuiEngineSession, prompt: &str, answer: &str) {
        let path = rebon_session::transcript_file_path(
            &session.engine_half.runtime.projects_root,
            &session.cwd,
            &session.session_id,
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let entries = [
            serde_json::json!({"type":"user","uuid":"u1","parentUuid":null,"message":{"role":"user","content":prompt}}),
            serde_json::json!({"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"role":"assistant","content":[{"type":"text","text":answer}]}}),
        ];
        std::fs::write(
            path,
            entries.iter().map(|e| format!("{e}\n")).collect::<String>(),
        )
        .unwrap();
    }

    fn setup(root: &std::path::Path, mock: &rebon_api::MockModelClient) -> TuiEngineSession {
        let session = super::super::test_support::make_test_tui_session_with_projects_root(
            root.to_path_buf(),
        );
        let mut config = session.model.runtime_model.get();
        config.client = Arc::new(ForkableMock(mock.clone()));
        session.model.runtime_model.set(config);
        session
    }

    async fn settle(
        runtime: &mut SuggestionRuntime,
        app: &mut AppState,
        session: &TuiEngineSession,
    ) {
        if let Some(pending) = runtime.pending.as_mut() {
            pending.await.unwrap();
        }
        runtime.sync(app, session, &Handle::current(), true);
    }

    #[tokio::test]
    async fn prompt_suggestion_sync_idle_context_session_and_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mock = rebon_api::MockModelClient::new();
        mock.push_turn(vec![
            rebon_api::StreamEvent::MessageStart {
                message_id: "id".into(),
                model: "mock".into(),
                usage: Default::default(),
            },
            rebon_api::StreamEvent::ContentBlockStart {
                index: 0,
                content_block: rebon_api::ContentBlockStart::Text {
                    text: "suggestion".into(),
                },
            },
            rebon_api::StreamEvent::ContentBlockStop { index: 0 },
            rebon_api::StreamEvent::MessageStop,
        ]);
        let session = setup(dir.path(), &mock);
        transcript(&session, "first prompt", "first answer");
        let mut app = AppState::new();
        app.prompt_suggestion_session = Some(session.session_id.clone());
        let mut runtime = SuggestionRuntime::default();
        runtime.sync(&mut app, &session, &Handle::current(), false);
        tokio::task::yield_now().await;
        assert_eq!(mock.call_count(), 0);
        app.input = "draft".into();
        runtime.sync(&mut app, &session, &Handle::current(), true);
        settle(&mut runtime, &mut app, &session).await;
        assert_eq!(app.input, "draft");
        app.input.clear();
        runtime.sync(&mut app, &session, &Handle::current(), true);
        assert_eq!(mock.call_count(), 1);
        let requests = mock.captured_requests();
        let request = &requests[0];
        assert_eq!(request.model, "test-small-model");
        let message = format!("{:?}", request.messages);
        assert!(message.contains("first prompt") && message.contains("first answer"));
        assert_eq!(app.prompt_suggestion_text.as_deref(), Some("suggestion"));
        runtime.sync(&mut app, &session, &Handle::current(), false);
        runtime.sync(&mut app, &session, &Handle::current(), true);
        settle(&mut runtime, &mut app, &session).await;
        assert_eq!(
            mock.call_count(),
            1,
            "clearing input must reuse cached suggestion"
        );
        assert_eq!(app.prompt_suggestion_text.as_deref(), Some("suggestion"));
        transcript(&session, "different prompt", "different answer");
        runtime.sync(&mut app, &session, &Handle::current(), true);
        settle(&mut runtime, &mut app, &session).await;
        assert_eq!(mock.call_count(), 2);
        assert!(app.prompt_suggestion_text.is_none());
        runtime.sync(&mut app, &session, &Handle::current(), false);
        runtime.sync(&mut app, &session, &Handle::current(), true);
        settle(&mut runtime, &mut app, &session).await;
        assert_eq!(
            mock.call_count(),
            3,
            "failed requests are retried after a new idle interval"
        );
        let message = format!("{:?}", mock.captured_requests()[1].messages);
        assert!(message.contains("different prompt") && message.contains("different answer"));
        assert!(!message.contains("first prompt"));
        let other = setup(dir.path(), &mock);
        transcript(&other, "other session prompt", "other session answer");
        runtime.sync(&mut app, &other, &Handle::current(), true);
        settle(&mut runtime, &mut app, &other).await;
        assert_eq!(mock.call_count(), 4);
        app.prompt_suggestion_session = Some(other.session_id.clone());
        runtime.sync(&mut app, &other, &Handle::current(), true);
        settle(&mut runtime, &mut app, &other).await;
        assert_eq!(mock.call_count(), 4);
        assert!(
            format!("{:?}", mock.captured_requests()[3].messages).contains("other session prompt")
        );
        assert_eq!(mock.reset_count(), 0);
        assert_eq!(mock.end_turn_count(), 0);
    }

    #[tokio::test]
    async fn prompt_suggestion_restored_session_and_truncated_history_invalidate() {
        let dir = tempfile::tempdir().unwrap();
        let mock = rebon_api::MockModelClient::new();
        let session = setup(dir.path(), &mock);
        let mut app = AppState::new();
        let mut runtime = SuggestionRuntime::default();
        let prefix = "x".repeat(PROMPT_CHARS);
        transcript(&session, &format!("{prefix}a"), "answer");
        runtime.sync(&mut app, &session, &Handle::current(), true);
        settle(&mut runtime, &mut app, &session).await;
        assert_eq!(mock.call_count(), 1);
        for _ in 0..3 {
            runtime.sync(&mut app, &session, &Handle::current(), true);
        }
        assert_eq!(mock.call_count(), 1, "failure must not retry each frame");
        transcript(&session, &format!("{prefix}b"), "answer");
        runtime.sync(&mut app, &session, &Handle::current(), true);
        settle(&mut runtime, &mut app, &session).await;
        assert_eq!(mock.call_count(), 2);
        assert_eq!(
            format!("{:?}", mock.captured_requests()[0].messages),
            format!("{:?}", mock.captured_requests()[1].messages)
        );
    }

    #[test]
    fn prompt_suggestion_reaches_rendered_placeholder() {
        let mut app = AppState::new();
        app.prompt_suggestion_text = Some("Inspect the tests".into());
        let input = app.build_runtime_input();
        let view = rebon_tui::promptinput::derive_prompt_input_runtime_state(&input, |_, _, _| {
            String::new()
        });
        assert_eq!(
            view.text_input_view.placeholder.as_deref(),
            Some("Inspect the tests")
        );
        app.input = "edited".into();
        assert!(app.build_runtime_input().prompt_suggestion.is_none());
    }

    #[tokio::test]
    async fn prompt_suggestion_sync_rejects_queued_old_result() {
        let dir = tempfile::tempdir().unwrap();
        let mock = rebon_api::MockModelClient::new();
        let session = setup(dir.path(), &mock);
        transcript(&session, "old prompt", "old answer");
        let mut app = AppState::new();
        app.prompt_suggestion_session = Some(session.session_id.clone());
        let mut runtime = SuggestionRuntime::default();
        runtime.sync(&mut app, &session, &Handle::current(), true);
        let old_sequence = runtime.sequence;
        let old_key = runtime.current.clone().unwrap();
        runtime
            .tx
            .as_ref()
            .unwrap()
            .send((old_sequence, old_key, Some("stale".into())))
            .unwrap();
        transcript(&session, "new prompt longer", "new answer longer");
        runtime.sync(&mut app, &session, &Handle::current(), true);
        assert_ne!(app.prompt_suggestion_text.as_deref(), Some("stale"));
        settle(&mut runtime, &mut app, &session).await;
        assert!(app.prompt_suggestion_text.is_none());
    }

    #[tokio::test]
    async fn prompt_suggestion_sync_refuses_unforkable_client() {
        let dir = tempfile::tempdir().unwrap();
        let mock = rebon_api::MockModelClient::new();
        let session = setup(dir.path(), &mock);
        let mut config = session.model.runtime_model.get();
        config.client = Arc::new(mock.clone());
        session.model.runtime_model.set(config);
        transcript(&session, "prompt", "answer");
        let mut app = AppState::new();
        app.prompt_suggestion_session = Some(session.session_id.clone());
        let mut runtime = SuggestionRuntime::default();
        runtime.sync(&mut app, &session, &Handle::current(), true);
        tokio::task::yield_now().await;
        assert_eq!(mock.call_count(), 0);
        assert!(app.prompt_suggestion_text.is_none());
    }

    #[tokio::test]
    async fn prompt_suggestion_sync_empty_title_and_missing_history() {
        let dir = tempfile::tempdir().unwrap();
        let mock = rebon_api::MockModelClient::new();
        let session = setup(dir.path(), &mock);
        let mut config = session.model.runtime_model.get();
        config.title_model = "  ".into();
        session.model.runtime_model.set(config);
        let mut app = AppState::new();
        app.prompt_suggestion_session = Some(session.session_id.clone());
        let mut runtime = SuggestionRuntime::default();
        runtime.sync(&mut app, &session, &Handle::current(), true);
        assert_eq!(mock.call_count(), 0);
        transcript(&session, "hello", "answer");
        runtime.sync(&mut app, &session, &Handle::current(), true);
        settle(&mut runtime, &mut app, &session).await;
        assert_eq!(mock.captured_requests()[0].model, "test-model");
    }

    fn key(context: &str) -> Key {
        Key {
            session: "session".into(),
            provider: "provider".into(),
            model: "small".into(),
            client_id: 1,
            context: context.into(),
            transcript: Sha256::digest(context.as_bytes()).into(),
        }
    }

    #[test]
    fn prompt_suggestion_stale_result_after_invalidation_is_ignored() {
        let mut runtime = SuggestionRuntime::default();
        let mut app = AppState::new();
        runtime.current = Some(key("old"));
        let old = runtime.sequence;
        runtime.invalidate(&mut app);
        runtime.current = Some(key("new"));
        runtime.accept_result(&mut app, old, key("old"), Some("stale".into()));
        assert!(app.prompt_suggestion_text.is_none());
        assert!(runtime.cache.is_empty());
    }

    #[test]
    fn prompt_suggestion_cache_is_bounded() {
        let mut runtime = SuggestionRuntime::default();
        let mut app = AppState::new();
        for i in 0..CACHE_LIMIT + 3 {
            let key = key(&i.to_string());
            runtime.sequence += 1;
            runtime.current = Some(key.clone());
            runtime.accept_result(&mut app, runtime.sequence, key, Some("next".into()));
        }
        assert_eq!(runtime.cache.len(), CACHE_LIMIT);
        assert!(!runtime.cache.contains_key(&key("0")));
    }

    #[test]
    fn prompt_suggestion_context_is_bounded_and_recent() {
        let prompts = ["old", "1", "2", "3", "4", "5", "6", "7", "recent"];
        let context = build_context(prompts.into_iter(), &"答".repeat(ANSWER_CHARS + 1));
        assert!(!context.contains("User: old"));
        assert!(context.contains("User: recent"));
        assert_eq!(context.matches('答').count(), ANSWER_CHARS);
        let request = build_request(&key(&context));
        assert_eq!(request.model, "small");
        assert!(!request.stream);
    }
}
