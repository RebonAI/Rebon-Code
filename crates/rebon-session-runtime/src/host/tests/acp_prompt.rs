//! Socket regressions driven through the worker's real claim, executor and
//! completion boundary. No test completes a JSON reply or publishes an Idle
//! event to stand in for an executor result.
use super::*;
use crate::host::worker::{
    execution::{execute_claimed_prompt, spawn_background_update_pump},
    pending_prompt::{claim_background_job, WorkerClaim},
};
use rebon_agent_core::{PromptExecutor, PromptExecutorError, PromptOutcome, PromptRequest};
use rebon_types::StopReason;

type ExecutionResult = Result<PromptOutcome, PromptExecutorError>;

struct ControlledExecutor {
    release: tokio::sync::Mutex<tokio::sync::oneshot::Receiver<ExecutionResult>>,
    started: std::sync::mpsc::Sender<PromptRequest>,
    permitted: std::sync::mpsc::Sender<()>,
    broker: Option<ChannelPermissionBroker>,
    events: SessionEventStream,
    poller: Arc<crate::host::worker::pending_prompt::BackgroundPendingPromptPoller>,
    consume: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<()>>,
    consumed: std::sync::mpsc::Sender<Vec<rebon_session_host::PendingPrompt>>,
}

#[async_trait::async_trait]
impl PromptExecutor for ControlledExecutor {
    async fn execute(&self, request: PromptRequest) -> ExecutionResult {
        self.started.send(request.clone()).unwrap();
        self.events.publish_update(&serde_json::from_value(serde_json::json!({
            "sessionId": request.session_id,
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "working"}}
        })).unwrap());
        if let Some(broker) = &self.broker {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            broker.forward_direct(OutboundPermissionQuery {
                id: 91,
                tool_name: "Read".into(),
                tool_call_id: "call-1".into(),
                session_id: request.session_id.clone(),
                title: "Read file".into(),
                message: "Read file".into(),
                tool_input: None,
                metadata: None,
                options: vec![PermissionQueryOption {
                    option_id: "allow_once".into(),
                    label: "Allow once".into(),
                    kind: PermissionOptionKind::AllowOnce,
                }],
                response_tx,
            });
            response_rx.await.expect("permission answered");
            self.permitted.send(()).unwrap();
        }
        let mut release = self.release.lock().await;
        let mut consume = self.consume.lock().await;
        let outcome = loop {
            tokio::select! {
                result = &mut *release => break result.unwrap_or_else(|_| Err(PromptExecutorError::Execution("test controller dropped".into()))),
                _ = request.cancel.notified() => break Err(PromptExecutorError::Cancelled),
                Some(()) = consume.recv() => {
                    self.consumed.send(self.poller.claim_pending_prompts(&request.session_id).unwrap()).unwrap();
                }
            }
        };
        if let Some(publisher) = &request.update_publisher {
            for text in ["last text", "final text"] {
                publisher.publish_owned(serde_json::from_value(serde_json::json!({
                    "sessionId": request.session_id,
                    "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}
                })).unwrap()).await;
            }
        }
        // ChannelSessionUpdatePublisher is immediately ready; there is no
        // yield between queuing final text and returning. The current-thread
        // runtime cannot drain the update task in this window.
        outcome
    }
}

struct Running {
    release: tokio::sync::oneshot::Sender<ExecutionResult>,
    permitted: std::sync::mpsc::Receiver<()>,
    consume: tokio::sync::mpsc::UnboundedSender<()>,
    consumed: std::sync::mpsc::Receiver<Vec<rebon_session_host::PendingPrompt>>,
    task: std::thread::JoinHandle<()>,
    state: BackgroundJobState,
}

fn start_execution(
    store: &BackgroundStore,
    ipc: &Arc<BackgroundIpcServer>,
    job: &str,
    permission: bool,
) -> Running {
    start_execution_with_updates(store, ipc, job, permission, false)
}

