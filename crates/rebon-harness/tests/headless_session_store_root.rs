//! Where a headless session's files land when the caller names the store.
//!
//! `rebon exec --ephemeral` is the caller this pins: a run that must not leave
//! a session in the user's store, because the whole point of the flag is that
//! an eval harness can drive `exec` a thousand times without a thousand
//! sessions to delete by hand afterwards.
//!
//! Its own test binary for the reason `headless_session_hooks` records:
//! `REBON_CONFIG_DIR` has to be set before the process kernel and the plane
//! singleton first initialize, and both are process-wide. Multi-threaded
//! because the session build boots the plugin plane, which cannot make
//! progress on the single-threaded runtime a bare `#[tokio::test]` gives.

/// A provider that only has to resolve. No turn is run here, so nothing ever
/// reaches it.
fn offline_provider_config() -> serde_json::Value {
    serde_json::json!({
        "activeCustomProvider": "offline-provider",
        "customProviders": [{
            "name": "offline-provider",
            "format": "openai",
            "baseUrl": "http://127.0.0.1:1/v1",
            "apiKey": "not-a-real-key",
            "model": "offline-model"
        }]
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_headless_session_writes_to_the_store_the_caller_named() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let cwd = tempfile::tempdir().expect("cwd");
    let store = tempfile::tempdir().expect("store");

    std::fs::write(
        config_dir.path().join("config.json"),
        offline_provider_config().to_string(),
    )
    .expect("config written");
    std::env::set_var("REBON_CONFIG_DIR", config_dir.path());

    let session = rebon_harness::build_headless_session(rebon_harness::HarnessOverrides {
        cwd: Some(cwd.path().to_string_lossy().into_owned()),
        projects_root: Some(store.path().to_path_buf()),
        ..rebon_harness::HarnessOverrides::default()
    })
    .await
    .expect("the headless session builds against the configured provider");

    assert_eq!(
        session.projects_root.as_path(),
        store.path(),
        "the session kept the root it was given"
    );

    // Session files really were written, into the named root: the store is
    // not an empty directory the session merely pointed at.
    let written: Vec<_> = std::fs::read_dir(store.path())
        .expect("the store exists")
        .map(|entry| entry.expect("an entry").file_name())
        .collect();
    assert!(
        !written.is_empty(),
        "the session's files went into the named store"
    );

    // The half that matters to the person: nothing of this run is in the store
    // they browse, which is where the session would have landed by default.
    let default_store = config_dir.path().join("projects");
    assert!(
        !default_store.exists(),
        "the user's store was left alone, but {default_store:?} exists"
    );

    rebon_harness::shutdown_process_plugin_plane().await;
}
