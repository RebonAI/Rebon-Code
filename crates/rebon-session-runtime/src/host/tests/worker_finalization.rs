use super::super::*;
use super::support::*;
use rebon_core::query::{AttachmentPollPhase, AttachmentPollRequest};

fn poll_request<'a>(
    session_id: &'a str,
    turn_id: &'a str,
    next_iteration: u64,
) -> AttachmentPollRequest<'a> {
    AttachmentPollRequest::new(
        session_id,
        turn_id,
        next_iteration,
        AttachmentPollPhase::Regular,
    )
}

#[derive(Debug, Clone, Copy)]
enum TestFinalizerOutcome {
    Cancelled,
    Completed,
    Failed,
}

fn invoke_test_finalizer(
    outcome: TestFinalizerOutcome,
    store: &BackgroundStore,
    state: &BackgroundJobState,
    ipc: &BackgroundIpcServer,
    completed_at: u64,
) -> anyhow::Result<BackgroundTurnFinalization> {
    let (outcome, transcript_completed) = match outcome {
        TestFinalizerOutcome::Cancelled => (BackgroundTurnTerminalOutcome::Cancelled, false),
        TestFinalizerOutcome::Completed => (
            BackgroundTurnTerminalOutcome::Completed {
                summary: "completed summary",
            },
            true,
        ),
        TestFinalizerOutcome::Failed => (
            BackgroundTurnTerminalOutcome::Failed {
                message: "failed message",
                summary: "failed summary",
            },
            true,
        ),
    };
    finalize_background_turn_with_completion_evidence(
        store,
        state,
        ipc,
        outcome,
        completed_at,
        transcript_completed,
    )
}

/// A finished turn has to land in two places, and they are not
/// interchangeable: the session's ledger carries every counter and is what
/// `/cost` answers from when the command runs in this worker, while the job
/// record's snapshot carries the four a mirroring client reads. Writing one
/// and forgetting the other is how a hosted session came to report zeros.
#[test]
fn claim_background_job_takes_unowned_job() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();

    let claim = claim_background_job(&store, &job.identity.job_id, 4321, 7777, "tok", |_| {
        Some(false)
    })
    .unwrap();

    let WorkerClaim::Claimed(state) = claim else {
        panic!("expected Claimed, got {claim:?}");
    };
    assert_eq!(state.process.pid, Some(4321));
    assert_eq!(state.process.status, BackgroundJobStatus::Running);
    assert_eq!(state.process.turn_generation, 1);
    assert_eq!(state.process.ipc_port, Some(7777));
    assert_eq!(state.process.ipc_token.as_deref(), Some("tok"));
}