fn start_execution_with_updates(
    store: &BackgroundStore,
    ipc: &Arc<BackgroundIpcServer>,
    job: &str,
    permission: bool,
    buffered_final: bool,
) -> Running {
    let (cancel, claim) = ipc.start_turn_and(|| {
        claim_background_job(store, job, std::process::id(), ipc.port, &ipc.token, |_| {
            Some(true)
        })
    });
    let WorkerClaim::Claimed(state) = claim.unwrap() else {
        panic!("queued prompt must be claimed")
    };
    let (publisher, updates) = rebon_agent_core::ChannelSessionUpdatePublisher::new();
    let publisher: Arc<dyn rebon_agent_core::SessionUpdatePublisher> = Arc::new(publisher);
    let request = PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: state.identity.session_id.clone().unwrap(),
        cwd: state.identity.cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: state.identity.pending_prompts[0].text.clone(),
            annotations: None,
        })],
        user_message_uuid: Some(state.identity.pending_prompts[0].id.clone()),
        cancel,
        mcp_servers: vec![],
        update_publisher: buffered_final.then(|| publisher.clone()),
        permission_publisher: None,
        thinking_budget: None,
        max_tokens: None,
        reasoning_effort_ordinal: None,
        additional_working_directories: vec![],
        coordinator_mode: None,
        coordinator_report_paths: vec![],
        background_agent_system: None,
        background_agent_tool_filter: None,
        execution_policy: None,
        replay_requests: vec![],
        skill_invocations: vec![],
    };
    let broker = permission.then(|| {
        let (broker, receiver) = ChannelPermissionBroker::new("session-under-test");
        ipc.attach_permission_receiver(state.process.turn_generation, receiver);
        broker
    });
    let (release, released) = tokio::sync::oneshot::channel();
    let (started, starting) = std::sync::mpsc::channel();
    let (permitted, permission_answered) = std::sync::mpsc::channel();
    let (consume, consuming) = tokio::sync::mpsc::unbounded_channel();
    let (consumed, consumed_prompts) = std::sync::mpsc::channel();
    let executor = ControlledExecutor {
        release: tokio::sync::Mutex::new(released),
        started,
        permitted,
        broker,
        events: ipc.events.clone(),
        poller: crate::host::worker::pending_prompt::BackgroundPendingPromptPoller::new(
            store.clone(),
            &state,
            ipc,
        ),
        consume: tokio::sync::Mutex::new(consuming),
        consumed,
    };
    let worker_store = store.clone();
    let worker_ipc = ipc.clone();
    let worker_state = *state.clone();
    let task = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let events = worker_ipc.events.clone();
                let update_pump = spawn_background_update_pump(updates, move |update| {
                    events.publish_update(&update);
                });
                let result = execute_claimed_prompt(
                    &worker_store,
                    &worker_state,
                    &worker_ipc,
                    &executor,
                    request,
                    None,
                    Some(update_pump),
                )
                .await;
                // A warm session keeps the publisher alive across completion.
                drop(publisher);
                match result {
                    Ok(_) => {
                        let mut latest = worker_store.read_state(worker_state.job_id()).unwrap();
                        mark_claimed_pending_prompts_completed(
                            &worker_store,
                            &mut latest,
                            &worker_ipc,
                        )
                        .unwrap();
                        // Like the finalization unit fixtures, this controlled
                        // executor supplies completion evidence instead of writing
                        // a model transcript. Claim retirement itself is real.
                        finalize_background_turn_with_completion_evidence(
                            &worker_store,
                            &latest,
                            &worker_ipc,
                            BackgroundTurnTerminalOutcome::Completed { summary: "done" },
                            now_ms(),
                            true,
                        )
                        .unwrap();
                    }
                    Err(PromptExecutorError::Cancelled) => {
                        finalize_cancelled_background_turn(
                            &worker_store,
                            &worker_state,
                            &worker_ipc,
                            now_ms(),
                        )
                        .unwrap();
                    }
                    Err(_) => {}
                }
            });
    });
    let received = starting
        .recv_timeout(READ_TIMEOUT)
        .expect("executor started");
    assert_eq!(
        received.user_message_uuid.as_deref(),
        Some(state.identity.pending_prompts[0].id.as_str())
    );
    Running {
        release,
        permitted: permission_answered,
        consume,
        consumed: consumed_prompts,
        task,
        state: *state,
    }
}

fn prompt(peer: &mut Peer, command: &str) -> i64 {
    let id = peer.send(
        "session/prompt",
        serde_json::json!({
            "sessionId": "session-under-test", "prompt": [{"type": "text", "text": command}],
            "_meta": {"rebon": {"commandId": command}}
        }),
    );
    // The pong is a read-loop barrier, not a timing assumption about absence.
    let pong = peer.call("_session/ping", serde_json::json!({}));
    assert_eq!(pong["result"], serde_json::json!({}));
    id
}

