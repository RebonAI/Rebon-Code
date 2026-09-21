//! The owner descriptor is a gate, not a courtesy (design §4.2, §12.2 item 1).
//!
//! RFC-0004 §I3 says a host must be reachable from the moment it starts writing.
//! Publishing the descriptor best-effort broke that quietly: a client would see
//! the transcript growing, find no `<sid>.owner.json`, and have no way to reach
//! the process doing the writing. The only exit was to stop the job.
//!
//! So the write is the last thing that can fail before the session is bound,
//! and failing it fails the build.

use super::super::*;
use super::support::*;

/// A projects root that cannot hold a descriptor.
///
/// A *file* where the directory has to be: `write_session_owner` creates the
/// project directory under this path, and creating a directory beneath a file
/// fails on every platform. Deterministic, and it needs no permissions games.
fn unwritable_projects_root(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("projects-is-a-file");
    std::fs::write(&path, b"not a directory").expect("the blocking file is written");
    path
}

#[test]
fn a_descriptor_that_cannot_be_written_fails_the_start() {
    let (dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-descriptor".into());
    let ipc = start_background_ipc_server(&store, state.job_id()).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let blocked = unwritable_projects_root(dir.path());
    let error = publish_session_owner_descriptor(
        &blocked,
        "/work/descriptor",
        "sess-descriptor",
        state.job_id(),
        &ipc,
    )
    .expect_err("a descriptor that cannot be written is a failure");

    // The message has to name the session and say what the consequence is —
    // it is what the user sees on the failed job.
    let text = error.to_string();
    assert!(text.contains("sess-descriptor"), "{text}");
    assert!(text.contains("no client could reach this worker"), "{text}");
}

/// The happy path, so the failure test is not passing for the wrong reason.
#[test]
fn a_published_descriptor_names_this_worker_endpoint() {
    let (dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-descriptor-ok".into());
    let ipc = start_background_ipc_server(&store, state.job_id()).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let projects = dir.path().join("projects");
    publish_session_owner_descriptor(
        &projects,
        "/work/descriptor-ok",
        "sess-descriptor-ok",
        state.job_id(),
        &ipc,
    )
    .expect("the descriptor is published");

    let descriptor =
        rebon_session::read_session_owner(&projects, "/work/descriptor-ok", "sess-descriptor-ok")
            .expect("the descriptor is on disk");
    assert_eq!(descriptor.ipc_port, Some(ipc.port));
    assert_eq!(descriptor.ipc_token.as_deref(), Some(ipc.token.as_str()));
    assert_eq!(descriptor.job_id.as_deref(), Some(state.job_id()));
    assert_eq!(
        descriptor.surface,
        rebon_session::SessionOwnerSurface::Worker
    );
    assert_eq!(descriptor.pid, std::process::id());

    // And the resolver reaches it: a descriptor nobody can resolve would
    // satisfy the write and still leave the invariant broken.
    let owner = rebon_session_host::OwnerHandle::from_descriptor("sess-descriptor-ok", &descriptor)
        .expect("the descriptor names an endpoint");
    assert!(owner.ping(), "the endpoint the descriptor names answers");
}

/// The gate runs before anything is written for this session.
///
/// Asserted on the store rather than by reading the source: a future edit that
/// moves the publish below the first `append_event` would leave a job whose log
/// says it started while no descriptor exists.
#[test]
fn nothing_is_written_for_a_session_whose_descriptor_failed() {
    let (dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-descriptor-none".into());
    let ipc = start_background_ipc_server(&store, state.job_id()).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let before = store
        .read_events(state.job_id())
        .map(|events| events.len())
        .unwrap_or(0);

    let blocked = unwritable_projects_root(dir.path());
    assert!(publish_session_owner_descriptor(
        &blocked,
        "/work/descriptor-none",
        "sess-descriptor-none",
        state.job_id(),
        &ipc,
    )
    .is_err());

    let after = store
        .read_events(state.job_id())
        .map(|events| events.len())
        .unwrap_or(0);
    assert_eq!(
        before, after,
        "the failed gate wrote nothing to the job's log"
    );
    assert!(
        rebon_session::read_session_owner(
            &blocked,
            "/work/descriptor-none",
            "sess-descriptor-none"
        )
        .is_none(),
        "and left no descriptor behind"
    );
}
