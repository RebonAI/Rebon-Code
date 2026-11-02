//! A cron fire on a hosted session: queued by the idle worker as a scheduled
//! prompt, claimed as a turn of its own, and never treated as user input.

use super::super::*;
use super::support::*;
use rebon_session_host::PendingPromptOrigin;

fn held_cron(session_id: &str, cwd: &std::path::Path) -> crate::session_handoff::HeldCron {
    let poller = rebon_core::cron::CronPoller::new();
    let scheduler = rebon_core::cron::start_scheduler(
        rebon_core::cron::SchedulerConfig::new(cwd.to_path_buf()),
        poller.clone(),
    );
    crate::session_handoff::HeldCron {
        session_id: session_id.into(),
        cwd: cwd.to_string_lossy().into_owned(),
        poller,
        session_store: rebon_tool::SessionCronStore::new(),
        scheduler,
        projects_root: cwd.join("projects"),
    }
}

#[tokio::test]
async fn an_idle_workers_cron_fire_becomes_a_scheduled_turn() {
    let (_dir, store) = store();
    let cwd = tempfile::tempdir().unwrap();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Idle;
    state.identity.session_id = Some("sess-cron".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    let mut handed_back = crate::session_handoff::HandedBack {
        cron: Some(held_cron("sess-cron", cwd.path())),
        ..Default::default()
    };
    handed_back
        .cron
        .as_ref()
        .unwrap()
        .poller
        .enqueue("check the build".into());

    let reused = tokio::time::timeout(
        Duration::from_secs(5),
        wait_for_terminal_worker_reuse(&store, &state.identity.job_id, &ipc, &mut handed_back),
    )
    .await
    .expect("the worker takes the fire as its next turn")
    .unwrap();

    assert!(reused);
    assert!(!handed_back.cron.as_ref().unwrap().poller.has_pending());
    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Queued);
    let [prompt] = loaded.identity.pending_prompts.as_slice() else {
        panic!(
            "one scheduled prompt, got {:?}",
            loaded.identity.pending_prompts
        );
    };
    assert!(prompt.id.starts_with("u-internal-scheduled-"));
    assert_eq!(prompt.origin, Some(PendingPromptOrigin::Scheduled));
    assert_eq!(
        prompt.text,
        rebon_tool::cron::mark_scheduled_prompt("check the build")
    );
    assert!(!prompt.typed_by_user());
    ipc.cancel.cancel();
}

/// A held cron whose scheduler also ticks the session store, as a hosted
/// session's does — the store is where `ScheduleWakeup` arms its wakeup.
fn held_session_cron(session_id: &str, cwd: &std::path::Path) -> crate::session_handoff::HeldCron {
    let poller = rebon_core::cron::CronPoller::new();
    let session_store = rebon_tool::SessionCronStore::new();
    let scheduler = rebon_core::cron::start_scheduler_with_session_store(
        rebon_core::cron::SchedulerConfig::new(cwd.to_path_buf()),
        poller.clone(),
        Some(session_store.clone()),
    );
    crate::session_handoff::HeldCron {
        session_id: session_id.into(),
        cwd: cwd.to_string_lossy().into_owned(),
        poller,
        session_store,
        scheduler,
        projects_root: cwd.join("projects"),
    }
}

/// An idle job whose linger has already run out.
fn expired_linger_job(
    store: &BackgroundStore,
    session_id: &str,
) -> (BackgroundJobState, BackgroundIpcServer) {
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Idle;
    state.identity.session_id = Some(session_id.into());
    state.process.completed_at_ms = Some(now_ms().saturating_sub(60_000));
    state.lease.linger_ms = Some(1_000);
    let ipc = start_background_ipc_server(store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    (state, ipc)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_armed_wakeup_keeps_a_lingering_worker_and_becomes_its_next_turn() {
    let (_dir, store) = store();
    let cwd = tempfile::tempdir().unwrap();
    let (state, ipc) = expired_linger_job(&store, "sess-wakeup");
    let mut handed_back = crate::session_handoff::HandedBack {
        cron: Some(held_session_cron("sess-wakeup", cwd.path())),
        ..Default::default()
    };
    // Due in two seconds: the linger is long gone by then.
    handed_back
        .cron
        .as_ref()
        .unwrap()
        .session_store
        .arm_wakeup(
            "Continue the self-paced /loop: check CI",
            "CI takes a while",
            rebon_tool::cron::tasks::now_ms(),
            2_000,
            None,
        )
        .unwrap();

    let reused = tokio::time::timeout(
        Duration::from_secs(10),
        wait_for_terminal_worker_reuse(&store, &state.identity.job_id, &ipc, &mut handed_back),
    )
    .await
    .expect("the worker waits for its wakeup instead of leaving")
    .unwrap();

    assert!(reused, "the wakeup is the worker's next turn");
    let held = handed_back.cron.as_ref().unwrap();
    assert!(held.session_store.wakeup().is_none(), "it fired once");
    assert!(!held.poller.has_pending());
    let loaded = store.read_state(&state.identity.job_id).unwrap();
    let [prompt] = loaded.identity.pending_prompts.as_slice() else {
        panic!(
            "one scheduled prompt, got {:?}",
            loaded.identity.pending_prompts
        );
    };
    assert_eq!(prompt.origin, Some(PendingPromptOrigin::Scheduled));
    assert_eq!(
        prompt.text,
        rebon_tool::cron::mark_scheduled_prompt("Continue the self-paced /loop: check CI")
    );
    assert!(!prompt.typed_by_user());
    ipc.cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lingering_worker_with_no_wakeup_leaves_at_its_deadline() {
    let (_dir, store) = store();
    let cwd = tempfile::tempdir().unwrap();
    let (state, ipc) = expired_linger_job(&store, "sess-no-wakeup");
    let mut handed_back = crate::session_handoff::HandedBack {
        cron: Some(held_session_cron("sess-no-wakeup", cwd.path())),
        ..Default::default()
    };
    // A stopped loop: armed, then disarmed.
    let held = handed_back.cron.as_ref().unwrap();
    held.session_store
        .arm_wakeup("x", "y", 0, 3_600_000, None)
        .unwrap();
    held.session_store.clear_wakeup();

    let reused = tokio::time::timeout(
        Duration::from_secs(5),
        wait_for_terminal_worker_reuse(&store, &state.identity.job_id, &ipc, &mut handed_back),
    )
    .await
    .expect("nothing holds the worker")
    .unwrap();
    assert!(!reused);
    ipc.cancel.cancel();
}

#[test]
fn a_scheduled_prompt_reaches_the_model_as_itself_not_as_user_input() {
    let mut prompt = pending_prompt(
        "u-internal-scheduled-1",
        &rebon_tool::cron::mark_scheduled_prompt("check the build"),
        Vec::new(),
    );
    prompt.origin = Some(PendingPromptOrigin::Scheduled);

    let message = pending_prompt_attachment_message(&prompt);
    let [rebon_api::ContentBlock::Text(text)] = message.content.as_slice() else {
        panic!(
            "one text block, no queued-user marker: {:?}",
            message.content
        );
    };
    assert_eq!(text.text, prompt.text);
    assert!(!text.text.contains("<rebon-queued-user-input"));

    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.turn_generation = 2;
    prompt.claimed_turn_generation = Some(2);
    state.identity.pending_prompts = vec![prompt];
    assert_eq!(
        background_user_prompt(&state, false),
        None,
        "the classifier is not told the user typed a cron fire"
    );
    state.identity.pending_prompts[0].origin = None;
    assert!(
        background_user_prompt(&state, false).is_some(),
        "the same text typed by a person is user input"
    );
}
