//! `MockModelClient` — scripted test double.
//!
//! The mock captures every [`crate::CreateMessageRequest`] it sees
//! and replays a pre-seeded list of [`crate::StreamEvent`]s (or an
//! error) for each call. It is the workhorse for testing anything
//! built on top of [`crate::ModelClient`]: the layers above it never
//! touch a real network.
//!
//! ```no_run
//! # use rebon_api::{MockModelClient, ModelClient, CreateMessageRequest, StreamEvent, StopReason};
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let client = MockModelClient::new();
//! client.push_turn(vec![
//!     StreamEvent::MessageStart {
//!         message_id: "msg_1".into(),
//!         model: "mock".into(),
//!         usage: Default::default(),
//!     },
//!     StreamEvent::MessageStop,
//! ]);
//! let msg = client
//!     .create_message(CreateMessageRequest::simple("mock", "hi"))
//!     .await?;
//! assert_eq!(msg.id, "msg_1");
//! # Ok(()) }
//! ```

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::Stream;

use crate::client::{ModelCapabilities, ModelClient};
use crate::error::{ModelError, ModelResult};
use crate::events::{StreamEvent, StreamEventStream};
use crate::request::CreateMessageRequest;

/// Scripted test double implementing [`ModelClient`].
#[derive(Debug, Clone, Default)]
pub struct MockModelClient {
    inner: Arc<Mutex<MockInner>>,
}

#[derive(Debug, Default)]
struct MockInner {
    /// Each entry is one "turn" worth of events that will be
    /// replayed in order on the next `create_message_stream` call.
    /// `Err` entries fail the call.
    scripted: VecDeque<ScriptedOutcome>,
    /// Every request the mock observed, in call order.
    captured_requests: Vec<CreateMessageRequest>,
    /// Number of times `reset_session_state` was called.
    reset_count: u32,
    /// Number of times `end_turn` was called.
    end_turn_count: u32,
    /// Number of times `invalidate_previous_response_id` was called.
    invalidate_previous_response_id_count: u32,
    /// Whether this mock advertises forced tool_choice support.
    supports_forced_tool_choice: bool,
    /// Whether this mock opts into Anchored Minimal.
    supports_anchored_minimal: bool,
    /// Overrides request-scoped transient context support. `None` keeps the
    /// trait default (`true`); plugin providers may advertise `false`, which
    /// makes the host fold transient context into the message list instead.
    supports_request_scoped_transient_context: Option<bool>,
    /// Overrides the thinking-replay signature requirement. `None`
    /// keeps the conservative trait default (`true`).
    thinking_replay_requires_signature: Option<bool>,
    /// Whether this mock bills hidden reasoning against `max_tokens`.
    output_budget_includes_reasoning: bool,
}

#[derive(Debug)]
enum ScriptedOutcome {
    Events(Vec<StreamEvent>),
    Failure(ModelError),
}

impl MockModelClient {
    /// Construct an empty mock. Without any scripted turns the next
    /// call returns a [`ModelError::Other`] so misconfigured tests
    /// fail loudly instead of silently producing an empty stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a scripted turn. The next call to
    /// `create_message_stream` will pop this and replay the events
    /// in order.
    pub fn push_turn(&self, events: Vec<StreamEvent>) {
        let mut guard = self.inner.lock().expect("mock poisoned");
        guard.scripted.push_back(ScriptedOutcome::Events(events));
    }

    /// Append a scripted failure. The next call fails with the
    /// provided error.
    pub fn push_error(&self, error: ModelError) {
        let mut guard = self.inner.lock().expect("mock poisoned");
        guard.scripted.push_back(ScriptedOutcome::Failure(error));
    }

    pub fn set_supports_forced_tool_choice(&self, supported: bool) {
        self.inner
            .lock()
            .expect("mock poisoned")
            .supports_forced_tool_choice = supported;
    }

    pub fn set_supports_anchored_minimal(&self, supported: bool) {
        self.inner
            .lock()
            .expect("mock poisoned")
            .supports_anchored_minimal = supported;
    }

    /// Model a provider that cannot take transient context as a request field
    /// (the `rebon.modelProvider` plugins that declare
    /// `requestScopedTransientContext: false`), so the host has to append it to
    /// the conversation instead.
    pub fn set_supports_request_scoped_transient_context(&self, supported: bool) {
        self.inner
            .lock()
            .expect("mock poisoned")
            .supports_request_scoped_transient_context = Some(supported);
    }

    /// Model an OpenAI Responses-style provider, where `max_tokens`
    /// covers hidden reasoning as well as the visible reply, so callers
    /// have to size the ceiling for both.
    pub fn set_output_budget_includes_reasoning(&self, includes: bool) {
        self.inner
            .lock()
            .expect("mock poisoned")
            .output_budget_includes_reasoning = includes;
    }

    /// Model a provider that drops unreplayable thinking blocks
    /// instead of rejecting them (OpenAI-style dialects, plugins).
    pub fn set_thinking_replay_requires_signature(&self, required: bool) {
        self.inner
            .lock()
            .expect("mock poisoned")
            .thinking_replay_requires_signature = Some(required);
    }

    /// Snapshot every request captured so far.
    pub fn captured_requests(&self) -> Vec<CreateMessageRequest> {
        let guard = self.inner.lock().expect("mock poisoned");
        guard.captured_requests.clone()
    }

    /// Number of requests seen so far.
    pub fn call_count(&self) -> usize {
        self.inner
            .lock()
            .expect("mock poisoned")
            .captured_requests
            .len()
    }

