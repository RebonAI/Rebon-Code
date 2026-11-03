use std::collections::VecDeque;

use super::super::*;
use super::support::*;
use rebon_core::query::{AttachmentPollPhase, AttachmentPollRequest};

fn poll_request<'a>(
    session_id: &'a str,
    turn_id: &'a str,
    next_iteration: u64,
    phase: AttachmentPollPhase,
) -> AttachmentPollRequest<'a> {
    AttachmentPollRequest::new(session_id, turn_id, next_iteration, phase)
}

struct TestSteeringBackend {
    outcomes: Mutex<
        VecDeque<Result<rebon_agent_core::SteerOutcome, rebon_agent_core::AgentBackendError>>,
    >,
    calls: Arc<Mutex<Vec<(String, String, usize)>>>,
    sent: tokio::sync::mpsc::UnboundedSender<String>,
    gate: Option<Arc<tokio::sync::Notify>>,
}

#[async_trait::async_trait]
impl rebon_agent_core::AgentBackend for TestSteeringBackend {
    fn kind(&self) -> rebon_agent_core::AgentBackendKind {
        rebon_agent_core::AgentBackendKind::Acp
    }

    fn name(&self) -> &str {
        "test-steering"
    }

    fn capabilities(&self) -> rebon_agent_core::AgentCapabilities {
        rebon_agent_core::AgentCapabilities::MINIMAL
    }

    async fn start_session(
        &self,
        spec: rebon_agent_core::AgentSessionSpec,
    ) -> Result<rebon_agent_core::AgentSessionStart, rebon_agent_core::AgentBackendError> {
        Ok(rebon_agent_core::AgentSessionStart::reused(spec.session_id))
    }

    async fn prompt(
        &self,
        _request: rebon_agent_core::PromptRequest,
    ) -> Result<rebon_agent_core::PromptOutcome, rebon_agent_core::PromptExecutorError> {
        Ok(rebon_agent_core::PromptOutcome::end_turn())
    }

    async fn steer(
        &self,
        _session_id: &str,
        blocks: Vec<rebon_types::ContentBlock>,
        user_message_uuid: &str,
    ) -> Result<rebon_agent_core::SteerOutcome, rebon_agent_core::AgentBackendError> {
        let text = blocks
            .iter()
            .find_map(|block| match block {
                rebon_types::ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let image_count = blocks
            .iter()
            .filter(|block| matches!(block, rebon_types::ContentBlock::Image(_)))
            .count();
        self.calls
            .lock()
            .unwrap()
            .push((user_message_uuid.to_string(), text, image_count));
        let _ = self.sent.send(user_message_uuid.to_string());
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(rebon_agent_core::SteerOutcome::Injected))
    }
}

fn test_steering_agents(
    outcomes: Vec<Result<rebon_agent_core::SteerOutcome, rebon_agent_core::AgentBackendError>>,
    session_id: &str,
    gate: Option<Arc<tokio::sync::Notify>>,
) -> (
    Arc<rebon_agent_core::routing::SessionAgents<rebon_acp_client::AcpAgentBackend>>,
    Arc<Mutex<Vec<(String, String, usize)>>>,
    tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    let (sent, rx) = tokio::sync::mpsc::unbounded_channel();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let backend: Arc<dyn rebon_agent_core::AgentBackend> = Arc::new(TestSteeringBackend {
        outcomes: Mutex::new(outcomes.into()),
        calls: calls.clone(),
        sent,
        gate,
    });
    let switch = Arc::new(rebon_agent_core::AgentBackendSwitch::new(backend.clone()));
    let agents = Arc::new(rebon_agent_core::routing::SessionAgents::local_only(
        switch, backend, ".", ".", session_id,
    ));
    (agents, calls, rx)
}

#[test]
fn background_pending_prompt_poller_claims_running_append_before_terminal_response() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 4;
    state.identity.session_id = Some("sess-mid-turn".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    let image = BackgroundImageAttachment {
        id: 17,
        data: "image-data".into(),
        media_type: "image/jpeg".into(),
        filename: Some("follow-up.jpg".into()),
        source_path: None,
    };
    state.identity.pending_prompts = vec![pending_prompt(
        "u-mobile-command-1",
        "look at this",
        vec![image.clone()],
    )];
    state.identity.pending_prompts[0].coordinator_report_paths =
        vec!["C:/reports/mid-turn.md".into()];
    store.write_state(&state).unwrap();
    let poller = BackgroundPendingPromptPoller::new(store.clone(), &state, &ipc);

    let messages = poller.poll(poll_request(
        "sess-mid-turn",
        "turn-1",
        1,
        AttachmentPollPhase::Eager,
    ));

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, rebon_api::Role::User);
    assert!(matches!(
        &messages[0].content[0],
        rebon_api::ContentBlock::Text(text) if text.text == "look at this"
    ));
    assert!(matches!(
        &messages[0].content[1],
        rebon_api::ContentBlock::Image(block)
            if block.source.media_type == image.media_type && block.source.data == image.data
    ));
    assert!(matches!(
        &messages[0].content[2],
        rebon_api::ContentBlock::Text(text)
            if text.text == "<rebon-queued-user-input uuid=\"u-mobile-command-1\" imagePasteIds=\"17\" />"
    ));
    let claimed = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(
        claimed.identity.pending_prompts[0].claimed_turn_generation,
        Some(state.process.turn_generation)
    );
    assert_eq!(claimed.process.pid, state.process.pid);
    assert_eq!(claimed.process.ipc_token, state.process.ipc_token);
    assert_eq!(
        poller.take_coordinator_report_paths_for_query("sess-mid-turn", "turn-1"),
        vec![PathBuf::from("C:/reports/mid-turn.md")]
    );
    assert!(poller
        .take_coordinator_report_paths_for_query("sess-mid-turn", "turn-1")
        .is_empty());
    assert!(poller
        .poll(poll_request(
            "sess-mid-turn",
            "turn-1",
            2,
            AttachmentPollPhase::Eager,
        ))
        .is_empty());
    assert!(poller
        .poll(poll_request(
            "sess-mid-turn",
            "turn-1",
            2,
            AttachmentPollPhase::Regular,
        ))
        .is_empty());
    ipc.cancel.cancel();
}

