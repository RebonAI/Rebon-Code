//! Pending standard ACP replies. Delivery remains the durable pending-prompt
//! queue; only the transient socket sinks and bounded replay results live here.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Weak};

use rebon_agent_core::{PromptExecutorError, PromptOutcome};
use rebon_proto::types::{
    JsonRpcError, JsonRpcResponse, JsonRpcVersion, RequestId, SessionPromptResult,
};
use rebon_session_host::{BackgroundJobState, BackgroundStore, HostReply, PendingPrompt};
use rebon_types::StopReason;

use super::acp_stream::Wire;
use super::server::{BackgroundIpcServer, RequestContext, WireResponse};

type Outcome = Result<StopReason, JsonRpcError>;
const COMPLETED_RESULTS: usize = 64;

pub(super) struct Sink {
    // Only the reader owns the Arc. A vanished connection must not keep its
    // writer alive, and completion must never go to a replacement connection.
    wire: Weak<Wire>,
    id: RequestId,
    subscription: u64,
    events: super::events::SessionEventStream,
}

impl Sink {
    fn answer(self, outcome: Outcome) {
        let Some(wire) = self.wire.upgrade() else {
            return;
        };
        let (result, error) = match outcome {
            Ok(stop_reason) => (
                Some(
                    serde_json::to_value(SessionPromptResult { stop_reason })
                        .expect("prompt result serializes"),
                ),
                None,
            ),
            Err(error) => (None, Some(error)),
        };
        wire.send_prompt_response(
            self.subscription,
            self.events.cursor().saturating_sub(1),
            &JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: Some(self.id),
                result,
                error,
            },
        );
    }
}

struct PromptCall {
    command_id: Option<String>,
    sinks: Vec<Sink>,
    outcome: Option<Outcome>,
}

#[derive(Default)]
pub(crate) struct PromptReplies {
    calls: HashMap<String, PromptCall>,
    completed: VecDeque<String>,
    stopped: bool,
    cancelled_claims: HashMap<u64, Vec<String>>,
}

impl PromptReplies {
    pub(crate) fn acknowledgement(&self, command_id: &str) -> Option<WireResponse> {
        self.calls
            .values()
            .any(|call| call.command_id.as_deref() == Some(command_id))
            .then(|| WireResponse::Standard(Ok(HostReply::default())))
    }

    pub(crate) fn disconnect(&mut self, wire: &Arc<Wire>) {
        let connection = Arc::downgrade(wire);
        self.calls.retain(|_, call| {
            call.sinks
                .retain(|sink| !sink.wire.ptr_eq(&connection) && sink.wire.strong_count() != 0);
            call.command_id.is_some() || !call.sinks.is_empty()
        });
    }

    fn finish(
        &mut self,
        ids: impl IntoIterator<Item = String>,
        outcome: Outcome,
    ) -> Vec<(Sink, Outcome)> {
        let mut deliveries = Vec::new();
        for id in ids {
            let Some(call) = self.calls.get_mut(&id) else {
                continue;
            };
            if call.outcome.is_some() {
                continue;
            }
            deliveries.extend(call.sinks.drain(..).map(|sink| (sink, outcome.clone())));
            if call.command_id.is_some() {
                call.outcome = Some(outcome.clone());
                self.completed.push_back(id);
            } else {
                self.calls.remove(&id);
            }
        }
        while self.completed.len() > COMPLETED_RESULTS {
            if let Some(id) = self.completed.pop_front() {
                self.calls.remove(&id);
            }
        }
        deliveries
    }

    pub(super) fn cancelled(
        &mut self,
        prompts: Vec<PendingPrompt>,
        running: Option<u64>,
    ) -> Vec<(Sink, Outcome)> {
        let mut queued = Vec::new();
        for prompt in prompts {
            if self
                .calls
                .get(&prompt.id)
                .is_none_or(|call| call.outcome.is_some())
            {
                // Completion can win before cancellation retires the durable
                // claim. Do not retain a finished id for an executor that has
                // already returned and will never consume it again.
                continue;
            }
            if running.is_some() && prompt.claimed_turn_generation == running {
                self.cancelled_claims
                    .entry(running.expect("running generation"))
                    .or_default()
                    .push(prompt.id);
            } else {
                queued.push(prompt.id);
            }
        }
        self.finish(queued, Ok(StopReason::Cancelled))
    }

    fn finish_all(&mut self, outcome: Outcome) -> Vec<(Sink, Outcome)> {
        self.cancelled_claims.clear();
        self.finish(self.calls.keys().cloned().collect::<Vec<_>>(), outcome)
    }
}

