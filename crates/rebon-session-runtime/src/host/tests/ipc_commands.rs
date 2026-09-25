use super::super::*;
use super::support::*;
use rebon_plugin_tasks::runtime::{TaskId, TaskStatus};
use rebon_session_host::{BackgroundPermissionOptionSnapshot, BackgroundPermissionQuerySnapshot};

fn attach_task_scope(
    ipc: &BackgroundIpcServer,
    session_id: &str,
) -> (
    Arc<rebon_kernel_seats::kernel_services::SessionKernelScopes>,
    Arc<TaskRegistry>,
) {
    let scopes = rebon_kernel_seats::kernel_services::SessionKernelScopes::new(
        rebon_harness::kernel_bootstrap::process_kernel(),
        Arc::new(rebon_core::Engine::with_builtin_tools()),
        rebon_harness::projects_root(),
    );
    let _binding = scopes.acquire(session_id);
    let registry = scopes.host_task_registry(session_id);
    ipc.attach_task_registry_resolver(scopes.task_registry_resolver());
    (scopes, registry)
}

/// A stream opened the way production opens one: through a connection.
fn session_event_stream(
    session_id: &str,
    job_id: &str,
    endpoint: &rebon_session_host::BackgroundIpcEndpoint,
) -> Option<std::sync::mpsc::Receiver<rebon_session_host::SessionEvent>> {
    rebon_session_host::SessionHostConnection::new(rebon_session_host::OwnerHandle::for_worker(
        session_id,
        Some(job_id),
        endpoint,
    ))
    .subscribe_in_background()
}

fn rebon_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rebon"))
}

#[test]
fn background_command_round_trips_through_the_ipc_consumer() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-command".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let client_state = state.clone();
    let port = ipc.port;
    let token = ipc.token.clone();
    let client = std::thread::spawn(move || {
        rebon_session_host::run_background_command(
            &client_state,
            port,
            token,
            "hooks".into(),
            vec!["errors".into()],
        )
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let drained = ipc.drain_commands(|name, args| {
            assert_eq!(name, "hooks");
            assert_eq!(args, ["errors"]);
            Ok(rebon_session_host::CommandOutput {
                text: "hook diagnostics".into(),
                tone: "warning".into(),
            })
        });
        if drained == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "IPC command was not delivered"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let output = client.join().unwrap().unwrap();
    assert_eq!(output.text, "hook diagnostics");
    assert_eq!(output.tone, "warning");
    ipc.cancel.cancel();
}

/// A session the way the worker binds one: a record on its own session
/// table, the gate's cell registered on it as `build.rs` registers it, and a
/// transcript directory for its sidecar.
struct BoundSession {
    state: Arc<rebon_acp::ServerState>,
    session_id: String,
    cell: Arc<Mutex<rebon_permissions::PermissionMode>>,
    projects: Arc<tempfile::TempDir>,
    cwd: String,
}

impl BoundSession {
    fn new(mode: rebon_permissions::PermissionMode) -> Self {
        let state = Arc::new(rebon_acp::ServerState::new());
        let record = state.create_session_with_permission_mode(
            "/repo/bound".into(),
            Vec::new(),
            mode.as_wire(),
        );
        Self::bind(
            state,
            record.id,
            mode,
            Arc::new(tempfile::tempdir().unwrap()),
        )
    }

    /// The same session built again for the next turn, the way the worker
    /// builds it: from the job record's mode, which a fresh record takes by
    /// being set to it.
    fn next_turn(&self, mode: rebon_permissions::PermissionMode) -> Self {
        let state = Arc::new(rebon_acp::ServerState::new());
        state.restore_empty_session(
            self.session_id.clone(),
            self.cwd.clone(),
            Vec::new(),
            "default",
        );
        assert!(state.set_permission_mode(&self.session_id, mode.as_wire()));
        Self::bind(
            state,
            self.session_id.clone(),
            mode,
            Arc::clone(&self.projects),
        )
    }

    fn bind(
        state: Arc<rebon_acp::ServerState>,
        session_id: String,
        mode: rebon_permissions::PermissionMode,
        projects: Arc<tempfile::TempDir>,
    ) -> Self {
        let cell = Arc::new(Mutex::new(mode));
        state.attach_permission_mode_cell(&session_id, Arc::clone(&cell));
        Self {
            state,
            session_id,
            cell,
            projects,
            cwd: "/repo/bound".into(),
        }
    }

    fn live_mode(&self) -> crate::host::ipc::server::LiveSessionMode<'_> {
        crate::host::ipc::server::LiveSessionMode {
            state: &self.state,
            session_id: &self.session_id,
            cell: &self.cell,
            projects_root: self.projects.path(),
            cwd: &self.cwd,
        }
    }

    fn attach(&self, ipc: &BackgroundIpcServer, store: &BackgroundStore, job_id: &str) {
        ipc.attach_session_permission_mode(store, job_id, self.live_mode());
    }

    fn cell_mode(&self) -> rebon_permissions::PermissionMode {
        *self.cell.lock().unwrap()
    }

    fn record_mode(&self) -> String {
        self.state
            .session_permission_mode(&self.session_id)
            .expect("the record exists")
    }
}

fn job_permission_mode(store: &BackgroundStore, job_id: &str) -> Option<String> {
    store
        .read_state(job_id)
        .unwrap()
        .identity
        .runtime
        .permission_mode
}

#[test]
fn permission_mode_ipc_updates_the_live_and_next_session_cells() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-permission-mode".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let first = BoundSession::new(rebon_permissions::PermissionMode::Default);
    first.attach(&ipc, &store, &state.identity.job_id);
    rebon_session_host::send_background_ipc_request(
        &state,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::SetPermissionMode {
            mode: "bypassPermissions".into(),
        },
    )
    .unwrap();
    assert_eq!(
        first.cell_mode(),
        rebon_permissions::PermissionMode::BypassPermissions
    );
    // The record is what the plan-mode reminders read; a cell that moved
    // without it is a gate and a prompt that disagree about the mode.
    assert_eq!(first.record_mode(), "bypassPermissions");
    assert_eq!(
        job_permission_mode(&store, &state.identity.job_id).as_deref(),
        Some("bypassPermissions")
    );

    let next = BoundSession::new(rebon_permissions::PermissionMode::Default);
    next.attach(&ipc, &store, &state.identity.job_id);
    assert_eq!(
        next.cell_mode(),
        rebon_permissions::PermissionMode::BypassPermissions
    );
    assert_eq!(next.record_mode(), "bypassPermissions");

    let error = rebon_session_host::send_background_ipc_request(
        &state,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::SetPermissionMode {
            mode: "automatic".into(),
        },
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unknown background job permission mode"),
        "expected the owner's typed refusal, got: {error}"
    );
    assert_eq!(
        next.cell_mode(),
        rebon_permissions::PermissionMode::BypassPermissions
    );
    ipc.cancel.cancel();
}

/// `ExitPlanMode` approved into `auto`, or `EnterPlanMode`, moves the
/// session's record and nothing else. The worker rebuilds the session from
/// the job record every turn, so a move that stayed in the session was
/// undone one turn later: the desktop app snapped back into plan mode.
#[test]
fn a_mode_the_session_takes_itself_reaches_the_job_and_the_next_turn() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-tool-mode".into());
    state.identity.runtime.permission_mode = Some("plan".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    let job_id = state.identity.job_id.clone();

    let turn = BoundSession::new(rebon_permissions::PermissionMode::Plan);
    turn.attach(&ipc, &store, &job_id);
    // What the plan-mode plugin does when the approval comes back.
    assert!(turn.state.set_permission_mode(&turn.session_id, "auto"));

    assert_eq!(
        job_permission_mode(&store, &job_id).as_deref(),
        Some("auto")
    );
    // The next turn is built from the job record, and comes up in the mode
    // the last one left.
    let overrides = store
        .read_state(&job_id)
        .unwrap()
        .identity
        .runtime
        .to_runtime_override()
        .unwrap();
    assert_eq!(
        overrides.permission_mode,
        Some(rebon_permissions::PermissionMode::Auto)
    );

    // And the other way: entering plan mode is remembered just the same.
    assert!(turn.state.set_permission_mode(&turn.session_id, "plan"));
    assert_eq!(
        job_permission_mode(&store, &job_id).as_deref(),
        Some("plan")
    );
    ipc.cancel.cancel();
}