fn update(peer: &mut Peer) {
    assert_eq!(peer.read().unwrap()["method"], "session/update");
}

fn result(peer: &mut Peer, id: i64, reason: StopReason) {
    let answer = peer.read().expect("completed prompt");
    assert_eq!(answer["id"], id, "{answer}");
    assert_eq!(
        answer["result"],
        serde_json::json!({"stopReason": reason}),
        "{answer}"
    );
    assert!(answer.get("error").is_none(), "{answer}");
}

#[test]
fn standard_prompt_waits_for_its_executor_and_preserves_every_stop_reason() {
    for reason in [
        StopReason::EndTurn,
        StopReason::MaxTokens,
        StopReason::MaxTurnRequests,
        StopReason::Refusal,
        StopReason::Cancelled,
    ] {
        let (_dir, store, state, ipc) = hosted_job();
        let ipc = Arc::new(ipc);
        let mut peer = attached(&ipc);
        let id = prompt(&mut peer, "first");
        let running = start_execution(&store, &ipc, state.job_id(), false);
        update(&mut peer);
        peer.call("_session/ping", serde_json::json!({}));
        running
            .release
            .send(Ok(PromptOutcome {
                stop_reason: reason,
                ..PromptOutcome::end_turn()
            }))
            .unwrap();
        result(&mut peer, id, reason);
        running.task.join().unwrap();
        // A completed replay preserves the outcome and never queues a new turn.
        let replay = peer.call(
            "session/prompt",
            serde_json::json!({
                "sessionId": "session-under-test", "prompt": [{"type": "text", "text": "first"}],
                "_meta": {"rebon": {"commandId": "first"}}
            }),
        );
        assert_eq!(replay["result"], serde_json::json!({"stopReason": reason}));
        assert!(store
            .read_state(state.job_id())
            .unwrap()
            .identity
            .pending_prompts
            .is_empty());
        ipc.stop();
    }
}

#[test]
fn executor_final_updates_drain_before_success_cancel_and_error_responses() {
    for execution in [
        Ok(PromptOutcome::end_turn()),
        Err(PromptExecutorError::Cancelled),
        Err(PromptExecutorError::Execution("controlled failure".into())),
    ] {
        let (_dir, store, state, ipc) = hosted_job();
        let ipc = Arc::new(ipc);
        let mut peer = attached(&ipc);
        let id = prompt(&mut peer, "buffered final");
        let running = start_execution_with_updates(&store, &ipc, state.job_id(), false, true);
        let failed = matches!(&execution, Err(PromptExecutorError::Execution(_)));
        let cancelled = matches!(&execution, Err(PromptExecutorError::Cancelled));
        running.release.send(execution).unwrap();
        // Deliberately do not read even "working" until the executor can return.
        for text in ["working", "last text", "final text"] {
            let update = peer.read().expect("all executor text precedes result");
            assert_eq!(update["method"], "session/update", "{update}");
            assert_eq!(update["params"]["update"]["content"]["text"], text);
        }
        if failed {
            let answer = peer.read().unwrap();
            assert_eq!(answer["id"], id);
            assert_eq!(answer["error"]["code"], -32603);
        } else {
            result(
                &mut peer,
                id,
                if cancelled {
                    StopReason::Cancelled
                } else {
                    StopReason::EndTurn
                },
            );
        }
        running.task.join().unwrap();
        ipc.stop();
    }
}