#[test]
fn background_pending_prompt_poller_still_claims_append_after_tool_round() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 7;
    state.identity.session_id = Some("sess-tool-round".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    state.identity.pending_prompts = vec![pending_prompt(
        "pp-tool-round",
        "continue after the tool",
        Vec::new(),
    )];
    store.write_state(&state).unwrap();
    let poller = BackgroundPendingPromptPoller::new(store.clone(), &state, &ipc);

    let messages = poller.poll(poll_request(
        "sess-tool-round",
        "turn-tool",
        2,
        AttachmentPollPhase::Regular,
    ));

    assert_eq!(messages.len(), 1);
    assert!(matches!(
        &messages[0].content[0],
        rebon_api::ContentBlock::Text(text) if text.text == "continue after the tool"
    ));
    assert_eq!(
        store
            .read_state(&state.identity.job_id)
            .unwrap()
            .identity
            .pending_prompts[0]
            .claimed_turn_generation,
        Some(state.process.turn_generation)
    );
    assert!(poller
        .poll(poll_request(
            "sess-tool-round",
            "turn-tool",
            3,
            AttachmentPollPhase::Eager,
        ))
        .is_empty());
    ipc.cancel.cancel();
}

#[test]
fn background_pending_prompt_poller_claims_fifo_suffix_after_primary_and_respects_fences() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 3;
    state.identity.session_id = Some("sess-fenced".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    let mut primary = pending_prompt("pp-primary", "primary", Vec::new());
    primary.claimed_turn_generation = Some(state.process.turn_generation);
    state.identity.pending_prompts = vec![
        primary,
        pending_prompt("pp-append-a", "append a", Vec::new()),
        pending_prompt("pp-append-b", "append b", Vec::new()),
    ];
    store.write_state(&state).unwrap();
    let poller = BackgroundPendingPromptPoller::new(store.clone(), &state, &ipc);

    let messages = poller.poll(poll_request(
        "sess-fenced",
        "turn-primary",
        0,
        AttachmentPollPhase::Eager,
    ));
    assert_eq!(messages.len(), 2);
    assert!(matches!(
        &messages[0].content[0],
        rebon_api::ContentBlock::Text(text) if text.text == "append a"
    ));
    assert!(matches!(
        &messages[1].content[0],
        rebon_api::ContentBlock::Text(text) if text.text == "append b"
    ));
    assert!(store
        .read_state(&state.identity.job_id)
        .unwrap()
        .identity
        .pending_prompts
        .iter()
        .all(|prompt| prompt.claimed_turn_generation == Some(state.process.turn_generation)));
    assert!(poller
        .poll(poll_request(
            "sess-fenced",
            "turn-primary",
            1,
            AttachmentPollPhase::Regular,
        ))
        .is_empty());

    store
        .update_state(&state.identity.job_id, |current| {
            current.identity.pending_prompts.push(pending_prompt(
                "pp-append-c",
                "append c",
                Vec::new(),
            ));
            Ok(())
        })
        .unwrap();
    let late = poller.poll(poll_request(
        "sess-fenced",
        "turn-primary",
        2,
        AttachmentPollPhase::Regular,
    ));
    assert_eq!(late.len(), 1);
    assert!(matches!(
        &late[0].content[0],
        rebon_api::ContentBlock::Text(text) if text.text == "append c"
    ));

    store
        .update_state(&state.identity.job_id, |current| {
            current.identity.pending_prompts.push(pending_prompt(
                "pp-stale",
                "must stay queued",
                Vec::new(),
            ));
            current.identity.session_id = Some("sess-replaced".into());
            Ok(())
        })
        .unwrap();
    assert!(poller
        .poll(poll_request(
            "sess-fenced",
            "turn-stale-session",
            3,
            AttachmentPollPhase::Regular,
        ))
        .is_empty());
    assert!(store
        .read_state(&state.identity.job_id)
        .unwrap()
        .identity
        .pending_prompts[4]
        .claimed_turn_generation
        .is_none());

    store
        .update_state(&state.identity.job_id, |current| {
            current.identity.session_id = Some("sess-fenced".into());
            current.process.turn_generation += 1;
            Ok(())
        })
        .unwrap();
    assert!(poller
        .poll(poll_request(
            "sess-fenced",
            "turn-stale-generation",
            4,
            AttachmentPollPhase::Regular,
        ))
        .is_empty());
    assert!(store
        .read_state(&state.identity.job_id)
        .unwrap()
        .identity
        .pending_prompts[4]
        .claimed_turn_generation
        .is_none());
    ipc.cancel.cancel();
}

