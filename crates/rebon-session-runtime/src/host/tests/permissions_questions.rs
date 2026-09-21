use super::super::*;
use super::support::*;
use rebon_core::permission::{
    ChannelPermissionBroker, PermissionOptionKind, PermissionQueryOption,
};
use rebon_session_host::{BackgroundPermissionOptionSnapshot, BackgroundPermissionQuerySnapshot};

#[test]
fn a_running_turn_refuses_only_a_permission_retry() {
    let retry = ["retry".to_string()];
    let refusal = permission_retry_blocked_by_running_turn("/permissions", &retry)
        .expect("a running turn refuses a permission retry");
    assert_eq!(refusal.tone, "warning");
    assert!(permission_retry_blocked_by_running_turn("permissions", &retry).is_some());
    assert!(permission_retry_blocked_by_running_turn(" /Permissions ", &retry).is_some());
    // Everything else stays servable mid-turn.
    assert!(permission_retry_blocked_by_running_turn("/permissions", &[]).is_none());
    assert!(
        permission_retry_blocked_by_running_turn("/permissions", &["list".to_string()]).is_none()
    );
    assert!(permission_retry_blocked_by_running_turn("/context", &retry).is_none());
}

#[test]
fn permission_query_ids_are_scoped_by_endpoint_token() {
    let first = permission_query_id_seed("0123456789abcdef0000000000000000");
    let second = permission_query_id_seed("fedcba98765432100000000000000000");
    assert_ne!(first, second);
    assert_ne!(first, 0);
    assert_ne!(second, 0);
}

#[test]
fn terminal_permission_cancel_does_not_report_parent_turn_cancelled() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 1;
    state.identity.session_id = Some("sess-terminal-permission-cancel".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    let turn_cancel = ipc.start_turn();
    let (broker, receiver) = ChannelPermissionBroker::new("terminal-permission-cancel");
    ipc.attach_permission_receiver(1, receiver);
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    broker.forward_direct(OutboundPermissionQuery {
        id: 1,
        tool_name: "Edit".into(),
        tool_call_id: "terminal-permission-tool".into(),
        session_id: "terminal-permission-cancel".into(),
        title: "Edit files".into(),
        message: "Allow late edit?".into(),
        tool_input: None,
        metadata: None,
        options: vec![PermissionQueryOption {
            option_id: "allow_once".into(),
            label: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        }],
        response_tx,
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if store
            .read_state(&state.identity.job_id)
            .unwrap()
            .outcome
            .pending_permission
            .is_some()
        {
            break;
        }
        assert!(Instant::now() < deadline, "permission was not forwarded");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        finalize_completed_background_turn(&store, &state, &ipc, "parent completed", now_ms(),)
            .unwrap(),
        BackgroundTurnFinalization::Applied
    );

    assert!(!store
        .cancel_background_job_turn(&state.identity.job_id)
        .unwrap());
    assert!(matches!(
        response_rx.blocking_recv().unwrap(),
        PermissionAnswer::Cancelled
    ));
    assert!(!turn_cancel.is_cancelled());
    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Succeeded);
    assert!(loaded.outcome.pending_permission.is_none());
    let events = store.read_events_tail(&state.identity.job_id, 20).unwrap();
    assert!(!events.iter().any(|event| event.kind == "turn_cancel_sent"));
    assert!(events
        .iter()
        .any(|event| event.kind == "permissions_cancelled_ipc"));
    drop(broker);
    ipc.cancel.cancel();
}