/// A plan entered from auto keeps auto's classifier, so where plan came
/// from has to outlive the session the worker drops at the end of the turn.
/// The next turn's record comes back in plan by being set there, which reads
/// as entering it from `default`; the restore puts the real origin back.
#[test]
fn where_plan_was_entered_from_survives_the_next_turns_rebuild() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-plan-origin".into());
    state.identity.runtime.permission_mode = Some("auto".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    let job_id = state.identity.job_id.clone();

    let first = BoundSession::new(rebon_permissions::PermissionMode::Auto);
    first.attach(&ipc, &store, &job_id);
    // `EnterPlanMode` from auto.
    assert!(first.state.set_permission_mode(&first.session_id, "plan"));
    assert_eq!(
        job_permission_mode(&store, &job_id).as_deref(),
        Some("plan")
    );

    let next = first.next_turn(rebon_permissions::PermissionMode::Plan);
    assert_eq!(
        next.state.plan_entered_from(&next.session_id).as_deref(),
        Some("default"),
        "a rebuilt record reads as entering plan from default"
    );
    next.live_mode().restore_plan_entered_from();
    next.attach(&ipc, &store, &job_id);
    assert_eq!(
        next.state.plan_entered_from(&next.session_id).as_deref(),
        Some("auto")
    );

    // Leaving plan forgets it, so the turn after that has nothing to restore.
    assert!(next.state.set_permission_mode(&next.session_id, "default"));
    let after = next.next_turn(rebon_permissions::PermissionMode::Plan);
    after.live_mode().restore_plan_entered_from();
    assert_eq!(after.state.plan_entered_from(&after.session_id), None);
    ipc.cancel.cancel();
}

/// Every client attached to the job hears a tool-driven move the moment it
/// happens, the way it hears one another client made.
#[test]
fn a_mode_the_session_takes_itself_is_announced_to_clients() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-tool-mode-status".into());
    state.identity.runtime.permission_mode = Some("plan".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let turn = BoundSession::new(rebon_permissions::PermissionMode::Plan);
    turn.attach(&ipc, &store, &state.identity.job_id);
    assert!(turn
        .state
        .set_permission_mode(&turn.session_id, "acceptEdits"));
    // Saying the same thing twice announces nothing new.
    assert!(turn
        .state
        .set_permission_mode(&turn.session_id, "acceptEdits"));

    let owner = rebon_session_host::OwnerHandle {
        session_id: "sess-tool-mode-status".into(),
        job_id: Some(state.identity.job_id.clone()),
        pid: std::process::id(),
        port: ipc.port,
        token: ipc.token.clone(),
        surface: rebon_session::SessionOwnerSurface::Worker,
    };
    // Read on a thread: a stream with nothing published waits for the next
    // event, and a regression here should fail rather than hang.
    let (events_tx, events_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stream = owner.subscribe(Some(0)).unwrap();
        for _ in 0..2 {
            let _ = events_tx.send(stream.next());
        }
    });
    let next = || {
        events_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the new mode was published")
            .expect("the stream stayed open")
    };
    assert!(matches!(
        next(),
        rebon_session_host::SessionEvent::Hello { .. }
    ));
    match next() {
        rebon_session_host::SessionEvent::Status { snapshot, .. } => {
            assert_eq!(snapshot.permission_mode.as_deref(), Some("acceptEdits"));
        }
        other => panic!("expected the new mode's status, got {other:?}"),
    }
    ipc.cancel.cancel();
}

/// A worker that no longer owns the job must not write its mode into it:
/// the job belongs to whoever replaced it.
#[test]
fn a_superseded_worker_does_not_publish_its_sessions_mode() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-superseded-mode".into());
    state.identity.runtime.permission_mode = Some("plan".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    state.process.ipc_token = Some("replacement-endpoint".into());
    store.write_state(&state).unwrap();

    let turn = BoundSession::new(rebon_permissions::PermissionMode::Plan);
    turn.attach(&ipc, &store, &state.identity.job_id);
    assert!(turn.state.set_permission_mode(&turn.session_id, "auto"));

    assert_eq!(
        job_permission_mode(&store, &state.identity.job_id).as_deref(),
        Some("plan")
    );
    ipc.cancel.cancel();
}

#[tokio::test]
async fn a_queued_command_wakes_the_turn_loop_on_its_own() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-command-wake".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let client_state = state.clone();
    let port = ipc.port;
    let token = ipc.token.clone();
    let client = std::thread::spawn(move || {
        rebon_session_host::run_background_command(
            &client_state,
            port,
            token,
            "hooks".into(),
            Vec::new(),
        )
    });

    // No timer and no polling: awaiting the channel is what the turn loop
    // does, and it has to resolve on the IPC reader thread's hand-off alone.
    let request = tokio::time::timeout(Duration::from_secs(5), ipc.recv_command())
        .await
        .expect("command receive resolved")
        .expect("command channel stays open");
    assert_eq!(request.name, "hooks");
    let _ = request
        .response_tx
        .send(Ok(rebon_session_host::CommandOutput {
            text: "delivered".into(),
            tone: "info".into(),
        }));

    assert_eq!(client.join().unwrap().unwrap().text, "delivered");
    ipc.cancel.cancel();
}

#[test]
fn reply_to_running_job_appends_without_cancelling_live_turn() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.identity.session_id = Some("sess-one".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let image = BackgroundImageAttachment {
        id: 5,
        data: "ipc-image".into(),
        media_type: "image/png".into(),
        filename: None,
        source_path: None,
    };
    let first_turn = ipc.start_turn();
    rebon_session_host::reply_to_background_job_in_store_with_images(
        &store,
        &state.identity.job_id,
        "later".into(),
        vec![image.clone()],
        false,
        true,
        &rebon_exe(),
    )
    .unwrap();

    std::thread::sleep(Duration::from_millis(100));
    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Running);
    assert_eq!(pending_text(&loaded), Some("later"));
    assert_eq!(loaded.identity.pending_prompts[0].images, vec![image]);
    assert_eq!(loaded.process.ipc_port, Some(ipc.port));
    assert_eq!(
        loaded.process.ipc_token.as_deref(),
        Some(ipc.token.as_str())
    );
    assert!(!first_turn.is_cancelled());
    let events = store.read_events_tail(&state.identity.job_id, 20).unwrap();
    assert!(events.iter().any(|event| {
        event.kind == "reply_queued" && event.data["nonInterrupting"] == serde_json::json!(true)
    }));
    ipc.cancel.cancel();
}

#[test]
fn cancelling_queued_live_reply_returns_worker_to_idle() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("original prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Idle;
    state.identity.session_id = Some("sess-one".into());
    state.process.pid = Some(std::process::id());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

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
    let queued = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(queued.process.status, BackgroundJobStatus::Queued);
    assert_eq!(pending_text(&queued), Some("queued follow-up"));

    let queued_turn = ipc.start_turn();
    send_background_ipc_request(
        &queued,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::cancel_for(&queued),
    )
    .unwrap();

    let cancelled = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(cancelled.process.status, BackgroundJobStatus::Idle);
    assert!(cancelled.identity.pending_prompts.is_empty());
    assert_eq!(cancelled.outcome.summary.as_deref(), Some("turn cancelled"));
    assert!(queued_turn.is_cancelled());
    ipc.cancel.cancel();
}

#[test]
fn streaming_state_churn_does_not_block_turn_cancellation() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 2;
    state.identity.session_id = Some("sess-streaming-cancel".into());
    state.process.pid = Some(std::process::id());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let running_turn = ipc.start_turn();
    // The fence a stop takes while the model is streaming: every appended
    // event bumps `updated_at_ms`, so by the time the cancel is handled the
    // timestamp has always moved on. The turn itself is unchanged, and that —
    // not the timestamp — is what decides whether the cancel may proceed.
    let observed = store.read_state(&state.identity.job_id).unwrap();
    let stale_timestamp_cancel = BackgroundIpcRequest::cancel_for(&observed);
    store
        .update_state(&state.identity.job_id, |current| {
            current.outcome.event_count = current.outcome.event_count.saturating_add(1);
            current.process.updated_at_ms = current.process.updated_at_ms.saturating_add(1_000);
            Ok(())
        })
        .unwrap();

    send_background_ipc_request(
        &observed,
        ipc.port,
        ipc.token.clone(),
        stale_timestamp_cancel,
    )
    .unwrap();

    let cancelled = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(cancelled.process.status, BackgroundJobStatus::Idle);
    assert_eq!(cancelled.outcome.summary.as_deref(), Some("turn cancelled"));
    assert!(running_turn.is_cancelled());
    ipc.cancel.cancel();
}