#[tokio::test]
async fn background_steer_pump_delivers_fifo_and_publishes_echoes() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 9;
    state.identity.session_id = Some("sess-steer".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    let mut primary = pending_prompt("pp-primary-steer", "primary", Vec::new());
    primary.claimed_turn_generation = Some(state.process.turn_generation);
    let image = BackgroundImageAttachment {
        id: 41,
        data: "image-data".into(),
        media_type: "image/png".into(),
        filename: None,
        source_path: None,
    };
    state.identity.pending_prompts = vec![
        primary,
        pending_prompt("pp-steer-a", "steer a", vec![image]),
        pending_prompt("pp-steer-b", "steer b", Vec::new()),
    ];
    store.write_state(&state).unwrap();

    let poller = BackgroundPendingPromptPoller::new(store.clone(), &state, &ipc);
    let (agents, calls, mut sent) = test_steering_agents(
        vec![
            Ok(rebon_agent_core::SteerOutcome::Injected),
            Ok(rebon_agent_core::SteerOutcome::Injected),
        ],
        "sess-steer",
        None,
    );
    let (publisher, mut update_rx) = rebon_agent_core::ChannelSessionUpdatePublisher::new();
    let publisher: Arc<dyn rebon_agent_core::SessionUpdatePublisher> = Arc::new(publisher);
    let pump = spawn_background_steer_pump(agents, poller, publisher, "sess-steer".into());

    for expected in ["pp-steer-a", "pp-steer-b"] {
        let delivered = tokio::time::timeout(Duration::from_secs(2), sent.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivered, expected);
    }
    pump.stop().await;

    assert_eq!(
        *calls.lock().unwrap(),
        vec![
            ("pp-steer-a".into(), "steer a".into(), 1),
            ("pp-steer-b".into(), "steer b".into(), 0),
        ]
    );
    let mut echoed = Vec::new();
    for _ in 0..2 {
        let update = tokio::time::timeout(Duration::from_secs(2), update_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let rebon_types::SessionUpdate::QueuedUserMessage { uuid, .. } = update.update else {
            panic!("expected queued user message echo");
        };
        echoed.push(uuid);
    }
    assert_eq!(echoed, vec!["pp-steer-a", "pp-steer-b"]);
    assert!(store
        .read_state(&state.identity.job_id)
        .unwrap()
        .identity
        .pending_prompts
        .iter()
        .all(|prompt| prompt.claimed_turn_generation == Some(state.process.turn_generation)));
    ipc.cancel.cancel();
}

/// A message from another agent is steered as the framed text, but it is not a
/// queued user row: nothing a person typed produced it.
#[tokio::test]
async fn background_steer_pump_does_not_echo_an_agent_message_as_user_input() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 3;
    state.identity.session_id = Some("sess-steer-agent".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    let envelope = rebon_session_host::AgentMessageEnvelope {
        message_id: "m-steer".into(),
        from_session_id: "sess-other".into(),
        from_agent: "main".into(),
        from_cwd: None,
        from_handle: None,
        from_name: None,
        summary: None,
        text: "take the next step".into(),
        hops: 0,
    };
    let frame = rebon_session_host::frame_agent_message(&envelope);
    let mut prompt = pending_prompt("m-steer", &frame, Vec::new());
    prompt.origin = Some(rebon_session_host::PendingPromptOrigin::AgentMessage {
        from_session_id: "sess-other".into(),
        from_agent: "main".into(),
        from_handle: None,
        from_name: None,
        hops: 0,
    });
    state.identity.pending_prompts = vec![prompt];
    store.write_state(&state).unwrap();

    let poller = BackgroundPendingPromptPoller::new(store.clone(), &state, &ipc);
    let (agents, calls, mut sent) = test_steering_agents(
        vec![Ok(rebon_agent_core::SteerOutcome::Injected)],
        "sess-steer-agent",
        None,
    );
    let (publisher, mut update_rx) = rebon_agent_core::ChannelSessionUpdatePublisher::new();
    let publisher: Arc<dyn rebon_agent_core::SessionUpdatePublisher> = Arc::new(publisher);
    let pump = spawn_background_steer_pump(agents, poller, publisher, "sess-steer-agent".into());

    let delivered = tokio::time::timeout(Duration::from_secs(2), sent.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivered, "m-steer");
    pump.stop().await;

    assert_eq!(
        *calls.lock().unwrap(),
        vec![("m-steer".to_string(), frame, 0)]
    );
    assert!(
        update_rx.try_recv().is_err(),
        "a teammate message must not be published as a queued user message"
    );
    ipc.cancel.cancel();
}

/// An agent message steered into a running external-agent turn makes that turn
/// the one answering its sender — when it arrived during the turn. One that was
/// already waiting when the turn was claimed is steered in too, but only rides
/// along: it was not what the turn was started for, and lends it no depth.
#[tokio::test]
async fn background_steer_pump_records_the_depth_of_a_message_sent_during_the_turn() {
    fn agent_prompt(id: &str, from: &str, hops: u8) -> PendingPrompt {
        let mut prompt = pending_prompt(
            id,
            &format!("<teammate-message>{id}</teammate-message>"),
            Vec::new(),
        );
        prompt.origin = Some(rebon_session_host::PendingPromptOrigin::AgentMessage {
            from_session_id: from.into(),
            from_agent: "main".into(),
            from_handle: None,
            from_name: None,
            hops,
        });
        prompt
    }

    let session_id = "sess-steer-hops";
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 3;
    state.identity.session_id = Some(session_id.into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    // Waiting before the turn was claimed: a passenger.
    state.identity.pending_prompts = vec![agent_prompt("m-passenger", "sess-passenger", 3)];
    store.write_state(&state).unwrap();
    let poller = BackgroundPendingPromptPoller::new(store.clone(), &state, &ipc);
    // Sent while the turn runs.
    store
        .update_state(&state.identity.job_id, |current| {
            current
                .identity
                .pending_prompts
                .push(agent_prompt("m-during", "sess-during", 1));
            Ok(())
        })
        .unwrap();

    let mid_turn_queue = crate::mid_turn_queue::MidTurnQueuedSubmitPoller::new(session_id);
    let _turn = mid_turn_queue.begin_turn();
    let (agents, _calls, mut sent) = test_steering_agents(
        vec![
            Ok(rebon_agent_core::SteerOutcome::Injected),
            Ok(rebon_agent_core::SteerOutcome::Injected),
        ],
        session_id,
        None,
    );
    let (publisher, _update_rx) = rebon_agent_core::ChannelSessionUpdatePublisher::new();
    let publisher: Arc<dyn rebon_agent_core::SessionUpdatePublisher> = Arc::new(publisher);
    let pump = spawn_background_steer_pump(agents, poller, publisher, session_id.into());
    for expected in ["m-passenger", "m-during"] {
        let delivered = tokio::time::timeout(Duration::from_secs(2), sent.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivered, expected);
    }
    pump.stop().await;

    let ledger = rebon_tool::peer::session_reply_ledger(session_id);
    assert_eq!(
        ledger.reply_hops("sess-during"),
        2,
        "the turn read a message sent during it, so it answers that sender"
    );
    assert_eq!(
        ledger.reply_hops("sess-passenger"),
        0,
        "a message that was only riding along is not what the turn answers"
    );
    ipc.cancel.cancel();
}

#[tokio::test]
async fn background_steer_pump_releases_undelivered_claims() {
    let outcomes = [
        Ok(rebon_agent_core::SteerOutcome::TurnAlreadyOver),
        Err(rebon_agent_core::AgentBackendError::Unsupported(
            "steering disabled".into(),
        )),
    ];
    for (index, outcome) in outcomes.into_iter().enumerate() {
        let (_dir, store) = store();
        let session_id = format!("sess-steer-fallback-{index}");
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Running;
        state.process.turn_generation = 3;
        state.identity.session_id = Some(session_id.clone());
        let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
        install_ipc_owner(&mut state, &ipc);
        state.identity.pending_prompts = vec![pending_prompt(
            &format!("pp-steer-fallback-{index}"),
            "send next turn",
            Vec::new(),
        )];
        store.write_state(&state).unwrap();

        let poller = BackgroundPendingPromptPoller::new(store.clone(), &state, &ipc);
        let (agents, calls, mut sent) = test_steering_agents(vec![outcome], &session_id, None);
        let (publisher, mut update_rx) = rebon_agent_core::ChannelSessionUpdatePublisher::new();
        let publisher: Arc<dyn rebon_agent_core::SessionUpdatePublisher> = Arc::new(publisher);
        let pump = spawn_background_steer_pump(agents, poller, publisher, session_id.clone());

        tokio::time::timeout(Duration::from_secs(2), sent.recv())
            .await
            .unwrap()
            .unwrap();
        pump.stop().await;

        assert_eq!(calls.lock().unwrap().len(), 1);
        assert!(store
            .read_state(&state.identity.job_id)
            .unwrap()
            .identity
            .pending_prompts[0]
            .claimed_turn_generation
            .is_none());
        assert!(update_rx.try_recv().is_err());
        ipc.cancel.cancel();
    }
}

#[tokio::test]
async fn background_steer_pump_waits_for_in_flight_result_before_stopping() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 4;
    state.identity.session_id = Some("sess-steer-stop".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    state.identity.pending_prompts =
        vec![pending_prompt("pp-steer-stop", "deliver once", Vec::new())];
    store.write_state(&state).unwrap();

    let gate = Arc::new(tokio::sync::Notify::new());
    let poller = BackgroundPendingPromptPoller::new(store.clone(), &state, &ipc);
    let (agents, _, mut sent) = test_steering_agents(
        vec![Ok(rebon_agent_core::SteerOutcome::Injected)],
        "sess-steer-stop",
        Some(gate.clone()),
    );
    let (publisher, mut update_rx) = rebon_agent_core::ChannelSessionUpdatePublisher::new();
    let publisher: Arc<dyn rebon_agent_core::SessionUpdatePublisher> = Arc::new(publisher);
    let pump = spawn_background_steer_pump(agents, poller, publisher, "sess-steer-stop".into());

    tokio::time::timeout(Duration::from_secs(2), sent.recv())
        .await
        .unwrap()
        .unwrap();
    let stop = tokio::spawn(pump.stop());
    tokio::task::yield_now().await;
    assert!(!stop.is_finished());
    assert_eq!(
        store
            .read_state(&state.identity.job_id)
            .unwrap()
            .identity
            .pending_prompts[0]
            .claimed_turn_generation,
        Some(state.process.turn_generation)
    );

    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(2), stop)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        store
            .read_state(&state.identity.job_id)
            .unwrap()
            .identity
            .pending_prompts[0]
            .claimed_turn_generation,
        Some(state.process.turn_generation)
    );
    let update = tokio::time::timeout(Duration::from_secs(2), update_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        update.update,
        rebon_types::SessionUpdate::QueuedUserMessage { uuid, .. }
            if uuid == "pp-steer-stop"
    ));
    ipc.cancel.cancel();
}