#[test]
fn later_prompt_waits_for_its_claim_and_pending_replays_share_one_execution() {
    let (_dir, store, state, ipc) = hosted_job();
    let ipc = Arc::new(ipc);
    let mut peer = attached(&ipc);
    let first = prompt(&mut peer, "first");
    let turn1 = start_execution(&store, &ipc, state.job_id(), false);
    update(&mut peer);
    let second = prompt(&mut peer, "second");
    let repeat = prompt(&mut peer, "second");
    assert_eq!(
        store
            .read_state(state.job_id())
            .unwrap()
            .identity
            .pending_prompts
            .len(),
        2
    );
    turn1.release.send(Ok(PromptOutcome::end_turn())).unwrap();
    result(&mut peer, first, StopReason::EndTurn);
    turn1.task.join().unwrap();
    peer.call("_session/ping", serde_json::json!({}));
    let turn2 = start_execution(&store, &ipc, state.job_id(), false);
    assert!(turn2.state.process.turn_generation > turn1.state.process.turn_generation);
    update(&mut peer);
    turn2
        .release
        .send(Ok(PromptOutcome {
            stop_reason: StopReason::MaxTokens,
            ..PromptOutcome::end_turn()
        }))
        .unwrap();
    result(&mut peer, second, StopReason::MaxTokens);
    result(&mut peer, repeat, StopReason::MaxTokens);
    turn2.task.join().unwrap();
    ipc.stop();
}

#[test]
fn permission_response_and_cancel_share_the_pending_prompt_socket() {
    let (_dir, store, state, ipc) = hosted_job();
    let ipc = Arc::new(ipc);
    let mut peer = attached(&ipc);
    let id = prompt(&mut peer, "permission turn");
    let running = start_execution(&store, &ipc, state.job_id(), true);
    update(&mut peer);
    let permission = peer.read().unwrap();
    assert_eq!(permission["method"], "session/request_permission");
    peer.call("_session/ping", serde_json::json!({}));
    peer.send_response(
        permission["id"].clone(),
        serde_json::json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}),
    );
    running
        .permitted
        .recv_timeout(READ_TIMEOUT)
        .expect("executor received its permission answer");
    // Cancel is a notification and must still be read while prompt is waiting.
    writeln!(peer.stream, "{}", serde_json::json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "session-under-test"}})).unwrap();
    result(&mut peer, id, StopReason::Cancelled);
    running.task.join().unwrap();
    ipc.stop();
}

#[test]
fn shutdown_and_executor_failure_release_all_waiting_prompts() {
    let (_dir, store, state, ipc) = hosted_job();
    let ipc = Arc::new(ipc);
    let mut peer = attached(&ipc);
    let first = prompt(&mut peer, "failed");
    let running = start_execution(&store, &ipc, state.job_id(), false);
    update(&mut peer);
    let second = prompt(&mut peer, "queued behind failure");
    running
        .release
        .send(Err(PromptExecutorError::Misconfigured(
            "controlled startup failure".into(),
        )))
        .unwrap();
    for expected in [first, second] {
        let answer = peer.read().unwrap();
        assert_eq!(answer["id"], expected);
        assert_eq!(answer["error"]["code"], -32603);
        assert!(answer.get("result").is_none());
    }
    running.task.join().unwrap();
    let last = prompt(&mut peer, "shutdown");
    ipc.stop();
    let answer = peer.read().unwrap();
    assert_eq!(answer["id"], last);
    assert_eq!(answer["error"]["code"], -32603);
}

#[test]
fn disconnect_releases_the_stream_and_reconnect_replays_without_duplicate_delivery() {
    let (_dir, store, state, ipc) = hosted_job();
    let ipc = Arc::new(ipc);
    let mut peer = attached(&ipc);
    prompt(&mut peer, "reconnect");
    let running = start_execution(&store, &ipc, state.job_id(), false);
    update(&mut peer);
    drop(peer);
    assert!(wait_until(2_000, || ipc.events.subscriber_count() == 0));
    let mut replacement = attached(&ipc);
    let id = prompt(&mut replacement, "reconnect");
    assert_eq!(
        store
            .read_state(state.job_id())
            .unwrap()
            .identity
            .pending_prompts
            .len(),
        1
    );
    running.release.send(Ok(PromptOutcome::end_turn())).unwrap();
    result(&mut replacement, id, StopReason::EndTurn);
    running.task.join().unwrap();
    ipc.stop();
}

