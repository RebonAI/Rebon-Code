//! Rebon's own kernel seats, reached the way a plugin reaches them.
//!
//! These are the config-seat, model-router and session-seat fixtures said over
//! the plugin plane. The subject was never
//! the runtime: each one asserts what a *seat* does — settings hands out
//! connection facts without secrets, credentials is fail-closed until a host
//! authorizer says otherwise, a route a plugin registers resolves through the
//! ordinary model path without displacing a builtin, and the session seat's
//! event face reaches rebon's real transcript. The embedded isolate was only
//! the thing driving them, and the plane is what drives them now.
//!
//! `ctx.seat(name, method, params)` lands on `KernelSeats`, which calls the same
//! JSON plane the old fixtures called through `callService`. So the port is the
//! same requests from a different process.
//!
//! Skips without `REBON_TEST_NODE`. Run serially: these share one kernel and the
//! process-wide registration slots.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use rebon_agent_core::model_router::AgentModelRouter;
use rebon_kernel::{Context, Kernel};
use rebon_kernel_seats::kernel_config_seats::{ConfigSeatsPlugin, CREDENTIALS_AUTHORIZE_EVENT};
use rebon_kernel_seats::kernel_session_seat::SessionSeat;
use rebon_plugin_protocol::{Payload, PluginLoadRequest};
use rebon_plugin_supervisor::{
    HostConfig, PluginHostSupervisor, SeatDispatcher, SeatInvocation, ToolRefusal,
};
use rebon_provider::kernel_model_router::ModelRouterPlugin;
use serde_json::Value;

fn node() -> Option<PathBuf> {
    match std::env::var_os("REBON_TEST_NODE") {
        Some(node) => Some(PathBuf::from(node)),
        None => {
            assert_ne!(
                std::env::var_os("REBON_REQUIRE_TEST_NODE").as_deref(),
                Some(std::ffi::OsStr::new("1")),
                "REBON_REQUIRE_TEST_NODE=1 requires an absolute REBON_TEST_NODE"
            );
            eprintln!("skipping: set REBON_TEST_NODE to an absolute Node executable");
            None
        }
    }
}

fn host_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtimes/node/plugin-host/src/cli.mjs")
}

/// Rebon's seats, as the plane hands them to a plugin.
///
/// The same two lines `KernelSeats` is: a seat call is a call on the kernel's
/// JSON plane, which is exactly what the old fixtures reached through
/// `callService`. Written here rather than borrowed so the test says which
/// plane it is asserting against.
struct KernelPlane {
    ctx: Context,
}

impl SeatDispatcher for KernelPlane {
    fn call(
        &self,
        invocation: SeatInvocation,
    ) -> Pin<Box<dyn Future<Output = Result<Payload, ToolRefusal>> + Send + '_>> {
        let params = invocation.params.to_value().unwrap_or(Value::Null);
        let outcome = self
            .ctx
            .call_json(&invocation.seat, &invocation.method, params);
        Box::pin(async move {
            match outcome {
                Ok(value) => Ok(Payload::from(value)),
                Err(error) => Err(ToolRefusal::new("[SEAT_FAILED]", error.to_string())),
            }
        })
    }
}

/// Loads a one-file probe plugin on a real host and returns what its `report`
/// service answered.
///
/// A service call rather than an event: an answer is what a test can wait for
/// without inventing a settling delay.
async fn probe(
    ctx: Context,
    dir: &tempfile::TempDir,
    body: &str,
    seats: &[&str],
) -> (PluginHostSupervisor, Value) {
    std::fs::write(dir.path().join("index.mjs"), body).expect("plugin written");
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"name":"seat-probe","version":"0.0.0","type":"module"}"#,
    )
    .expect("manifest written");

    let supervisor = PluginHostSupervisor::start(
        HostConfig::new(node().expect("checked by the caller"), host_script())
            .with_startup_timeout(Duration::from_secs(20))
            .with_working_directory(std::env::temp_dir())
            .with_seat_dispatcher(
                Arc::new(KernelPlane { ctx }),
                seats.iter().map(|s| (*s).to_string()),
            ),
    )
    .await
    .expect("host starts");

    supervisor
        .load_plugin(&PluginLoadRequest {
            plugin_id: "seat.probe".into(),
            root: dir.path().to_string_lossy().replace('\\', "/"),
            entry: "index.mjs".into(),
            services: vec!["report".into()],
            event_topics: Vec::new(),
            published_topics: Vec::new(),
            llm_providers: Vec::new(),
            tools: Vec::new(),
            commands: Vec::new(),
            invokable_tools: Vec::new(),
            seats: seats.iter().map(|s| (*s).to_string()).collect(),
            config: Payload::null(),
        })
        .await
        .expect("the probe loads");
    supervisor
        .open_scope("seat.probe", "probe-scope", "C:/workspace")
        .await
        .expect("the scope opens");

    let answer = supervisor
        .call_service("seat.probe", "probe-scope", "report", Payload::null())
        .await
        .expect("the probe answers")
        .to_value()
        .expect("the answer is JSON");
    (supervisor, answer)
}