#[test]
fn claim_background_job_keeps_pending_prompt_persisted_until_terminal() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let image = BackgroundImageAttachment {
        id: 3,
        data: "image-data".into(),
        media_type: "image/jpeg".into(),
        filename: None,
        source_path: None,
    };
    store
        .update_state(&job.identity.job_id, |current| {
            current.identity.pending_prompts = vec![pending_prompt(
                "pp-claim-persisted",
                "follow up",
                vec![image.clone()],
            )];
            Ok(())
        })
        .unwrap();

    let WorkerClaim::Claimed(claimed) =
        claim_background_job(&store, &job.identity.job_id, 4321, 7777, "tok", |_| {
            Some(false)
        })
        .unwrap()
    else {
        panic!("expected claimed job");
    };
    assert_eq!(pending_text(&claimed), Some("follow up"));
    assert_eq!(
        claimed.identity.pending_prompts[0].images,
        vec![image.clone()]
    );
    assert_eq!(
        claimed.identity.pending_prompts[0].claimed_turn_generation,
        Some(claimed.process.turn_generation)
    );

    // A crash between claim and completion must not lose the queued
    // reply: it stays persisted until a terminal transition clears it.
    let persisted = store.read_state(&job.identity.job_id).unwrap();
    assert_eq!(pending_text(&persisted), Some("follow up"));
    assert_eq!(persisted.identity.pending_prompts[0].images, vec![image]);
}

#[test]
fn no_image_followup_does_not_reuse_initial_prompt_images() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("initial prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state
        .identity
        .prompt_images
        .push(BackgroundImageAttachment {
            id: 1,
            data: "initial-image".into(),
            media_type: "image/png".into(),
            filename: None,
            source_path: None,
        });
    state.process.turn_generation = 1;
    state.identity.pending_prompts = vec![pending_prompt(
        "pp-image-selection",
        "text-only follow up",
        Vec::new(),
    )];
    state.identity.pending_prompts[0].claimed_turn_generation = Some(1);
    assert!(background_prompt_images_for_execution(&state).is_empty());

    state.identity.pending_prompts[0]
        .images
        .push(BackgroundImageAttachment {
            id: 2,
            data: "followup-image".into(),
            media_type: "image/jpeg".into(),
            filename: None,
            source_path: None,
        });
    assert_eq!(background_prompt_images_for_execution(&state)[0].id, 2);
}

#[test]
fn claimed_internal_followup_preserves_coordinator_report_paths() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("initial prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.turn_generation = 1;
    let mut prompt = pending_prompt(
        "u-internal-task-notification-0001",
        "<task-notification />",
        Vec::new(),
    );
    prompt.coordinator_report_paths = vec!["C:/reports/worker.md".into()];
    prompt.claimed_turn_generation = Some(1);
    state.identity.pending_prompts = vec![prompt];

    assert_eq!(
        background_prompt_coordinator_report_paths(&state),
        vec!["C:/reports/worker.md"]
    );
    assert!(claimed_pending_prompt(&state)
        .unwrap()
        .id
        .starts_with("u-internal-"));
}

