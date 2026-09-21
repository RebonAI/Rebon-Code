use super::super::*;
use super::support::*;
use rebon_session_host::{BackgroundPermissionOptionSnapshot, BackgroundPermissionQuerySnapshot};

/// The supervisor loop with nothing lent to it.
///
/// The CLI's two hooks — the updater's Windows migration and the `gh`-backed
/// pull-request refresh — are the binary's, not the loop's, and nothing here
/// exercises them. Their absence is also the assertion that the loop is
/// complete without them.
fn no_hooks() -> rebon_session_host::SupervisorHooks<'static> {
    rebon_session_host::SupervisorHooks::default()
}

#[cfg(windows)]
fn spawn_long_running_child() -> std::process::Child {
    std::process::Command::new("cmd")
        .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

#[cfg(unix)]
fn spawn_long_running_child() -> std::process::Child {
    std::process::Command::new("sh")
        .args(["-c", "sleep 30"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn supervisor_preserves_live_local_takeover_owner_fence() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Queued;
    state.identity.session_id = Some("sess-local-takeover-fence".into());
    state.identity.pending_prompts = vec![pending_prompt(
        "pp-local-takeover-fence",
        "foreground owns this",
        Vec::new(),
    )];
    state.process.pid = Some(std::process::id());
    state.process.pid_identity = rebon_session_host::process_identity(std::process::id());
    state.process.process_owner_fenced = true;
    state.process.updated_at_ms = now_ms().saturating_sub(QUEUED_LIVE_WORKER_GRACE_MS + 1);
    store.write_state(&state).unwrap();

    assert!(supervisor_tick(&store, &no_hooks()).unwrap());
    let preserved = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(preserved.process.status, BackgroundJobStatus::Queued);
    assert_eq!(preserved.process.pid, state.process.pid);
    assert_eq!(preserved.process.pid_identity, state.process.pid_identity);
    assert!(preserved.process.process_owner_fenced);
    assert_eq!(
        preserved.identity.pending_prompts,
        state.identity.pending_prompts
    );
}

fn queued_state(pid: Option<u32>, updated_at_ms: u64) -> BackgroundJobState {
    let mut state = BackgroundJobState::new("prompt".into(), ".".into(), runtime(), None);
    state.process.status = BackgroundJobStatus::Queued;
    state.process.pid = pid;
    state.process.updated_at_ms = updated_at_ms;
    state
}

#[test]
fn worker_process_uses_the_launchers_path() {
    let mut state = queued_state(None, 1);
    state.process.process_path = Some("launcher-bin".into());
    let mut command = Command::new("rebon");

    apply_worker_process_environment(&mut command, &state);

    let path = command
        .get_envs()
        .find_map(|(key, value)| (key == "PATH").then_some(value).flatten());
    assert_eq!(path, Some(std::ffi::OsStr::new("launcher-bin")));
}

#[test]
fn worker_process_inherits_path_for_legacy_jobs() {
    let mut state = queued_state(None, 1);
    state.process.process_path = None;
    let mut command = Command::new("rebon");

    apply_worker_process_environment(&mut command, &state);

    assert!(!command.get_envs().any(|(key, _)| key == "PATH"));
}

/// A session runs in the worker, not in the process that resolved the runtime.
/// Without this the worker would re-resolve and could land on a different Node
/// than the app vetted — or on none at all, when the app's `PATH` is the only
/// place one exists.
#[test]
fn worker_process_receives_the_node_runtime_the_job_was_created_with() {
    let mut state = queued_state(None, 1);
    state.process.node_runtime_path = Some("/opt/node/bin/node".into());
    let mut command = Command::new("rebon");

    apply_worker_process_environment(&mut command, &state);

    let node = command.get_envs().find_map(|(key, value)| {
        (key == rebon_node_runtime::NODE_EXECUTABLE_ENV)
            .then_some(value)
            .flatten()
    });
    assert_eq!(node, Some(std::ffi::OsStr::new("/opt/node/bin/node")));
}

/// Nothing recorded means nobody had resolved a runtime; the worker walks the
/// ladder itself rather than being handed an empty demand it must then reject.
#[test]
fn worker_process_is_left_to_resolve_node_when_the_job_recorded_none() {
    let mut state = queued_state(None, 1);
    state.process.node_runtime_path = None;
    let mut command = Command::new("rebon");

    apply_worker_process_environment(&mut command, &state);

    assert!(!command
        .get_envs()
        .any(|(key, _)| key == rebon_node_runtime::NODE_EXECUTABLE_ENV));
}

#[test]
fn spawned_worker_commit_completes_an_early_claim_with_detached_provenance() {
    let mut state = queued_state(None, 1);
    state.process.status = BackgroundJobStatus::Running;
    state.process.pid = Some(4321);
    state.process.ipc_port = Some(41000);
    state.process.ipc_token = Some("early-claim".into());
    state.process.spawn_admitted = true;
    state.process.turn_generation = 8;

    assert!(state_was_claimed_by_spawned_worker(&state, 4321, &None, 7));
    assert!(complete_early_worker_claim(
        &mut state, 4321, &None, 7, true, 99
    ));
    assert!(state.process.owner_detached_group);
    assert!(!state.process.spawn_admitted);
    assert_eq!(state.process.ipc_port, Some(41000));
    assert_eq!(state.process.ipc_token.as_deref(), Some("early-claim"));
    assert_eq!(state.process.turn_generation, 8);
    assert_eq!(state.process.updated_at_ms, 99);

    state.process.spawn_admitted = true;
    state.process.owner_detached_group = false;
    assert!(!state_was_claimed_by_spawned_worker(&state, 9999, &None, 7));
    assert!(!state_was_claimed_by_spawned_worker(&state, 4321, &None, 8));
    state.process.process_owner_fenced = true;
    assert!(!state_was_claimed_by_spawned_worker(&state, 4321, &None, 7));
}

#[cfg(any(unix, windows))]
#[test]
fn background_worker_reaper_waits_for_child_exit() {
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 0"]);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "exit", "0"]);
        hide_background_command_window(&mut command);
        command
    };
    let child = command.spawn().unwrap();

    let reaper = spawn_background_worker_reaper(child).unwrap();
    let status = reaper.join().unwrap().unwrap();

    assert!(status.success());
}

#[cfg(any(unix, windows))]
#[test]
fn background_worker_reaper_thread_start_failure_proves_child_exit() {
    let child = spawn_long_running_child();
    let error = spawn_background_worker_reaper_with(child, |_pid, _receiver| {
        Err(std::io::Error::other("forced reaper thread failure"))
    })
    .unwrap_err();

    assert!(error.exit_verified);
}

#[cfg(any(unix, windows))]
#[test]
fn background_worker_reaper_handoff_failure_proves_child_exit() {
    let child = spawn_long_running_child();
    let error = spawn_background_worker_reaper_with(child, |_pid, receiver| {
        let (dropped_tx, dropped_rx) = std::sync::mpsc::sync_channel(0);
        let handle = std::thread::spawn(move || {
            drop(receiver);
            dropped_tx.send(()).unwrap();
            Err(std::io::Error::other("forced receiver exit"))
        });
        dropped_rx.recv().unwrap();
        Ok(handle)
    })
    .unwrap_err();

    assert!(error.exit_verified);
}

#[cfg(any(unix, windows))]
#[test]
fn retained_child_poll_keeps_supervisor_alive_after_reaping() {
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 0"]);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "exit", "0"]);
        hide_background_command_window(&mut command);
        command
    };
    let mut child = command.spawn().unwrap();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::yield_now();
    }
    let mut children = vec![child];

    assert!(poll_retained_background_worker_children_in(&mut children));
    assert!(children.is_empty());
}