/// A worker has no screen, so an `ExitPlanMode` on it is published as a
/// pending permission and waits for whoever opens the job. Nothing on that
/// road may approve it on the person's behalf, and nothing may quietly let the
/// turn carry on: the plan is not approved until someone says so, and if the
/// turn ends first the tool is told the call was cancelled.
///
/// The published options are plan mode's own four, which is what an app draws
/// its plan dialog from — a worker that published a bare allow/deny would give
/// the person no way to say which mode to leave plan mode into.
#[test]
fn an_unanswered_exit_plan_mode_is_never_approved_for_the_person() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 1;
    state.identity.session_id = Some("sess-exit-plan-unanswered".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    let _turn_cancel = ipc.start_turn();
    let (broker, receiver) = ChannelPermissionBroker::new("exit-plan-unanswered");
    ipc.attach_permission_receiver(1, receiver);

    let (response_tx, mut response_rx) = tokio::sync::oneshot::channel();
    broker.forward_direct(OutboundPermissionQuery {
        id: 1,
        tool_name: "ExitPlanMode".into(),
        tool_call_id: "exit-plan-tool".into(),
        session_id: "sess-exit-plan-unanswered".into(),
        title: "Plan ready for review".into(),
        message: "Review the proposed plan and choose how to proceed.".into(),
        tool_input: Some(serde_json::json!({"plan": "P1. Rewrite the loader"})),
        metadata: None,
        options: ["yes_auto", "yes_accept_edits", "yes_default", "reject_once"]
            .into_iter()
            .map(|option_id| PermissionQueryOption {
                option_id: option_id.into(),
                label: option_id.into(),
                kind: PermissionOptionKind::AllowOnce,
            })
            .collect(),
        response_tx,
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    let published = loop {
        if let Some(pending) = store
            .read_state(&state.identity.job_id)
            .unwrap()
            .outcome
            .pending_permission
            .take()
        {
            break pending;
        }
        assert!(Instant::now() < deadline, "the plan never reached a person");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(published.tool.as_deref(), Some("ExitPlanMode"));
    assert_eq!(
        published
            .options
            .iter()
            .map(|option| option.option_id.as_str())
            .collect::<Vec<_>>(),
        ["yes_auto", "yes_accept_edits", "yes_default", "reject_once"],
        "the person chooses which mode the session leaves plan mode into"
    );
    assert_eq!(
        store
            .read_state(&state.identity.job_id)
            .unwrap()
            .process
            .status,
        BackgroundJobStatus::NeedsInput,
        "the worker parks rather than carrying on unapproved"
    );
    // Still unanswered: nothing on the worker's road approves a plan.
    assert!(matches!(
        response_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    // The turn ends with nobody having answered. Fail-closed: the tool is told
    // the call was cancelled, so `ExitPlanMode` never reports the plan
    // approved and the session stays in plan mode.
    assert_eq!(
        finalize_completed_background_turn(&store, &state, &ipc, "parent completed", now_ms())
            .unwrap(),
        BackgroundTurnFinalization::Applied
    );
    assert!(matches!(
        response_rx.blocking_recv().unwrap(),
        PermissionAnswer::Cancelled
    ));
    assert!(store
        .read_state(&state.identity.job_id)
        .unwrap()
        .outcome
        .pending_permission
        .is_none());

    drop(broker);
    ipc.cancel.cancel();
}

#[test]
fn delayed_terminal_permission_cancel_cannot_cancel_replacement_turn() {
    for claimed in [false, true] {
        let (_dir, store) = store();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Succeeded;
        state.process.turn_generation = 4;
        state.identity.session_id = Some(format!("sess-delayed-terminal-cancel-{claimed}"));
        let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
        install_ipc_owner(&mut state, &ipc);
        state.outcome.pending_permission = Some(BackgroundPermissionQuerySnapshot {
            query_id: 17,
            turn_generation: 4,
            endpoint: Some(ipc.owner().endpoint),
            tool: Some("Edit".into()),
            tool_call_id: Some("old-terminal-permission".into()),
            session_id: state.identity.session_id.clone(),
            title: None,
            message: Some("Allow old edit?".into()),
            tool_input: None,
            metadata: None,
            options: vec![BackgroundPermissionOptionSnapshot {
                option_id: "allow_once".into(),
                label: "Allow once".into(),
                kind: "AllowOnce".into(),
            }],
        });
        store.write_state(&state).unwrap();
        let delayed_cancel = BackgroundIpcRequest::cancel_for(&state);

        store
            .update_state(&state.identity.job_id, |current| {
                current.outcome.pending_permission = None;
                current.identity.pending_prompts = vec![pending_prompt(
                    "pp-replacement-followup",
                    "replacement follow-up",
                    Vec::new(),
                )];
                current.process.status = if claimed {
                    BackgroundJobStatus::Running
                } else {
                    BackgroundJobStatus::Queued
                };
                if claimed {
                    current.process.turn_generation += 1;
                    current.identity.pending_prompts[0].claimed_turn_generation =
                        Some(current.process.turn_generation);
                }
                current.process.updated_at_ms = current.process.updated_at_ms.saturating_add(1);
                Ok(())
            })
            .unwrap();

        let turn_cancel = Arc::new(Mutex::new(rebon_types::PromptCancel::new()));
        let task_registry_resolver = Arc::new(Mutex::new(None));
        let teammate_runtimes = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let rule_context = Arc::new(Mutex::new(None));
        let live_permission_mode = Arc::new(Mutex::new(LivePermissionModeState::default()));
        let stop_gate: Arc<Mutex<Option<StopGate>>> = Arc::new(Mutex::new(None));
        let live_mcp_status = Arc::new(Mutex::new(None));
        let live_agent = Arc::new(Mutex::new(None));
        let response = handle_background_ipc_request(
            delayed_cancel,
            &store,
            &state.identity.job_id,
            &ipc.owner(),
            &ipc.events,
            &turn_cancel,
            &task_registry_resolver,
            &teammate_runtimes,
            &responses,
            &rule_context,
            &stop_gate,
            &live_permission_mode,
            &live_mcp_status,
            &live_agent,
            &Default::default(),
        );

        let (kind, message) = response.unwrap_err();
        assert_eq!(kind, rebon_session_host::HostCallError::StaleGeneration);
        assert!(message.contains("changed before cancellation"));
        assert!(!turn_cancel.lock().unwrap().is_cancelled());
        let loaded = store.read_state(&state.identity.job_id).unwrap();
        assert_eq!(
            loaded.process.status,
            if claimed {
                BackgroundJobStatus::Running
            } else {
                BackgroundJobStatus::Queued
            }
        );
        assert_eq!(pending_text(&loaded), Some("replacement follow-up"));
        ipc.cancel.cancel();
    }
}

#[test]
fn live_reply_queues_while_permission_pending_without_cancelling_turn() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 1;
    state.identity.session_id = Some("sess-reply-permission".into());
    state.process.pid = Some(std::process::id());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    let turn_cancel = ipc.start_turn();
    let (broker, receiver) = ChannelPermissionBroker::new("reply-permission");
    ipc.attach_permission_receiver(1, receiver);
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    broker.forward_direct(OutboundPermissionQuery {
        id: 1,
        tool_name: "Bash".into(),
        tool_call_id: "tool-reply-permission".into(),
        session_id: "reply-permission".into(),
        title: "Run command".into(),
        message: "Allow command?".into(),
        tool_input: None,
        metadata: None,
        options: vec![PermissionQueryOption {
            option_id: "allow_once".into(),
            label: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        }],
        response_tx,
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if store
            .read_state(&state.identity.job_id)
            .unwrap()
            .outcome
            .pending_permission
            .is_some()
        {
            break;
        }
        assert!(Instant::now() < deadline, "permission was not forwarded");
        std::thread::sleep(Duration::from_millis(20));
    }

    send_background_ipc_request(
        &state,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::Reply {
            message: "do something else".into(),
            images: Vec::new(),
        },
    )
    .unwrap();

    assert!(!turn_cancel.is_cancelled());
    let queued = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(pending_text(&queued), Some("do something else"));
    let pending = queued.outcome.pending_permission.unwrap();
    send_background_ipc_request(
        &state,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::PermissionAnswer {
            query_id: pending.query_id,
            turn_generation: pending.turn_generation,
            option_id: None,
            extra_text: None,
            updated_input: None,
        },
    )
    .unwrap();
    assert!(matches!(
        response_rx.blocking_recv().unwrap(),
        PermissionAnswer::Cancelled
    ));
    drop(broker);
    ipc.cancel.cancel();
}

#[test]
fn queued_live_reply_preserves_current_turn_permission() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 1;
    state.identity.session_id = Some("sess-reply-late-permission".into());
    state.process.pid = Some(std::process::id());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    let turn_cancel = ipc.start_turn();
    let (broker, receiver) = ChannelPermissionBroker::new("reply-late-permission");
    ipc.attach_permission_receiver(1, receiver);

    send_background_ipc_request(
        &state,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::Reply {
            message: "queued follow-up".into(),
            images: Vec::new(),
        },
    )
    .unwrap();
    assert!(!turn_cancel.is_cancelled());

    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    broker.forward_direct(OutboundPermissionQuery {
        id: 1,
        tool_name: "Edit".into(),
        tool_call_id: "late-old-tool".into(),
        session_id: "reply-late-permission".into(),
        title: "Edit files".into(),
        message: "Allow stale edit?".into(),
        tool_input: None,
        metadata: None,
        options: vec![PermissionQueryOption {
            option_id: "allow_once".into(),
            label: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        }],
        response_tx,
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    let pending = loop {
        let loaded = store.read_state(&state.identity.job_id).unwrap();
        if let Some(pending) = loaded.outcome.pending_permission.clone() {
            assert_eq!(loaded.process.status, BackgroundJobStatus::NeedsInput);
            assert_eq!(pending_text(&loaded), Some("queued follow-up"));
            break pending;
        }
        assert!(Instant::now() < deadline, "permission was not forwarded");
        std::thread::sleep(Duration::from_millis(20));
    };
    send_background_ipc_request(
        &state,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::PermissionAnswer {
            query_id: pending.query_id,
            turn_generation: pending.turn_generation,
            option_id: None,
            extra_text: None,
            updated_input: None,
        },
    )
    .unwrap();
    assert!(matches!(
        response_rx.blocking_recv().unwrap(),
        PermissionAnswer::Cancelled
    ));
    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Running);
    assert_eq!(pending_text(&loaded), Some("queued follow-up"));
    assert!(loaded.outcome.pending_permission.is_none());
    drop(broker);
    ipc.cancel.cancel();
}

#[test]
fn stale_permission_forwarder_cannot_publish_after_endpoint_replacement() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 1;
    state.identity.session_id = Some("sess-stale-forwarder".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    let (broker, receiver) = ChannelPermissionBroker::new("stale-forwarder");
    ipc.attach_permission_receiver(1, receiver);
    store
        .update_state(&state.identity.job_id, |current| {
            current.process.turn_generation = 2;
            current.process.ipc_port = Some(ipc.port.wrapping_add(1));
            current.process.ipc_token = Some("replacement-endpoint".into());
            Ok(())
        })
        .unwrap();

    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    broker.forward_direct(OutboundPermissionQuery {
        id: 1,
        tool_name: "Edit".into(),
        tool_call_id: "stale-forwarder-tool".into(),
        session_id: "stale-forwarder".into(),
        title: "Edit files".into(),
        message: "Allow stale edit?".into(),
        tool_input: None,
        metadata: None,
        options: vec![PermissionQueryOption {
            option_id: "allow_once".into(),
            label: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        }],
        response_tx,
    });

    assert!(matches!(
        response_rx.blocking_recv().unwrap(),
        PermissionAnswer::Cancelled
    ));
    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.turn_generation, 2);
    assert!(loaded.outcome.pending_permission.is_none());
    drop(broker);
    ipc.cancel.cancel();
}

#[test]
fn permission_forwarder_polls_new_turn_while_old_receiver_remains_open() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 1;
    state.identity.session_id = Some("sess-one".into());
    state.process.pid = Some(std::process::id());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let (old_broker, old_rx) = ChannelPermissionBroker::new("old-turn");
    let (new_broker, new_rx) = ChannelPermissionBroker::new("new-turn");
    ipc.attach_permission_receiver(1, old_rx);
    ipc.attach_permission_receiver(2, new_rx);

    let (old_response_tx, old_response_rx) = tokio::sync::oneshot::channel();
    old_broker.forward_direct(OutboundPermissionQuery {
        id: 1,
        tool_name: "Bash".into(),
        tool_call_id: "old-tool".into(),
        session_id: "old-turn".into(),
        title: "Run command".into(),
        message: "Allow old turn?".into(),
        tool_input: None,
        metadata: None,
        options: vec![PermissionQueryOption {
            option_id: "allow_once".into(),
            label: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        }],
        response_tx: old_response_tx,
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let old_query_id = loop {
        let loaded = store.read_state(&state.identity.job_id).unwrap();
        if let Some(pending) = loaded
            .outcome
            .pending_permission
            .as_ref()
            .filter(|pending| pending.session_id.as_deref() == Some("old-turn"))
        {
            break pending.query_id;
        }
        assert!(
            Instant::now() < deadline,
            "old permission was not forwarded; status={:?} pending={:?}",
            loaded.process.status,
            loaded.outcome.pending_permission
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::NeedsInput);
    assert_eq!(
        loaded
            .outcome
            .pending_permission
            .as_ref()
            .map(|pending| pending.turn_generation),
        Some(1)
    );
    assert_eq!(
        finalize_completed_background_turn(&store, &state, &ipc, "parent completed", now_ms(),)
            .unwrap(),
        BackgroundTurnFinalization::Applied
    );
    assert!(matches!(
        old_response_rx.blocking_recv().unwrap(),
        PermissionAnswer::Cancelled
    ));
    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Succeeded);
    assert!(loaded.outcome.pending_permission.is_none());

    let (late_response_tx, late_response_rx) = tokio::sync::oneshot::channel();
    old_broker.forward_direct(OutboundPermissionQuery {
        id: 2,
        tool_name: "Edit".into(),
        tool_call_id: "late-old-tool".into(),
        session_id: "old-turn-late".into(),
        title: "Edit files".into(),
        message: "Allow late old turn?".into(),
        tool_input: None,
        metadata: None,
        options: vec![PermissionQueryOption {
            option_id: "allow_once".into(),
            label: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        }],
        response_tx: late_response_tx,
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let late_query_id = loop {
        let loaded = store.read_state(&state.identity.job_id).unwrap();
        if let Some(pending) = loaded
            .outcome
            .pending_permission
            .as_ref()
            .filter(|pending| pending.session_id.as_deref() == Some("old-turn-late"))
        {
            assert_eq!(loaded.process.status, BackgroundJobStatus::Succeeded);
            assert_eq!(pending.turn_generation, 1);
            break pending.query_id;
        }
        assert!(
            Instant::now() < deadline,
            "late old-turn permission was not forwarded"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    store
        .answer_permission_query(
            &state.identity.job_id,
            late_query_id,
            Some("allow_once".into()),
            None,
        )
        .unwrap();
    assert!(matches!(
        late_response_rx.blocking_recv().unwrap(),
        PermissionAnswer::Selected { .. }
    ));
    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Succeeded);
    assert!(loaded.outcome.pending_permission.is_none());
    store
        .update_state(&state.identity.job_id, |current| {
            current.process.status = BackgroundJobStatus::Running;
            current.process.turn_generation = 2;
            current.process.completed_at_ms = None;
            Ok(())
        })
        .unwrap();

    let (new_response_tx, new_response_rx) = tokio::sync::oneshot::channel();
    new_broker.forward_direct(OutboundPermissionQuery {
        id: 1,
        tool_name: "Read".into(),
        tool_call_id: "new-tool".into(),
        session_id: "new-turn".into(),
        title: "Read files".into(),
        message: "Allow new turn?".into(),
        tool_input: None,
        metadata: None,
        options: vec![PermissionQueryOption {
            option_id: "allow_once".into(),
            label: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        }],
        response_tx: new_response_tx,
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let new_query_id = loop {
        let loaded = store.read_state(&state.identity.job_id).unwrap();
        if let Some(pending) = loaded
            .outcome
            .pending_permission
            .filter(|pending| pending.session_id.as_deref() == Some("new-turn"))
        {
            break pending.query_id;
        }
        assert!(
            Instant::now() < deadline,
            "new permission was not forwarded"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::NeedsInput);
    assert_eq!(
        loaded
            .outcome
            .pending_permission
            .as_ref()
            .map(|pending| pending.turn_generation),
        Some(2)
    );
    assert_ne!(new_query_id, old_query_id);
    send_background_ipc_request(
        &state,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::PermissionAnswer {
            query_id: new_query_id,
            turn_generation: 2,
            option_id: Some("allow_once".into()),
            extra_text: None,
            updated_input: None,
        },
    )
    .unwrap();
    assert!(matches!(
        new_response_rx.blocking_recv().unwrap(),
        PermissionAnswer::Selected { .. }
    ));

    let turn_two = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(turn_two.process.turn_generation, 2);
    assert_eq!(turn_two.process.status, BackgroundJobStatus::Running);
    assert_eq!(
        finalize_completed_background_turn(
            &store,
            &turn_two,
            &ipc,
            "second parent turn completed",
            now_ms(),
        )
        .unwrap(),
        BackgroundTurnFinalization::Applied
    );

    let (late_after_new_tx, late_after_new_rx) = tokio::sync::oneshot::channel();
    old_broker.forward_direct(OutboundPermissionQuery {
        id: 3,
        tool_name: "Edit".into(),
        tool_call_id: "late-old-after-new-terminal".into(),
        session_id: "old-after-new-terminal".into(),
        title: "Edit files".into(),
        message: "Allow old child after the newer turn completed?".into(),
        tool_input: None,
        metadata: None,
        options: vec![PermissionQueryOption {
            option_id: "allow_once".into(),
            label: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        }],
        response_tx: late_after_new_tx,
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let late_snapshot = loop {
        let loaded = store.read_state(&state.identity.job_id).unwrap();
        if let Some(pending) = loaded
            .outcome
            .pending_permission
            .as_ref()
            .filter(|pending| pending.session_id.as_deref() == Some("old-after-new-terminal"))
        {
            assert_eq!(loaded.process.status, BackgroundJobStatus::Succeeded);
            assert_eq!(loaded.process.turn_generation, 2);
            assert_eq!(pending.turn_generation, 1);
            break pending.clone();
        }
        assert!(
            Instant::now() < deadline,
            "old-turn permission was not forwarded after the newer turn completed"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    store
        .answer_permission_query_for_target_with_updated_input(
            &state.identity.job_id,
            late_snapshot.query_id,
            Some(late_snapshot.turn_generation),
            late_snapshot.endpoint.as_ref(),
            Some("allow_once".into()),
            None,
            None,
        )
        .unwrap();
    assert!(matches!(
        late_after_new_rx.blocking_recv().unwrap(),
        PermissionAnswer::Selected { .. }
    ));

    let (question_tx, question_rx) = tokio::sync::oneshot::channel();
    old_broker.forward_direct(OutboundPermissionQuery {
        id: 4,
        tool_name: "AskUserQuestion".into(),
        tool_call_id: "late-old-question-after-new-terminal".into(),
        session_id: "old-question-after-new-terminal".into(),
        title: "Answer questions".into(),
        message: "Choose a mode".into(),
        tool_input: Some(serde_json::json!({
            "questions": [{
                "header": "Mode",
                "question": "Choose a mode",
                "options": [
                    {"label": "Fast", "description": "Finish quickly"},
                    {"label": "Safe", "description": "Check everything"}
                ]
            }]
        })),
        metadata: None,
        options: vec![PermissionQueryOption {
            option_id: "allow_once".into(),
            label: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        }],
        response_tx: question_tx,
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let question_snapshot = loop {
        let loaded = store.read_state(&state.identity.job_id).unwrap();
        if let Some(pending) = loaded
            .outcome
            .pending_permission
            .as_ref()
            .filter(|pending| {
                pending.session_id.as_deref() == Some("old-question-after-new-terminal")
            })
        {
            assert_eq!(loaded.process.status, BackgroundJobStatus::Succeeded);
            assert_eq!(loaded.process.turn_generation, 2);
            assert_eq!(pending.turn_generation, 1);
            break pending.clone();
        }
        assert!(
            Instant::now() < deadline,
            "old-turn question was not forwarded after the newer turn completed"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    store
        .answer_question_query_for_target(
            &state.identity.job_id,
            question_snapshot.query_id,
            Some(question_snapshot.turn_generation),
            question_snapshot.endpoint.as_ref(),
            vec![rebon_session_host::ForegroundQuestionAnswer {
                selected_options: vec![1],
                other_text: None,
            }],
        )
        .unwrap();
    let PermissionAnswer::Selected {
        updated_input: Some(updated_input),
        ..
    } = question_rx.blocking_recv().unwrap()
    else {
        panic!("expected structured old-turn question answer");
    };
    assert_eq!(updated_input["answers"]["Choose a mode"], "Safe");
    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Succeeded);
    assert_eq!(loaded.process.turn_generation, 2);
    assert!(loaded.outcome.pending_permission.is_none());

    drop(old_broker);
    drop(new_broker);
    ipc.cancel.cancel();
}

#[test]
fn turn_cancel_resolves_pending_and_late_permission_queries() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 1;
    state.identity.session_id = Some("sess-cancel-permission".into());
    state.process.pid = Some(std::process::id());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let (broker, rx) = ChannelPermissionBroker::new("cancel-permission");
    ipc.attach_permission_receiver(1, rx);
    let send_query = |id, response_tx| {
        broker.forward_direct(OutboundPermissionQuery {
            id,
            tool_name: "Bash".into(),
            tool_call_id: format!("tool-{id}"),
            session_id: "cancel-permission".into(),
            title: "Run command".into(),
            message: "Allow command?".into(),
            tool_input: None,
            metadata: None,
            options: vec![PermissionQueryOption {
                option_id: "allow_once".into(),
                label: "Allow once".into(),
                kind: PermissionOptionKind::AllowOnce,
            }],
            response_tx,
        });
    };

    let (pending_tx, pending_rx) = tokio::sync::oneshot::channel();
    send_query(1, pending_tx);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if store
            .read_state(&state.identity.job_id)
            .unwrap()
            .outcome
            .pending_permission
            .is_some()
        {
            break;
        }
        assert!(Instant::now() < deadline, "permission was not forwarded");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Cancel through the production helper: the IPC forwarder may write
    // state again between our read and the cancel reaching the worker, so
    // a hand-built fence intermittently trips "turn changed" — the helper
    // retries against a fresh fence.
    assert!(store
        .cancel_background_job_turn(&state.identity.job_id)
        .unwrap());
    assert!(matches!(
        pending_rx.blocking_recv().unwrap(),
        PermissionAnswer::Cancelled
    ));
    let cancelled = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(cancelled.process.status, BackgroundJobStatus::Idle);
    assert!(cancelled.outcome.pending_permission.is_none());

    let (late_tx, late_rx) = tokio::sync::oneshot::channel();
    send_query(2, late_tx);
    assert!(matches!(
        late_rx.blocking_recv().unwrap(),
        PermissionAnswer::Cancelled
    ));
    let after_late_query = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(after_late_query.process.status, BackgroundJobStatus::Idle);
    assert!(after_late_query.outcome.pending_permission.is_none());
    drop(broker);
    ipc.cancel.cancel();
}

#[test]
fn permission_pending_lifecycle_uses_state_not_event_tail() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    let owner = ipc.owner();
    state.process.status = BackgroundJobStatus::NeedsInput;
    state.outcome.pending_permission = Some(BackgroundPermissionQuerySnapshot {
        query_id: 7,
        turn_generation: 0,
        endpoint: Some(owner.endpoint.clone()),
        tool: Some("Bash".into()),
        tool_call_id: Some("toolu_1".into()),
        session_id: Some("sess-one".into()),
        title: None,
        message: None,
        tool_input: Some(serde_json::json!({"command": "echo ok"})),
        metadata: None,
        options: vec![BackgroundPermissionOptionSnapshot {
            option_id: "allow_once".into(),
            label: "Allow once".into(),
            kind: "AllowOnce".into(),
        }],
    });
    store.write_state(&state).unwrap();
    store
        .append_event(
            &state.identity.job_id,
            "permission_requested",
            serde_json::json!({ "queryId": 1 }),
        )
        .unwrap();

    let peek = store.read_peek_lines(&state.identity.job_id, 10).join("\n");
    assert!(peek.contains("permission query 7"));
    assert!(!peek.contains("permission query 1"));

    let responses = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let (tx, rx) = tokio::sync::oneshot::channel();
    responses.lock().unwrap().insert(7, tx);
    let updated_input = serde_json::json!({
        "answers": {"Choose a mode": "Safe"},
        "annotations": {},
    });
    let response = send_background_permission_answer(
        &store,
        &state.identity.job_id,
        &owner,
        &responses,
        &Arc::new(Mutex::new(None)),
        7,
        0,
        Some("allow_once".into()),
        None,
        Some(updated_input.clone()),
    );
    response.unwrap();
    let PermissionAnswer::Selected {
        updated_input: Some(received_input),
        ..
    } = rx.blocking_recv().unwrap()
    else {
        panic!("expected selected answer with updated input");
    };
    assert_eq!(received_input, updated_input);
    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.outcome.pending_permission, None);
    assert_eq!(loaded.process.status, BackgroundJobStatus::Running);
    ipc.cancel.cancel();
}

#[test]
fn question_answer_ipc_validates_and_consumes_sender_once() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    let owner = ipc.owner();
    state.process.status = BackgroundJobStatus::NeedsInput;
    state.outcome.pending_permission = Some(BackgroundPermissionQuerySnapshot {
        query_id: 8,
        turn_generation: 0,
        endpoint: Some(owner.endpoint.clone()),
        tool: Some("AskUserQuestion".into()),
        tool_call_id: Some("toolu_2".into()),
        session_id: Some("sess-two".into()),
        title: None,
        message: None,
        tool_input: Some(serde_json::json!({
            "questions": [{
                "header": "Mode",
                "question": "Choose a mode",
                "options": [
                    {"label": "Fast", "description": "Finish quickly"},
                    {"label": "Safe", "description": "Check everything"}
                ]
            }]
        })),
        metadata: None,
        options: vec![BackgroundPermissionOptionSnapshot {
            option_id: "allow_once".into(),
            label: "Allow once".into(),
            kind: "AllowOnce".into(),
        }],
    });
    store.write_state(&state).unwrap();
    let responses = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let (tx, rx) = tokio::sync::oneshot::channel();
    responses.lock().unwrap().insert(8, tx);

    let invalid = send_background_question_answer(
        &store,
        &state.identity.job_id,
        &owner,
        &responses,
        8,
        0,
        vec![rebon_session_host::ForegroundQuestionAnswer {
            selected_options: vec![],
            other_text: None,
        }],
    );
    assert!(invalid.is_err());
    assert!(responses.lock().unwrap().contains_key(&8));

    let response = send_background_question_answer(
        &store,
        &state.identity.job_id,
        &owner,
        &responses,
        8,
        0,
        vec![rebon_session_host::ForegroundQuestionAnswer {
            selected_options: vec![1],
            other_text: None,
        }],
    );
    response.unwrap();
    let PermissionAnswer::Selected {
        option_id,
        updated_input: Some(updated_input),
        ..
    } = rx.blocking_recv().unwrap()
    else {
        panic!("expected structured selected answer");
    };
    assert_eq!(option_id, "allow_once");
    assert_eq!(updated_input["answers"]["Choose a mode"], "Safe");

    let duplicate = send_background_question_answer(
        &store,
        &state.identity.job_id,
        &owner,
        &responses,
        8,
        0,
        vec![rebon_session_host::ForegroundQuestionAnswer {
            selected_options: vec![0],
            other_text: None,
        }],
    );
    assert!(duplicate.is_err());
    ipc.cancel.cancel();
}

#[test]
fn terminal_cleanup_fence_rejects_permission_change_without_timestamp_change() {
    let (_dir, store) = store();
    let mut stale = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    stale.process.status = BackgroundJobStatus::Succeeded;
    stale.process.turn_generation = 3;
    stale.process.pid = Some(std::process::id());
    stale.process.pid_identity = rebon_session_host::process_identity(std::process::id());
    stale.process.ipc_port = Some(1234);
    stale.process.ipc_token = Some("same-owner".into());
    stale.outcome.pending_permission = Some(BackgroundPermissionQuerySnapshot {
        query_id: 44,
        turn_generation: 3,
        endpoint: None,
        tool: Some("Edit".into()),
        tool_call_id: Some("old-permission".into()),
        session_id: None,
        title: None,
        message: None,
        tool_input: None,
        metadata: None,
        options: Vec::new(),
    });
    store.write_state(&stale).unwrap();
    store
        .update_state(&stale.identity.job_id, |current| {
            current.outcome.pending_permission = None;
            Ok(())
        })
        .unwrap();

    assert!(!fence_terminal_worker_for_cleanup(&store, &mut stale).unwrap());

    let loaded = store.read_state(&stale.identity.job_id).unwrap();
    assert_eq!(loaded.process.pid, Some(std::process::id()));
    assert_eq!(loaded.process.ipc_port, Some(1234));
    assert_eq!(loaded.process.ipc_token.as_deref(), Some("same-owner"));
    assert!(loaded.outcome.pending_permission.is_none());
}

#[test]
fn allow_always_answer_persists_rule_and_sends_canonical_option() {
    let (_dir, store) = store();
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().to_string();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    let owner = ipc.owner();
    state.process.status = BackgroundJobStatus::NeedsInput;
    state.outcome.pending_permission = Some(BackgroundPermissionQuerySnapshot {
        query_id: 9,
        turn_generation: 0,
        endpoint: Some(owner.endpoint.clone()),
        tool: Some("Bash".into()),
        tool_call_id: Some("toolu_allow".into()),
        session_id: Some("sess-allow".into()),
        title: None,
        message: None,
        tool_input: Some(serde_json::json!({
            "command": "cargo test -p a && cargo test -p b"
        })),
        metadata: None,
        options: vec![
            BackgroundPermissionOptionSnapshot {
                option_id: "allow_always".into(),
                label: "Allow always exact command".into(),
                kind: "AllowAlways".into(),
            },
            BackgroundPermissionOptionSnapshot {
                option_id: "allow_always_generalized".into(),
                label: "Allow always cargo test commands".into(),
                kind: "AllowAlways".into(),
            },
        ],
    });
    store.write_state(&state).unwrap();

    let policy_store = rebon_core::policy::PolicyStore::new();
    let rule_context = Arc::new(Mutex::new(Some(BackgroundPermissionRuleContext {
        cwd: cwd.clone(),
        policy_store: policy_store.clone(),
    })));
    let responses = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let (tx, rx) = tokio::sync::oneshot::channel();
    responses.lock().unwrap().insert(9, tx);
    let response = send_background_permission_answer(
        &store,
        &state.identity.job_id,
        &owner,
        &responses,
        &rule_context,
        9,
        0,
        Some("allow_always_generalized".into()),
        None,
        None,
    );
    response.unwrap();

    // The engine only ever sees the canonical option id.
    let PermissionAnswer::Selected { option_id, .. } = rx.blocking_recv().unwrap() else {
        panic!("expected selected answer");
    };
    assert_eq!(option_id, "allow_always");

    // The live policy store auto-allows subsequent matching calls.
    let evaluator = rebon_core::policy::PolicyEvaluator::new(policy_store);
    assert!(matches!(
        evaluator.evaluate(
            "Bash",
            &serde_json::json!({"command": "cargo test -p xyz"}),
            Some(&cwd),
        ),
        rebon_core::policy::PolicyOutcome::AutoAllow { .. }
    ));

    // The generalized rule reached `.rebon/settings.json` under the
    // session cwd.
    let settings =
        std::fs::read_to_string(cwd_dir.path().join(".rebon").join("settings.json")).unwrap();
    assert!(settings.contains("Bash(cargo test:*)"), "{settings}");
    ipc.cancel.cancel();
}