/// A turn starts with the job's whole grant list, not just the prompt
/// it is claiming: the coordinator keeps Read access to reports from
/// earlier turns, whose prompts were long since drained.
#[test]
fn turn_start_allowlist_includes_earlier_granted_reports() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("initial prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.turn_generation = 2;
    state.identity.coordinator_report_grants = vec![
        "C:/reports/earlier.md".into(),
        "C:/reports/current.md".into(),
    ];
    let mut prompt = pending_prompt(
        "u-internal-task-notification-0002",
        "<task-notification />",
        Vec::new(),
    );
    prompt.coordinator_report_paths = vec!["C:/reports/current.md".into()];
    prompt.claimed_turn_generation = Some(2);
    state.identity.pending_prompts = vec![prompt];

    assert_eq!(
        background_prompt_coordinator_report_paths(&state),
        vec!["C:/reports/earlier.md", "C:/reports/current.md"],
        "the claimed prompt's path must not be duplicated, and the earlier grant must survive"
    );
}

#[test]
fn pending_prompt_transcript_state_requires_terminal_assistant_response() {
    let prompt_id = "pp-partial-turn";
    let user = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({"message": {"role": "user", "content": "continue"}}),
        )
        .with_uuid(prompt_id),
    );
    let tool_use = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "assistant",
            serde_json::json!({
                "message": {
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "tool-1", "name": "Read", "input": {}}],
                    "stop_reason": "tool_use"
                }
            }),
        )
        .with_uuid("a-tool-use")
        .with_parent(prompt_id),
    );
    let tool_result = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({
                "message": {
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "tool-1", "content": "ok"}]
                }
            }),
        )
        .with_uuid("u-tool-result")
        .with_parent("a-tool-use"),
    );
    let visible_answer = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({
                "message": {"role": "user", "content": "approved"},
                "isVisibleInTranscriptOnly": true
            }),
        )
        .with_uuid("u-visible-answer")
        .with_parent("u-tool-result"),
    );
    let terminal = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "assistant",
            serde_json::json!({
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "done"}],
                    "stop_reason": "end_turn"
                }
            }),
        )
        .with_uuid("a-terminal")
        .with_parent("u-visible-answer"),
    );

    let partial = vec![user, tool_use, tool_result, visible_answer];
    assert_eq!(
        pending_prompt_transcript_state_in_messages(&partial, prompt_id),
        PendingPromptTranscriptState::ResumeSavedUser
    );
    let mut completed = partial;
    completed.push(terminal);
    assert_eq!(
        pending_prompt_transcript_state_in_messages(&completed, prompt_id),
        PendingPromptTranscriptState::Completed
    );
}

#[test]
fn pending_prompt_transcript_state_persists_missing_tool_results_once() {
    let prompt_id = "pp-interrupted-tools";
    let user = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({"message": {"role": "user", "content": "continue"}}),
        )
        .with_uuid(prompt_id),
    );
    let tool_use = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "assistant",
            serde_json::json!({
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "tool-1", "name": "Read", "input": {}},
                        {"type": "tool_use", "id": "tool-2", "name": "Bash", "input": {}}
                    ],
                    "stop_reason": "tool_use"
                }
            }),
        )
        .with_uuid("a-interrupted-tools")
        .with_parent(prompt_id),
    );
    let mut messages = vec![user, tool_use];
    let state = pending_prompt_transcript_state_in_messages(&messages, prompt_id);
    assert_eq!(
        state,
        PendingPromptTranscriptState::ResumeInterruptedToolUse {
            assistant_uuid: "a-interrupted-tools".into(),
            parent_uuid: "a-interrupted-tools".into(),
            missing_tool_uses: vec![
                ("tool-1".into(), "Read".into()),
                ("tool-2".into(), "Bash".into()),
            ],
        }
    );

    let repaired = rebon_session::finalize_transcript_entry(
        &interrupted_tool_result_entry(&state).expect("interrupted turn must produce a repair row"),
    );
    assert_eq!(repaired.parent_uuid.as_deref(), Some("a-interrupted-tools"));
    let blocks = repaired.raw["message"]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert!(blocks.iter().all(|block| block["is_error"] == true));
    messages.push(repaired);
    let replayed = rebon_core::query::transcript_to_api_messages(&messages);
    assert_eq!(replayed.last().unwrap().role, rebon_api::Role::User);
    assert!(replayed.last().unwrap().content.iter().all(|block| {
        matches!(block, rebon_api::ContentBlock::ToolResult(result) if result.is_error)
    }));

    let resumed = pending_prompt_transcript_state_in_messages(&messages, prompt_id);
    assert_eq!(resumed, PendingPromptTranscriptState::ResumeSavedUser);
    assert!(interrupted_tool_result_entry(&resumed).is_none());
}

/// A group member's background turn: the `groups` plugin hands it new group
/// entries mid-turn as runtime attachments, written as queued user rows. They
/// are part of the turn they arrived in, not the start of another, so the
/// turn's last answer still completes the claimed prompt.
#[test]
fn pending_prompt_transcript_state_reads_past_mid_turn_runtime_attachments() {
    let prompt_id = "pp-group-request";
    let user = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({"message": {"role": "user", "content": "(r4) do the work"}}),
        )
        .with_uuid(prompt_id),
    );
    let attachment = |uuid: &str, parent: &str| {
        rebon_session::finalize_transcript_entry(
            &rebon_session::TranscriptWriteEntry::new(
                "user",
                serde_json::json!({
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": "Group g: 1 new — Cleo"}]
                    },
                    "modelContent": [{"type": "text", "text": "<system-reminder>…</system-reminder>"}],
                    "queuedCommand": true,
                    "runtimeAttachment": true
                }),
            )
            .with_uuid(uuid)
            .with_parent(parent),
        )
    };
    let tool_use = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "assistant",
            serde_json::json!({
                "message": {
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "tool-1", "name": "Read", "input": {}}],
                    "stop_reason": "tool_use"
                }
            }),
        )
        .with_uuid("a-tool-use")
        .with_parent("u-group-attachment-1"),
    );
    let tool_result = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({
                "message": {
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "tool-1", "content": "ok"}]
                }
            }),
        )
        .with_uuid("u-tool-result")
        .with_parent("a-tool-use"),
    );
    let terminal = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "assistant",
            serde_json::json!({
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "reported to the group"}],
                    "stop_reason": "end_turn"
                }
            }),
        )
        .with_uuid("a-terminal")
        .with_parent("u-group-attachment-2"),
    );

    // Right after the prompt, and again between a tool result and the answer.
    let partial = vec![
        user,
        attachment("u-group-attachment-1", prompt_id),
        tool_use,
        tool_result,
        attachment("u-group-attachment-2", "u-tool-result"),
    ];
    assert_eq!(
        pending_prompt_transcript_state_in_messages(&partial, prompt_id),
        PendingPromptTranscriptState::ResumeSavedUser,
        "a turn the attachments interrupt is still unfinished"
    );
    let mut completed = partial;
    completed.push(terminal);
    assert_eq!(
        pending_prompt_transcript_state_in_messages(&completed, prompt_id),
        PendingPromptTranscriptState::Completed
    );

    // A queued message the user typed is still a turn of its own.
    let typed = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({
                "message": {"role": "user", "content": "also check the docs"},
                "queuedCommand": true
            }),
        )
        .with_uuid("u-typed")
        .with_parent(prompt_id),
    );
    assert!(transcript_user_entry_is_turn_boundary(&typed));
}