#[test]
fn unverified_reaper_failure_retains_and_fences_process_owner() {
    let mut state = BackgroundJobState::new("prompt".into(), ".".into(), runtime(), None);
    state.process.status = BackgroundJobStatus::Running;
    state.process.pid = Some(4321);
    state.process.pid_identity = Some("identity".into());
    state.process.ipc_port = Some(41000);
    state.process.ipc_token = Some("endpoint".into());
    state.identity.pending_prompts = vec![pending_prompt(
        "pp-accepted",
        "accepted follow-up",
        Vec::new(),
    )];

    let expected_owner = state.recorded_owner();
    assert!(apply_background_worker_reaper_start_failure(
        &mut state,
        &expected_owner,
        false,
        99,
        "reaper failed",
    ));
    assert_eq!(state.process.status, BackgroundJobStatus::Failed);
    assert_eq!(state.process.pid, Some(4321));
    assert_eq!(state.process.pid_identity.as_deref(), Some("identity"));
    assert!(state.process.process_owner_fenced);
    assert_eq!(state.process.ipc_port, None);
    assert_eq!(state.process.ipc_token, None);
    assert_eq!(pending_text(&state), Some("accepted follow-up"));
}

#[test]
fn verified_reaper_failure_releases_process_owner() {
    let mut state = BackgroundJobState::new("prompt".into(), ".".into(), runtime(), None);
    state.process.status = BackgroundJobStatus::Queued;
    state.process.pid = Some(4321);
    state.process.pid_identity = Some("identity".into());

    let expected_owner = state.recorded_owner();
    assert!(apply_background_worker_reaper_start_failure(
        &mut state,
        &expected_owner,
        true,
        99,
        "reaper failed",
    ));
    assert_eq!(state.process.status, BackgroundJobStatus::Failed);
    assert_eq!(state.process.pid, None);
    assert_eq!(state.process.pid_identity, None);
    assert!(!state.process.process_owner_fenced);
}