#[test]
fn consumed_followup_shares_only_its_claimed_turn_result() {
    let (_dir, store, state, ipc) = hosted_job();
    let ipc = Arc::new(ipc);
    let mut peer = attached(&ipc);
    let first = prompt(&mut peer, "first");
    let running = start_execution(&store, &ipc, state.job_id(), false);
    update(&mut peer);
    let consumed = prompt(&mut peer, "consumed");
    running.consume.send(()).unwrap();
    let claimed = running.consumed.recv_timeout(READ_TIMEOUT).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].text, "consumed");
    assert_eq!(
        claimed[0].claimed_turn_generation,
        Some(running.state.process.turn_generation)
    );
    let later = prompt(&mut peer, "not consumed");
    running
        .release
        .send(Ok(PromptOutcome {
            stop_reason: StopReason::MaxTurnRequests,
            ..PromptOutcome::end_turn()
        }))
        .unwrap();
    let mut ids = vec![];
    for _ in 0..2 {
        let answer = peer.read().unwrap();
        ids.push(answer["id"].as_i64().unwrap());
        assert_eq!(
            answer["result"],
            serde_json::json!({"stopReason": "max_turn_requests"})
        );
    }
    ids.sort();
    assert_eq!(ids, vec![first, consumed]);
    running.task.join().unwrap();
    peer.call("_session/ping", serde_json::json!({}));
    let next = start_execution(&store, &ipc, state.job_id(), false);
    update(&mut peer);
    next.release.send(Ok(PromptOutcome::end_turn())).unwrap();
    result(&mut peer, later, StopReason::EndTurn);
    next.task.join().unwrap();
    ipc.stop();
}

#[test]
fn failed_worktree_startup_releases_prompt_before_an_executor_exists() {
    let (dir, store, state, ipc) = hosted_job();
    let mut peer = attached(&ipc);
    let id = prompt(&mut peer, "cannot start");
    store
        .update_state(state.job_id(), |state| {
            state.identity.cwd = dir.path().to_string_lossy().into_owned();
            state.workspace.isolate_in_worktree = true;
            state.workspace.require_worktree = true;
            state.workspace.worktree_path = Some(
                dir.path()
                    .join("missing-worktree")
                    .to_string_lossy()
                    .into_owned(),
            );
            Ok(())
        })
        .unwrap();
    let (cancel, claim) = ipc.start_turn_and(|| {
        claim_background_job(
            &store,
            state.job_id(),
            std::process::id(),
            ipc.port,
            &ipc.token,
            |_| Some(true),
        )
    });
    let WorkerClaim::Claimed(mut claimed) = claim.unwrap() else {
        panic!("must claim")
    };
    let failed = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(crate::host::worker::execution::execute_background_job(
            &store,
            &mut claimed,
            &ipc,
            cancel,
            &mut crate::session_handoff::HandedBack::default(),
        ));
    assert!(failed.is_err());
    let answer = peer.read().unwrap();
    assert_eq!(answer["id"], id);
    assert_eq!(answer["error"]["code"], -32603);
    peer.call("_session/ping", serde_json::json!({}));
    ipc.stop();
}

#[test]
fn completed_prompt_replay_retention_is_bounded_like_delivery_memory() {
    let (_dir, store, state, ipc) = hosted_job();
    let mut peer = attached(&ipc);
    for index in 0..65 {
        let id = prompt(&mut peer, &format!("bounded-{index}"));
        writeln!(peer.stream, "{}", serde_json::json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "session-under-test"}})).unwrap();
        result(&mut peer, id, StopReason::Cancelled);
    }
    let retained = peer.call(
        "session/prompt",
        serde_json::json!({
            "sessionId": "session-under-test", "prompt": [{"type": "text", "text": "bounded-64"}],
            "_meta": {"rebon": {"commandId": "bounded-64"}}
        }),
    );
    assert_eq!(
        retained["result"],
        serde_json::json!({"stopReason": "cancelled"})
    );
    assert!(store
        .read_state(state.job_id())
        .unwrap()
        .identity
        .pending_prompts
        .is_empty());
    // Once both bounded replay memories evict an id it is a new delivery,
    // exactly as for existing queued commands; it must not invent an outcome.
    let expired = prompt(&mut peer, "bounded-0");
    assert_eq!(
        store
            .read_state(state.job_id())
            .unwrap()
            .identity
            .pending_prompts
            .len(),
        1
    );
    ipc.stop();
    assert_eq!(peer.read().unwrap()["id"], expired);
}

#[test]
fn cancelling_a_queued_prompt_does_not_wait_for_an_executor_that_never_started() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = attached(&ipc);
    let id = prompt(&mut peer, "never started");
    writeln!(peer.stream, "{}", serde_json::json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "session-under-test"}})).unwrap();
    result(&mut peer, id, StopReason::Cancelled);
    ipc.stop();
}