/// A hosted turn another session's message started opens with that message's
/// row: named for the message and marked as a runtime attachment, so it is not
/// read as the person's words. It still opens its turn — unlike the same
/// message queued into a running turn — so the turn before it ends there and
/// the durability check finds the claimed prompt by that name.
#[test]
fn a_turn_another_session_started_opens_with_the_message_row_and_is_found_by_it() {
    let frame = "<teammate-message teammate_id=\"main@session:sess-sender\" from_session=\"sess-sender\">\ngo\n</teammate-message>";
    let mut head = pending_prompt("m-opened", frame, Vec::new());
    head.origin = Some(rebon_session_host::PendingPromptOrigin::AgentMessage {
        from_session_id: "sess-sender".into(),
        from_agent: "main".into(),
        from_handle: None,
        from_name: None,
        hops: 0,
    });
    assert_eq!(pending_prompt_row_uuid(&head), "u-agent-message-m-opened");
    assert_eq!(
        pending_prompt_row_uuid(&pending_prompt("pp-typed", "typed", Vec::new())),
        "pp-typed",
        "a prompt the person typed keeps its own id"
    );

    // The shape the executor persists: one text block, written as a string.
    let opening = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({
                "message": {"role": "user", "content": frame},
                "runtimeAttachment": true
            }),
        )
        .with_uuid("u-agent-message-m-opened"),
    );
    // The same message queued into a running turn: marker stripped, one block.
    let queued = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({
                "message": {"role": "user", "content": [{"type": "text", "text": frame}]},
                "queuedCommand": true,
                "iteration": 1,
                "runtimeAttachment": true
            }),
        )
        .with_uuid("u-agent-message-m-queued"),
    );
    assert!(transcript_user_entry_is_turn_boundary(&opening));
    assert!(!transcript_user_entry_is_turn_boundary(&queued));

    let terminal = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "assistant",
            serde_json::json!({
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "answered"}],
                    "stop_reason": "end_turn"
                }
            }),
        )
        .with_uuid("a-opened-terminal")
        .with_parent("u-agent-message-m-opened"),
    );
    head.claimed_turn_generation = Some(4);
    assert_eq!(
        claimed_prompts_transcript_state_in_messages(
            std::slice::from_ref(&opening),
            std::slice::from_ref(&head)
        ),
        PendingPromptTranscriptState::ResumeSavedUser
    );
    assert_eq!(
        claimed_prompts_transcript_state_in_messages(
            &[opening, terminal],
            std::slice::from_ref(&head)
        ),
        PendingPromptTranscriptState::Completed
    );

    // A row written before it was named for the message is still found.
    let legacy = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({"message": {"role": "user", "content": frame}}),
        )
        .with_uuid("m-opened"),
    );
    assert_eq!(
        claimed_prompts_transcript_state_in_messages(&[legacy], &[head]),
        PendingPromptTranscriptState::ResumeSavedUser
    );
}

#[test]
fn pending_prompt_transcript_state_ignores_prompt_on_noncanonical_branch() {
    let prompt = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({"message": {"role": "user", "content": "stale"}}),
        )
        .with_uuid("pp-stale-branch")
        .with_timestamp("2026-01-01T00:00:00.000Z"),
    );
    let stale_answer = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "assistant",
            serde_json::json!({
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "stale answer"}],
                    "stop_reason": "end_turn"
                }
            }),
        )
        .with_uuid("a-stale-branch")
        .with_parent("pp-stale-branch")
        .with_timestamp("2026-01-01T00:00:01.000Z"),
    );
    let canonical = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({"message": {"role": "user", "content": "current"}}),
        )
        .with_uuid("u-current-branch")
        .with_timestamp("2026-01-01T00:00:02.000Z"),
    );

    assert_eq!(
        pending_prompt_transcript_state_in_messages(
            &[prompt, stale_answer, canonical],
            "pp-stale-branch"
        ),
        PendingPromptTranscriptState::Missing
    );
}

#[test]
fn pending_prompt_transcript_state_tracks_a_mid_turn_claimed_batch() {
    let first = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({"message": {"role": "user", "content": "first"}}),
        )
        .with_uuid("pp-batch-a"),
    );
    let second = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({"message": {"role": "user", "content": "second"}}),
        )
        .with_uuid("pp-batch-b")
        .with_parent("pp-batch-a"),
    );
    let terminal = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "assistant",
            serde_json::json!({
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "done"}],
                    "stop_reason": "end_turn"
                }
            }),
        )
        .with_uuid("a-batch-terminal")
        .with_parent("pp-batch-b"),
    );
    let messages = vec![first, second, terminal];

    assert_eq!(
        pending_prompt_transcript_state_in_messages(&messages, "pp-batch-a"),
        PendingPromptTranscriptState::ResumeSavedUser
    );
    assert_eq!(
        pending_prompts_transcript_state_in_messages(&messages, &["pp-batch-a", "pp-batch-b"]),
        PendingPromptTranscriptState::Completed
    );
    assert_eq!(
        pending_prompts_transcript_state_in_messages(
            &messages,
            &["pp-batch-a", "pp-batch-missing"]
        ),
        PendingPromptTranscriptState::ResumeSavedUser
    );
}