    /// Number of times `reset_session_state` was called.
    pub fn reset_count(&self) -> u32 {
        self.inner.lock().expect("mock poisoned").reset_count
    }

    /// Number of times `end_turn` was called.
    pub fn end_turn_count(&self) -> u32 {
        self.inner.lock().expect("mock poisoned").end_turn_count
    }

    /// Number of times `invalidate_previous_response_id` was called.
    pub fn invalidate_previous_response_id_count(&self) -> u32 {
        self.inner
            .lock()
            .expect("mock poisoned")
            .invalidate_previous_response_id_count
    }
}

#[async_trait]
impl ModelClient for MockModelClient {
    fn provider_name(&self) -> &'static str {
        "mock"
    }

    fn reset_session_state(&self) {
        self.inner.lock().expect("mock poisoned").reset_count += 1;
    }

    fn end_turn(&self) {
        self.inner.lock().expect("mock poisoned").end_turn_count += 1;
    }

    fn invalidate_previous_response_id(&self) {
        self.inner
            .lock()
            .expect("mock poisoned")
            .invalidate_previous_response_id_count += 1;
    }

    fn capabilities(&self) -> ModelCapabilities {
        let inner = self.inner.lock().expect("mock poisoned");
        ModelCapabilities {
            forced_tool_choice: inner.supports_forced_tool_choice,
            anchored_minimal: inner.supports_anchored_minimal,
            requires_inline_transient_context: !inner
                .supports_request_scoped_transient_context
                .unwrap_or(true),
            accepts_unsigned_thinking_replay: !inner
                .thinking_replay_requires_signature
                .unwrap_or(true),
            output_budget_includes_reasoning: inner.output_budget_includes_reasoning,
            ..ModelCapabilities::default()
        }
    }

    async fn create_message_stream(
        &self,
        request: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream> {
        let outcome = {
            let mut guard = self.inner.lock().expect("mock poisoned");
            guard.captured_requests.push(request);
            guard.scripted.pop_front()
        };
        let events = match outcome {
            Some(ScriptedOutcome::Events(events)) => events,
            Some(ScriptedOutcome::Failure(err)) => return Err(err),
            None => {
                return Err(ModelError::other(
                    "MockModelClient has no scripted turn for this call",
                ))
            }
        };
        let stream = futures_util::stream::iter(events.into_iter().map(Ok));
        let boxed: Pin<Box<dyn Stream<Item = ModelResult<StreamEvent>> + Send>> = Box::pin(stream);
        Ok(boxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ContentBlockDelta, ContentBlockStart, MessageDeltaFields};
    use crate::types::{StopReason, Usage};

    fn simple_turn() -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message_id: "msg_mock".into(),
                model: "mock".into(),
                usage: Usage {
                    input_tokens: 3,
                    ..Default::default()
                },
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text {
                    text: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::TextDelta {
                    text: "pong".into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Usage {
                        output_tokens: 1,
                        ..Default::default()
                    },
                },
            },
            StreamEvent::MessageStop,
        ]
    }

    #[tokio::test]
    async fn mock_replays_scripted_turn_through_accumulator() {
        let client = MockModelClient::new();
        client.push_turn(simple_turn());
        let msg = client
            .create_message(CreateMessageRequest::simple("mock", "ping"))
            .await
            .unwrap();
        assert_eq!(msg.id, "msg_mock");
        assert_eq!(msg.text(), "pong");
        assert_eq!(msg.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(msg.usage.input_tokens, 3);
        assert_eq!(msg.usage.output_tokens, 1);
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn mock_replays_turns_fifo() {
        let client = MockModelClient::new();
        client.push_turn(simple_turn());
        client.push_turn(vec![
            StreamEvent::MessageStart {
                message_id: "second".into(),
                model: "mock".into(),
                usage: Default::default(),
            },
            StreamEvent::MessageStop,
        ]);
        let first = client
            .create_message(CreateMessageRequest::simple("mock", "1"))
            .await
            .unwrap();
        let second = client
            .create_message(CreateMessageRequest::simple("mock", "2"))
            .await
            .unwrap();
        assert_eq!(first.id, "msg_mock");
        assert_eq!(second.id, "second");
    }

    #[tokio::test]
    async fn mock_scripted_failure_surfaces() {
        let client = MockModelClient::new();
        client.push_error(ModelError::transient("network"));
        let err = client
            .create_message(CreateMessageRequest::simple("mock", "x"))
            .await
            .unwrap_err();
        assert!(matches!(err, ModelError::Transient { .. }));
    }

    #[tokio::test]
    async fn mock_without_scripted_turn_errors_clearly() {
        let client = MockModelClient::new();
        let err = client
            .create_message(CreateMessageRequest::simple("mock", "x"))
            .await
            .unwrap_err();
        assert!(matches!(err, ModelError::Other(_)));
    }

    #[tokio::test]
    async fn mock_captures_every_request() {
        let client = MockModelClient::new();
        client.push_turn(simple_turn());
        client.push_turn(simple_turn());
        client
            .create_message(CreateMessageRequest::simple("mock", "first"))
            .await
            .unwrap();
        client
            .create_message(CreateMessageRequest::simple("mock", "second"))
            .await
            .unwrap();
        let captured = client.captured_requests();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].messages[0].content[0].as_text(), Some("first"));
        assert_eq!(captured[1].messages[0].content[0].as_text(), Some("second"));
    }
}