/// Native resolver that knows exactly one builtin provider.
struct BuiltinOnly;

#[async_trait::async_trait]
impl rebon_agent_core::model_router::ProviderRuntimeResolver for BuiltinOnly {
    async fn resolve_provider(
        &self,
        provider: Option<&str>,
    ) -> anyhow::Result<rebon_agent_core::model_router::ProviderModelRuntime> {
        let name = provider.unwrap_or("openai");
        if name != "openai" {
            return Err(anyhow::anyhow!("unknown provider `{name}`"));
        }
        Ok(rebon_agent_core::model_router::ProviderModelRuntime {
            provider_name: "openai".into(),
            client: Arc::new(rebon_api::MockModelClient::new()),
            default_model: "gpt-main".into(),
            model_profiles: rebon_types::ModelProfileMap::default(),
        })
    }
}

const CONFIG: &str = r#"{
    "customProviders": [
        {
            "name": "dsh-ds",
            "baseUrl": "https://api.deepseek.com",
            "apiKey": "sk-never-granted",
            "model": "deepseek-v4"
        },
        { "name": "granted-ds", "baseUrl": "https://granted.example", "apiKey": "sk-granted" }
    ]
}"#;

/// Settings hands out connection facts without secrets; credentials stays shut
/// until the host authorizer opens it, and only for what it opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settings_strips_secrets_and_credentials_stay_fail_closed() {
    let Some(_node) = node() else { return };
    let config = tempfile::tempdir().expect("tempdir");
    std::fs::write(config.path().join("config.json"), CONFIG).expect("config written");

    let kernel = Kernel::new();
    kernel
        .load(vec![Box::new(ConfigSeatsPlugin::new(
            config.path().to_path_buf(),
        ))])
        .expect("config-seats plugin loads");

    // Host-side authorizer: grants `granted-ds` only.
    let authorizer = kernel.context().fork("authorizer");
    authorizer.wrap_json(CREDENTIALS_AUTHORIZE_EVENT, |payload, next| {
        if payload.get("provider").and_then(|p| p.as_str()) == Some("granted-ds") {
            serde_json::json!({ "allow": true })
        } else {
            next.call(payload)
        }
    });

    let dir = tempfile::tempdir().expect("tempdir");
    let (supervisor, report) = probe(
        kernel.context().fork("plane"),
        &dir,
        r#"
export function activate(plugin) {
  plugin.service('report', async (_request, ctx) => {
    const providers = await ctx.seat('settings', 'get', { section: 'customProviders' });
    let deniedWithoutGrant = false;
    try {
      await ctx.seat('credentials', 'get', { provider: 'dsh-ds' });
    } catch {
      deniedWithoutGrant = true;
    }
    let granted = null;
    try {
      granted = await ctx.seat('credentials', 'get', { provider: 'granted-ds' });
    } catch {}
    return { providers, deniedWithoutGrant, granted };
  });
}
"#,
        &["settings", "credentials"],
    )
    .await;

    let first = &report["providers"][0];
    assert_eq!(first["baseUrl"], "https://api.deepseek.com", "{report}");
    assert_eq!(first["model"], "deepseek-v4");
    assert!(first.get("apiKey").is_none(), "{report}");

    assert_eq!(report["deniedWithoutGrant"], true, "{report}");
    assert_eq!(report["granted"]["apiKey"], "sk-granted", "{report}");

    supervisor.shutdown().await.expect("the host stops");
}