#[test]
fn claim_background_job_does_not_reactivate_cancelled_idle_job() {
    let (_dir, store) = store();
    let job = store
        .create_job("original prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    store
        .update_state(&job.identity.job_id, |current| {
            current.process.status = BackgroundJobStatus::Idle;
            current.identity.session_id = Some("sess-one".into());
            current.process.pid = Some(4321);
            current.clear_pending_prompts();
            Ok(())
        })
        .unwrap();

    let claim = claim_background_job(&store, &job.identity.job_id, 4321, 7777, "tok", |_| {
        Some(true)
    })
    .unwrap();

    assert_eq!(claim, WorkerClaim::NoWork);
    let persisted = store.read_state(&job.identity.job_id).unwrap();
    assert_eq!(persisted.process.status, BackgroundJobStatus::Idle);
    assert!(persisted.identity.pending_prompts.is_empty());
}

#[test]
fn injected_pending_prompt_uses_existing_completion_finalizer() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 2;
    state.identity.session_id = Some("sess-injected-complete".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    state.identity.pending_prompts = vec![
        pending_prompt("pp-injected-complete-a", "finish a", Vec::new()),
        pending_prompt("pp-injected-complete-b", "finish b", Vec::new()),
    ];
    store.write_state(&state).unwrap();
    let poller = BackgroundPendingPromptPoller::new(store.clone(), &state, &ipc);
    assert_eq!(
        poller
            .poll(poll_request("sess-injected-complete", "turn-2", 1,))
            .len(),
        2
    );

    let claimed = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(claimed_pending_prompts(&claimed).len(), 2);
    assert!(mark_claimed_pending_prompts_completed(&store, &claimed, &ipc).unwrap());
    assert!(store
        .read_state(&state.identity.job_id)
        .unwrap()
        .identity
        .pending_prompts
        .iter()
        .all(|prompt| prompt.completed_turn_generation == Some(state.process.turn_generation)));
    let finalization = finalize_background_turn_with_completion_evidence(
        &store,
        &claimed,
        &ipc,
        BackgroundTurnTerminalOutcome::Completed { summary: "done" },
        now_ms(),
        true,
    )
    .unwrap();

    assert_eq!(finalization, BackgroundTurnFinalization::Applied);
    let completed = store.read_state(&state.identity.job_id).unwrap();
    assert!(completed.identity.pending_prompts.is_empty());
    assert_eq!(completed.process.status, BackgroundJobStatus::Succeeded);
    ipc.cancel.cancel();
}

#[test]
fn injected_pending_prompt_failure_keeps_durable_resume_claim() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 5;
    state.identity.session_id = Some("sess-injected-retry".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    state.identity.pending_prompts = vec![
        pending_prompt("pp-injected-retry-a", "do not lose a", Vec::new()),
        pending_prompt("pp-injected-retry-b", "do not lose b", Vec::new()),
    ];
    store.write_state(&state).unwrap();
    let poller = BackgroundPendingPromptPoller::new(store.clone(), &state, &ipc);
    assert_eq!(
        poller
            .poll(poll_request("sess-injected-retry", "turn-5", 1))
            .len(),
        2
    );

    let claimed = store.read_state(&state.identity.job_id).unwrap();
    let finalization = finalize_background_turn_with_completion_evidence(
        &store,
        &claimed,
        &ipc,
        BackgroundTurnTerminalOutcome::Failed {
            message: "provider disconnected",
            summary: "failed",
        },
        now_ms(),
        false,
    )
    .unwrap();

    assert_eq!(finalization, BackgroundTurnFinalization::Applied);
    let failed = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(failed.process.status, BackgroundJobStatus::Failed);
    assert_eq!(failed.identity.pending_prompts.len(), 2);
    assert_eq!(failed.identity.pending_prompts[0].id, "pp-injected-retry-a");
    assert_eq!(failed.identity.pending_prompts[1].id, "pp-injected-retry-b");
    assert!(failed.identity.pending_prompts.iter().all(|prompt| {
        prompt.claimed_turn_generation == Some(state.process.turn_generation)
            && prompt.completed_turn_generation.is_none()
    }));
    ipc.cancel.cancel();
}

#[tokio::test]
async fn terminal_worker_exits_without_releasing_ownership_before_process_exit() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Idle;
    state.identity.session_id = Some("sess-terminal-owner".into());
    state.process.completed_at_ms = Some(now_ms().saturating_sub(TERMINAL_WORKER_IDLE_TTL_MS));
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    assert!(!wait_for_terminal_worker_reuse(
        &store,
        &state.identity.job_id,
        &ipc,
        &mut Default::default()
    )
    .await
    .unwrap());

    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.pid, Some(std::process::id()));
    assert_eq!(loaded.process.pid_identity, ipc.pid_identity);
    assert_eq!(loaded.process.ipc_port, Some(ipc.port));
    assert_eq!(
        loaded.process.ipc_token.as_deref(),
        Some(ipc.token.as_str())
    );
    ipc.cancel.cancel();
}

/// A worker with a client on it is not idle in the sense the linger
/// measures: the lease pushes its deadline out for as long as it is
/// renewed, and only once it is released does the clock run — so a
/// terminal sitting on a hosted session does not watch its host leave and
/// come back every ten minutes.