#[test]
fn cancelling_claimed_live_reply_cancels_the_preinstalled_turn() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("original prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Queued;
    state.identity.session_id = Some("sess-one".into());
    state.identity.pending_prompts = vec![pending_prompt(
        "pp-cancel-claimed",
        "queued follow-up",
        Vec::new(),
    )];
    state.process.pid = Some(std::process::id());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let turn_cancel = ipc.start_turn();
    let WorkerClaim::Claimed(claimed) = claim_background_job(
        &store,
        &state.identity.job_id,
        std::process::id(),
        ipc.port,
        &ipc.token,
        |_| Some(true),
    )
    .unwrap() else {
        panic!("queued reply should be claimed");
    };
    assert_eq!(claimed.process.status, BackgroundJobStatus::Running);
    assert!(claimed.process.turn_generation > 0);
    assert_eq!(pending_text(&claimed), Some("queued follow-up"));

    send_background_ipc_request(
        &claimed,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::cancel_for(&claimed),
    )
    .unwrap();

    let cancelled = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(cancelled.process.status, BackgroundJobStatus::Idle);
    assert!(cancelled.identity.pending_prompts.is_empty());
    assert!(turn_cancel.is_cancelled());

    let finalization = finalize_failed_background_turn(
        &store,
        &claimed,
        &ipc,
        "session setup failed after cancellation",
        "setup failed",
        now_ms(),
    )
    .unwrap();
    assert_eq!(
        finalization,
        BackgroundTurnFinalization::CancelledOrSuperseded
    );
    let after_setup_error = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(after_setup_error.process.status, BackgroundJobStatus::Idle);
    assert!(after_setup_error.outcome.error.is_none());
    assert!(after_setup_error.process.completed_at_ms.is_none());
    let completion = finalize_completed_background_turn(
        &store,
        &claimed,
        &ipc,
        "should not win cancellation",
        now_ms(),
    )
    .unwrap();
    assert_eq!(
        completion,
        BackgroundTurnFinalization::CancelledOrSuperseded
    );
    assert_eq!(
        store
            .read_state(&state.identity.job_id)
            .unwrap()
            .process
            .status,
        BackgroundJobStatus::Idle
    );
    ipc.cancel.cancel();
}

#[test]
fn reply_cannot_cancel_token_installed_for_accepted_follow_up() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Succeeded;
    let ipc = Arc::new(start_background_ipc_server(&store, &state.identity.job_id).unwrap());
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    let old_turn = ipc.start_turn();

    let (queued_tx, queued_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    let reply_store = store.clone();
    let reply_job_id = state.identity.job_id.clone();
    let reply_owner = ipc.owner();
    let reply_turn_cancel = Arc::clone(&ipc.turn_cancel);
    let reply = std::thread::spawn(move || {
        with_current_turn_locked(&reply_turn_cancel, |current_turn| {
            queue_live_background_reply(
                &reply_store,
                &reply_job_id,
                &reply_owner,
                "accepted follow-up".into(),
                Vec::new(),
            )
            .unwrap();
            queued_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            current_turn.cancel();
        });
    });
    queued_rx.recv().unwrap();

    let (claim_started_tx, claim_started_rx) = std::sync::mpsc::sync_channel(0);
    let claim_store = store.clone();
    let claim_job_id = state.identity.job_id.clone();
    let claim_ipc = Arc::clone(&ipc);
    let claim = std::thread::spawn(move || {
        claim_ipc.start_turn_and(|| {
            claim_started_tx.send(()).unwrap();
            claim_background_job(
                &claim_store,
                &claim_job_id,
                std::process::id(),
                claim_ipc.port,
                &claim_ipc.token,
                |_| Some(true),
            )
            .unwrap()
        })
    });
    assert!(claim_started_rx
        .recv_timeout(Duration::from_millis(100))
        .is_err());

    release_tx.send(()).unwrap();
    reply.join().unwrap();
    claim_started_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    let (new_turn, claim_result) = claim.join().unwrap();
    assert!(matches!(claim_result, WorkerClaim::Claimed(_)));
    assert!(old_turn.is_cancelled());
    assert!(!new_turn.is_cancelled());
    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Running);
    assert_eq!(pending_text(&loaded), Some("accepted follow-up"));
    ipc.cancel.cancel();
}

#[test]
fn terminal_parent_ipc_can_cancel_running_remote_subagent() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Succeeded;
    state.identity.session_id = Some("sess-one".into());
    state.process.pid = Some(std::process::id());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let (_scopes, registry) = attach_task_scope(&ipc, "sess-one");
    let task_id = TaskId::new("agent-after-parent");
    let task_cancel = rebon_types::PromptCancel::new();
    registry.insert(
        task_id.clone(),
        running_local_agent_snapshot(task_id.as_str(), "still working"),
        task_cancel.clone(),
    );

    send_background_ipc_request(
        &state,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::CancelTasks {
            task_ids: vec![task_id.to_string(), "already-gone".into()],
        },
    )
    .unwrap();

    assert!(task_cancel.is_cancelled());
    assert_eq!(
        registry.snapshot(&task_id).expect("task snapshot").status,
        TaskStatus::Killed
    );
    let events = registry.session_live_events(None);
    let finished_events = events
        .events
        .iter()
        .filter(|event| matches!(&event.kind, TaskLiveEventKind::Finished { .. }))
        .collect::<Vec<_>>();
    assert_eq!(finished_events.len(), 1);
    assert!(matches!(
        &finished_events[0].kind,
        TaskLiveEventKind::Finished {
            status: TaskStatus::Killed,
            error: None,
        }
    ));
    ipc.cancel.cancel();
}

#[test]
fn single_task_cancel_only_stops_the_requested_remote_subagent() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Succeeded;
    state.identity.session_id = Some("sess-single-cancel".into());
    state.process.pid = Some(std::process::id());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let (_scopes, registry) = attach_task_scope(&ipc, "sess-single-cancel");
    let target_id = TaskId::new("agent-cancel-target");
    let target_cancel = rebon_types::PromptCancel::new();
    registry.insert(
        target_id.clone(),
        running_local_agent_snapshot(target_id.as_str(), "target"),
        target_cancel.clone(),
    );
    let sibling_id = TaskId::new("agent-keep-running");
    let sibling_cancel = rebon_types::PromptCancel::new();
    registry.insert(
        sibling_id.clone(),
        running_local_agent_snapshot(sibling_id.as_str(), "sibling"),
        sibling_cancel.clone(),
    );

    store
        .cancel_background_task(&state.identity.job_id, target_id.to_string())
        .unwrap();

    assert!(target_cancel.is_cancelled());
    assert_eq!(
        registry
            .snapshot(&target_id)
            .expect("target snapshot")
            .status,
        TaskStatus::Killed
    );
    assert!(!sibling_cancel.is_cancelled());
    assert_eq!(
        registry
            .snapshot(&sibling_id)
            .expect("sibling snapshot")
            .status,
        TaskStatus::Running
    );
    ipc.cancel.cancel();
}

/// A row whose worker is gone is exactly what a user presses stop over, and
/// exactly what no live process can act on. Refusing it left the row
/// unstoppable for the rest of the job's life, so the id nothing holds is
/// settled in the store instead — the one place every client projects from.
/// A task that *is* held and has simply finished still reports that honestly.

#[test]
fn single_task_cancel_settles_an_orphan_and_reports_a_finished_task() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Succeeded;
    state.identity.session_id = Some("sess-cancel-errors".into());
    state.process.pid = Some(std::process::id());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let (_scopes, registry) = attach_task_scope(&ipc, "sess-cancel-errors");
    let terminal_id = TaskId::new("agent-already-finished");
    let terminal_cancel = rebon_types::PromptCancel::new();
    let mut terminal_snapshot =
        running_local_agent_snapshot(terminal_id.as_str(), "already finished");
    terminal_snapshot.status = TaskStatus::Completed;
    registry.insert(
        terminal_id.clone(),
        terminal_snapshot,
        terminal_cancel.clone(),
    );

    store
        .cancel_background_task(&state.identity.job_id, "agent-missing".into())
        .expect("stopping a row no worker holds must not fail");
    let settled = store
        .read_events(&state.identity.job_id)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "task_live_batch")
        .filter_map(|event| {
            serde_json::from_value::<rebon_session_host::BackgroundTaskEventBatch>(event.data).ok()
        })
        .flat_map(|batch| batch.events)
        .any(|event| {
            event.task_id == "agent-missing"
                && matches!(
                    &event.event,
                    rebon_session_host::BackgroundTaskEventKind::Finished { status, .. }
                        if status == "killed"
                )
        });
    assert!(
        settled,
        "the orphaned row must be closed out in the store, not just refused"
    );

    let terminal = store
        .cancel_background_task(&state.identity.job_id, terminal_id.to_string())
        .unwrap_err();
    assert_eq!(
        terminal.to_string(),
        "task agent-already-finished is not running (status: completed)"
    );
    assert!(!terminal_cancel.is_cancelled());
    assert_eq!(
        registry
            .snapshot(&terminal_id)
            .expect("terminal snapshot")
            .status,
        TaskStatus::Completed
    );
    ipc.cancel.cancel();
}