#[test]
fn pending_prompt_completion_marker_does_not_replace_transcript_evidence() {
    let transcript_dir = tempfile::tempdir().unwrap();
    let mut state = BackgroundJobState::new(
        "prompt".into(),
        transcript_dir.path().to_string_lossy().into_owned(),
        runtime(),
        None,
    );
    state.identity.session_id = Some("sess-unverified-completion-marker".into());
    state.process.turn_generation = 7;
    let mut prompt = pending_prompt("pp-unverified-completion", "retry me", Vec::new());
    prompt.claimed_turn_generation = Some(5);
    prompt.completed_turn_generation = Some(3);
    state.identity.pending_prompts = vec![prompt];

    assert_eq!(
        pending_prompt_transcript_state(&state),
        PendingPromptTranscriptState::Missing
    );
}

#[test]
fn claimed_scheduled_batch_requires_every_durable_row() {
    let ids = [
        "u-internal-scheduled-router-batch-0",
        "u-internal-scheduled-router-batch-1",
        "u-internal-scheduled-router-batch-2",
    ];
    for typed_head in [false, true] {
        let claimed = ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
                let mut prompt = pending_prompt(id, "continue", Vec::new());
                if index != 0 || !typed_head {
                    prompt.origin = Some(rebon_session_host::PendingPromptOrigin::Scheduled);
                }
                prompt.claimed_turn_generation = Some(4);
                prompt.completed_turn_generation = Some(4);
                prompt
            })
            .collect::<Vec<_>>();
        for missing in [None, Some(0), Some(1), Some(2)] {
            let mut messages = Vec::new();
            let mut parent = None;
            for (index, id) in ids.iter().enumerate() {
                if missing == Some(index) {
                    continue;
                }
                messages.push(scheduled_injection_row(id, parent, index as u64));
                parent = Some(*id);
            }
            messages.push(assistant_row("a-final", parent.unwrap(), "end_turn"));
            let expected = match missing {
                None => PendingPromptTranscriptState::Completed,
                Some(0) => PendingPromptTranscriptState::Missing,
                Some(_) => PendingPromptTranscriptState::ResumeSavedUser,
            };
            assert_eq!(
                claimed_prompts_transcript_state_in_messages(&messages, &claimed),
                expected,
                "typed_head={typed_head}, missing={missing:?}"
            );
        }
    }
}

#[test]
fn claimed_scheduled_batch_requires_a_response_after_the_last_continuation() {
    let ids = [
        "u-internal-scheduled-router-before-response",
        "u-internal-scheduled-router-after-response",
    ];
    let claimed = ids.map(|id| {
        let mut prompt = pending_prompt(id, "continue", Vec::new());
        prompt.origin = Some(rebon_session_host::PendingPromptOrigin::Scheduled);
        prompt.claimed_turn_generation = Some(9);
        prompt
    });
    let mut messages = vec![
        scheduled_injection_row(ids[0], None, 0),
        assistant_row("a-earlier", ids[0], "end_turn"),
        scheduled_injection_row(ids[1], Some("a-earlier"), 1),
    ];
    assert_eq!(
        claimed_prompts_transcript_state_in_messages(&messages, &claimed),
        PendingPromptTranscriptState::ResumeSavedUser
    );
    messages.push(assistant_row("a-resumed", ids[1], "end_turn"));
    assert_eq!(
        claimed_prompts_transcript_state_in_messages(&messages, &claimed),
        PendingPromptTranscriptState::Completed
    );
    let mut reclaimed = claimed;
    for prompt in &mut reclaimed {
        prompt.claimed_turn_generation = Some(10);
    }
    assert_eq!(
        claimed_prompts_transcript_state_in_messages(&messages, &reclaimed),
        PendingPromptTranscriptState::Completed
    );
}

#[test]
fn claimed_scheduled_batch_requires_rows_in_claim_order() {
    let ids = [
        "u-internal-scheduled-router-first",
        "u-internal-scheduled-router-second",
        "u-internal-scheduled-router-third",
    ];
    let claimed = ids.map(|id| {
        let mut prompt = pending_prompt(id, "continue", Vec::new());
        prompt.origin = Some(rebon_session_host::PendingPromptOrigin::Scheduled);
        prompt.claimed_turn_generation = Some(2);
        prompt
    });
    let messages = vec![
        scheduled_injection_row(ids[0], None, 0),
        scheduled_injection_row(ids[2], Some(ids[0]), 1),
        scheduled_injection_row(ids[1], Some(ids[2]), 2),
        assistant_row("a-final", ids[1], "end_turn"),
    ];
    assert_eq!(
        claimed_prompts_transcript_state_in_messages(&messages, &claimed),
        PendingPromptTranscriptState::ResumeSavedUser
    );
}

/// A scheduled prompt — a cron fire, or the router's coordination message —
/// is injected mid-turn. The row that injection writes is named for the prompt,
/// so the turn that answered it can prove it did: this is the shape the
/// executor produces for `pending_prompt_attachment_message`.
fn scheduled_injection_row(
    prompt_id: &str,
    parent: Option<&str>,
    iteration: u64,
) -> rebon_session::TranscriptEntry {
    let mut entry = rebon_session::TranscriptWriteEntry::new(
        "user",
        serde_json::json!({
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": "[Scheduled task: delivered by Rebon's scheduler]"}],
            },
            "isMeta": true,
            "queuedCommand": true,
            "runtimeAttachment": true,
            "iteration": iteration,
        }),
    )
    .with_uuid(prompt_id);
    if let Some(parent) = parent {
        entry = entry.with_parent(parent);
    }
    rebon_session::finalize_transcript_entry(&entry)
}

fn assistant_row(uuid: &str, parent: &str, stop_reason: &str) -> rebon_session::TranscriptEntry {
    rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "assistant",
            serde_json::json!({
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "coordination received"}],
                    "stop_reason": stop_reason
                }
            }),
        )
        .with_uuid(uuid)
        .with_parent(parent),
    )
}

