//! The three IPC tests that need the binary's mirror types.
//!
//! A mirror's lease and the session options it forwards are read back through
//! `RemoteBackgroundAttachment` and `tui_lease_client_id`, which live in
//! `background::client` and stay with the terminal. The rest of the IPC
//! command tests live with the server in `rebon-session-runtime`.

use super::super::*;

fn install_ipc_owner(state: &mut BackgroundJobState, ipc: &BackgroundIpcServer) {
    let owner = ipc.owner();
    state.process.pid = Some(owner.endpoint.pid);
    state.process.pid_identity = owner.pid_identity;
    state.process.ipc_port = Some(owner.endpoint.port);
    state.process.ipc_token = Some(owner.endpoint.token);
}

fn store() -> (tempfile::TempDir, BackgroundStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = BackgroundStore::new(dir.path());
    (dir, store)
}

fn runtime() -> BackgroundRuntimeFields {
    BackgroundRuntimeFields {
        provider: None,
        model: None,
        fast_mode: None,
        channels: Vec::new(),
        development_channels: Vec::new(),
        provider_format: None,
        ui_mode: None,
        effort_level: None,
        permission_mode: None,
        capability_mode: rebon_types::AgentCapabilityMode::Normal,
        settings: Vec::new(),
        add_dirs: Vec::new(),
        plugin_dirs: Vec::new(),
        mcp_configs: Vec::new(),
        strict_mcp_config: false,
    }
}

#[test]
fn a_mirror_holds_a_lease_for_as_long_as_it_is_attached() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-mirror-lease".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let attachment = RemoteBackgroundAttachment::new(
        state.identity.job_id.clone(),
        "sess-mirror-lease".into(),
        ".".into(),
        BackgroundJobStatus::Idle,
        0,
        BackgroundIpcEndpoint {
            pid: std::process::id(),
            port: ipc.owner().endpoint.port,
            token: ipc.owner().endpoint.token.clone(),
        },
    );
    let client_id = tui_lease_client_id();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let leases = store
            .read_state(&state.identity.job_id)
            .unwrap()
            .lease
            .client_leases;
        if leases.iter().any(|lease| {
            lease.client_id == client_id && lease.kind == rebon_session_host::ClientLeaseKind::Tui
        }) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "attaching never took a lease"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(attachment.is_live());

    drop(attachment);

    assert!(
        store
            .read_state(&state.identity.job_id)
            .unwrap()
            .lease
            .client_leases
            .is_empty(),
        "dropping the attachment gives the lease up"
    );
    ipc.stop();
}

#[test]
fn a_mirrored_session_option_reaches_the_owner() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-option".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let endpoint = rebon_session_host::BackgroundIpcEndpoint {
        pid: std::process::id(),
        port: ipc.owner().endpoint.port,
        token: ipc.owner().endpoint.token.clone(),
    };
    let pending = spawn_remote_session_option(
        &state.identity.job_id,
        state.identity.session_id.as_deref().unwrap(),
        &endpoint,
        "effort".into(),
        "high".into(),
    );

    let output = pending
        .rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the owner answers")
        .expect("the owner accepts a known option");

    assert!(
        output.text.contains("next turn"),
        "the answer says when it takes effect rather than implying it already has: {}",
        output.text
    );
    assert_eq!(
        store
            .read_state(&state.identity.job_id)
            .unwrap()
            .identity
            .runtime
            .effort_level
            .as_deref(),
        Some("high"),
        "the change landed on the session the worker will build its next turn from"
    );
    ipc.stop();
}

/// An option the owner does not know must come back as a refusal the terminal
/// can show, not as a silent no-op that leaves the user believing otherwise.

#[test]
fn a_session_option_the_owner_rejects_is_reported() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-option-bad".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let endpoint = rebon_session_host::BackgroundIpcEndpoint {
        pid: std::process::id(),
        port: ipc.owner().endpoint.port,
        token: ipc.owner().endpoint.token.clone(),
    };
    let pending = spawn_remote_session_option(
        &state.identity.job_id,
        state.identity.session_id.as_deref().unwrap(),
        &endpoint,
        "effort".into(),
        "louder".into(),
    );

    let error = pending
        .rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the owner answers")
        .expect_err("`louder` is not an effort level");

    assert!(error.contains("louder"), "unexpected refusal: {error}");
    ipc.stop();
}