#[test]
fn reply_to_completed_job_queues_on_lingering_worker_without_interrupting() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Succeeded;
    state.identity.session_id = Some("sess-one".into());
    state.process.pid = Some(std::process::id());
    state.process.completed_at_ms = Some(now_ms());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    rebon_session_host::reply_to_background_job_in_store(
        &store,
        &state.identity.job_id,
        "follow up".into(),
        false,
        true,
        &rebon_exe(),
    )
    .unwrap();

    std::thread::sleep(Duration::from_millis(100));
    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Queued);
    assert_eq!(pending_text(&loaded), Some("follow up"));
    assert_eq!(loaded.process.pid, Some(std::process::id()));
    assert_eq!(loaded.process.ipc_port, Some(ipc.port));
    assert_eq!(
        loaded.process.ipc_token.as_deref(),
        Some(ipc.token.as_str())
    );
    let events = store.read_events_tail(&state.identity.job_id, 20).unwrap();
    assert!(events.iter().any(|event| event.kind == "reply_queued"));
    ipc.cancel.cancel();
}

#[test]
fn live_ipc_requires_token_and_matching_job() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.identity.session_id = Some("sess-one".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let bad_token = send_background_ipc_request(
        &state,
        ipc.port,
        "wrong-token".into(),
        BackgroundIpcRequest::Reply {
            message: "later".into(),
            images: Vec::new(),
        },
    )
    .unwrap_err();
    assert!(
        bad_token.to_string().contains("authentication failed"),
        "expected the token check's refusal, got: {bad_token}"
    );

    let mut wrong_job = state.clone();
    wrong_job.identity.job_id = "bg-wrong".into();
    let wrong_job_err = send_background_ipc_request(
        &wrong_job,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::Reply {
            message: "later".into(),
            images: Vec::new(),
        },
    )
    .unwrap_err();
    assert!(
        wrong_job_err.to_string().contains("job id mismatch"),
        "expected the fence's refusal, got: {wrong_job_err}"
    );

    ipc.cancel.cancel();
}

#[test]
fn stale_ipc_endpoint_cannot_mutate_replacement_owner_state() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 1;
    state.identity.session_id = Some("sess-stale-endpoint".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    let stale_owner = ipc.owner();
    let replacement_endpoint = BackgroundIpcEndpoint {
        pid: std::process::id(),
        port: ipc.port.wrapping_add(1),
        token: "replacement-endpoint".into(),
    };
    store
        .update_state(&state.identity.job_id, |current| {
            current.process.turn_generation = 2;
            current.process.ipc_port = Some(replacement_endpoint.port);
            current.process.ipc_token = Some(replacement_endpoint.token.clone());
            current.outcome.pending_permission = Some(BackgroundPermissionQuerySnapshot {
                query_id: 91,
                turn_generation: 2,
                endpoint: Some(replacement_endpoint.clone()),
                tool: Some("Bash".into()),
                tool_call_id: Some("replacement-tool".into()),
                session_id: current.identity.session_id.clone(),
                title: None,
                message: None,
                tool_input: None,
                metadata: None,
                options: vec![BackgroundPermissionOptionSnapshot {
                    option_id: "allow_once".into(),
                    label: "Allow once".into(),
                    kind: "AllowOnce".into(),
                }],
            });
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
    let reply = handle_background_ipc_request(
        BackgroundIpcRequest::Reply {
            message: "stale reply".into(),
            images: Vec::new(),
        },
        &store,
        &state.identity.job_id,
        &stale_owner,
        &SessionEventStream::new(),
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
    let (kind, message) = reply.unwrap_err();
    assert_eq!(
        kind,
        rebon_session_host::HostCallError::OwnerFence(message.clone())
    );
    assert!(message.contains("no longer owns"));
    let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
    responses.lock().unwrap().insert(90, cancelled_tx);
    let stale_cancel = handle_background_ipc_request(
        BackgroundIpcRequest::cancel_for(&state),
        &store,
        &state.identity.job_id,
        &stale_owner,
        &SessionEventStream::new(),
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
    assert!(matches!(
        stale_cancel,
        Err((rebon_session_host::HostCallError::OwnerFence(_), _))
    ));
    assert!(turn_cancel.lock().unwrap().is_cancelled());
    assert!(matches!(
        cancelled_rx.blocking_recv().unwrap(),
        PermissionAnswer::Cancelled
    ));

    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    responses.lock().unwrap().insert(91, response_tx);
    let permission = send_background_permission_answer(
        &store,
        &state.identity.job_id,
        &stale_owner,
        &responses,
        &rule_context,
        91,
        1,
        Some("allow_once".into()),
        None,
        None,
    );
    assert!(matches!(
        permission,
        Err((rebon_session_host::HostCallError::OwnerFence(_), _))
    ));

    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.turn_generation, 2);
    assert_eq!(
        loaded.process.ipc_token.as_deref(),
        Some("replacement-endpoint")
    );
    assert!(loaded.identity.pending_prompts.is_empty());
    assert_eq!(
        loaded
            .outcome
            .pending_permission
            .as_ref()
            .and_then(|pending| pending.endpoint.as_ref()),
        Some(&replacement_endpoint)
    );
    ipc.cancel.cancel();
}

/// `Status` is the one round trip a client makes to agree with the owner about
/// everything invariant I4 covers. It must report the mode actually in force,
/// not the copy the job record last published.

#[test]
fn status_reports_what_the_owner_is_actually_running_under() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-status".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let turn = BoundSession::new(rebon_permissions::PermissionMode::Plan);
    turn.attach(&ipc, &store, &state.identity.job_id);

    let snapshot =
        rebon_session_host::request_session_status(&state, ipc.port, ipc.token.clone()).unwrap();

    assert_eq!(snapshot.job_id, state.identity.job_id);
    assert_eq!(snapshot.session_id.as_deref(), Some("sess-status"));
    assert_eq!(snapshot.permission_mode.as_deref(), Some("plan"));
    assert!(snapshot.client_leases.is_empty());
    ipc.cancel.cancel();
}

/// A lease is what keeps an owner alive, so it has to be visible in the record
/// the supervisor and the Agent View read — and renewing must not pile up a
/// second lease for the same client.

#[test]
fn a_lease_is_published_renewed_and_released() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-lease".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    for _ in 0..2 {
        rebon_session_host::send_background_ipc_request(
            &state,
            ipc.port,
            ipc.token.clone(),
            BackgroundIpcRequest::Lease {
                client_id: "tui-1".into(),
                kind: rebon_session_host::ClientLeaseKind::Tui,
            },
        )
        .unwrap();
    }
    let leased = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(
        leased.lease.client_leases.len(),
        1,
        "renewal must not duplicate"
    );
    assert_eq!(leased.lease.client_leases[0].client_id, "tui-1");
    assert!(leased.has_live_client_lease(rebon_session_host::now_ms()));

    rebon_session_host::send_background_ipc_request(
        &state,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::ReleaseLease {
            client_id: "tui-1".into(),
            deliberate: false,
        },
    )
    .unwrap();

    let released = store.read_state(&state.identity.job_id).unwrap();
    assert!(released.lease.client_leases.is_empty());
    // A detach is not a goodbye: the host still lingers for whoever comes back.
    assert!(!released.lease.exit_when_idle);
    ipc.cancel.cancel();
}

/// `/model` used to answer "it applies when the session is next built", which
/// was true and useless: the record changed, the client was told the change
/// landed, and the running session kept the model it had for as long as it
/// lived. The worker now re-reads the model at the top of every turn, the same
/// way it reads the effort level, so both say the same thing about when they
/// take effect.

