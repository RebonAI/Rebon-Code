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