#[test]
fn queued_spawn_decision_spawns_when_unowned_or_owner_dead() {
    let now = 100_000;
    assert_eq!(
        queued_spawn_decision(&queued_state(None, now), now, 1, |_| Some(true)),
        QueuedSpawnDecision::Spawn
    );
    assert_eq!(
        queued_spawn_decision(&queued_state(Some(999), now), now, 1, |_| Some(false)),
        QueuedSpawnDecision::Spawn
    );
    // A recorded self pid marks a legacy detached session, not a worker.
    assert_eq!(
        queued_spawn_decision(&queued_state(Some(1), now), now, 1, |_| Some(true)),
        QueuedSpawnDecision::Spawn
    );
}

#[test]
fn queued_spawn_decision_defers_to_live_worker_within_grace() {
    let now = 100_000;
    assert_eq!(
        queued_spawn_decision(&queued_state(Some(999), now - 5_000), now, 1, |_| Some(
            true
        )),
        QueuedSpawnDecision::DeferToLiveWorker
    );
}

#[test]
fn queued_spawn_decision_replaces_stuck_worker_past_grace() {
    let now = 100_000;
    assert_eq!(
        queued_spawn_decision(
            &queued_state(Some(999), now - QUEUED_LIVE_WORKER_GRACE_MS),
            now,
            1,
            |_| Some(true)
        ),
        QueuedSpawnDecision::ReplaceStuckWorker(999)
    );
}

#[test]
fn stuck_worker_fence_rejects_turn_claimed_after_supervisor_snapshot() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.pid = Some(std::process::id());
    state.process.pid_identity = rebon_session_host::process_identity(std::process::id());
    state.process.ipc_port = Some(41000);
    state.process.ipc_token = Some("old-endpoint".into());
    state.identity.pending_prompts = vec![pending_prompt(
        "pp-accepted",
        "accepted follow-up",
        Vec::new(),
    )];
    state.process.turn_generation = 4;
    store.write_state(&state).unwrap();
    let mut supervisor_snapshot = state.clone();

    store
        .update_state(&state.identity.job_id, |current| {
            current.process.status = BackgroundJobStatus::Running;
            current.process.turn_generation += 1;
            current.process.updated_at_ms = current.process.updated_at_ms.saturating_add(1);
            Ok(())
        })
        .unwrap();

    assert!(!fence_stuck_queued_worker_for_replacement(&store, &mut supervisor_snapshot).unwrap());
    assert_eq!(
        supervisor_snapshot.process.status,
        BackgroundJobStatus::Running
    );
    assert!(!supervisor_snapshot.process.process_owner_fenced);
    assert_eq!(supervisor_snapshot.process.ipc_port, Some(41000));
    assert_eq!(
        pending_text(&supervisor_snapshot),
        Some("accepted follow-up")
    );
}