#[test]
fn a_model_change_takes_effect_on_the_next_turn_like_effort_does() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-model-applies".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let projects = store.root().join("projects");
    for id in ["sess-model-applies", "other-session"] {
        rebon_session::model_selection::claim(&projects, &state.identity.cwd, id).unwrap();
        rebon_session::model_selection::save_manual_model(
            &projects,
            &state.identity.cwd,
            id,
            "mock",
            "auto-selected",
        )
        .unwrap();
    }
    let applies_from = |key: &str, value: &str| -> rebon_session_host::SessionOptionAppliesFrom {
        rebon_session_host::OwnerHandle {
            session_id: "sess-model-applies".into(),
            job_id: Some(state.identity.job_id.clone()),
            pid: std::process::id(),
            port: ipc.port,
            token: ipc.token.clone(),
            surface: rebon_session::SessionOwnerSurface::Worker,
        }
        .set_session_option(key, value)
        .unwrap()
    };

    assert_eq!(
        applies_from("model", "gpt-5.6-luna"),
        rebon_session_host::SessionOptionAppliesFrom::NextTurn,
        "a model the worker re-reads each turn lands on the next one"
    );
    assert_eq!(
        applies_from("effort", "high"),
        rebon_session_host::SessionOptionAppliesFrom::NextTurn,
        "and effort has always said so"
    );
    // The record is still where it lands, because that is where the worker
    // reads it from.
    let written = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(
        written.identity.runtime.model.as_deref(),
        Some("gpt-5.6-luna")
    );
    let choice =
        rebon_session::model_selection::load(&projects, &state.identity.cwd, "sess-model-applies")
            .unwrap()
            .unwrap();
    assert_eq!(choice.model.as_deref(), Some("gpt-5.6-luna"));
    assert_eq!(choice.effort.as_deref(), Some("high"));
    let other =
        rebon_session::model_selection::load(&projects, &state.identity.cwd, "other-session")
            .unwrap()
            .unwrap();
    assert_eq!(other.model.as_deref(), Some("auto-selected"));
    assert!(other.effort.is_none());

    let empty = rebon_session::model_selection::SessionModelSelection::default();
    assert!(rebon_session::model_selection::compare_exchange(
        &projects,
        &state.identity.cwd,
        "sess-model-applies",
        &choice,
        &empty,
    )
    .unwrap());
    store
        .update_state(&state.identity.job_id, |state| {
            state.identity.runtime.provider = None;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        applies_from("model", "manual-during-classification"),
        rebon_session_host::SessionOptionAppliesFrom::NextTurn
    );
    assert!(!rebon_session::model_selection::compare_exchange(
        &projects,
        &state.identity.cwd,
        "sess-model-applies",
        &empty,
        &choice,
    )
    .unwrap());
    let pending =
        rebon_session::model_selection::load(&projects, &state.identity.cwd, "sess-model-applies")
            .unwrap()
            .unwrap();
    assert!(pending.manual_override);
    assert!(pending.model.is_none());
    assert_eq!(
        store
            .read_state(&state.identity.job_id)
            .unwrap()
            .identity
            .runtime
            .model
            .as_deref(),
        Some("manual-during-classification")
    );
    ipc.cancel.cancel();
}

/// A `/exit` is the user saying they are done, and the host should go rather
/// than park for ten minutes holding a plugin and MCP stack for a window that
/// was closed on purpose. Three conditions gate it, and each is tested here.

#[test]
fn a_deliberate_exit_ends_the_linger_but_only_for_a_watched_session() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-linger".into());
    state.lease.placement = rebon_session_host::JobPlacement::Foreground;
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let release = |client_id: &str, deliberate: bool| {
        rebon_session_host::send_background_ipc_request(
            &state,
            ipc.port,
            ipc.token.clone(),
            BackgroundIpcRequest::ReleaseLease {
                client_id: client_id.into(),
                deliberate,
            },
        )
        .unwrap();
    };
    let lease = |client_id: &str| {
        rebon_session_host::send_background_ipc_request(
            &state,
            ipc.port,
            ipc.token.clone(),
            BackgroundIpcRequest::Lease {
                client_id: client_id.into(),
                kind: rebon_session_host::ClientLeaseKind::Tui,
            },
        )
        .unwrap();
    };

    // One of two watchers leaving ends nothing: the other is still here.
    lease("tui-1");
    lease("app-1");
    release("tui-1", true);
    let partial = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(partial.lease.client_leases.len(), 1);
    assert_eq!(
        partial.lease.exit_when_idle, false,
        "a session somebody is still watching must keep its linger"
    );

    // The last watcher saying goodbye on purpose ends it.
    release("app-1", true);
    let done = store.read_state(&state.identity.job_id).unwrap();
    assert!(done.lease.client_leases.is_empty());
    assert!(
        done.lease.exit_when_idle,
        "a deliberate exit stops the linger"
    );
    assert_eq!(
        done.lease.linger_ms, None,
        "the signal must not be written over the job's own linger setting"
    );

    // And somebody arriving takes it back, so the next client does not inherit
    // a session already scheduled to die.
    lease("tui-2");
    let reopened = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(
        reopened.lease.exit_when_idle, false,
        "a client arriving cancels the shutdown the last exit scheduled"
    );

    // A job `/bg` handed off keeps its hour however its watcher leaves: the
    // terminal moving on is exactly what `Background` records.
    store
        .update_state(&state.identity.job_id, |state| {
            state.lease.placement = rebon_session_host::JobPlacement::Background;
            Ok(())
        })
        .unwrap();
    release("tui-2", true);
    let handed_off = store.read_state(&state.identity.job_id).unwrap();
    assert!(handed_off.lease.client_leases.is_empty());
    assert_eq!(
        handed_off.lease.exit_when_idle, false,
        "work handed to the background outlives the terminal that started it"
    );
    ipc.cancel.cancel();
}

/// The whole point of `command_id`: a client that lost its connection retries,
/// and the owner must answer from memory rather than run the command twice.
/// `/compact` retried is a second compaction.

#[test]
fn a_repeated_command_id_is_answered_without_running_again() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-idempotent".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let drain_runs = Arc::clone(&runs);
    let drain_ipc_port = ipc.port;
    let _ = drain_ipc_port;

    let send = |state: &BackgroundJobState, port: u16, token: String| {
        rebon_session_host::send_background_ipc_request_full(
            state,
            port,
            token,
            BackgroundIpcRequest::SetSessionOption {
                key: "effort".into(),
                value: "high".into(),
            },
            Some("cmd-1".into()),
        )
    };

    let first = send(&state, ipc.port, ipc.token.clone()).unwrap();
    // Change the record underneath so a second execution would be visible.
    store
        .update_state(&state.identity.job_id, |current| {
            current.identity.runtime.effort_level = Some("low".into());
            Ok(())
        })
        .unwrap();

    let second = send(&state, ipc.port, ipc.token.clone()).unwrap();

    assert_eq!(
        first.data, second.data,
        "the retry replays the first answer"
    );
    assert_eq!(
        store
            .read_state(&state.identity.job_id)
            .unwrap()
            .identity
            .runtime
            .effort_level
            .as_deref(),
        Some("low"),
        "the retry must not have applied the option a second time"
    );
    assert_eq!(drain_runs.load(std::sync::atomic::Ordering::Relaxed), 0);
    ipc.cancel.cancel();
}

/// RFC-0004 §11.2: the owner publishes the command it last ran, so a client
/// whose connection died between sending and being answered can read the
/// outcome instead of guessing whether to retry. The three fields were in the
/// wire type from the start and the owner wrote `None` into all of them.

#[test]
fn the_owner_publishes_the_command_it_last_ran() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-last-command".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let status = |state: &BackgroundJobState| -> rebon_session_host::SessionStatusSnapshot {
        let response = rebon_session_host::send_background_ipc_request_full(
            state,
            ipc.port,
            ipc.token.clone(),
            BackgroundIpcRequest::Status,
            None,
        )
        .unwrap();
        serde_json::from_value(response.data.expect("status carries a snapshot")).unwrap()
    };

    // Nothing has been asked of it yet.
    let before = status(&state);
    assert_eq!(before.last_command_id, None);
    assert_eq!(before.last_command_at_ms, 0);

    rebon_session_host::send_background_ipc_request_full(
        &state,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::SetSessionOption {
            key: "effort".into(),
            value: "high".into(),
        },
        Some("cmd-applied".into()),
    )
    .unwrap();

    let applied = status(&state);
    assert_eq!(applied.last_command_id.as_deref(), Some("cmd-applied"));
    assert!(applied.last_command_at_ms > 0);
    assert_eq!(applied.last_command_error, None);

    // A refusal is an outcome too, and the one a client most needs to read.
    // The caller sees it as an `Err`; the owner still ran the command and
    // still has to say how it went.
    assert!(rebon_session_host::send_background_ipc_request_full(
        &state,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::SetSessionOption {
            key: "nonsense".into(),
            value: "whatever".into(),
        },
        Some("cmd-refused".into()),
    )
    .is_err());

    let refused = status(&state);
    assert_eq!(refused.last_command_id.as_deref(), Some("cmd-refused"));
    assert!(
        refused.last_command_error.is_some(),
        "a refused command says why: {refused:?}"
    );

    // A `Status` carries no `command_id` of its own, so asking twice does not
    // rewrite the answer the client is looking for.
    assert_eq!(
        status(&state).last_command_id.as_deref(),
        Some("cmd-refused")
    );
    ipc.cancel.cancel();
}

