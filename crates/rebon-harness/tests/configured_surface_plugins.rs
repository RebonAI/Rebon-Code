//! The surfaces that declare nothing — the terminal, `serve`, the background
//! host — still run model routing and Code Mode when the settings turn them
//! on.
//!
//! The other half of `plain_surface_plugins`, in its own binary for the same
//! reason: the process kernel boots once, and which surface it booted for is
//! decided then. Same settings, no declaration.

use rebon_harness::kernel_bootstrap::{
    declared_plugin_surface, process_plugin_registry, PluginSurface,
};
use rebon_kernel::PluginState;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_undeclared_surface_runs_routing_and_code_mode_as_configured() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let cwd = tempfile::tempdir().expect("cwd");
    write_user_style_config(config_dir.path());
    std::env::set_var("REBON_CONFIG_DIR", config_dir.path());
    assert_eq!(declared_plugin_surface(), PluginSurface::Configured);

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
        assert!(!registry.is_withheld(id), "{id} is withheld");
        let state = registry
            .snapshot()
            .into_iter()
            .find(|status| status.id == id)
            .map(|status| status.state)
            .expect("a built-in plugin");
        assert_eq!(state, PluginState::Loaded, "{id}");
    }
    let upstream = session
        .engine
        .upstream_tool_context()
        .expect("the session scope attached the process kernel");
    assert!(
        upstream
            .get::<rebon_core::model_routing::ModelRoutingService>()
            .is_some(),
        "the router is not where the executor looks for it"
    );
    // A resumed session here keeps the model it was routed onto.
    assert!(!rebon_core::model_routing::routing_withheld(Some(upstream)));
    let status = rebon_kernel_seats::kernel_code_mode::command(&session.kernel_ctx, &[])
        .expect("the session binds Code Mode");
    assert!(
        status.starts_with("Code Mode: on"),
        "defaultOn did not switch it on: {status}"
    );

    rebon_harness::shutdown_process_plugin_plane().await;
}