#[test]
fn stuck_worker_fence_blocks_old_worker_reclaim_and_preserves_prompt() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.pid = Some(std::process::id());
    state.process.pid_identity = rebon_session_host::process_identity(std::process::id());
    state.process.ipc_port = Some(41001);
    state.process.ipc_token = Some("stuck-endpoint".into());
    state.identity.pending_prompts = vec![pending_prompt(
        "pp-accepted",
        "accepted follow-up",
        Vec::new(),
    )];
    store.write_state(&state).unwrap();

    assert!(fence_stuck_queued_worker_for_replacement(&store, &mut state).unwrap());
    assert!(state.process.process_owner_fenced);
    assert_eq!(state.process.pid, Some(std::process::id()));
    assert_eq!(state.process.ipc_port, None);
    assert_eq!(state.process.ipc_token, None);
    assert_eq!(pending_text(&state), Some("accepted follow-up"));
    assert_eq!(
        claim_background_job(
            &store,
            &state.identity.job_id,
            std::process::id(),
            41002,
            "replacement-endpoint",
            |_| Some(true),
        )
        .unwrap(),
        WorkerClaim::NoWork
    );

    assert!(clear_fenced_stuck_worker_after_exit(&store, &mut state).unwrap());
    assert_eq!(state.process.pid, None);
    assert_eq!(state.process.pid_identity, None);
    assert!(!state.process.process_owner_fenced);
    assert!(matches!(
        claim_background_job(
            &store,
            &state.identity.job_id,
            std::process::id(),
            41002,
            "replacement-endpoint",
            |_| Some(true),
        )
        .unwrap(),
        WorkerClaim::Claimed(_)
    ));
    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(pending_text(&loaded), Some("accepted follow-up"));
}

#[test]
fn supervisor_does_not_replace_a_live_worker_without_verified_identity() {
    let (_dir, store) = store();
    let mut child = spawn_long_running_child();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Queued;
    state.process.pid = Some(child.id());
    state.process.pid_identity = None;
    state.process.updated_at_ms = 0;
    store.write_state(&state).unwrap();

    assert!(supervisor_tick(&store, &no_hooks()).unwrap());

    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Queued);
    assert_eq!(loaded.process.pid, Some(child.id()));
    assert!(child.try_wait().unwrap().is_none());
    let events = store.read_events_tail(&state.identity.job_id, 10).unwrap();
    assert!(events
        .iter()
        .any(|event| event.kind == "stuck_worker_replacement_blocked"));
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn background_worktree_requires_explicit_isolation() {
    let mut state = BackgroundJobState::new("prompt".into(), "F:/repo".into(), runtime(), None);
    assert!(!should_prepare_background_worktree(&state));

    state.workspace.isolate_in_worktree = true;
    assert!(should_prepare_background_worktree(&state));

    state.identity.cwd = "F:/repo/.rebon/worktrees/manual".into();
    assert!(!should_prepare_background_worktree(&state));
}

#[test]
fn disabled_isolation_does_not_reuse_stored_worktree_path() {
    let (dir, store) = store();
    let worktree = dir.path().join("old-worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let mut state = BackgroundJobState::new(
        "prompt".into(),
        dir.path().join("repo").to_string_lossy().into_owned(),
        runtime(),
        None,
    );
    state.workspace.worktree_path = Some(worktree.to_string_lossy().into_owned());

    let guard = prepare_background_worktree(&store, &mut state).unwrap();

    assert!(guard.path().is_none());
}

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn run_git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A seeded repo plus a job whose worktree already exists, mirroring a job
/// that has taken at least one turn.
///
/// The returned env guard is held for the whole test: other tests in this
/// binary swap `PATH`/`PATHEXT` process-wide, and a test that shells out to
/// `git` while that is in flight cannot resolve it.
fn isolated_job_with_worktree(
    label: &str,
) -> Option<(
    std::sync::MutexGuard<'static, ()>,
    tempfile::TempDir,
    BackgroundStore,
    BackgroundJobState,
    PathBuf,
)> {
    let env_guard = crate::test_env::lock_env();
    if !git_available() {
        return None;
    }
    let dir = tempfile::Builder::new().prefix(label).tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run_git(&repo, &["init", "-q"]);
    run_git(&repo, &["branch", "-M", "main"]);
    run_git(&repo, &["config", "user.email", "agent@example.com"]);
    run_git(&repo, &["config", "user.name", "Rebon Agent"]);
    std::fs::write(repo.join(".gitignore"), ".rebon\n").unwrap();
    std::fs::write(repo.join("README.md"), "seed\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-qm", "seed"]);

    let store = BackgroundStore::new(dir.path().join("store"));
    let mut state = store
        .create_job("prompt".into(), repo.clone(), runtime())
        .unwrap();
    state.workspace.isolate_in_worktree = true;
    let slug = background_worktree_slug(&state.identity.job_id);
    let info = rebon_tool::worktree::create_agent_worktree(&repo, &slug).unwrap();
    state.workspace.worktree_path = Some(info.worktree_path.to_string_lossy().into_owned());
    store.write_state(&state).unwrap();
    Some((env_guard, dir, store, state, info.worktree_path))
}