/// A client that found the owner through `<sid>.owner.json` addresses it by
/// session. A worker that is not running that session must say so rather than
/// treat a session-only envelope as a wildcard.

#[test]
fn an_envelope_addressed_by_session_alone_is_routed_and_fenced() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-routed".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let owner = rebon_session_host::OwnerHandle {
        session_id: "sess-routed".into(),
        job_id: None,
        pid: std::process::id(),
        port: ipc.port,
        token: ipc.token.clone(),
        surface: rebon_session::SessionOwnerSurface::Worker,
    };
    assert!(owner.ping(), "the session it is running is reachable by id");

    let wrong = rebon_session_host::OwnerHandle {
        session_id: "sess-somebody-else".into(),
        ..owner.clone()
    };
    let error = wrong
        .send(BackgroundIpcRequest::Ping, None)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("session id mismatch"),
        "unexpected refusal: {error}"
    );
    ipc.cancel.cancel();
}

/// The same routing on a worker that has not bound a session yet: it cannot be
/// the owner a client resolved from a descriptor, and must say so rather than
/// treat a session-addressed envelope as unaddressed.

#[test]
fn a_session_addressed_envelope_is_refused_before_a_session_is_bound() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    assert!(state.identity.session_id.is_none());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let owner = rebon_session_host::OwnerHandle {
        session_id: "sess-not-bound".into(),
        job_id: None,
        pid: std::process::id(),
        port: ipc.port,
        token: ipc.token.clone(),
        surface: rebon_session::SessionOwnerSurface::Worker,
    };

    let error = owner
        .send(BackgroundIpcRequest::Ping, None)
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("not open on this worker"),
        "unexpected refusal: {error}"
    );
    ipc.cancel.cancel();
}

/// The whole point of `Subscribe`: a client sees what the owner is doing as it
/// happens, over a connection that stays open, instead of re-reading a file on
/// a timer. `hello` must arrive first and carry the state the deltas are
/// interpreted against.

#[test]
fn subscribing_streams_hello_then_live_events() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-subscribe".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let owner = rebon_session_host::OwnerHandle {
        session_id: "sess-subscribe".into(),
        job_id: Some(state.identity.job_id.clone()),
        pid: std::process::id(),
        port: ipc.port,
        token: ipc.token.clone(),
        surface: rebon_session::SessionOwnerSurface::Worker,
    };
    let mut stream = owner.subscribe(None).unwrap();

    let hello = stream.next().expect("hello arrives first");
    match hello {
        rebon_session_host::SessionEvent::Hello { status, .. } => {
            assert_eq!(status.job_id, state.identity.job_id);
            assert_eq!(status.session_id.as_deref(), Some("sess-subscribe"));
        }
        other => panic!("expected hello, got {other:?}"),
    }

    // Wait for the owner to register the subscriber before publishing, so the
    // test asserts delivery rather than racing the attach.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while ipc.events.subscriber_count() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "subscriber never attached"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    ipc.events
        .publish_turn(rebon_session_host::TurnStreamState::Running, None);
    ipc.events.publish_turn(
        rebon_session_host::TurnStreamState::Idle,
        Some("end_turn".into()),
    );

    let running = stream.next().expect("the running turn is streamed");
    let idle = stream.next().expect("the idle turn is streamed");
    assert!(matches!(
        running,
        rebon_session_host::SessionEvent::Turn {
            state: rebon_session_host::TurnStreamState::Running,
            ..
        }
    ));
    match idle {
        rebon_session_host::SessionEvent::Turn {
            state: rebon_session_host::TurnStreamState::Idle,
            stop_reason,
            ..
        } => assert_eq!(stop_reason.as_deref(), Some("end_turn")),
        other => panic!("expected an idle turn, got {other:?}"),
    }
    ipc.cancel.cancel();
}

/// A turn's end is two facts on the stream: the `Turn Idle` the worker
/// publishes before it writes the record, and a status snapshot of the
/// record once written — status terminal, `busy` off, the usage total with
/// this turn in it. A client that only had the first showed the previous
/// total, and the previous status, until something unrelated published one.

#[test]
fn finalizing_a_turn_publishes_the_record_it_wrote() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-finalize".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    state.process.status = BackgroundJobStatus::Running;
    state.outcome.usage = Some(rebon_session_host::SessionUsageSnapshot {
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    });
    store.write_state(&state).unwrap();

    let finalization = worker::finalize_background_turn(
        &store,
        &state,
        &ipc,
        worker::BackgroundTurnTerminalOutcome::Completed { summary: "done" },
        1,
    )
    .unwrap();
    assert_eq!(finalization, worker::BackgroundTurnFinalization::Applied);

    let owner = rebon_session_host::OwnerHandle {
        session_id: "sess-finalize".into(),
        job_id: Some(state.identity.job_id.clone()),
        pid: std::process::id(),
        port: ipc.port,
        token: ipc.token.clone(),
        surface: rebon_session::SessionOwnerSurface::Worker,
    };
    let mut stream = owner.subscribe(Some(0)).unwrap();
    assert!(matches!(
        stream.next().unwrap(),
        rebon_session_host::SessionEvent::Hello { .. }
    ));
    match stream.next().expect("the finalized record was published") {
        rebon_session_host::SessionEvent::Status { snapshot, .. } => {
            assert_eq!(snapshot.status, BackgroundJobStatus::Succeeded);
            assert!(!snapshot.busy, "the turn is over on the record too");
            assert_eq!(
                snapshot.usage.map(|usage| usage.input_tokens),
                Some(10),
                "the total a client shows at `idle` includes this turn"
            );
        }
        other => panic!("expected the finalized status, got {other:?}"),
    }
    ipc.cancel.cancel();
}

/// Reconnecting is the ordinary case, not the exceptional one: a client says
/// the cursor it reached and gets exactly what it missed.

#[test]
fn resubscribing_from_a_cursor_replays_the_difference() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-resubscribe".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    ipc.events
        .publish_turn(rebon_session_host::TurnStreamState::Running, None);
    ipc.events.publish_turn(
        rebon_session_host::TurnStreamState::Idle,
        Some("end_turn".into()),
    );

    let owner = rebon_session_host::OwnerHandle {
        session_id: "sess-resubscribe".into(),
        job_id: Some(state.identity.job_id.clone()),
        pid: std::process::id(),
        port: ipc.port,
        token: ipc.token.clone(),
        surface: rebon_session::SessionOwnerSurface::Worker,
    };
    let mut stream = owner.subscribe(Some(1)).unwrap();

    assert!(matches!(
        stream.next().unwrap(),
        rebon_session_host::SessionEvent::Hello { .. }
    ));
    let replayed = stream.next().expect("the missed event is replayed");
    assert_eq!(replayed.cursor(), Some(2), "cursor 1 was already seen");
    ipc.cancel.cancel();
}

/// A client should not have to hand-roll the renewal cadence: the guard holds
/// the lease for as long as it lives and gives it up the moment it does not,
/// so an owner learns its last client left when it leaves rather than a TTL
/// later.

#[test]
fn a_lease_guard_holds_the_lease_and_releases_it_on_drop() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-guard".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let owner = rebon_session_host::OwnerHandle {
        session_id: "sess-guard".into(),
        job_id: Some(state.identity.job_id.clone()),
        pid: std::process::id(),
        port: ipc.port,
        token: ipc.token.clone(),
        surface: rebon_session::SessionOwnerSurface::Worker,
    };

    let guard = owner.hold_lease("tui-guarded", rebon_session_host::ClientLeaseKind::Tui);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let leases = store
            .read_state(&state.identity.job_id)
            .unwrap()
            .lease
            .client_leases;
        if leases.iter().any(|lease| lease.client_id == "tui-guarded") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the guard never took a lease"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // While it is held, the worker's linger deadline sits beyond the lease's
    // own expiry — which is what keeps a watched session's host alive.
    let held = store.read_state(&state.identity.job_id).unwrap();
    let now = rebon_session_host::now_ms();
    assert!(held.has_live_client_lease(now));
    assert!(
        held.linger_deadline_ms(now) > now,
        "a watched session must not be eligible to exit"
    );

    drop(guard);

    let after = store.read_state(&state.identity.job_id).unwrap();
    assert!(
        after.lease.client_leases.is_empty(),
        "dropping the guard gives the lease up instead of waiting out the TTL"
    );
    ipc.cancel.cancel();
}