pub(super) fn deliver(deliveries: Vec<(Sink, Outcome)>) {
    // No store or registry lock is held while a socket takes its answer.
    for (sink, outcome) in deliveries {
        sink.answer(outcome);
    }
}

pub(crate) fn enqueue(
    context: &RequestContext<'_>,
    wire: &Arc<Wire>,
    id: RequestId,
    command_id: Option<&str>,
    prompt: PendingPrompt,
) -> Result<(), JsonRpcError> {
    let sink = Sink {
        wire: Arc::downgrade(wire),
        id,
        subscription: wire
            .subscription
            .lock()
            .expect("poisoned")
            .ok_or_else(|| JsonRpcError::internal_error("prompt event stream closed"))?,
        events: context.events.clone(),
    };
    let mut recent = context.recent_command_results.lock().expect("poisoned");
    if let Some(call) = command_id.and_then(|id| {
        recent
            .prompts
            .calls
            .values_mut()
            .find(|call| call.command_id.as_deref() == Some(id))
    }) {
        if let Some(outcome) = call.outcome.clone() {
            drop(recent);
            sink.answer(outcome);
        } else {
            call.sinks.push(sink);
        }
        return Ok(());
    }
    if recent.prompts.stopped {
        return Err(JsonRpcError::internal_error("the session host has stopped"));
    }
    if command_id.and_then(|id| recent.get(id)).is_some() {
        // A delivery ack is not evidence of a completed turn. In particular a
        // retry of a legacy/enqueue call must neither invent a stop reason nor
        // enqueue the same command again.
        return Err(JsonRpcError::invalid_params(
            "commandId belongs to a delivery acknowledgement, not a retained prompt result",
        ));
    }
    let prompt_id = prompt.id.clone();
    let message = prompt.text.clone();
    super::commands::queue_live_background_prompt(
        context.store,
        context.job_id,
        context.owner,
        prompt,
    )
    .map_err(|error| JsonRpcError::internal_error(error.to_string()))?;
    // Register under the same lock used by completion, before the worker can
    // deliver a result. Queue ids and claimed generations remain authoritative.
    recent.prompts.calls.insert(
        prompt_id,
        PromptCall {
            command_id: command_id.map(str::to_owned),
            sinks: vec![sink],
            outcome: None,
        },
    );
    drop(recent);
    context.record_answer(
        command_id,
        &WireResponse::Standard(Ok(HostReply::default())),
    );
    let _ = context.store.append_event(
        context.job_id,
        "reply_received_ipc",
        serde_json::json!({ "message": message, "nonInterrupting": true }),
    );
    context.wake.notify_one();
    Ok(())
}

impl BackgroundIpcServer {
    /// Called only after the executor and steering pump have settled, before
    /// finalization retires the claimed queue entries. Idle events carry no
    /// generation and are deliberately not part of this path.
    pub(crate) fn complete_prompt_turn(
        &self,
        store: &BackgroundStore,
        state: &BackgroundJobState,
        execution: &Result<PromptOutcome, PromptExecutorError>,
    ) {
        let outcome = match execution {
            Ok(outcome) => Ok(outcome.stop_reason),
            Err(PromptExecutorError::Cancelled) => Ok(StopReason::Cancelled),
            Err(error) => Err(JsonRpcError::internal_error(error.to_string())),
        };
        // Match cancellation/enqueue's registry -> store lock order. Reading
        // first could miss a newly registered claim or resurrect a claim that
        // cancellation has just retired from this generation.
        let mut recent = self.recent_command_results.lock().expect("poisoned");
        let latest = match store.read_state(&state.identity.job_id) {
            Ok(latest) if self.owner().matches(&latest) => latest,
            _ => {
                drop(recent);
                self.fail_prompt_calls("prompt turn ownership or job state was lost");
                return;
            }
        };
        let mut ids = super::super::worker::pending_prompt::claimed_pending_prompts(&latest)
            .iter()
            .filter(|prompt| prompt.claimed_turn_generation == Some(state.process.turn_generation))
            .map(|prompt| prompt.id.clone())
            .collect::<Vec<_>>();
        ids.extend(
            recent
                .prompts
                .cancelled_claims
                .remove(&state.process.turn_generation)
                .unwrap_or_default(),
        );
        let deliveries = recent.prompts.finish(ids, outcome);
        drop(recent);
        deliver(deliveries);
    }

    /// Startup/execution failure may leave the queue deliberately parked for
    /// recovery. Its callers must not remain parked on dead executions too.
    pub(crate) fn fail_prompt_calls(&self, reason: &str) {
        let deliveries = self
            .recent_command_results
            .lock()
            .expect("poisoned")
            .prompts
            .finish_all(Err(JsonRpcError::internal_error(reason)));
        deliver(deliveries);
    }