/// Gut the worktree the way a half-completed cleanup does: the `.git` link
/// and the checkout are gone, the directory survives.
fn gut_worktree(worktree: &Path) {
    for entry in std::fs::read_dir(worktree).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
    }
}

#[test]
fn isolated_job_reopens_its_own_worktree() {
    let Some((_env, _dir, store, mut state, worktree)) =
        isolated_job_with_worktree("rebon-bg-reopen-ok-")
    else {
        return;
    };

    let guard = prepare_background_worktree(&store, &mut state).unwrap();

    assert_eq!(guard.path(), Some(worktree.as_path()));
    let events = store.read_events_tail(&state.identity.job_id, 10).unwrap();
    assert!(events.iter().any(|event| event.kind == "worktree_reopened"));
}

#[test]
fn isolated_job_fails_the_turn_when_its_worktree_was_gutted() {
    let Some((_env, _dir, store, mut state, worktree)) =
        isolated_job_with_worktree("rebon-bg-reopen-gutted-")
    else {
        return;
    };
    gut_worktree(&worktree);

    let error = prepare_background_worktree(&store, &mut state)
        .unwrap_err()
        .to_string();

    // Falling back would run the turn in the source checkout and merge
    // straight onto its branch.
    assert!(error.contains("Refusing to run this turn"), "{error}");
    let events = store.read_events_tail(&state.identity.job_id, 10).unwrap();
    let lost = events
        .iter()
        .find(|event| event.kind == "worktree_lost")
        .expect("worktree_lost event");
    assert_eq!(
        lost.data["expected_branch"].as_str(),
        Some(format!("rebon/{}", background_worktree_slug(&state.identity.job_id)).as_str())
    );
}

#[test]
fn isolated_job_fails_the_turn_when_its_worktree_moved_to_another_branch() {
    let Some((_env, _dir, store, mut state, worktree)) =
        isolated_job_with_worktree("rebon-bg-reopen-branch-")
    else {
        return;
    };
    run_git(&worktree, &["switch", "-q", "-c", "somebody-elses-branch"]);

    let error = prepare_background_worktree(&store, &mut state)
        .unwrap_err()
        .to_string();

    assert!(error.contains("somebody-elses-branch"), "{error}");
    let events = store.read_events_tail(&state.identity.job_id, 10).unwrap();
    assert!(events.iter().any(|event| event.kind == "worktree_lost"));
}

#[test]
fn run_background_supervisor_with_store_exits_after_idle_tick_without_active_work() {
    let (_dir, store) = store();

    run_background_supervisor_with_store(&store, &no_hooks()).unwrap();

    let roster = store.read_roster().unwrap();
    assert_eq!(roster.supervisor_pid, std::process::id());
    assert!(roster.jobs.is_empty());
}

#[test]
fn supervisor_tick_without_agent_view_client_skips_pull_request_refresh() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Succeeded;
    state.outcome.summary = Some("opened https://github.com/org/repo/pull/2048".into());
    store.write_state(&state).unwrap();

    assert!(!supervisor_tick(&store, &no_hooks()).unwrap());

    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert!(loaded.outcome.pull_requests.is_empty());
    let events = store.read_events_tail(&state.identity.job_id, 10).unwrap();
    assert!(!events
        .iter()
        .any(|event| event.kind == "pr_status_refreshed"));
}

#[test]
fn supervisor_tick_with_agent_view_client_skips_fresh_pull_request_cache() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Succeeded;
    state.outcome.summary = Some("opened https://github.com/org/repo/pull/2048".into());
    state.outcome.pull_requests = vec![BackgroundPullRequestStatus {
        url: "https://github.com/org/repo/pull/2048".into(),
        owner: "org".into(),
        repo: "repo".into(),
        number: 2048,
        dot: Some(BackgroundPullRequestDotStatus::Ready),
        state: Some("OPEN".into()),
        merge_state: Some("CLEAN".into()),
        review_decision: Some("APPROVED".into()),
        checks_summary: Some("1/1 checks passed".into()),
        error: None,
        updated_at_ms: now_ms(),
    }];
    let original_updated_at = state.process.updated_at_ms;
    store.write_state(&state).unwrap();
    store.touch_supervisor_client("agent-view").unwrap();

    assert!(supervisor_tick(&store, &no_hooks()).unwrap());

    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.updated_at_ms, original_updated_at);
    assert_eq!(loaded.outcome.pull_requests.len(), 1);
    let events = store.read_events_tail(&state.identity.job_id, 10).unwrap();
    assert!(!events
        .iter()
        .any(|event| event.kind == "pr_status_refreshed"));
}