/// The terminal's attachment is what holds the lease: attaching takes it,
/// letting the attachment go — leaving the session, exiting, stopping the
/// worker — releases it. Nothing else has to remember to.

#[test]
fn a_mirror_takes_the_owners_session_usage_from_its_stream() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-usage".into());
    state.outcome.usage = Some(rebon_session_host::SessionUsageSnapshot {
        input_tokens: 4_242,
        output_tokens: 17,
        cache_read_tokens: 9,
        cache_creation_tokens: 3,
    });
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let endpoint = rebon_session_host::BackgroundIpcEndpoint {
        pid: std::process::id(),
        port: ipc.port,
        token: ipc.token.clone(),
    };
    let events = session_event_stream("sess-usage", &state.identity.job_id, &endpoint)
        .expect("the owner serves a stream");

    // `hello` carries the same snapshot `Status` would, which is what a
    // client attaching mid-session needs before it renders anything.
    let hello = events
        .recv_timeout(Duration::from_secs(5))
        .expect("hello arrives");
    let rebon_session_host::SessionEvent::Hello { status, .. } = hello else {
        panic!("expected hello, got {hello:?}");
    };
    let usage = status.usage.expect("the owner publishes session usage");
    assert_eq!(usage.input_tokens, 4_242);
    assert_eq!(usage.output_tokens, 17);
    ipc.cancel.cancel();
}

/// A worker that predates the stream — or one that is simply gone — must not
/// break a client that asks for one, and must not make it wait: the channel
/// closes and everything keeps arriving the way it always did.

#[test]
fn a_stream_that_cannot_be_opened_closes_instead_of_blocking() {
    let endpoint = rebon_session_host::BackgroundIpcEndpoint {
        pid: std::process::id(),
        // Nothing is listening here.
        port: 1,
        token: "no-such-owner".into(),
    };

    let events = session_event_stream("sess-absent", "job-absent", &endpoint)
        .expect("the client always gets a channel; the connection happens off-thread");

    assert!(matches!(
        events.recv_timeout(Duration::from_secs(5)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    ));
}

/// `/model` and `/effort` change what the *session* runs under. A mirrored
/// terminal that applied them to its own configuration would show one model
/// while the worker kept using another — and the user would believe the
/// change had landed. They go to the owner, which is the only thing that can
/// actually make them true.

/// The owner's MCP snapshot rides on `Status`, and a change is pushed to
/// subscribers once — repeating the same snapshot says nothing new.
#[test]
fn the_owner_mcp_snapshot_rides_on_status_and_is_pushed_once_per_change() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-mcp".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let before =
        rebon_session_host::request_session_status(&state, ipc.port, ipc.token.clone()).unwrap();
    assert!(before.mcp.is_none(), "nothing published yet");

    let subscription = ipc.events.subscribe(None);
    let snapshot = rebon_session_host::McpStatusSnapshot {
        loader: "ready".into(),
        client: "ready".into(),
        servers: vec![rebon_session_host::McpServerSnapshot {
            name: "fixture".into(),
            transport: "stdio".into(),
            source: "project".into(),
        }],
        tools: vec![rebon_session_host::McpToolSnapshot {
            name: "mcp__fixture__ping".into(),
            tokens: 12,
        }],
        warnings: Vec::new(),
        error: None,
    };
    ipc.publish_mcp_status(&store, &state.identity.job_id, Some(snapshot.clone()));
    ipc.publish_mcp_status(&store, &state.identity.job_id, Some(snapshot.clone()));

    let after =
        rebon_session_host::request_session_status(&state, ipc.port, ipc.token.clone()).unwrap();
    assert_eq!(after.mcp.as_ref(), Some(&snapshot));

    let pushed = subscription
        .events
        .try_iter()
        .map(|line| serde_json::from_str::<rebon_session_host::SessionEvent>(&line).unwrap())
        .filter(|event| {
            matches!(
                event,
                rebon_session_host::SessionEvent::Status { snapshot: status, .. }
                    if status.mcp.as_ref() == Some(&snapshot)
            )
        })
        .count();
    assert_eq!(pushed, 1, "one change, one status event");
    ipc.cancel.cancel();
}

#[test]
fn a_stop_hook_that_blocks_keeps_the_turn_running_and_says_why() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("original prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.identity.session_id = Some("sess-one".into());
    state.process.pid = Some(std::process::id());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    let turn = ipc.start_turn();
    ipc.attach_stop_gate(Arc::new(|_reason: &str| Err("keep going".to_string())));

    let refused = send_background_ipc_request(
        &state,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::cancel_for(&state),
    );
    // A refused request comes back as the client's error, reason and all.
    let said = refused
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(
        said.contains("keep going"),
        "the hook reason reaches the client: {said}"
    );
    let still = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(still.process.status, BackgroundJobStatus::Running);
    assert!(!turn.is_cancelled(), "the turn keeps running");

    // The gate consulted and letting it through: the cancel lands as before.
    ipc.attach_stop_gate(Arc::new(|_reason: &str| Ok(())));
    let cancelled = send_background_ipc_request(
        &state,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::cancel_for(&state),
    );
    assert!(cancelled.is_ok(), "{cancelled:?}");
    assert_eq!(
        store
            .read_state(&state.identity.job_id)
            .unwrap()
            .process
            .status,
        BackgroundJobStatus::Idle
    );
    assert!(turn.is_cancelled());
    ipc.cancel.cancel();
}

/// A permission raised before a client subscribes still reaches it.
///
/// This is the deterministic version of a race that showed up once as a flaky
/// `three_clients_share_one_permission_and_the_first_answer_wins`: a fresh
/// subscriber is given no replay on purpose, because it reads history from the
/// transcript, and a permission that was raised before it attached is
/// therefore in nothing it will be sent. The question is not history -- a tool
/// is blocked on it -- so the owner restates it once to the new subscriber.
///
/// Written the other way round from the flaky one on purpose: the permission
/// is in the record *before* anybody subscribes, so there is no window to race
/// and nothing to wait for.
#[test]
fn a_permission_raised_before_a_client_subscribes_still_reaches_it() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-late-attach".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    state.outcome.pending_permission = Some(BackgroundPermissionQuerySnapshot {
        query_id: 77,
        turn_generation: 0,
        endpoint: Some(ipc.owner().endpoint.clone()),
        tool: Some("Bash".into()),
        tool_call_id: Some("call-1".into()),
        session_id: Some("sess-late-attach".into()),
        title: Some("Run a command".into()),
        message: None,
        tool_input: None,
        metadata: None,
        options: Vec::new(),
    });
    store.write_state(&state).unwrap();

    let owner = rebon_session_host::OwnerHandle {
        session_id: "sess-late-attach".into(),
        job_id: Some(state.identity.job_id.clone()),
        pid: std::process::id(),
        port: ipc.port,
        token: ipc.token.clone(),
        surface: rebon_session::SessionOwnerSurface::Worker,
    };
    let mut stream = owner.subscribe(None).unwrap();

    // `hello` carries the snapshot, which already names the pending
    // permission -- but a client that only watches for the event would never
    // act on it, and every consumer of this stream watches for the event.
    match stream.next().expect("hello arrives first") {
        rebon_session_host::SessionEvent::Hello { status, .. } => {
            assert_eq!(
                status.pending_permission.map(|query| query.query_id),
                Some(77)
            );
        }
        other => panic!("expected hello, got {other:?}"),
    }

    match stream.next().expect("the pending permission is restated") {
        rebon_session_host::SessionEvent::Permission { query, .. } => {
            assert_eq!(query.query_id, 77);
            assert_eq!(query.tool.as_deref(), Some("Bash"));
        }
        other => panic!("expected the pending permission, got {other:?}"),
    }
}