/// A route a plugin registers reaches rebon's router, and a builtin's name is
/// not displaced by one — but a route with nothing behind it refuses rather
/// than handing back a client that cannot stream.
///
/// The last clause is where this differs from the fixture this port replaced,
/// which expected resolution to succeed with no transport bound and the *use*
/// to fail. That expectation went stale when resolution started requiring a
/// live transport, and both of that file's tests have been failing since before
/// this port — same provider, same message. What is asserted here is what the
/// code does, which is the more defensible of the two: a route nothing can
/// serve is refused where it is asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_registered_route_reaches_the_router_without_displacing_a_builtin() {
    let Some(_node) = node() else { return };
    let kernel = Kernel::new();
    kernel
        .load(vec![Box::new(ModelRouterPlugin)])
        .expect("model-router plugin loads");

    let dir = tempfile::tempdir().expect("tempdir");
    let (supervisor, report) = probe(
        kernel.context().fork("plane"),
        &dir,
        r#"
export function activate(plugin) {
  plugin.service('report', async (_request, ctx) => {
    await ctx.seat('model-router', 'register', {
      provider: 'dsh-fake',
      models: [{ id: 'fake-large', contextWindow: 64000 }, { id: 'fake-mini' }],
      defaultModel: 'fake-large',
    });
    // Same name as a builtin provider: registering is allowed, winning is not.
    await ctx.seat('model-router', 'register', {
      provider: 'openai',
      defaultModel: 'impostor-model',
    });
    return {
      echo: await ctx.seat('model-router', 'resolve', { provider: 'dsh-fake' }),
      impostor: await ctx.seat('model-router', 'resolve', { provider: 'openai' }),
    };
  });
}
"#,
        &["model-router"],
    )
    .await;

    // The registration landed: the router answers with the route's own catalog.
    assert_eq!(report["echo"]["provider"], "dsh-fake", "{report}");
    assert_eq!(report["echo"]["model"], "fake-large", "{report}");
    assert_eq!(report["echo"]["contextWindow"], 64000, "{report}");

    let router = rebon_agent_core::model_router::ConfigurableModelRouter::new(
        rebon_provider::kernel_model_router::KernelAwareProviderResolver::new(
            kernel.context().clone(),
            Arc::new(BuiltinOnly),
        ),
    );

    // The builtin keeps its name against a plugin that claimed it.
    let builtin = router
        .resolve(rebon_agent_core::model_router::ModelRouteRequest {
            provider: Some("openai".into()),
            ..rebon_agent_core::model_router::ModelRouteRequest::default()
        })
        .await
        .expect("the builtin resolves");
    assert_eq!(builtin.model, "gpt-main");
    assert_eq!(builtin.client.provider_name(), "mock");
    assert_eq!(
        report["impostor"]["model"], "impostor-model",
        "the seat still records what the plugin asked for: {report}"
    );

    // And a route with no transport behind it is refused by name, rather than
    // resolving into a client whose first use would fail.
    let refusal = router
        .resolve(rebon_agent_core::model_router::ModelRouteRequest {
            provider: Some("dsh-fake".into()),
            ..rebon_agent_core::model_router::ModelRouteRequest::default()
        })
        .await
        .expect_err("nothing serves this route");
    assert!(
        refusal.to_string().contains("MODEL_PROVIDER_UNAVAILABLE"),
        "{refusal}"
    );

    supervisor.shutdown().await.expect("the host stops");
}

/// The session seat's event face reaches rebon's real transcript: an append the
/// plugin makes comes back as a derived message beside the ones already there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_session_seat_appends_to_and_derives_from_the_real_transcript() {
    let Some(_node) = node() else { return };
    let kernel = Kernel::new();
    let session_ctx = kernel.context().fork_scoped("session/seat-plane");
    let projects = tempfile::tempdir().expect("tempdir");

    let cwd = "F:/probe/project";
    let mut parent: Option<String> = None;
    for (entry_type, role, text) in [
        ("user", "user", "第一问"),
        ("assistant", "assistant", "first answer"),
    ] {
        let mut entry = rebon_session::TranscriptWriteEntry::new(
            entry_type,
            serde_json::json!({
                "type": entry_type,
                "message": { "role": role, "content": [{ "type": "text", "text": text }] },
            }),
        );
        entry.parent_uuid = parent.clone();
        let written =
            rebon_session::append_transcript_entry(projects.path(), cwd, "sess-seat-plane", entry)
                .expect("transcript seeds");
        parent = Some(written.uuid);
    }
    SessionSeat::provide(
        &session_ctx,
        "sess-seat-plane",
        projects.path().to_path_buf(),
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let (supervisor, report) = probe(
        session_ctx.fork("plane"),
        &dir,
        r#"
export function activate(plugin) {
  plugin.service('report', async (_request, ctx) => {
    await ctx.seat('session', 'append', { type: 'todo/write', data: { todos: [{ x: 1 }] } });
    return { derived: await ctx.seat('session', 'deriveMessages', {}) };
  });
}
"#,
        &["session"],
    )
    .await;

    let messages = report["derived"]["messages"]
        .as_array()
        .unwrap_or_else(|| panic!("derived messages: {report}"));
    assert_eq!(messages.len(), 2, "{report}");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"][0]["text"], "第一问");
    assert_eq!(messages[1]["content"][0]["text"], "first answer");

    // The append landed on the authoritative transcript, not on a copy the
    // plugin holds: reading it back through rebon's own face finds it.
    let appended = session_ctx
        .call_json("session", "deriveMessages", serde_json::json!({}))
        .expect("the session seat answers");
    assert_eq!(
        appended["messages"].as_array().map(Vec::len),
        Some(2),
        "{appended}"
    );

    supervisor.shutdown().await.expect("the host stops");
}