#[test]
fn supervisor_tick_rosters_existing_running_worker_after_restart() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.identity.session_id = Some("session-reconnect".into());
    state.process.pid = Some(std::process::id());
    state.process.ipc_port = Some(49152);
    state.process.ipc_token = Some("secret".into());
    store.write_state(&state).unwrap();

    assert!(supervisor_tick(&store, &no_hooks()).unwrap());

    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Running);
    assert_eq!(loaded.process.pid, Some(std::process::id()));
    assert_eq!(loaded.process.ipc_port, Some(49152));
    assert_eq!(loaded.process.ipc_token.as_deref(), Some("secret"));

    let roster = store.read_roster().unwrap();
    assert_eq!(roster.supervisor_pid, std::process::id());
    assert_eq!(roster.jobs.len(), 1);
    let job = &roster.jobs[0];
    assert_eq!(job.job_id, state.identity.job_id);
    assert_eq!(job.session_id.as_deref(), Some("session-reconnect"));
    assert_eq!(job.status, BackgroundJobStatus::Running);
    assert_eq!(job.pid, Some(std::process::id()));
}

#[test]
fn supervisor_tick_rosters_needs_input_worker_after_restart() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::NeedsInput;
    state.process.pid = Some(std::process::id());
    state.process.ipc_port = Some(49153);
    state.process.ipc_token = Some("secret".into());
    state.outcome.pending_permission = Some(BackgroundPermissionQuerySnapshot {
        query_id: 42,
        turn_generation: 0,
        endpoint: None,
        tool: Some("Bash".into()),
        tool_call_id: Some("tool-1".into()),
        session_id: Some("session-needs-input".into()),
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
    store.write_state(&state).unwrap();

    assert!(supervisor_tick(&store, &no_hooks()).unwrap());

    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::NeedsInput);
    assert_eq!(loaded.process.pid, Some(std::process::id()));
    assert_eq!(
        loaded
            .outcome
            .pending_permission
            .as_ref()
            .map(|query| query.query_id),
        Some(42)
    );

    let roster = store.read_roster().unwrap();
    assert_eq!(roster.jobs.len(), 1);
    let job = &roster.jobs[0];
    assert_eq!(job.job_id, state.identity.job_id);
    assert_eq!(job.status, BackgroundJobStatus::NeedsInput);
    assert_eq!(job.pid, Some(std::process::id()));
}

#[test]
fn supervisor_tick_marks_dead_running_worker_failed_in_roster() {
    let dead_pid = u32::MAX - 1;
    if process_is_running(dead_pid) != Some(false) {
        return;
    }

    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.pid = Some(dead_pid);
    state.process.ipc_port = Some(49154);
    state.process.ipc_token = Some("secret".into());
    store.write_state(&state).unwrap();

    assert!(!supervisor_tick(&store, &no_hooks()).unwrap());

    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Failed);
    assert!(loaded.outcome.error.is_some());
    assert_eq!(loaded.process.pid, None);
    assert_eq!(loaded.process.ipc_port, None);
    assert_eq!(loaded.process.ipc_token, None);
    assert_eq!(loaded.outcome.pending_permission, None);

    let roster = store.read_roster().unwrap();
    assert_eq!(roster.jobs.len(), 1);
    let job = &roster.jobs[0];
    assert_eq!(job.job_id, state.identity.job_id);
    assert_eq!(job.status, BackgroundJobStatus::Failed);
    assert_eq!(job.pid, None);

    let events = store.read_events_tail(&state.identity.job_id, 10).unwrap();
    assert!(events
        .iter()
        .any(|event| event.kind == "stale_pid_reconciled"));
}