/// The field failure this fixes: a scheduled prompt was claimed, injected,
/// answered and finished, and the job was still failed with "no durable user
/// row for the claimed prompt". The prompt's row existed only under an
/// anonymous name, so the check could not find it.
#[test]
fn a_scheduled_prompt_answered_mid_turn_is_found_by_the_row_its_injection_wrote() {
    let prompt_id = "u-internal-scheduled-router-1793698808520-2";
    let mut head = pending_prompt(prompt_id, "[Scheduled task] continue", Vec::new());
    head.origin = Some(rebon_session_host::PendingPromptOrigin::Scheduled);
    head.claimed_turn_generation = Some(1);

    let prompt_row = scheduled_injection_row(prompt_id, Some("u-user-turn-head"), 23);
    let terminal = assistant_row("a-answered", prompt_id, "end_turn");

    // Mid-turn: the row is durable but the turn has not answered yet.
    assert_eq!(
        claimed_prompts_transcript_state_in_messages(
            std::slice::from_ref(&prompt_row),
            std::slice::from_ref(&head)
        ),
        PendingPromptTranscriptState::ResumeSavedUser
    );
    assert_eq!(
        claimed_prompts_transcript_state_in_messages(
            &[prompt_row, terminal],
            std::slice::from_ref(&head)
        ),
        PendingPromptTranscriptState::Completed,
        "the claimed prompt's row is durable and the turn after it answered"
    );
}

/// The same turn with many continuations: each injection writes its own named
/// row, chained behind the last, so every one of them is durable and the turn
/// reads as completed once it answers.
#[test]
fn several_scheduled_continuations_each_keep_their_own_durable_row() {
    let ids = [
        "u-internal-scheduled-router-1-0",
        "u-internal-scheduled-router-1-1",
        "u-internal-scheduled-router-1-2",
    ];
    let head_row = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({"message": {"role": "user", "content": "do the work"}}),
        )
        .with_uuid("u-turn-head"),
    );
    let mut messages = vec![head_row];

    let mut parent = "u-turn-head".to_string();
    for (iteration, id) in ids.iter().enumerate() {
        let row = scheduled_injection_row(id, Some(&parent), iteration as u64);
        messages.push(row);
        parent = id.to_string();
    }
    // Every continuation is durable under its own name, in arrival order.
    for id in ids {
        assert_eq!(
            pending_prompt_transcript_state_in_messages(&messages, id),
            PendingPromptTranscriptState::ResumeSavedUser,
            "{id} is durable and still unanswered"
        );
    }

    messages.push(assistant_row("a-final", &parent, "end_turn"));
    for id in ids {
        assert_eq!(
            pending_prompt_transcript_state_in_messages(&messages, id),
            PendingPromptTranscriptState::Completed,
            "{id} was answered by the turn it joined"
        );
    }
}

/// A prompt appended after the turn already produced tool rounds is still
/// findable, and the turn it joined is reported finished only once it ends.
#[test]
fn a_continuation_appended_after_a_tool_round_is_answered_by_the_same_turn() {
    let prompt_id = "u-internal-scheduled-rework-1";
    let mut head = pending_prompt(prompt_id, "[Scheduled task] keep going", Vec::new());
    head.origin = Some(rebon_session_host::PendingPromptOrigin::Scheduled);
    head.claimed_turn_generation = Some(3);

    let head_row = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({"message": {"role": "user", "content": "start"}}),
        )
        .with_uuid("u-turn-head"),
    );
    let tool_use = assistant_row("a-tool-use", "u-turn-head", "tool_use");
    let tool_result = rebon_session::finalize_transcript_entry(
        &rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({
                "message": {
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "call-1", "content": "ok"}]
                }
            }),
        )
        .with_uuid("u-tool-result")
        .with_parent("a-tool-use"),
    );
    let appended = scheduled_injection_row(prompt_id, Some("u-tool-result"), 2);

    let partial = vec![head_row, tool_use, tool_result, appended];
    assert_eq!(
        claimed_prompts_transcript_state_in_messages(&partial, std::slice::from_ref(&head)),
        PendingPromptTranscriptState::ResumeSavedUser,
        "the tool round did not end the turn"
    );
    let mut completed = partial;
    completed.push(assistant_row("a-final", prompt_id, "end_turn"));
    assert_eq!(
        claimed_prompts_transcript_state_in_messages(&completed, std::slice::from_ref(&head)),
        PendingPromptTranscriptState::Completed
    );
}

/// The old broken shape is still a failure. A scheduled injection that left
/// only an anonymous `u-attach-…` row gives the check nothing to find, and it
/// must keep saying so rather than accepting a turn whose prompt has no
/// durable record.
#[test]
fn a_scheduled_prompt_with_only_an_anonymous_row_is_still_not_durable() {
    let prompt_id = "u-internal-scheduled-router-1793698808520-2";
    let mut head = pending_prompt(prompt_id, "[Scheduled task] continue", Vec::new());
    head.origin = Some(rebon_session_host::PendingPromptOrigin::Scheduled);
    head.claimed_turn_generation = Some(1);

    let anonymous = scheduled_injection_row("u-attach-sess-23-1793698828646498800", None, 23);
    let terminal = assistant_row(
        "a-answered",
        "u-attach-sess-23-1793698828646498800",
        "end_turn",
    );

    assert_eq!(
        claimed_prompts_transcript_state_in_messages(
            &[anonymous, terminal],
            std::slice::from_ref(&head)
        ),
        PendingPromptTranscriptState::Missing,
        "a row under another name is not the claimed prompt's durable row"
    );
}

/// A continuation interrupted before it answered resumes instead of being run
/// again from scratch: its row is durable, so the next owner continues the
/// turn rather than duplicating the prompt.
#[test]
fn an_interrupted_continuation_resumes_the_saved_row_instead_of_duplicating_it() {
    let prompt_id = "u-internal-scheduled-wakeup-1";
    let mut head = pending_prompt(prompt_id, "[Scheduled task] wake", Vec::new());
    head.origin = Some(rebon_session_host::PendingPromptOrigin::Scheduled);
    head.claimed_turn_generation = Some(2);

    let interrupted = scheduled_injection_row(prompt_id, Some("u-turn-head"), 1);
    assert_eq!(
        claimed_prompts_transcript_state_in_messages(
            std::slice::from_ref(&interrupted),
            std::slice::from_ref(&head)
        ),
        PendingPromptTranscriptState::ResumeSavedUser,
        "durable prompt row, no answer yet: resume it"
    );
    assert!(
        interrupted_tool_result_entry(&PendingPromptTranscriptState::ResumeSavedUser).is_none(),
        "a resume with a sound transcript needs no repair row"
    );
}
