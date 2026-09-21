//! The TUI's shell around a session, and the facade the rest of the binary
//! reaches harness helpers through.
//!
//! Everything this module used to assemble — provider clients, the model,
//! the policy store, the tool filter, the MCP stack, the session runtime —
//! now lives under [`crate::session`]. What is left is the terminal's launch
//! flags and the function that wraps a finished build in them.
//!
//! [`TuiEngineSession`] itself moved to `crate::session_shell` and is
//! re-exported below. It is cli-local either way; the point of moving it is
//! that it is not a *drawing* concern, and this module is where drawing
//! concerns live.

// The last harness helpers still reached through this module rather than
// from `rebon_harness` directly.
pub use rebon_harness::default_model;
pub(crate) use rebon_harness::{
    build_default_tool_filter_for_context, env_openai_service_tier_available,
    fallback_provider_format_from_env, openai_service_tier_available,
};

/// What a terminal was launched with: the flags it reads off the run's own
/// arguments before the builder consumes them, plus the notices the build
/// wants shown once.
///
/// They never reach the assembly: no surface without a screen has ever read
/// one, which is why the builder stopped filling them. The terminal's *live*
/// UI mode is not here — that one changes at runtime, and lives on the shell.
pub(crate) struct TuiStartupParams {
    /// Startup flag from `rebon agents`; opens Agent View before prompt input.
    pub agent_view: bool,
    /// Startup flag from `--hosted`; hands this session to a background
    /// worker once the UI has settled (after any resume dialog), so the
    /// terminal is a viewer from the first turn rather than the owner.
    /// Cleared by the event loop when it fires — it is a one-shot.
    pub hosted: bool,
    /// Startup flag from `--local`: this process hosts the session itself.
    /// A preference for how *this* terminal opens sessions, not a property
    /// of the session — `/hosted` still hands it to a worker.
    pub local: bool,
    /// Optional cwd scope from `rebon agents --cwd`; filters startup Agent View jobs.
    pub agent_view_cwd_scope: Option<String>,
    /// Non-persistent startup notices injected into the TUI transcript.
    /// Drained on the first frame.
    pub notices: Vec<String>,
}

impl TuiStartupParams {
    /// The flags, before the build runs. `notices` fills in from the build.
    pub(crate) fn from_overrides(overrides: &crate::rebon_config::RuntimeOverride) -> Self {
        Self {
            agent_view: overrides.startup_agent_view,
            hosted: overrides.startup_hosted,
            local: overrides.startup_local,
            agent_view_cwd_scope: overrides.startup_agent_view_cwd_scope.clone(),
            notices: Vec::new(),
        }
    }
}

/// Wrap an assembled session in the terminal's view state.
///
/// The staleness line is formatted here rather than in the assembly: the
/// assembly hands back when the transcript was written, and only a surface
/// with a screen turns that into a sentence.
pub(crate) fn into_tui_session(
    build: crate::session::build::SessionBuild,
    ui_mode: crate::ui_config::UiMode,
    mut startup: TuiStartupParams,
) -> TuiEngineSession {
    let resume_warning = build.resumed_at.and_then(|created_at| {
        crate::tui::runner::stale_resume_warning(
            created_at,
            Some(build.session.model.prune_level.budget.last_input_tokens()),
        )
    });
    startup.notices = build.notices;
    TuiEngineSession {
        session: build.session,
        resume_warning,
        ui_mode,
        configured_ui_mode: ui_mode,
        math_rendering_mode: crate::rebon_config::MathRenderingMode::Off,
        remote_background_attachment: None,
        pending_hosted_session: None,
        terminal_startup: startup,
    }
}