#[test]
fn terminal_cleanup_does_not_clear_a_replacement_owner() {
    let (_dir, store) = store();
    let mut stale = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    stale.process.status = BackgroundJobStatus::Succeeded;
    stale.process.turn_generation = 3;
    stale.process.pid = Some(std::process::id());
    stale.process.ipc_port = Some(1234);
    stale.process.ipc_token = Some("old-owner".into());
    store.write_state(&stale).unwrap();
    store
        .update_state(&stale.identity.job_id, |current| {
            current.process.status = BackgroundJobStatus::Running;
            current.process.turn_generation = 4;
            current.process.ipc_port = Some(5678);
            current.process.ipc_token = Some("replacement-owner".into());
            Ok(())
        })
        .unwrap();

    clear_terminal_worker_process_refs(
        &store,
        &mut stale,
        "stale_cleanup_should_not_apply",
        now_ms(),
    )
    .unwrap();

    let loaded = store.read_state(&stale.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Running);
    assert_eq!(loaded.process.turn_generation, 4);
    assert_eq!(loaded.process.ipc_port, Some(5678));
    assert_eq!(
        loaded.process.ipc_token.as_deref(),
        Some("replacement-owner")
    );
    assert!(!store
        .read_events_tail(&stale.identity.job_id, 10)
        .unwrap()
        .iter()
        .any(|event| event.kind == "stale_cleanup_should_not_apply"));
}

#[test]
fn terminal_cleanup_does_not_clear_same_owner_after_follow_up_is_queued() {
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
    store.write_state(&stale).unwrap();
    store
        .update_state(&stale.identity.job_id, |current| {
            current.process.status = BackgroundJobStatus::Queued;
            current.identity.pending_prompts = vec![pending_prompt(
                "pp-terminal-cleanup",
                "accepted follow-up",
                Vec::new(),
            )];
            current.process.updated_at_ms = current.process.updated_at_ms.saturating_add(1);
            Ok(())
        })
        .unwrap();

    clear_terminal_worker_process_refs(
        &store,
        &mut stale,
        "stale_cleanup_should_not_apply",
        now_ms(),
    )
    .unwrap();

    let loaded = store.read_state(&stale.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Queued);
    assert_eq!(pending_text(&loaded), Some("accepted follow-up"));
    assert_eq!(loaded.process.pid, Some(std::process::id()));
    assert_eq!(loaded.process.ipc_port, Some(1234));
    assert_eq!(loaded.process.ipc_token.as_deref(), Some("same-owner"));
    assert!(!store
        .read_events_tail(&stale.identity.job_id, 10)
        .unwrap()
        .iter()
        .any(|event| event.kind == "stale_cleanup_should_not_apply"));
}

#[test]
fn terminal_supervision_does_not_stop_follow_up_queued_after_snapshot() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Succeeded;
    state.process.turn_generation = 3;
    state.identity.session_id = Some("sess-terminal-cleanup-race".into());
    state.process.completed_at_ms = Some(now_ms().saturating_sub(TERMINAL_WORKER_IDLE_TTL_MS + 1));
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    let mut stale_jobs = vec![state.clone()];

    send_background_ipc_request(
        &state,
        ipc.port,
        ipc.token.clone(),
        BackgroundIpcRequest::Reply {
            message: "accepted follow-up".into(),
            images: Vec::new(),
        },
    )
    .unwrap();
    let replacement_turn = ipc.start_turn();

    supervise_terminal_worker_processes(&store, &mut stale_jobs, now_ms()).unwrap();

    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Queued);
    assert_eq!(pending_text(&loaded), Some("accepted follow-up"));
    assert_eq!(loaded.process.pid, Some(std::process::id()));
    assert_eq!(loaded.process.ipc_port, Some(ipc.port));
    assert_eq!(
        loaded.process.ipc_token.as_deref(),
        Some(ipc.token.as_str())
    );
    assert!(!replacement_turn.is_cancelled());
    assert_eq!(stale_jobs[0].process.status, BackgroundJobStatus::Queued);
    ipc.cancel.cancel();
}

#[test]
fn terminal_cleanup_fence_removes_endpoint_before_owner_release() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Succeeded;
    state.process.pid = Some(std::process::id());
    state.process.pid_identity = rebon_session_host::process_identity(std::process::id());
    state.process.ipc_port = Some(1234);
    state.process.ipc_token = Some("owner-token".into());
    state.outcome.pending_permission = Some(BackgroundPermissionQuerySnapshot {
        query_id: 45,
        turn_generation: 0,
        endpoint: None,
        tool: Some("Bash".into()),
        tool_call_id: Some("terminal-permission".into()),
        session_id: None,
        title: None,
        message: None,
        tool_input: None,
        metadata: None,
        options: Vec::new(),
    });
    store.write_state(&state).unwrap();

    assert!(fence_terminal_worker_for_cleanup(&store, &mut state).unwrap());

    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Succeeded);
    assert_eq!(loaded.process.pid, Some(std::process::id()));
    assert_eq!(loaded.process.pid_identity, state.process.pid_identity);
    assert!(loaded.process.ipc_port.is_none());
    assert!(loaded.process.ipc_token.is_none());
    assert!(loaded.outcome.pending_permission.is_none());
}

