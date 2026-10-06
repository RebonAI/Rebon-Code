//! The server half of `Mods`: a surface in another process asking a session's
//! owner about the Claude Code mods that owner loaded.
//!
//! The mods themselves are exercised against a real plane in
//! `rebon-plugin-host`'s `mods_plane` test. What is proven here is the wire:
//! that the request reaches the owner over its real server — the ACP spelling
//! first, as a client sends it — and that an owner whose plane loaded no mods
//! answers a snapshot with an empty table, so a polling surface reads
//! "nothing to draw", and refuses what needs a mod rather than hanging.

use super::super::*;
use super::support::*;

#[test]
fn an_owner_without_mods_answers_an_empty_table_and_refuses_the_rest() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-mods".into());
    let ipc = start_background_ipc_server(&store, state.job_id()).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    let owner = rebon_session_host::OwnerHandle::for_worker(
        "sess-mods",
        Some(state.job_id()),
        &ipc.owner().endpoint,
    );

    let first = owner
        .mods(serde_json::json!({ "op": "snapshot", "take": true, "surface": "desktop" }))
        .expect("a snapshot is answered");
    assert_eq!(first["version"], serde_json::json!(0));
    assert_eq!(first["changed"], serde_json::json!(true));
    assert_eq!(first["mods"], serde_json::json!([]));
    assert_eq!(first["commands"], serde_json::json!([]));

    let again = owner
        .mods(serde_json::json!({ "op": "snapshot", "since": 0 }))
        .expect("a second snapshot is answered");
    assert_eq!(again["changed"], serde_json::json!(false));

    owner
        .mods(serde_json::json!({ "op": "facts", "sessionId": "sess-mods" }))
        .expect("facts with nobody to tell are fine");

    let press = owner.mods(serde_json::json!({
        "op": "press", "plugin": "counter", "component": "Pane",
        "requestId": "counter", "element": "more",
    }));
    let error = press.expect_err("a press with no mod to take it is refused");
    assert!(
        format!("{error:#}").contains("no mods are loaded"),
        "{error:#}"
    );
}