// The shell itself lives in `crate::session_shell`: it is cli-local but not a
// drawing concern, and keeping it out of this module is what lets the session
// half leave the binary. Re-exported so every `crate::tui::wiring::
// TuiEngineSession` keeps resolving.
pub(crate) use crate::session_shell::TuiEngineSession;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::mpsc::unbounded_channel;

    use rebon_tool::{McpClient, McpClientError, McpToolCall, McpToolDefinition};

    use crate::rebon_config::RuntimeOverride;
    use crate::session::build::{
        activate_initial_session_agent, build_session_with_runtime_controller, build_tui_session,
        resolve_runtime_model, system_prompt_config_for_engine,
    };
    use crate::session::mcp::{
        build_default_mcp_client_for_cwd, DelayedTuiMcpClient, McpClientBuild, SessionMcp,
        TuiMcpLoadEvent, TuiMcpLoadStatus,
    };
    use crate::session::session_handoff::HandedBack;

    /// Stands in for the `remote` plugin on the `declared-agent-source`
    /// seat: it names every host, whether or not one is configured.
    fn names_every_host(name: &str) -> Option<String> {
        Some(rebon_plugin_remote::agent_id(name))
    }

    #[test]
    fn initial_agent_resolution_preserves_remote_fallback_tristate() {
        let mut attempted = Vec::new();
        activate_initial_session_agent(None, None, names_every_host, |wanted| {
            attempted.push(wanted.to_string());
            Err("unavailable".into())
        })
        .expect("no requested agent stays local");
        assert!(attempted.is_empty());

        activate_initial_session_agent(
            None,
            Some("configured-missing".into()),
            names_every_host,
            |wanted| {
                attempted.push(wanted.to_string());
                Err("unavailable".into())
            },
        )
        .expect("configured default may stay local");
        assert_eq!(attempted, ["configured-missing"]);

        attempted.clear();
        let err = activate_initial_session_agent(
            Some("demanded-host"),
            None,
            names_every_host,
            |wanted| {
                attempted.push(wanted.to_string());
                Err("unavailable".into())
            },
        )
        .expect_err("explicit remote must fail instead of falling back");
        assert_eq!(attempted, [rebon_plugin_remote::agent_id("demanded-host")]);
        assert!(err.to_string().contains("demanded-host"));
    }

    /// With the `remote` plugin switched off nothing can name the host, and
    /// the session must say so rather than reporting a missing host: the
    /// user's spelling is fine, the feature is not loaded. Nothing is
    /// attempted either — there is no id to attempt.
    #[test]
    fn a_demanded_remote_with_the_plugin_disabled_names_the_plugin_not_the_host() {
        let mut attempted: Vec<String> = Vec::new();
        let err = activate_initial_session_agent(
            Some("demanded-host"),
            None,
            |_| None,
            |wanted| {
                attempted.push(wanted.to_string());
                Ok(wanted.to_string())
            },
        )
        .expect_err("a demand nobody can name must fail the session");
        assert!(attempted.is_empty(), "{attempted:?}");
        let message = err.to_string();
        assert!(message.contains("plugins.remote.enabled"), "{message}");
        assert!(message.contains("--remote demanded-host"), "{message}");
    }

    /// The `remote` plugin pins the server build to its own crate version
    /// and calls it "this binary's". Both take `version.workspace = true`,
    /// which is only checkable from here.
    #[test]
    fn the_remote_server_version_is_this_binarys_own() {
        assert_eq!(
            rebon_plugin_remote::server_version(),
            env!("CARGO_PKG_VERSION")
        );
    }

    fn clear_model_env() {
        for key in [
            "ANTHROPIC_API_KEY",
            "DEEPSEEK_API_KEY",
            "DEEPSEEK_MODEL",
            "OPENAI_API_KEY",
            "REBON_MODEL",
        ] {
            std::env::remove_var(key);
        }
    }

    struct EnvRestore {
        values: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvRestore {
        fn new(keys: &[&'static str]) -> Self {
            Self {
                values: keys
                    .iter()
                    .map(|key| (*key, std::env::var_os(key)))
                    .collect(),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, value) in self.values.drain(..).rev() {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn shared_system_prompt_config_loads_saved_prompt_overrides() {
        let _guard = crate::test_env::lock_env();
        let _env = EnvRestore::new(&["REBON_CONFIG_DIR"]);
        let config_dir = tempfile::tempdir().expect("config dir");
        std::env::set_var("REBON_CONFIG_DIR", config_dir.path());
        crate::rebon_config::save_system_prompt_overrides_in_dir(
            config_dir.path(),
            &crate::rebon_config::SystemPromptOverrides {
                normal: Some("custom normal persona".into()),
                minimal: Some("custom minimal persona".into()),
                chat: Some("custom chat persona".into()),
            },
        )
        .expect("save prompt overrides");

        let config = system_prompt_config_for_engine(
            &rebon_core::Engine::new(),
            &rebon_tool::ToolFilter::unrestricted(),
            "test-model",
            true,
        );

        assert_eq!(
            config.normal_system_prompt_override.as_deref(),
            Some("custom normal persona")
        );
        assert_eq!(
            config.minimal_system_prompt_override.as_deref(),
            Some("custom minimal persona")
        );
        assert_eq!(
            config.chat_system_prompt_override.as_deref(),
            Some("custom chat persona")
        );
    }

    // The stdio provider transport is retired, and with it the `rustc`-compiled
    // fake provider these tests drove. What it covered — a selected package
    // provider resolving and streaming — is covered against a real Node host by
    // `rebon-harness`'s `plugin_plane_e2e`, which is where the plane lives.

    #[tokio::test]
    async fn unselected_broken_plugin_provider_does_not_block_builtin_provider() {
        let _guard = crate::test_env::lock_env();
        let _env = EnvRestore::new(&[
            "ANTHROPIC_API_KEY",
            "DEEPSEEK_API_KEY",
            "OPENAI_API_KEY",
            "REBON_MODEL",
            "REBON_CONFIG_DIR",
            "HOME",
            "USERPROFILE",
        ]);
        clear_model_env();
        let temp = tempfile::tempdir().expect("temp dir");
        let cwd = temp.path().join("cwd");
        let config_dir = temp.path().join("config");
        let home = temp.path().join("home");
        let plugin_dir = temp.path().join("broken-plugin");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&plugin_dir).expect("plugin dir");
        std::env::set_var("REBON_CONFIG_DIR", &config_dir);
        std::env::set_var("HOME", &home);
        std::env::set_var("USERPROFILE", &home);
        std::fs::write(
            plugin_dir.join("rebon-plugin.json"),
            serde_json::json!({
                "name":"broken-provider-plugin",
                "version":"1.0.0",
                "capabilities":{
                    "modelProviders":{
                        "broken-provider":{
                            "transport":{"type":"plugin","entry":"missing.mjs"}
                        }
                    }
                }
            })
            .to_string(),
        )
        .expect("broken manifest");
        std::fs::write(
            config_dir.join("config.json"),
            serde_json::json!({
                "activeCustomProvider":"builtin-openai",
                "customProviders":[{
                    "name":"builtin-openai",
                    "format":"openai",
                    "baseUrl":"https://api.openai.com/v1",
                    "apiKey":"sk-test",
                    "model":"gpt-test"
                }]
            })
            .to_string(),
        )
        .expect("config");

        let runtime = resolve_runtime_model(RuntimeOverride {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            plugin_dirs: vec![plugin_dir.to_string_lossy().into_owned()],
            ..RuntimeOverride::default()
        })
        .await
        .expect("built-in provider still resolves");
        assert_eq!(runtime.provider_name, "builtin-openai");
        assert_eq!(runtime.model, "gpt-test");
    }

    /// Regression: a stale `settings.json` `model` key (written by the old
    /// `/model` flow) must not shadow the active provider's own model —
    /// switching providers has to switch to that provider's configured
    /// model. The global key only applies on the env fallback path.
    #[tokio::test]
    async fn legacy_global_user_model_does_not_shadow_active_provider_model() {
        let _guard = crate::test_env::lock_env();
        let _env = EnvRestore::new(&[
            "ANTHROPIC_API_KEY",
            "DEEPSEEK_API_KEY",
            "OPENAI_API_KEY",
            "REBON_MODEL",
            "REBON_CONFIG_DIR",
            "HOME",
            "USERPROFILE",
        ]);
        clear_model_env();
        let temp = tempfile::tempdir().expect("temp dir");
        let cwd = temp.path().join("cwd");
        let config_dir = temp.path().join("config");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::create_dir_all(&home).expect("home");
        std::env::set_var("REBON_CONFIG_DIR", &config_dir);
        std::env::set_var("HOME", &home);
        std::env::set_var("USERPROFILE", &home);
        std::fs::write(
            config_dir.join("config.json"),
            serde_json::json!({
                "activeCustomProvider":"jun",
                "customProviders":[{
                    "name":"jun",
                    "format":"openai",
                    "baseUrl":"https://relay.example",
                    "apiKey":"sk-test",
                    "model":"grok-4.5"
                }]
            })
            .to_string(),
        )
        .expect("config");
        std::fs::write(
            config_dir.join("settings.json"),
            serde_json::json!({"model":"gpt-5.6-sol"}).to_string(),
        )
        .expect("settings");

        let runtime = resolve_runtime_model(RuntimeOverride {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..RuntimeOverride::default()
        })
        .await
        .expect("provider resolves");
        assert_eq!(runtime.provider_name, "jun");
        assert_eq!(runtime.model, "grok-4.5");

        // An explicit --model override still wins.
        let runtime = resolve_runtime_model(RuntimeOverride {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            model: Some("explicit-model".into()),
            ..RuntimeOverride::default()
        })
        .await
        .expect("provider resolves with explicit override");
        assert_eq!(runtime.model, "explicit-model");
    }

    #[tokio::test]
    async fn delayed_tui_mcp_client_delegates_after_ready() {
        let delayed = DelayedTuiMcpClient::new();
        assert!(delayed.server_names().is_empty());
        assert!(delayed.cached_tool_definitions("server").is_none());
        assert!(delayed.list_tool_definitions("server").await.is_none());
        let err = delayed
            .call_tool(McpToolCall {
                server: "server".into(),
                name: "ping".into(),
                arguments: serde_json::json!({}),
            })
            .await
            .expect_err("call should fail before MCP load completes");
        assert!(matches!(err, McpClientError::UnknownServer(server) if server == "server"));

        let client = rebon_plugin_mcp::InMemoryMcpClient::new();
        client.register_tool_definition(
            "server",
            McpToolDefinition {
                name: "ping".into(),
                description: "Ping the server".into(),
                input_schema: serde_json::json!({"type":"object"}),
                read_only: true,
                destructive: false,
                open_world: false,
                search_hint: None,
                always_load: true,
            },
            serde_json::json!({"ok": true}),
        );
        delayed.set_delegate(Arc::new(client.clone()));

        assert_eq!(delayed.server_names(), vec!["server".to_string()]);
        assert_eq!(
            delayed
                .cached_tool_definitions("server")
                .expect("cached definitions")[0]
                .name,
            "ping"
        );
        assert_eq!(
            delayed
                .list_tool_definitions("server")
                .await
                .expect("listed definitions")[0]
                .description,
            "Ping the server"
        );
        let result = delayed
            .call_tool(McpToolCall {
                server: "server".into(),
                name: "ping".into(),
                arguments: serde_json::json!({"value": 1}),
            })
            .await
            .expect("delegated call succeeds");
        assert_eq!(result.content, serde_json::json!({"ok": true}));
        assert_eq!(client.calls().len(), 1);
    }

    #[tokio::test]
    async fn delayed_tui_mcp_client_shutdown_transport_detaches_delegate() {
        let delayed = DelayedTuiMcpClient::new();
        // Idempotent with no delegate attached.
        delayed.shutdown_transport().await;

        let client = rebon_plugin_mcp::InMemoryMcpClient::new();
        client.register_tool("server", "ping", serde_json::json!({"ok": true}));
        delayed.set_delegate(Arc::new(client));
        assert!(delayed.is_ready());

        delayed.shutdown_transport().await;

        // The delegate is detached so no further calls can reach a
        // torn-down transport.
        assert!(!delayed.is_ready());
        assert!(delayed.server_names().is_empty());
    }

    #[tokio::test]
    async fn build_tui_session_does_not_await_mcp_load() {
        let _guard = crate::test_env::lock_env();
        let _env = EnvRestore::new(&[
            "ANTHROPIC_API_KEY",
            "DEEPSEEK_API_KEY",
            "DEEPSEEK_MODEL",
            "OPENAI_API_KEY",
            "REBON_MODEL",
            "REBON_CONFIG_DIR",
            "HOME",
            "USERPROFILE",
            "REBON_MCP_SERVERS_JSON",
            "REBON_MCP_SERVERS",
        ]);
        clear_model_env();
        std::env::remove_var("REBON_MCP_SERVERS_JSON");
        std::env::remove_var("REBON_MCP_SERVERS");
        let temp = tempfile::tempdir().expect("temp dir");
        let cwd = temp.path().join("cwd");
        let config_dir = temp.path().join("config");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::create_dir_all(&home).expect("home");
        std::env::set_var("REBON_CONFIG_DIR", &config_dir);
        std::env::set_var("HOME", &home);
        std::env::set_var("USERPROFILE", &home);
        std::env::set_var("OPENAI_API_KEY", "sk-test");
        std::env::set_var("REBON_MODEL", "gpt-test");

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
        let _builder_guard =
            crate::session::mcp::set_test_tui_mcp_builder(Arc::new(move |_, _, _, _| {
                let started_tx = started_tx.clone();
                Box::pin(async move {
                    if let Some(tx) = started_tx.lock().expect("started tx").take() {
                        let _ = tx.send(());
                    }
                    std::future::pending::<anyhow::Result<McpClientBuild>>().await
                })
            }));

        let session = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            build_tui_session(RuntimeOverride {
                cwd: Some(cwd.to_string_lossy().into_owned()),
                ..RuntimeOverride::default()
            }),
        )
        .await
        .expect("TUI session construction should not wait for MCP loader")
        .expect("session builds");

        tokio::time::timeout(std::time::Duration::from_secs(5), started_rx)
            .await
            .expect("MCP loader future should start")
            .expect("started signal");
        let mcp = session
            .session
            .engine_half
            .mcp
            .as_ref()
            .expect("a built session hosts its MCP stack");
        assert!(matches!(mcp.load_status, TuiMcpLoadStatus::Loading));
        assert!(!mcp.delayed.is_ready());
    }

    /// RFC-0004 §16.15: a mirror carries the engine-core recipe, not the
    /// core — no system prompt config, no sandbox policy, no query
    /// executor built on its behalf. A session built to run turns fills
    /// the core during wiring, and the factory can mint the deferred
    /// core the day a mirror's session becomes this process's.
    #[tokio::test]
    async fn a_mirror_defers_the_engine_core_and_a_session_that_runs_turns_does_not() {
        let _guard = crate::test_env::lock_env();
        let _env = EnvRestore::new(&[
            "ANTHROPIC_API_KEY",
            "DEEPSEEK_API_KEY",
            "DEEPSEEK_MODEL",
            "OPENAI_API_KEY",
            "REBON_MODEL",
            "REBON_CONFIG_DIR",
            "HOME",
            "USERPROFILE",
            "REBON_MCP_SERVERS_JSON",
            "REBON_MCP_SERVERS",
        ]);
        clear_model_env();
        std::env::remove_var("REBON_MCP_SERVERS_JSON");
        std::env::remove_var("REBON_MCP_SERVERS");
        let temp = tempfile::tempdir().expect("temp dir");
        let cwd = temp.path().join("cwd");
        let config_dir = temp.path().join("config");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::create_dir_all(&home).expect("home");
        std::env::set_var("REBON_CONFIG_DIR", &config_dir);
        std::env::set_var("HOME", &home);
        std::env::set_var("USERPROFILE", &home);
        std::env::set_var("OPENAI_API_KEY", "sk-test");
        std::env::set_var("REBON_MODEL", "gpt-test");

        let mirror = build_tui_session(RuntimeOverride {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            attached_background_job_id: Some("bg-test-mirror".into()),
            ..RuntimeOverride::default()
        })
        .await
        .expect("mirror session builds");
        assert!(
            !mirror.session.engine_half.engine_core.is_built(),
            "a mirror must not pay for an engine core it never uses"
        );

        let local = build_tui_session(RuntimeOverride {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..RuntimeOverride::default()
        })
        .await
        .expect("local session builds");
        assert!(
            local.session.engine_half.engine_core.is_built(),
            "a session built to run turns fills the core during wiring"
        );

        // The mirror's factory reaches the same deferred cell: minting a
        // runtime through it is what would build the core late.
        let mirror_core = mirror.session.engine_half.engine_core.clone();
        let _ = mirror_core.core();
        assert!(mirror_core.is_built());
    }

    #[tokio::test]
    async fn session_mcp_wait_for_load_installs_a_load_that_lands_in_time() {
        let (tx, rx) = unbounded_channel();
        let mut mcp = SessionMcp::with_loader(rx, TuiMcpLoadStatus::Loading);
        let client = rebon_plugin_mcp::InMemoryMcpClient::new();
        client.register_tool("server", "ping", serde_json::json!({"ok": true}));
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let _ = tx.send(TuiMcpLoadEvent::Ready(McpClientBuild {
                client: Some(Arc::new(client) as Arc<dyn McpClient>),
                warnings: Vec::new(),
            }));
        });

        assert!(mcp.wait_for_load(std::time::Duration::from_secs(5)).await);

        assert!(matches!(mcp.load_status, TuiMcpLoadStatus::Ready { .. }));
        assert!(mcp.delayed.is_ready());
        assert_eq!(mcp.client.server_names(), vec!["server".to_string()]);
    }

    #[tokio::test]
    async fn session_mcp_wait_for_load_gives_up_on_a_slow_loader() {
        let (_tx, rx) = unbounded_channel();
        let mut mcp = SessionMcp::with_loader(rx, TuiMcpLoadStatus::Loading);

        assert!(
            !mcp.wait_for_load(std::time::Duration::from_millis(30))
                .await
        );

        assert!(matches!(mcp.load_status, TuiMcpLoadStatus::Loading));
        assert!(!mcp.delayed.is_ready());
    }

    /// A worker lends its MCP stack to every session it builds. The second
    /// build must get the servers the first one loaded — the same seam, still
    /// ready — and must not start a second load.
    #[tokio::test]
    async fn a_held_mcp_stack_is_lent_to_the_next_session_instead_of_reloaded() {
        let _guard = crate::test_env::lock_env();
        let _env = EnvRestore::new(&[
            "ANTHROPIC_API_KEY",
            "DEEPSEEK_API_KEY",
            "DEEPSEEK_MODEL",
            "OPENAI_API_KEY",
            "REBON_MODEL",
            "REBON_CONFIG_DIR",
            "HOME",
            "USERPROFILE",
            "REBON_MCP_SERVERS_JSON",
            "REBON_MCP_SERVERS",
        ]);
        clear_model_env();
        std::env::remove_var("REBON_MCP_SERVERS_JSON");
        std::env::remove_var("REBON_MCP_SERVERS");
        let temp = tempfile::tempdir().expect("temp dir");
        let cwd = temp.path().join("cwd");
        let config_dir = temp.path().join("config");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::create_dir_all(&home).expect("home");
        std::env::set_var("REBON_CONFIG_DIR", &config_dir);
        std::env::set_var("HOME", &home);
        std::env::set_var("USERPROFILE", &home);
        std::env::set_var("OPENAI_API_KEY", "sk-test");
        std::env::set_var("REBON_MODEL", "gpt-test");

        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let loads_seen = Arc::clone(&loads);
        let _builder =
            crate::session::mcp::set_test_tui_mcp_builder(Arc::new(move |_, _, _, _| {
                loads_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async {
                    let client = rebon_plugin_mcp::InMemoryMcpClient::new();
                    client.register_tool("server", "ping", serde_json::json!({"ok": true}));
                    Ok(McpClientBuild {
                        client: Some(Arc::new(client) as Arc<dyn McpClient>),
                        warnings: Vec::new(),
                    })
                })
            }));
        let overrides = || RuntimeOverride {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..RuntimeOverride::default()
        };

        let mut first = build_tui_session(overrides())
            .await
            .expect("first session builds");
        let mut held = first
            .session
            .engine_half
            .mcp
            .take()
            .expect("first session hosts MCP");
        assert!(held.wait_for_load(std::time::Duration::from_secs(5)).await);
        assert!(held.delayed.is_ready());
        let seam = Arc::clone(&held.client);

        let mut lent = HandedBack::default();
        lent.mcp = Some(held);
        let second = build_session_with_runtime_controller(
            overrides(),
            None,
            None,
            rebon_types::AgentCapabilityMode::Normal,
            None,
            lent,
        )
        .await
        .expect("second session builds");

        let mcp = second
            .session
            .engine_half
            .mcp
            .as_ref()
            .expect("second session hosts MCP");
        assert!(
            Arc::ptr_eq(&seam, &mcp.client),
            "the held seam is the one lent on"
        );
        assert!(mcp.delayed.is_ready());
        assert!(matches!(mcp.load_status, TuiMcpLoadStatus::Ready { .. }));
        assert_eq!(
            loads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a held stack is reused, not loaded again"
        );
    }

    #[tokio::test]
    async fn unreachable_mcp_server_is_skipped_with_a_warning_not_a_hard_error() {
        let temp = tempfile::tempdir().expect("temp dir");
        // An HTTP MCP server pointed at a port nothing is listening on: the
        // handshake is refused. Startup must degrade gracefully — surface a
        // warning and continue — rather than aborting the whole session.
        // `strict_mcp_config = true` ignores any ambient env / project
        // `.mcp.json`, so the test reads only this inline config and stays
        // hermetic regardless of the developer's environment.
        let inline =
            r#"{"mcpServers":{"unreachable":{"type":"http","url":"http://127.0.0.1:1/mcp"}}}"#;
        let build = build_default_mcp_client_for_cwd(
            temp.path(),
            Vec::new(),
            &[inline.to_string()],
            true,
            &[],
        )
        .await
        .expect("an unreachable MCP server must not fail session startup");

        assert!(
            build.client.is_none(),
            "no server came up, so there is no client to attach"
        );
        assert_eq!(
            build.warnings.len(),
            1,
            "the skipped server should produce exactly one warning: {:?}",
            build.warnings
        );
        assert!(
            build.warnings[0].contains("unreachable") && build.warnings[0].contains("skipped"),
            "warning should name the server and say it was skipped: {:?}",
            build.warnings[0]
        );
    }
}
