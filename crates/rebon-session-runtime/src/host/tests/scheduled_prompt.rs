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