#[tokio::test]
async fn a_client_lease_keeps_the_worker_past_its_linger_and_dropping_it_lets_it_leave() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Idle;
    state.identity.session_id = Some("sess-leased-linger".into());
    state.process.completed_at_ms = Some(now_ms());
    // Short enough to expire inside the test; the lease is what has to
    // keep the worker past it.
    state.lease.linger_ms = Some(1_500);
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let owner = rebon_session_host::OwnerHandle {
        session_id: "sess-leased-linger".into(),
        job_id: Some(state.identity.job_id.clone()),
        pid: std::process::id(),
        port: ipc.port,
        token: ipc.token.clone(),
        surface: rebon_session::SessionOwnerSurface::Worker,
    };
    let guard = owner.hold_lease("tui-watching", rebon_session_host::ClientLeaseKind::Tui);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !store
        .read_state(&state.identity.job_id)
        .unwrap()
        .has_live_client_lease(now_ms())
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the guard never took a lease"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let held = tokio::time::timeout(
        Duration::from_secs(4),
        wait_for_terminal_worker_reuse(
            &store,
            &state.identity.job_id,
            &ipc,
            &mut Default::default(),
        ),
    )
    .await;
    assert!(
        held.is_err(),
        "a watched worker must still be lingering well past its own linger"
    );

    drop(guard);

    let left = tokio::time::timeout(
        Duration::from_secs(6),
        wait_for_terminal_worker_reuse(
            &store,
            &state.identity.job_id,
            &ipc,
            &mut Default::default(),
        ),
    )
    .await
    .expect("the worker leaves once nobody holds it")
    .unwrap();
    assert!(
        !left,
        "nothing was queued for it; it leaves rather than reuses"
    );
    ipc.cancel.cancel();
}

#[test]
fn parent_publish_match_requires_the_complete_preclaim_owner_snapshot() {
    let identity = Some("worker-identity".to_string());
    let owner = RecordedOwnerSnapshot::owned(4321, identity.clone(), false, false, None, None, 7);
    assert!(parent_published_this_worker(
        &owner, 7, 4321, &identity, false
    ));

    let mut changed = owner.clone();
    changed.ipc_port = Some(41000);
    assert!(!parent_published_this_worker(
        &changed, 7, 4321, &identity, false
    ));
    changed = owner.clone();
    changed.owner_detached_group = true;
    assert!(!parent_published_this_worker(
        &changed, 7, 4321, &identity, false
    ));
    changed = owner;
    changed.turn_generation = 8;
    assert!(!parent_published_this_worker(
        &changed, 7, 4321, &identity, false
    ));
}

#[test]
fn claim_background_job_yields_to_live_owner() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    store
        .update_state(&job.identity.job_id, |current| {
            current.process.pid = Some(999);
            current.process.status = BackgroundJobStatus::Running;
            Ok(())
        })
        .unwrap();

    let claim = claim_background_job(&store, &job.identity.job_id, 4321, 7777, "tok", |pid| {
        Some(pid == 999)
    })
    .unwrap();

    assert_eq!(claim, WorkerClaim::OwnedByOther(999));
    // The losing claimer must not have stomped the owner's state.
    let stored = store.read_state(&job.identity.job_id).unwrap();
    assert_eq!(stored.process.pid, Some(999));
}

#[test]
fn claim_background_job_overwrites_dead_owner() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    store
        .update_state(&job.identity.job_id, |current| {
            current.process.pid = Some(999);
            Ok(())
        })
        .unwrap();

    let claim = claim_background_job(&store, &job.identity.job_id, 4321, 7777, "tok", |_| {
        Some(false)
    })
    .unwrap();

    let WorkerClaim::Claimed(state) = claim else {
        panic!("expected Claimed, got {claim:?}");
    };
    assert_eq!(state.process.pid, Some(4321));
}

