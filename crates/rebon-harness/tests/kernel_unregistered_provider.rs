//! The path the TUI takes when someone selects a plugin model provider that
//! is not actually there, walked end to end: `resolve_runtime_model` →
//! `provider_registry::build_client` → `bind_plane_model_provider` →
//! `PluginPlane::load_standalone`.
//!
//! The package here is the one shape that used to slip through every gate:
//! a manifest that declares `silent-provider` and an `activate` that
//! registers nothing for it. Every check upstream passes — the contribution
//! is well formed, the package loads, the host answers — and the only place
//! left that knows the truth is the ready report. Before
//! `[UNREGISTERED_PROVIDER]` this resolved *successfully*, handing back a
//! client whose first turn died with `[UNDECLARED_ADAPTER]` long after the
//! person had committed to the session.
//!
//! Its own test binary, for the reason `kernel_mainline_deepseek_js` gives:
//! `REBON_CONFIG_DIR` has to be set before the process kernel and the plane
//! singleton first initialize, and both are process-wide. Multi-threaded for
//! the second reason that file records — the plane cannot make progress on a
//! single-threaded runtime, which is what a bare `#[tokio::test]` gives you.

use std::collections::BTreeMap;
use std::time::Duration;

use rebon_provider::model_provider_plugin::{
    MaterializedModelProviderTransport, MaterializedPluginModelProviderTransport,
    ModelProviderCapabilityManifest, PluginModelProviderContribution,
};

/// The Node the plane runs on, and the checkout it loads from.
fn point_at_checkout() -> Option<std::path::PathBuf> {
    let node = std::env::var_os("REBON_TEST_NODE")?;
    let repo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root resolves");
    let plain = |path: std::path::PathBuf| {
        let text = path.to_string_lossy().replace('\\', "/");
        text.strip_prefix("//?/").unwrap_or(&text).to_owned()
    };
    std::env::set_var("REBON_PLUGIN_NODE", node);
    std::env::set_var(
        "REBON_PLUGIN_HOST_JS",
        plain(repo.join("runtimes/node/plugin-host/src/cli.mjs")),
    );
    std::env::set_var(
        "REBON_COMPOSE_LOADER_JS",
        plain(repo.join("runtimes/node/compose-runtime/src/index.mjs")),
    );
    Some(repo)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_selected_provider_the_package_never_registers_fails_the_resolution() {
    let Some(repo) = point_at_checkout() else {
        eprintln!("skipping: set REBON_TEST_NODE to an absolute Node executable");
        return;
    };

    // The whole setup a person performs: select the provider in config.json.
    // Empty `baseUrl`/`apiKey`/`model` on purpose — the package is supposed to
    // be what supplies all three, which is exactly why nothing upstream of the
    // load can tell that it will not.
    let config_dir = tempfile::tempdir().expect("config dir");
    let cwd = tempfile::tempdir().expect("cwd");
    std::fs::write(
        config_dir.path().join("config.json"),
        serde_json::json!({
            "activeCustomProvider": "silent-provider",
            "customProviders": [{
                "name": "silent-provider",
                "format": "openai",
                "baseUrl": "",
                "apiKey": "",
                "model": ""
            }]
        })
        .to_string(),
    )
    .expect("config written");
    std::env::set_var("REBON_CONFIG_DIR", config_dir.path());

    let root = repo.join("crates/rebon-harness/tests/fixtures/silent-provider");
    let overrides = rebon_harness::HarnessOverrides {
        cwd: Some(cwd.path().to_string_lossy().into_owned()),
        plugin_model_providers: vec![PluginModelProviderContribution {
            id: "silent-provider".into(),
            plugin_name: "silent-provider-plugin".into(),
            source: "test:silent".into(),
            display_name: None,
            transport: MaterializedModelProviderTransport::Plugin(
                MaterializedPluginModelProviderTransport {
                    root,
                    entry: "provider.mjs".into(),
                },
            ),
            capabilities: ModelProviderCapabilityManifest::default(),
            default_model: Some("missing-model".into()),
            models: BTreeMap::new(),
            profiles: Default::default(),
        }],
        ..rebon_harness::HarnessOverrides::default()
    };

    // Bounded, because the failure this pins is "it never comes back" as much
    // as it is "it came back Ok": a refusal a person waits a minute for is not
    // a refusal they can act on.
    let error = tokio::time::timeout(
        Duration::from_secs(60),
        rebon_harness::resolve_runtime_model(&overrides),
    )
    .await
    .expect("the resolution answers well inside a minute");
    let error = match error {
        Ok(_) => panic!("a provider the package never registered must not resolve"),
        Err(error) => error,
    };

    let message = error.to_string();
    // The three words the person needs: which provider, that they selected it,
    // and that it is not there.
    assert!(message.contains("silent-provider"), "{message}");
    assert!(message.contains("selected"), "{message}");
    assert!(message.contains("unavailable"), "{message}");
    // And the two the packager needs: the code, and the name of the promise
    // the package broke.
    assert!(message.contains("[UNREGISTERED_PROVIDER]"), "{message}");

    // Stop what the resolution started, for the reason
    // `kernel_mainline_deepseek_js` records: the plane is a real Node child
    // held in a process-global, and a multi-threaded runtime does not finish
    // dropping while a task is still reading that child's stdout.
    if let Some(plane) = rebon_plugin_host::plugin_boot::ensure_process_plugin_plane(
        &rebon_harness::kernel_bootstrap::process_plugin_registry(),
    )
    .await
    {
        plane.shutdown().await;
    }
    std::env::remove_var("REBON_CONFIG_DIR");
}
