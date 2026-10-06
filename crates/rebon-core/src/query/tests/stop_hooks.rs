//! The Stop event at a turn's natural end: raised once the model answers
//! without asking for tools, carrying the answer; a hook that blocks keeps
//! the turn going with its reason as the next user message, a bounded number
//! of times; a quiet hook does not.

use super::*;
use crate::policy_seat::{
    HookEventPayload, PolicyEventKind, PolicyFuture, PolicyRequest, PolicySources,
    PolicySubscriber, Verdict,
};
use std::sync::Mutex as StdMutex;

/// Hears every Stop: records its payload, and blocks the first `blocks` of
/// them with a reason naming the count.
struct StopRecorder {
    seen: StdMutex<Vec<HookEventPayload>>,
    blocks: usize,
}

impl StopRecorder {
    fn new(blocks: usize) -> Arc<Self> {
        Arc::new(Self {
            seen: StdMutex::new(Vec::new()),
            blocks,
        })
    }

    fn seen(&self) -> Vec<HookEventPayload> {
        self.seen.lock().expect("stop recorder poisoned").clone()
    }
}

impl PolicySubscriber for StopRecorder {
    fn interest(&self, kind: PolicyEventKind) -> bool {
        kind == PolicyEventKind::Stop
    }

    fn decide<'a>(&'a self, request: &'a PolicyRequest) -> PolicyFuture<'a> {
        let count = {
            let mut seen = self.seen.lock().expect("stop recorder poisoned");
            seen.push(request.payload.clone());
            seen.len()
        };
        let blocks = self.blocks;
        Box::pin(async move {
            if count <= blocks {
                Verdict::Modify {
                    effects: vec![rebon_hooks::HookEffect::BlockStop {
                        reason: format!("not yet ({count})"),
                    }],
                }
            } else {
                Verdict::Allow
            }
        })
    }
}

/// Hears Stop and never answers within its budget.
struct QuietStop;

impl PolicySubscriber for QuietStop {
    fn interest(&self, kind: PolicyEventKind) -> bool {
        kind == PolicyEventKind::Stop
    }

    fn budget(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_millis(20))
    }

    fn decide<'a>(&'a self, _request: &'a PolicyRequest) -> PolicyFuture<'a> {
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_secs(3_600)).await;
            Verdict::Allow
        })
    }
}

fn policy_with(subscriber: Arc<dyn PolicySubscriber>) -> PolicySources {
    PolicySources::default().with_subscriber(
        "test/stop",
        crate::turn_hook::Order::NORMAL,
        subscriber,
    )
}

async fn run(mock: &MockModelClient, policy: PolicySources) -> Vec<QueryEvent> {
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("say hello")])
        .with_max_iterations(4)
        .with_policy(policy);
    let mut rx = run_query(
        Arc::new(Engine::new()),
        SessionHandle::new(client),
        params,
        ToolContext::new(),
        CancelToken::new(),
    );
    drain(&mut rx).await
}

fn final_text(events: &[QueryEvent]) -> String {
    match events.last() {
        Some(QueryEvent::Done { final_message, .. }) => final_message.text(),
        other => panic!("expected Done, got {other:?}"),
    }
}

fn stop_fields(payload: &HookEventPayload) -> (Option<String>, Option<String>, bool) {
    match payload {
        HookEventPayload::Stop {
            stop_reason,
            last_assistant_message,
            stop_hook_active,
        } => (
            stop_reason.clone(),
            last_assistant_message.clone(),
            *stop_hook_active,
        ),
        other => panic!("a Stop, not {other:?}"),
    }
}

fn last_user_text(messages: &[ApiMessage]) -> String {
    messages
        .last()
        .and_then(|message| message.content.first())
        .and_then(ApiContentBlock::as_text)
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn a_natural_end_raises_stop_with_the_answer() {
    let mock = MockModelClient::new();
    mock.push_turn(text_turn("msg_1", "hello world"));
    let recorder = StopRecorder::new(0);
    let events = run(&mock, policy_with(recorder.clone())).await;
    assert_eq!(final_text(&events), "hello world");
    let seen = recorder.seen();
    assert_eq!(seen.len(), 1, "one Stop for one natural end");
    assert_eq!(
        stop_fields(&seen[0]),
        (Some("end_turn".into()), Some("hello world".into()), false)
    );
}

#[tokio::test]
async fn a_tool_round_does_not_raise_stop_until_the_turn_ends() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "hi"})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let mock = MockModelClient::new();
    mock.push_turn(tool_turn("msg_1", "Read", "call_1", r#"{"path":"a"}"#));
    mock.push_turn(text_turn("msg_2", "read it"));
    let recorder = StopRecorder::new(0);
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("read a")])
        .with_max_iterations(4)
        .with_policy(policy_with(recorder.clone()));
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;
    assert_eq!(final_text(&events), "read it");
    assert_eq!(recorder.seen().len(), 1);
}

#[tokio::test]
async fn a_blocking_stop_hook_keeps_the_turn_going_with_its_reason() {
    let mock = MockModelClient::new();
    mock.push_turn(text_turn("msg_1", "first"));
    mock.push_turn(text_turn("msg_2", "second"));
    let recorder = StopRecorder::new(1);
    let events = run(&mock, policy_with(recorder.clone())).await;
    assert_eq!(final_text(&events), "second");
    let requests = mock.captured_requests();
    assert_eq!(
        requests.len(),
        2,
        "the hook's block cost one more model call"
    );
    assert_eq!(last_user_text(&requests[1].messages), "not yet (1)");
    let assistant_before = &requests[1].messages[requests[1].messages.len() - 2];
    assert_eq!(
        assistant_before.role,
        Role::Assistant,
        "the answer stays in history"
    );
    let seen = recorder.seen();
    assert_eq!(seen.len(), 2);
    assert!(!stop_fields(&seen[0]).2);
    assert!(
        stop_fields(&seen[1]).2,
        "the second Stop says a hook already kept it going"
    );
}

#[tokio::test]
async fn a_stop_hook_that_never_lets_go_is_ended_after_the_bound() {
    let mock = MockModelClient::new();
    for n in 0..6 {
        mock.push_turn(text_turn(&format!("msg_{n}"), &format!("answer {n}")));
    }
    let recorder = StopRecorder::new(usize::MAX);
    let events = run(&mock, policy_with(recorder.clone())).await;
    assert_eq!(
        mock.captured_requests().len(),
        1 + super::super::turn_control::MAX_STOP_HOOK_CONTINUATIONS,
        "kept going the bounded number of times, then ended"
    );
    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
}

#[tokio::test]
async fn a_quiet_stop_hook_does_not_keep_the_turn_going() {
    let mock = MockModelClient::new();
    mock.push_turn(text_turn("msg_1", "hello"));
    let events = run(&mock, policy_with(Arc::new(QuietStop))).await;
    assert_eq!(final_text(&events), "hello");
    assert_eq!(mock.captured_requests().len(), 1);
}

#[tokio::test]
async fn a_sub_agents_turn_raises_no_stop_of_its_own() {
    let mock = MockModelClient::new();
    mock.push_turn(text_turn("msg_1", "done"));
    let recorder = StopRecorder::new(0);
    let events = run(&mock, policy_with(recorder.clone()).in_agent("worker-1")).await;
    assert_eq!(final_text(&events), "done");
    assert!(
        recorder.seen().is_empty(),
        "a sub-agent's end is its spawner's SubagentStop"
    );
}