/// And it is restated once, not twice: a client that resumes from a cursor old
/// enough to replay the permission has already been told. Two questions with
/// one id would be raised twice by any consumer that is not idempotent about
/// them, and the terminal mirror is not.
#[test]
fn a_replayed_permission_is_not_restated_on_top_of_the_replay() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-replayed".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    let query = BackgroundPermissionQuerySnapshot {
        query_id: 78,
        turn_generation: 0,
        endpoint: Some(ipc.owner().endpoint.clone()),
        tool: Some("Bash".into()),
        tool_call_id: Some("call-2".into()),
        session_id: Some("sess-replayed".into()),
        title: None,
        message: None,
        tool_input: None,
        metadata: None,
        options: Vec::new(),
    };
    state.outcome.pending_permission = Some(query.clone());
    store.write_state(&state).unwrap();

    // Published, so it is in the ring, and then resumed from before it: the
    // replay carries it.
    ipc.events.publish_permission(query);

    let owner = rebon_session_host::OwnerHandle {
        session_id: "sess-replayed".into(),
        job_id: Some(state.identity.job_id.clone()),
        pid: std::process::id(),
        port: ipc.port,
        token: ipc.token.clone(),
        surface: rebon_session::SessionOwnerSurface::Worker,
    };
    let mut stream = owner.subscribe(Some(0)).unwrap();

    assert!(matches!(
        stream.next().expect("hello arrives first"),
        rebon_session_host::SessionEvent::Hello { .. }
    ));
    match stream.next().expect("the replayed permission") {
        rebon_session_host::SessionEvent::Permission { query, .. } => {
            assert_eq!(query.query_id, 78);
        }
        other => panic!("expected the replayed permission, got {other:?}"),
    }

    // Publishing something else proves the stream moved on rather than
    // carrying a second copy of the permission.
    ipc.events
        .publish_turn(rebon_session_host::TurnStreamState::Idle, None);
    match stream.next().expect("the next event") {
        rebon_session_host::SessionEvent::Turn { .. } => {}
        rebon_session_host::SessionEvent::Permission { query, .. } => {
            panic!("the permission was sent twice: {}", query.query_id)
        }
        other => panic!("expected the turn, got {other:?}"),
    }
}

/// How long one legacy round trip costs on this machine.
///
/// Not a benchmark that guards anything -- it is `#[ignore]`d and prints
/// rather than asserts. It exists so a question like "did the ACP probe layer
/// make the hosted attach path slower" is answered with a number measured
/// here, on the same machine, in the same run, rather than with an argument
/// about syscalls.
///
/// Run with `cargo test -p rebon-session-runtime -- --ignored --nocapture
/// legacy_round_trip_cost`.
#[test]
#[ignore = "a measurement, not an assertion"]
fn legacy_round_trip_cost() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-cost".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let owner = rebon_session_host::OwnerHandle::for_worker(
        "sess-cost",
        Some(&state.identity.job_id),
        &ipc.owner().endpoint,
    );

    // Warm the path: the first connection pays for whatever the OS lazily
    // sets up, and reporting that as the per-call cost would overstate it.
    for _ in 0..20 {
        owner
            .send(rebon_session_host::BackgroundIpcRequest::Ping, None)
            .expect("ping");
    }

    let rounds = 200;
    let started = std::time::Instant::now();
    for _ in 0..rounds {
        owner
            .send(rebon_session_host::BackgroundIpcRequest::Ping, None)
            .expect("ping");
    }
    let elapsed = started.elapsed();
    println!(
        "legacy ping round trip: {rounds} calls in {:?} = {:?} each",
        elapsed,
        elapsed / rounds
    );

    // `Status` reads the job record, which is what the hosted attach path
    // actually does on the way to answering `session/load`.
    let started = std::time::Instant::now();
    for _ in 0..rounds {
        owner.status().expect("status");
    }
    let elapsed = started.elapsed();
    println!(
        "legacy status round trip: {rounds} calls in {:?} = {:?} each",
        elapsed,
        elapsed / rounds
    );
}

/// Shutting down with nobody connected ends the accept thread promptly.
///
/// It used to end by timing out a poll. Now it ends because shutdown connects
/// to the listener to wake it, and the thread checks the flag the moment
/// accept returns.
///
/// The thread reports on itself, because the socket cannot report on it: a
/// listening socket completes connections in the kernel backlog whether or not
/// anyone calls `accept`, so a connect succeeding says nothing about whether
/// the thread is still there. The first version of this test asked the socket
/// and passed with the wake-up removed.
#[test]
fn shutting_down_with_no_clients_ends_the_accept_thread_promptly() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    assert!(
        ipc.accept_thread_running(),
        "the accept thread should be serving before anything stops it"
    );

    ipc.stop();

    let deadline = std::time::Instant::now() + Duration::from_millis(100);
    while ipc.accept_thread_running() {
        assert!(
            std::time::Instant::now() < deadline,
            "the accept thread was still in its loop 100 ms after stop"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A new connection is accepted immediately, not on the next tick of a poll.
///
/// This is the number the whole change is about. A polling listener made every
/// connection wait up to its interval before anything it asked for began, and
/// a hosted call is one connection, so a chain of them stretched into seconds.
///
/// The median of twenty rather than one sample: a loaded machine can stall any
/// single connection, and a test that failed on that would be reporting the
/// machine rather than the code. The median has to move for this to fail, and
/// a return to polling would move it to tens of milliseconds.
#[test]
fn a_new_connection_is_accepted_without_waiting_for_a_poll() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-accept-latency".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let owner = rebon_session_host::OwnerHandle::for_worker(
        "sess-accept-latency",
        Some(&state.identity.job_id),
        &ipc.owner().endpoint,
    );
    // Warm whatever the OS sets up lazily, so the samples measure the server.
    for _ in 0..5 {
        owner
            .send(rebon_session_host::BackgroundIpcRequest::Ping, None)
            .expect("ping");
    }

    let mut samples: Vec<Duration> = (0..20)
        .map(|_| {
            let started = std::time::Instant::now();
            owner
                .send(rebon_session_host::BackgroundIpcRequest::Ping, None)
                .expect("ping");
            started.elapsed()
        })
        .collect();
    samples.sort();
    let median = samples[samples.len() / 2];
    assert!(
        median < Duration::from_millis(5),
        "a fresh connection's round trip has a median of {median:?}; \
         it was 50 ms when the listener was polled, and this test exists to \
         keep it from going back"
    );
}

/// A Stop hook that keeps a turn running tells everybody watching, not only
/// whoever pressed stop.
///
/// The synchronous answer to the caller is unchanged and still says it. What
/// is new is that the reason also travels on the stream, which is the line
/// every client shares -- and the only line an ACP client has, because
/// `session/cancel` is a standard notification with no answer to carry one.
#[test]
fn a_refused_cancel_tells_every_subscriber_why() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 1;
    state.identity.session_id = Some("sess-refused-cancel".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    ipc.attach_stop_gate(std::sync::Arc::new(|_| {
        Err("a file is still being written".to_string())
    }));

    let owner = rebon_session_host::OwnerHandle::for_worker(
        "sess-refused-cancel",
        Some(&state.identity.job_id),
        &ipc.owner().endpoint,
    );
    // Two subscribers, because "everybody watching" is the claim.
    let mut first = owner.subscribe(None).unwrap();
    let mut second = owner.subscribe(None).unwrap();
    assert!(matches!(
        first.next(),
        Some(rebon_session_host::SessionEvent::Hello { .. })
    ));
    assert!(matches!(
        second.next(),
        Some(rebon_session_host::SessionEvent::Hello { .. })
    ));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while ipc.events.subscriber_count() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "both subscribers never attached"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let refusal = rebon_session_host::send_background_ipc_request(
        &state,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::cancel_for(&state),
    )
    .unwrap_err()
    .to_string();
    // The caller still hears it, byte for byte as it always did.
    assert!(
        refusal.contains("Stop hook kept the turn running: a file is still being written"),
        "the asker was not told: {refusal}"
    );

    for (which, stream) in [("first", &mut first), ("second", &mut second)] {
        let event = stream.next().expect("a refusal reaches the stream");
        match event {
            rebon_session_host::SessionEvent::Turn {
                state,
                stop_refused: Some(reason),
                ..
            } => {
                assert_eq!(
                    state,
                    rebon_session_host::TurnStreamState::Running,
                    "the turn is still running, and the event has to say so"
                );
                assert!(
                    reason.contains("a file is still being written"),
                    "{which} subscriber got a refusal without the reason: {reason}"
                );
            }
            other => panic!("{which} subscriber expected a refused turn, got {other:?}"),
        }
    }
    ipc.cancel.cancel();
}