#[test]
fn claim_background_job_reclaims_own_pid() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    store
        .update_state(&job.identity.job_id, |current| {
            current.process.pid = Some(4321);
            Ok(())
        })
        .unwrap();

    // A worker re-entering its own loop (reuse) must not yield to
    // itself, regardless of what liveness reports.
    let claim = claim_background_job(&store, &job.identity.job_id, 4321, 7777, "tok", |_| {
        Some(true)
    })
    .unwrap();

    assert!(matches!(claim, WorkerClaim::Claimed(_)));
}

#[test]
fn claim_background_job_respects_stopped() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    store
        .update_state(&job.identity.job_id, |current| {
            current.process.status = BackgroundJobStatus::Stopped;
            Ok(())
        })
        .unwrap();

    let claim = claim_background_job(&store, &job.identity.job_id, 4321, 7777, "tok", |_| {
        Some(false)
    })
    .unwrap();

    assert_eq!(claim, WorkerClaim::Stopped);
}

#[test]
fn unified_terminal_finalizer_preserves_all_outcomes_with_and_without_follow_up() {
    for outcome in [
        TestFinalizerOutcome::Cancelled,
        TestFinalizerOutcome::Completed,
        TestFinalizerOutcome::Failed,
    ] {
        for has_follow_up in [false, true] {
            let (_dir, store) = store();
            let mut state = store
                .create_job("prompt".into(), PathBuf::from("."), runtime())
                .unwrap();
            state.process.status = BackgroundJobStatus::Running;
            state.process.turn_generation = 7;
            state.identity.session_id = Some(format!("sess-finalizer-{outcome:?}-{has_follow_up}"));
            let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
            install_ipc_owner(&mut state, &ipc);
            let mut claimed = pending_prompt("pp-finalizer-claimed", "claimed", Vec::new());
            claimed.claimed_turn_generation = Some(state.process.turn_generation);
            if !matches!(outcome, TestFinalizerOutcome::Cancelled) {
                claimed.completed_turn_generation = Some(state.process.turn_generation);
            }
            state.identity.pending_prompts = vec![claimed];
            if has_follow_up {
                state.identity.pending_prompts.push(pending_prompt(
                    "pp-finalizer-follow-up",
                    "follow-up",
                    Vec::new(),
                ));
            }
            state.outcome.summary = Some("old summary".into());
            state.outcome.summary_updated_at_ms = Some(1);
            state.process.completed_at_ms = Some(2);
            state.outcome.exit_code = Some(99);
            state.outcome.error = Some("old error".into());
            store.write_state(&state).unwrap();
            let completed_at = now_ms();

            let finalization =
                invoke_test_finalizer(outcome, &store, &state, &ipc, completed_at).unwrap();
            let finalized = store.read_state(&state.identity.job_id).unwrap();
            assert!(finalized.outcome.summary_updated_at_ms.is_none());
            assert_eq!(finalized.process.updated_at_ms, completed_at);
            if has_follow_up {
                assert_eq!(finalization, BackgroundTurnFinalization::Queued);
                assert_eq!(finalized.process.status, BackgroundJobStatus::Queued);
                assert_eq!(finalized.identity.pending_prompts.len(), 1);
                assert_eq!(
                    finalized.identity.pending_prompts[0].id,
                    "pp-finalizer-follow-up"
                );
                assert_eq!(
                    finalized.outcome.summary.as_deref(),
                    Some(match outcome {
                        TestFinalizerOutcome::Cancelled => {
                            "queued follow-up after cancelled turn"
                        }
                        TestFinalizerOutcome::Completed => {
                            "queued follow-up after completed turn"
                        }
                        TestFinalizerOutcome::Failed => {
                            "queued follow-up after failed turn"
                        }
                    })
                );
                assert!(finalized.process.completed_at_ms.is_none());
                assert!(finalized.outcome.exit_code.is_none());
                assert!(finalized.outcome.error.is_none());
            } else {
                assert_eq!(finalization, BackgroundTurnFinalization::Applied);
                assert!(finalized.identity.pending_prompts.is_empty());
                match outcome {
                    TestFinalizerOutcome::Cancelled => {
                        assert_eq!(finalized.process.status, BackgroundJobStatus::Idle);
                        assert_eq!(finalized.outcome.summary.as_deref(), Some("turn cancelled"));
                        assert!(finalized.process.completed_at_ms.is_none());
                        assert!(finalized.outcome.exit_code.is_none());
                        assert!(finalized.outcome.error.is_none());
                    }
                    TestFinalizerOutcome::Completed => {
                        assert_eq!(finalized.process.status, BackgroundJobStatus::Succeeded);
                        assert_eq!(
                            finalized.outcome.summary.as_deref(),
                            Some("completed summary")
                        );
                        assert_eq!(finalized.process.completed_at_ms, Some(completed_at));
                        assert_eq!(finalized.outcome.exit_code, Some(0));
                        assert!(finalized.outcome.error.is_none());
                    }
                    TestFinalizerOutcome::Failed => {
                        assert_eq!(finalized.process.status, BackgroundJobStatus::Failed);
                        assert_eq!(finalized.outcome.summary.as_deref(), Some("failed summary"));
                        assert_eq!(finalized.process.completed_at_ms, Some(completed_at));
                        assert_eq!(finalized.outcome.exit_code, Some(1));
                        assert_eq!(finalized.outcome.error.as_deref(), Some("failed message"));
                    }
                }
            }
            ipc.cancel.cancel();
        }
    }
}