    pub(crate) fn close_prompt_calls(&self) {
        let deliveries = {
            let mut recent = self.recent_command_results.lock().expect("poisoned");
            recent.prompts.stopped = true;
            recent.prompts.finish_all(Err(JsonRpcError::internal_error(
                "the session host stopped before the prompt completed",
            )))
        };
        deliver(deliveries);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{acp_connection::forward_stream_event, events::SessionEventStream};
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn live_gap_closes_socket_instead_of_stranding_a_prompt_reply() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let (server, _) = listener.accept().unwrap();
        let wire =
            Arc::new(Wire::spawn(server, rebon_proto::framing::FramingMode::Ndjson).unwrap());
        let events = SessionEventStream::new();
        let subscription = events.subscribe(None);
        *wire.subscription.lock().unwrap() = Some(subscription.id);
        assert!(wire.begin_stream(subscription.id, subscription.cursor));
        // Eviction closes the same receiver. Advance the ring beyond it while
        // holding the forwarder so the pending completion cannot be delivered.
        events.unsubscribe(subscription.id);
        events.publish_turn(rebon_session_host::TurnStreamState::Idle, None);
        Sink {
            wire: Arc::downgrade(&wire),
            id: RequestId::Number(7),
            subscription: subscription.id,
            events: events.clone(),
        }
        .answer(Ok(StopReason::EndTurn));
        super::super::acp_connection::forward_live_stream(
            &wire,
            "ordered",
            subscription.events,
            subscription.id,
            subscription.cursor,
            &events,
        );
        assert_eq!(*wire.subscription.lock().unwrap(), None);
        assert_eq!(events.subscriber_count(), 0);
        assert!(!wire.forwarded(subscription.id, events.cursor()));
        let mut line = String::new();
        assert_eq!(
            BufReader::new(client).read_line(&mut line).unwrap(),
            0,
            "a live gap must close, not reply successfully"
        );
    }

    #[test]
    fn cancellation_after_completion_does_not_retain_finished_generation() {
        let mut replies = PromptReplies::default();
        let mut prompt = PendingPrompt::new("prompt".into(), "text".into(), vec![], 0).unwrap();
        prompt.claimed_turn_generation = Some(7);
        replies.calls.insert(
            prompt.id.clone(),
            PromptCall {
                command_id: Some("command".into()),
                sinks: vec![],
                outcome: None,
            },
        );
        replies.finish([prompt.id.clone()], Ok(StopReason::MaxTokens));
        replies.cancelled(vec![prompt.clone()], Some(7));
        assert!(replies.cancelled_claims.is_empty());
        assert_eq!(
            replies.calls[&prompt.id].outcome,
            Some(Ok(StopReason::MaxTokens))
        );
    }

    #[test]
    fn prompt_reply_cannot_overtake_published_final_updates() {
        for outcome in [
            Ok(StopReason::EndTurn),
            Ok(StopReason::Cancelled),
            Err(JsonRpcError::internal_error("executor failed")),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            client
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let (server, _) = listener.accept().unwrap();
            let wire =
                Arc::new(Wire::spawn(server, rebon_proto::framing::FramingMode::Ndjson).unwrap());
            let events = SessionEventStream::new();
            let subscription = events.subscribe(None);
            *wire.subscription.lock().unwrap() = Some(subscription.id);
            assert!(wire.begin_stream(subscription.id, subscription.cursor));

            // Hold the actual event receiver here instead of scheduling its
            // forwarder. Both final updates are published before completion,
            // but neither can reach the wire until answer() has returned.
            for text in ["last text", "final text"] {
                events.publish_update(&serde_json::from_value(serde_json::json!({
                    "sessionId": "ordered", "update": {"sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text}}
                })).unwrap()).unwrap();
            }
            Sink {
                wire: Arc::downgrade(&wire),
                id: RequestId::Number(7),
                subscription: subscription.id,
                events: events.clone(),
            }
            .answer(outcome.clone());
            for _ in 0..2 {
                let line = subscription.events.recv().unwrap();
                let event = serde_json::from_str(&line).unwrap();
                assert!(forward_stream_event(
                    &wire,
                    subscription.id,
                    "ordered",
                    event
                ));
            }
            let mut reader = BufReader::new(client);
            for text in ["last text", "final text"] {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(
                    frame["method"], "session/update",
                    "reply overtook final update: {frame}"
                );
                assert_eq!(frame["params"]["update"]["content"]["text"], text);
            }
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(frame["id"], 7);
            match outcome {
                Ok(reason) => {
                    assert_eq!(frame["result"], serde_json::json!({"stopReason": reason}))
                }
                Err(error) => assert_eq!(frame["error"], serde_json::to_value(error).unwrap()),
            }
        }
    }
}
