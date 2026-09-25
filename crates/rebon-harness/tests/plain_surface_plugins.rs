//! `rebon exec` and `--acp` run without model routing and Code Mode, whatever
//! the user's settings turn on.
//!
//! Its own test binary on purpose: the process kernel boots once and reads the
//! plugin switches and the declared surface while it does, so a kernel booted
//! for the plain surface cannot share a process with one booted as configured.
//! `configured_surface_plugins` is the same settings without the declaration.
//!
//! The session is the one `rebon exec` builds (`build_headless_session`), and
//! the questions are the ones the running code asks: the executor and the
//! sub-agent spawner look the router up on the engine's upstream context
//! before every first prompt, and `run_code` asks the kernel for its plugin
//! before it is offered or switched on.

use rebon_harness::kernel_bootstrap::{
    declare_plugin_surface, declared_plugin_surface, desired_from_settings,
    process_plugin_registry, PluginSurface,
};
use rebon_kernel::{PluginState, RegistryError};

/// A person's settings with both features on: routing configured the way the
/// settings row writes it, and Code Mode on for every new session.
fn write_user_style_config(config_dir: &std::path::Path) {
    std::fs::write(
        config_dir.join("config.json"),
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
        .to_string(),
    )
    .expect("config written");
    std::fs::write(
        config_dir.join("settings.json"),
        serde_json::json!({
            "plugins": {
                "code-mode": { "enabled": true, "defaultOn": true },
                "model-routing": {
                    "enabled": true,
                    "backend": "typesafe",
                    "classifierModel": "jev",
                    "classifierEndpoint": "https://classifier.invalid/v1",
                    "policy": "cheap first",
                    "routerModel": "offline-model"
                }
            }
        })
        .to_string(),
    )
    .expect("settings written");
}

fn state_of(id: &str) -> PluginState {
    process_plugin_registry()
        .snapshot()
        .into_iter()
        .find(|status| status.id == id)
        .map(|status| status.state)
        .expect("a built-in plugin")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plain_surface_session_gets_neither_routing_nor_run_code() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let cwd = tempfile::tempdir().expect("cwd");
    write_user_style_config(config_dir.path());
    std::env::set_var("REBON_CONFIG_DIR", config_dir.path());

    // What `rebon exec` and `run_acp_server` do first.
    declare_plugin_surface(PluginSurface::Plain);
    assert_eq!(declared_plugin_surface(), PluginSurface::Plain);

    let session = rebon_harness::build_headless_session(rebon_harness::HarnessOverrides {
        cwd: Some(cwd.path().to_string_lossy().into_owned()),
        ..rebon_harness::HarnessOverrides::default()
    })
    .await
    .expect("the headless session builds against the configured provider");

    let registry = process_plugin_registry();
    for id in [
        rebon_plugin_model_routing::PLUGIN_ID,
        rebon_kernel_seats::kernel_code_mode::PLUGIN_ID,
    ] {
        assert!(registry.is_withheld(id), "{id} is not withheld");
        assert_eq!(state_of(id), PluginState::Disabled, "{id} loaded anyway");
    }
    // Only those two: the rest of the table loads as the settings say.
    assert_eq!(state_of(rebon_plugin_skill::PLUGIN_ID), PluginState::Loaded);
    assert_eq!(
        state_of(rebon_plugin_agents::PLUGIN_ID),
        PluginState::Loaded
    );

    // Where the executor and the sub-agent spawner look for a router.
    let upstream = session
        .engine
        .upstream_tool_context()
        .expect("the session scope attached the process kernel");
    assert!(
        upstream
            .get::<rebon_core::model_routing::ModelRoutingService>()
            .is_none(),
        "a first prompt here would be routed"
    );

    // `defaultOn` did not switch the session on, `/codemode on` cannot, and
    // the tool the session carries is not offered.
    let status = rebon_kernel_seats::kernel_code_mode::command(&session.kernel_ctx, &[])
        .expect("the session binds Code Mode");
    assert!(status.starts_with("Code Mode: off"), "{status}");
    assert!(
        rebon_kernel_seats::kernel_code_mode::command(&session.kernel_ctx, &["on".into()]).is_err(),
        "/codemode on switched Code Mode on"
    );
    let run_code = session
        .kernel_ctx
        .get::<rebon_core::tool_seat::SessionToolsService>()
        .expect("the session registers its tools")
        .tool(rebon_kernel_seats::kernel_code_mode::RUN_CODE_TOOL_NAME)
        .expect("the session carries its run_code slot");
    assert!(!run_code.is_enabled(), "run_code would be offered");

    // Nothing turns them back on: not `/kernel enable`, not a settings write
    // (which reconciles from the same switches), not a reload.
    for id in [
        rebon_plugin_model_routing::PLUGIN_ID,
        rebon_kernel_seats::kernel_code_mode::PLUGIN_ID,
    ] {
        let err = registry.set_enabled(id, true).unwrap_err();
        assert!(matches!(err, RegistryError::Withheld(ref named) if named == id));
        registry.reload(id).expect("a known plugin");
    }
    registry.reconcile(&desired_from_settings());
    for id in [
        rebon_plugin_model_routing::PLUGIN_ID,
        rebon_kernel_seats::kernel_code_mode::PLUGIN_ID,
    ] {
        assert_eq!(state_of(id), PluginState::Disabled, "{id} came back");
    }
    assert!(upstream
        .get::<rebon_core::model_routing::ModelRoutingService>()
        .is_none());
    assert!(
        rebon_kernel_seats::kernel_code_mode::command(&session.kernel_ctx, &["on".into()]).is_err()
    );

    rebon_harness::shutdown_process_plugin_plane().await;
}