#[test]
fn failed_turn_preserves_claim_without_terminal_transcript_even_with_completion_marker() {
    let (_dir, store) = store();
    let transcript_dir = tempfile::tempdir().unwrap();
    let mut state = store
        .create_job(
            "prompt".into(),
            transcript_dir.path().to_path_buf(),
            runtime(),
        )
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 9;
    state.identity.session_id = Some("sess-uncompleted-failure".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    let mut claimed = pending_prompt("pp-uncompleted", "retry me", Vec::new());
    claimed.claimed_turn_generation = Some(state.process.turn_generation);
    claimed.completed_turn_generation = Some(state.process.turn_generation - 1);
    let follow_up = pending_prompt("pp-after-uncompleted", "after", Vec::new());
    state.identity.pending_prompts = vec![claimed.clone(), follow_up.clone()];
    store.write_state(&state).unwrap();

    let completed_at = now_ms();
    assert_eq!(
        finalize_failed_background_turn(
            &store,
            &state,
            &ipc,
            "session build failed",
            "failed summary",
            completed_at,
        )
        .unwrap(),
        BackgroundTurnFinalization::Applied
    );
    let failed = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(failed.process.status, BackgroundJobStatus::Failed);
    assert_eq!(failed.identity.pending_prompts, vec![claimed, follow_up]);
    assert_eq!(failed.process.completed_at_ms, Some(completed_at));
    assert_eq!(
        failed.outcome.error.as_deref(),
        Some("session build failed")
    );
    ipc.cancel.cancel();
}

#[test]
fn refused_prompt_retires_its_claim_without_a_transcript_and_records_why() {
    let (_dir, store) = store();
    let transcript_dir = tempfile::tempdir().unwrap();
    let mut state = store
        .create_job(
            "prompt".into(),
            transcript_dir.path().to_path_buf(),
            runtime(),
        )
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 4;
    state.identity.session_id = Some("sess-refused".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    // Claimed, never completed, no transcript row: exactly the state a
    // failed turn keeps the claim in. A refusal lets it go instead.
    let mut claimed = pending_prompt("pp-refused", "rm -rf /", Vec::new());
    claimed.claimed_turn_generation = Some(state.process.turn_generation);
    state.identity.pending_prompts = vec![claimed];
    store.write_state(&state).unwrap();

    let completed_at = now_ms();
    assert_eq!(
        finalize_refused_background_turn(
            &store,
            &state,
            &ipc,
            "a hook said no",
            "refused summary",
            completed_at,
        )
        .unwrap(),
        BackgroundTurnFinalization::Applied
    );
    let refused = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(refused.process.status, BackgroundJobStatus::Failed);
    assert!(
        refused.identity.pending_prompts.is_empty(),
        "the claim is retired, not retried"
    );
    assert_eq!(refused.outcome.error.as_deref(), Some("a hook said no"));
    assert_eq!(refused.outcome.summary.as_deref(), Some("refused summary"));
    assert_eq!(refused.process.completed_at_ms, Some(completed_at));
    ipc.cancel.cancel();
}

#[test]
fn refused_prompt_lets_the_follow_up_behind_it_run() {
    let (_dir, store) = store();
    let transcript_dir = tempfile::tempdir().unwrap();
    let mut state = store
        .create_job(
            "prompt".into(),
            transcript_dir.path().to_path_buf(),
            runtime(),
        )
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 5;
    state.identity.session_id = Some("sess-refused-follow-up".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    let mut claimed = pending_prompt("pp-refused", "blocked", Vec::new());
    claimed.claimed_turn_generation = Some(state.process.turn_generation);
    let follow_up = pending_prompt("pp-next", "allowed", Vec::new());
    state.identity.pending_prompts = vec![claimed, follow_up.clone()];
    store.write_state(&state).unwrap();

    assert_eq!(
        finalize_refused_background_turn(
            &store,
            &state,
            &ipc,
            "a hook said no",
            "refused summary",
            now_ms(),
        )
        .unwrap(),
        BackgroundTurnFinalization::Queued
    );
    let queued = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(queued.process.status, BackgroundJobStatus::Queued);
    assert_eq!(queued.identity.pending_prompts, vec![follow_up]);
    assert_eq!(
        queued.outcome.summary.as_deref(),
        Some("queued follow-up after refused prompt")
    );
    assert_eq!(queued.outcome.error, None);
    ipc.cancel.cancel();
}

#[test]
fn completed_turn_requires_durable_completion_ack_before_retiring_prompt() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 11;
    state.identity.session_id = Some("sess-missing-completion-ack".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    let mut claimed = pending_prompt("pp-missing-completion-ack", "keep me", Vec::new());
    claimed.claimed_turn_generation = Some(state.process.turn_generation);
    state.identity.pending_prompts = vec![claimed.clone()];
    store.write_state(&state).unwrap();

    assert!(finalize_completed_background_turn(
        &store,
        &state,
        &ipc,
        "completed summary",
        now_ms(),
    )
    .is_err());
    assert_eq!(
        store
            .read_state(&state.identity.job_id)
            .unwrap()
            .identity
            .pending_prompts,
        vec![claimed]
    );
    ipc.cancel.cancel();
}

#[test]
fn unified_terminal_finalizer_leaves_superseded_and_stopped_turns_unchanged() {
    for outcome in [
        TestFinalizerOutcome::Cancelled,
        TestFinalizerOutcome::Completed,
        TestFinalizerOutcome::Failed,
    ] {
        for stopped in [false, true] {
            let (_dir, store) = store();
            let mut claimed = store
                .create_job("prompt".into(), PathBuf::from("."), runtime())
                .unwrap();
            claimed.process.status = BackgroundJobStatus::Running;
            claimed.process.turn_generation = 4;
            claimed.identity.session_id = Some(format!("sess-stale-{outcome:?}-{stopped}"));
            let ipc = start_background_ipc_server(&store, &claimed.identity.job_id).unwrap();
            install_ipc_owner(&mut claimed, &ipc);
            let mut claimed_prompt =
                pending_prompt("pp-finalizer-stale", "stale claimed", Vec::new());
            claimed_prompt.claimed_turn_generation = Some(claimed.process.turn_generation);
            claimed.identity.pending_prompts = vec![claimed_prompt];
            store.write_state(&claimed).unwrap();
            store
                .update_state(&claimed.identity.job_id, |current| {
                    if stopped {
                        current.process.status = BackgroundJobStatus::Stopped;
                    } else {
                        current.process.turn_generation += 1;
                    }
                    Ok(())
                })
                .unwrap();
            let before = store.read_state(&claimed.identity.job_id).unwrap();

            let finalization =
                invoke_test_finalizer(outcome, &store, &claimed, &ipc, now_ms()).unwrap();
            assert_eq!(
                finalization,
                if stopped {
                    BackgroundTurnFinalization::Stopped
                } else {
                    BackgroundTurnFinalization::CancelledOrSuperseded
                }
            );
            assert_eq!(store.read_state(&claimed.identity.job_id).unwrap(), before);
            ipc.cancel.cancel();
        }
    }
}

#[test]
fn terminal_finalization_wins_over_a_late_cancel_without_reporting_success() {
    for succeeds in [true, false] {
        let (_dir, store) = store();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Running;
        state.process.turn_generation = 1;
        state.identity.session_id = Some(format!("sess-finalize-cancel-{succeeds}"));
        let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
        install_ipc_owner(&mut state, &ipc);
        store.write_state(&state).unwrap();
        let turn_cancel = ipc.start_turn();

        let finalization = if succeeds {
            finalize_completed_background_turn(
                &store,
                &state,
                &ipc,
                "completed before cancel",
                now_ms(),
            )
        } else {
            finalize_failed_background_turn(
                &store,
                &state,
                &ipc,
                "failed before cancel",
                "failed",
                now_ms(),
            )
        }
        .unwrap();
        assert_eq!(finalization, BackgroundTurnFinalization::Applied);

        let error = send_background_ipc_request(
            &state,
            ipc.port,
            ipc.token.clone(),
            BackgroundIpcRequest::cancel_for(&state),
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("changed before cancellation"),
            "expected the fence's typed refusal, got: {error}"
        );
        assert!(!turn_cancel.is_cancelled());
        assert_eq!(
            store
                .read_state(&state.identity.job_id)
                .unwrap()
                .process
                .status,
            if succeeds {
                BackgroundJobStatus::Succeeded
            } else {
                BackgroundJobStatus::Failed
            }
        );
        ipc.cancel.cancel();
    }
}

/// The strict-isolation path a queue row takes end to end: `prepare` refuses to
/// hand back the origin checkout, and the worker's failure finalization turns
/// that error into a normal Failed job — no session, no model prompt.

#[test]
fn required_worktree_failure_finalizes_the_turn_as_failed() {
    let (_dir, store) = store();
    let repo = tempfile::tempdir().unwrap();
    let mut state = store
        .create_job("prompt".into(), repo.path().to_path_buf(), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 4;
    state.workspace.isolate_in_worktree = true;
    state.workspace.require_worktree = true;
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let error = prepare_background_worktree(&store, &mut state).unwrap_err();
    let message = error.to_string();
    let completed_at = now_ms();
    assert_eq!(
        finalize_failed_background_turn(
            &store,
            &state,
            &ipc,
            &message,
            &background_job_failure_summary(&message),
            completed_at,
        )
        .unwrap(),
        BackgroundTurnFinalization::Applied
    );

    let failed = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(failed.process.status, BackgroundJobStatus::Failed);
    assert_eq!(failed.outcome.exit_code, Some(1));
    assert!(
        failed
            .outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("requires an isolated worktree")),
        "expected the worktree refusal, got: {:?}",
        failed.outcome.error
    );
    assert_eq!(
        failed.workspace.worktree_path, None,
        "a required worktree that never existed must not be recorded"
    );
    assert_eq!(
        failed.identity.cwd, state.identity.cwd,
        "the origin checkout is never entered"
    );
    ipc.cancel.cancel();
}