/// The supervisor's hour is the backstop for a worker that forgot to
/// leave. A worker somebody is sitting on has not forgotten anything: the
/// lease says so, and the reaper waits until it lapses.
#[test]
fn a_live_lease_keeps_the_reaper_off_an_idle_terminal_worker() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Idle;
    state.identity.session_id = Some("sess-leased-idle".into());
    state.process.pid = Some(std::process::id());
    state.process.ipc_port = Some(1234);
    state.process.ipc_token = Some("secret".into());
    let now = now_ms();
    state.process.completed_at_ms = Some(now.saturating_sub(TERMINAL_WORKER_IDLE_TTL_MS + 1));
    state.touch_client_lease(
        rebon_session_host::ClientLease {
            client_id: "tui-watching".into(),
            kind: rebon_session_host::ClientLeaseKind::Tui,
            pid: None,
            updated_at_ms: now,
        },
        now,
    );
    store.write_state(&state).unwrap();

    let mut jobs = vec![state.clone()];
    supervise_terminal_worker_processes(&store, &mut jobs, now).unwrap();

    let watched = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(watched.process.pid, Some(std::process::id()));
    assert_eq!(watched.process.ipc_port, Some(1234));
    assert!(store
        .read_events_tail(&state.identity.job_id, 10)
        .unwrap()
        .iter()
        .all(|event| event.kind != "idle_process_stopped"));

    // Once the lease lapses the hour applies as it always did.
    let mut jobs = vec![store.read_state(&state.identity.job_id).unwrap()];
    supervise_terminal_worker_processes(
        &store,
        &mut jobs,
        now + rebon_session_host::CLIENT_LEASE_TTL_MS + 1,
    )
    .unwrap();

    let unwatched = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(unwatched.process.pid, None);
    assert_eq!(unwatched.process.ipc_port, None);
    assert!(store
        .read_events_tail(&state.identity.job_id, 10)
        .unwrap()
        .iter()
        .any(|event| event.kind == "idle_process_stopped"));
}

#[test]
fn supervisor_tick_clears_idle_terminal_worker_process_refs_without_relabeling_job() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Succeeded;
    state.process.pid = Some(std::process::id());
    state.process.ipc_port = Some(1234);
    state.process.ipc_token = Some("secret".into());
    state.process.completed_at_ms = Some(now_ms().saturating_sub(TERMINAL_WORKER_IDLE_TTL_MS + 1));
    store.write_state(&state).unwrap();

    assert!(!supervisor_tick(&store, &no_hooks()).unwrap());

    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Succeeded);
    assert_eq!(loaded.process.pid, None);
    assert_eq!(loaded.process.ipc_port, None);
    assert_eq!(loaded.process.ipc_token, None);
    let events = store.read_events_tail(&state.identity.job_id, 10).unwrap();
    assert!(events
        .iter()
        .any(|event| event.kind == "idle_process_stopped"));
}

#[test]
fn supervisor_tick_stays_alive_while_agent_view_client_is_fresh() {
    let (_dir, store) = store();

    let client_id = store.touch_supervisor_client("agent view").unwrap();

    assert!(supervisor_tick(&store, &no_hooks()).unwrap());
    assert!(store.supervisor_client_path(&client_id).exists());
    let roster = store.read_roster().unwrap();
    assert_eq!(roster.supervisor_pid, std::process::id());
    assert!(roster.jobs.is_empty());
}

#[test]
fn supervisor_tick_prunes_stale_agent_view_clients() {
    let (_dir, store) = store();
    std::fs::create_dir_all(store.supervisor_clients_dir()).unwrap();
    let client_id = "agent-view-stale";
    // Epoch timestamp makes the lease stale-by-time regardless of `now`.
    std::fs::write(
        store.supervisor_client_path(client_id),
        serde_json::json!({
            "clientId": client_id,
            "kind": "agent-view",
            "pid": std::process::id(),
            "updatedAtMs": 0u64,
        })
        .to_string(),
    )
    .unwrap();

    assert!(!supervisor_tick(&store, &no_hooks()).unwrap());
    assert!(!store.supervisor_client_path(client_id).exists());
}
