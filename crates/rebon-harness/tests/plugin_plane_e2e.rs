//! The whole composition, on the plugin plane, from rebon's side.
//!
//! This is `kernel_compose_deepseek_js.rs` and `kernel_compose_dsh_tools_js.rs`
//! said over the new transport: a real Node process, the real plugin host, the
//! real composition runtime, the real vendored dsh plugins — and rebon's own
//! kernel seats on this side, unchanged. What each test asserts is what the
//! embedded runtime asserted: the route resolves through the ordinary model
//! client path, the tool arrives in the process `tool-registry`, and unloading
//! an entry withdraws exactly what it registered.
//!
//! Skips without `REBON_TEST_NODE`, like every other test that drives a real
//! child process; `REBON_REQUIRE_TEST_NODE=1` turns the skip into a failure.
//!
//! Run these serially (`--test-threads=1`). They share one process kernel and
//! the process-wide registration slots a composition writes into, so running
//! them at once is three tests writing one seat table.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use rebon_agent_core::model_router::{
    AgentModelRouter, ConfigurableModelRouter, ModelRouteRequest, ProviderModelRuntime,
    ProviderRuntimeResolver,
};
use rebon_api::CreateMessageRequest;
use rebon_kernel::Kernel;
use rebon_kernel_seats::kernel_compose_tools::{
    process_compose_tools, set_process_compose_tools, ComposeToolRegistry,
};
use rebon_kernel_seats::kernel_config_seats::{ConfigSeatsPlugin, CREDENTIALS_AUTHORIZE_EVENT};
use rebon_kernel_seats::kernel_core_commands::CoreCommandsPlugin;
use rebon_kernel_seats::kernel_prompt_sections::{ComposePromptSections, SYSTEM_PROMPT_SERVICE};
use rebon_plugin_host::plugin_plane::{ComposeEntry, ComposeNode, PluginPlane, PluginPlaneConfig};
use rebon_plugin_supervisor::{ToolInvocation, ToolInvoker, ToolRefusal};
use rebon_provider::kernel_llm_dispatch::unregister_llm_host;
use rebon_provider::kernel_model_router::{KernelAwareProviderResolver, ModelRouterPlugin};
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

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root resolves")
}

/// Windows verbatim prefixes are not paths the protocol's own rule accepts.
fn plain(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/")
}

/// Answers `tool/invoke` for the tests that need one.
struct RecordingTools {
    seen: Mutex<Vec<(String, Value)>>,
    answer: Value,
}

impl ToolInvoker for RecordingTools {
    fn invoke(
        &self,
        invocation: ToolInvocation,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<rebon_plugin_protocol::Payload, ToolRefusal>>
                + Send
                + '_,
        >,
    > {
        self.seen.lock().unwrap().push((
            invocation.tool.clone(),
            invocation.input.to_value().unwrap_or(Value::Null),
        ));
        let answer = self.answer.clone();
        Box::pin(async move { Ok(rebon_plugin_protocol::Payload::from(answer)) })
    }
}

/// These tests share one process-wide composition tool table.
///
/// `set_process_compose_tools` installs into a process-level slot — that is
/// what the real plane does, and what the tools seat is — so two of these
/// running side by side have one test's unload removing a tool the other is
/// about to look up. Serialising them says that out loud; making the table
/// per-test would be testing a table rebon does not have.
/// A tokio mutex rather than a std one: the guard is held across every await in
/// the test, which is the point, and this is the lock built for that. It also
/// has no poisoning — a test that panics holding it releases it, and the next
/// test runs instead of inheriting a failure.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn serialised() -> tokio::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().await
}

struct Fixture {
    kernel: Arc<Kernel>,
    plane: Arc<PluginPlane>,
    tools: Arc<RecordingTools>,
    _config_dir: tempfile::TempDir,
}

impl Fixture {
    /// Both halves of the settings chain this fixture's seat resolves — the
    /// config home and the project directory are the same temporary directory.
    fn config_dir(&self) -> &std::path::Path {
        self._config_dir.path()
    }
}

async fn boot(structure: Vec<ComposeNode>, exposed_tools: &[&str]) -> Option<Fixture> {
    boot_with_bound(structure, exposed_tools, None).await
}

/// `boot`, with the plane's unary-call deadline named. A test that wants to
/// watch the bound fire says a short one rather than waiting out the real
/// twenty seconds.
async fn boot_with_bound(
    structure: Vec<ComposeNode>,
    exposed_tools: &[&str],
    unary_call_timeout: Option<std::time::Duration>,
) -> Option<Fixture> {
    let node = node()?;
    let repo = repo();
    let config_dir = tempfile::tempdir().expect("config dir");
    let kernel = Kernel::new();
    kernel
        .load(vec![
            Box::new(ModelRouterPlugin),
            // The project half of the settings chain is pinned at the same
            // temporary directory, so a `.rebon/settings.json` in whatever
            // checkout the tests run from cannot reach a plugin's namespace.
            Box::new(
                ConfigSeatsPlugin::new(config_dir.path().to_path_buf())
                    .with_cwd(config_dir.path().to_path_buf()),
            ),
            // The command seat, with the real built-in table behind it: a
            // plugin's command has to land next to `/help` for the collision
            // rule to mean anything.
            Box::new(CoreCommandsPlugin::new(kernel.clone())),
        ])
        .expect("kernel seats load");

    let ctx = kernel.context().fork("plane");
    let registry = ComposeToolRegistry::new(Vec::<String>::new());
    ctx.provide_json("tool-registry", registry.clone())
        .expect("the composition tool seat provides");
    set_process_compose_tools(registry.clone());
    // A composition entry may contribute prompt sections, so the seat that
    // holds them has to exist — a registration rebon cannot hold fails the
    // load rather than being dropped on the floor.
    ctx.provide_json(SYSTEM_PROMPT_SERVICE, ComposePromptSections::new())
        .expect("the prompt section seat provides");

    let tools = Arc::new(RecordingTools {
        seen: Mutex::new(Vec::new()),
        answer: serde_json::json!({ "ok": true }),
    });
    let plane = PluginPlane::start(
        PluginPlaneConfig {
            node,
            // Plain paths throughout: a Windows verbatim prefix survives Rust
            // fine and confuses `pathToFileURL` on the other side, and every one
            // of these crosses to Node as a string.
            host_script: PathBuf::from(plain(&repo.join("runtimes/node/plugin-host/src/cli.mjs"))),
            loader: PathBuf::from(plain(
                &repo.join("runtimes/node/compose-runtime/src/index.mjs"),
            )),
            compose_root: PathBuf::from(plain(&repo.join("runtimes/node/compose-runtime"))),
            payload_dir: Some(PathBuf::from(plain(
                &repo.join("runtimes/node/compose-runtime/payload"),
            ))),
            structure,
            web: Value::Null,
            modules: BTreeMap::new(),
            exposed_tools: exposed_tools.iter().map(|t| (*t).to_string()).collect(),
            // The three the real plane exposes — `default_exposed_seats`. The
            // list used to be two here, so a package declaring `settings` (the
            // example one does now) was refused at load by the fixture and by
            // nothing else.
            exposed_seats: vec!["credentials".into(), "logger".into(), "settings".into()],
            tool_catalog: serde_json::json!([]),
            scope_id: None,
            working_directory: repo.clone(),
            unary_call_timeout,
        },
        ctx,
        registry,
        tools.clone() as Arc<dyn ToolInvoker>,
    )
    .await
    .expect("the plugin plane starts");

    Some(Fixture {
        kernel,
        plane,
        tools,
        _config_dir: config_dir,
    })
}

fn payload_entry(id: &str, module: &str, config: Value) -> ComposeEntry {
    ComposeEntry {
        id: id.to_owned(),
        root: plain(&repo().join("runtimes/node/compose-runtime/payload")),
        entry: module.to_owned(),
        config,
        ..ComposeEntry::default()
    }
}

/// One canned chat-completions SSE reply, so the adapter's whole transport
/// pipeline runs offline.
async fn fake_deepseek() -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fake endpoint binds");
    let addr = listener.local_addr().expect("bound address");
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    match socket.read(&mut tmp).await {
                        Ok(0) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(_) => return,
                    }
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let sse = concat!(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\"}}]}\n\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n",
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    sse.len(),
                    sse
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    addr
}

/// A reload restarts exactly what changed, against the real host.
///
/// The classifier is unit-tested next to the rule; this checks the half that
/// only a running composition can show: that stopping and starting real
/// plugins works, that an unchanged entry is left alone rather than restarted,
/// and that the generation only moves when something did.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reload_restarts_what_changed_and_leaves_the_rest_running() {
    let _serial = serialised().await;
    let Some(fixture) = boot(
        vec![
            ComposeNode {
                id: "timer".into(),
                ..Default::default()
            },
            ComposeNode {
                id: "logger".into(),
                ..Default::default()
            },
        ],
        &[],
    )
    .await
    else {
        return;
    };

    let timer = payload_entry("timer", "vendor/dsh/timer.js", serde_json::json!({}));
    let logger = payload_entry(
        "logger",
        "vendor/dsh/logger-console.js",
        serde_json::json!({}),
    );
    fixture
        .plane
        .load_entry(&timer)
        .await
        .expect("the timer plugin loads");
    fixture
        .plane
        .load_entry(&logger)
        .await
        .expect("the logger plugin loads");
    assert_eq!(fixture.plane.generation(), 0, "loading is not reloading");

    // Same logger, reconfigured timer.
    let mut retimed = timer.clone();
    retimed.config = serde_json::json!({ "tick": 5 });
    let outcome = fixture
        .plane
        .reload(&[logger.clone(), retimed.clone()])
        .await
        .expect("the reload runs");

    assert_eq!(outcome.failed, Vec::new(), "{:?}", outcome.failed);
    assert_eq!(outcome.unchanged, vec!["logger".to_string()]);
    assert_eq!(outcome.changed, vec!["timer".to_string()]);
    assert!(outcome.added.is_empty());
    assert!(outcome.removed.is_empty());
    assert_eq!(outcome.generation, 1);

    // The reconfigured entry is what is running now — a reload that reported a
    // restart but kept the old configuration would pass every check above.
    let running = fixture.plane.loaded_entries();
    let timer_now = running
        .iter()
        .find(|entry| entry.id == "timer")
        .expect("the timer is still loaded");
    assert_eq!(timer_now.config, serde_json::json!({ "tick": 5 }));

    // Asking again with the same composition moves nothing, and the generation
    // stays put: it is what a caller compares to know whether anything changed.
    let quiet = fixture
        .plane
        .reload(&[logger.clone(), retimed.clone()])
        .await
        .expect("the second reload runs");
    assert!(!quiet.touched_anything(), "{quiet:?}");
    assert_eq!(quiet.unchanged.len(), 2);
    assert_eq!(quiet.generation, 1);

    // And dropping one stops it without touching the other.
    let dropped = fixture
        .plane
        .reload(&[retimed])
        .await
        .expect("the third reload runs");
    assert_eq!(dropped.removed, vec!["logger".to_string()]);
    assert_eq!(dropped.unchanged, vec!["timer".to_string()]);
    assert_eq!(dropped.generation, 2);
    assert_eq!(
        fixture
            .plane
            .loaded_entries()
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>(),
        vec!["timer".to_string()]
    );

    fixture.plane.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_model_route_a_composition_registered_resolves_through_the_ordinary_path() {
    let _serial = serialised().await;
    let Some(fixture) = boot(
        vec![ComposeNode {
            id: "llm-deepseek".into(),
            ..Default::default()
        }],
        &[],
    )
    .await
    else {
        return;
    };
    let addr = fake_deepseek().await;
    std::env::set_var("REBON_TEST_PLANE_KEY", "sk-plane-fake");

    let mut entry = payload_entry(
        "llm-deepseek",
        "vendor/dsh/llm-deepseek.js",
        serde_json::json!({
            "apiKeyEnv": "REBON_TEST_PLANE_KEY",
            "baseURL": format!("http://{addr}"),
            "models": [{ "id": "deepseek-v4-flash", "name": "F", "contextWindow": 128000 }],
        }),
    );
    entry.llm_providers = vec!["deepseek-official".into()];
    entry.seats = vec!["credentials".into()];

    let report = fixture
        .plane
        .load_entry(&entry)
        .await
        .expect("the real dsh plugin loads");
    assert_eq!(report.ready.llm_providers, vec!["deepseek-official"]);
    fixture
        .plane
        .open_session("session-1", "C:/workspace")
        .await
        .expect("the session opens for every plugin");

    // The grant the credentials seat needs. Without it the composition's own
    // fail-closed path fires, which the next test covers.
    let authorizer = fixture.kernel.context().fork("authorizer");
    authorizer.wrap_json(CREDENTIALS_AUTHORIZE_EVENT, |payload, next| {
        if payload.get("env").and_then(|e| e.as_bool()) == Some(true)
            && payload.get("ref").and_then(|r| r.as_str()) == Some("REBON_TEST_PLANE_KEY")
        {
            serde_json::json!({ "allow": true })
        } else {
            next.call(payload)
        }
    });

    // Nothing below knows a plugin plane exists: this is the same resolver and
    // the same model client a session uses.
    let resolver =
        KernelAwareProviderResolver::new(fixture.kernel.context().clone(), Arc::new(NoBuiltins));
    let resolved = ConfigurableModelRouter::new(resolver)
        .resolve(ModelRouteRequest {
            provider: Some("deepseek-official".into()),
            ..ModelRouteRequest::default()
        })
        .await
        .expect("the composition's route resolves");
    assert_eq!(resolved.provider_name, "deepseek-official");
    assert_eq!(resolved.model, "deepseek-v4-flash");
    let mut stream = resolved
        .client
        .create_message_stream(CreateMessageRequest::simple("deepseek-v4-flash", "你好"))
        .await
        .expect("the stream starts");
    let mut sawtext = false;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("the turn must not hang")
    {
        if let Ok(rebon_api::StreamEvent::ContentBlockDelta { delta, .. }) = event {
            if let rebon_api::ContentBlockDelta::TextDelta { text } = delta {
                sawtext |= text.contains('你');
            }
        }
    }
    assert!(sawtext, "the model's text reached the ordinary client path");

    drop(stream);
    fixture.plane.shutdown().await;
    unregister_llm_host("deepseek-official");
    std::env::remove_var("REBON_TEST_PLANE_KEY");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_composition_tool_lands_in_the_process_registry_and_leaves_on_unload() {
    let _serial = serialised().await;
    let Some(fixture) = boot(
        vec![ComposeNode {
            id: "tool-todo".into(),
            ..Default::default()
        }],
        &[],
    )
    .await
    else {
        return;
    };

    let mut entry = payload_entry(
        "tool-todo",
        "vendor/dsh/tool-todo.js",
        serde_json::json!({ "allowParallelInProgress": false }),
    );
    entry.tools = vec!["todo_write".into()];
    entry.published_topics = vec!["compose:session/append".into()];

    fixture
        .plane
        .load_entry(&entry)
        .await
        .expect("the real dsh tool plugin loads");

    let registry = process_compose_tools().expect("the process tool table is bound");
    assert_eq!(registry.tool_names(), vec!["todo_write".to_string()]);
    let tool = registry
        .tool("todo_write")
        .expect("the tool is dispatchable");
    assert_eq!(tool.id().as_str(), "todo_write");

    fixture
        .plane
        .unload_entry("tool-todo")
        .await
        .expect("the entry drains");
    assert!(
        registry.tool_names().is_empty(),
        "unloading withdrew exactly what the entry registered"
    );

    fixture.plane.shutdown().await;
    let _ = fixture.tools;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_composition_tool_runs_through_the_plane_and_reaches_rebon_own_tools() {
    let _serial = serialised().await;
    let Some(fixture) = boot(
        vec![ComposeNode {
            id: "tool-web".into(),
            ..Default::default()
        }],
        &["WebSearch", "WebFetch"],
    )
    .await
    else {
        return;
    };

    let mut entry = payload_entry("tool-web", "vendor/dsh/tool-web.js", serde_json::json!({}));
    entry.tools = vec!["web_search".into(), "web_fetch".into()];
    entry.invokable_tools = vec!["WebSearch".into(), "WebFetch".into()];

    fixture
        .plane
        .load_entry(&entry)
        .await
        .expect("the real dsh web tool plugin loads");
    fixture
        .plane
        .open_session("session-1", "C:/workspace")
        .await
        .expect("the session opens");

    let registry = process_compose_tools().expect("the process tool table is bound");
    let tool = registry
        .tool("web_search")
        .expect("web_search is dispatchable");

    // The tool body runs in the composition, reaches its web seat, and the seat
    // reaches rebon's own WebSearch through `tool/invoke` — attributed to the
    // plugin whose tool caused it.
    let context = rebon_tool::ToolContext::new();
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        tool.call(serde_json::json!({ "query": "rebon" }), &context),
    )
    .await
    .expect("the tool call must not hang");
    assert!(outcome.is_ok(), "{outcome:?}");
    let invoked = fixture.tools.seen.lock().unwrap().clone();
    assert_eq!(invoked.len(), 1, "{invoked:?}");
    assert_eq!(invoked[0].0, "WebSearch");

    fixture.plane.shutdown().await;
}

/// A provider resolver with nothing built in, so only the composition's route
/// can answer.
struct NoBuiltins;

#[async_trait::async_trait]
impl ProviderRuntimeResolver for NoBuiltins {
    async fn resolve_provider(
        &self,
        provider: Option<&str>,
    ) -> anyhow::Result<ProviderModelRuntime> {
        anyhow::bail!("unknown provider `{}`", provider.unwrap_or("<none>"))
    }
}

/// A package's model provider, end to end on the plane.
///
/// This is the second cut of RFC kernel-plugins §8 from rebon's side: a
/// provider that used to be a child process speaking a protocol of its own is
/// a plugin on the shared host, and every piece of what that protocol carried
/// has somewhere to be. What only a running host can show, and this asserts:
///
/// * a turn goes out as `ModelProviderTurnV1` and comes back as
///   `StreamEventV1` chunks, translated into rebon's own events;
/// * the user's connection settings ride the turn rather than a handshake;
/// * capabilities are the union of what the package declared and what the
///   adapter reported — neither alone;
/// * `reset` / `endTurn` / `invalidate` reach the adapter on `llm/control`;
/// * a reader that walks away mid-turn makes the adapter's own signal fire.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_package_model_provider_streams_signals_and_cancels_on_the_plane() {
    use rebon_api::model_provider_protocol::ProviderConnectionConfigV1;
    use rebon_api::ModelClient;
    use rebon_provider::model_provider_plugin::{
        MaterializedModelProviderTransport, MaterializedPluginModelProviderTransport,
        ModelProviderCapabilityManifest, PluginModelProviderContribution,
    };
    use rebon_provider::plane_model_provider::PlaneModelProviderClient;

    let _serial = serialised().await;
    let Some(fixture) = boot(Vec::new(), &[]).await else {
        return;
    };

    let root = repo().join("crates/rebon-harness/tests/fixtures/model-provider");
    let entry = ComposeEntry {
        id: "fixture-provider".into(),
        root: plain(&root),
        entry: "provider.mjs".into(),
        llm_providers: vec!["fixture-provider".into()],
        ..ComposeEntry::default()
    };
    fixture
        .plane
        .load_standalone(&entry)
        .await
        .expect("the provider plugin loads on the shared host");
    // Idempotent: a second session selecting the same provider reaches the
    // adapter that is already up rather than being refused.
    fixture
        .plane
        .load_standalone(&entry)
        .await
        .expect("loading it again is a no-op");

    let contribution = PluginModelProviderContribution {
        id: "fixture-provider".into(),
        plugin_name: "fixture".into(),
        source: "test:fixture".into(),
        display_name: None,
        transport: MaterializedModelProviderTransport::Plugin(
            MaterializedPluginModelProviderTransport {
                root: root.clone(),
                entry: "provider.mjs".into(),
            },
        ),
        // The manifest half of the capability set; the adapter reports the
        // other half, and the client must end up with both.
        capabilities: ModelProviderCapabilityManifest {
            forced_tool_choice: true,
            ..Default::default()
        },
        default_model: Some("fixture-pro".into()),
        models: BTreeMap::new(),
        profiles: Default::default(),
    };
    let connection = ProviderConnectionConfigV1 {
        api_key: "sk-fixture".into(),
        ..Default::default()
    };
    let client = PlaneModelProviderClient::bind(PlaneModelProviderClient::config_for(
        &contribution,
        "fixture-provider".into(),
        fixture.plane.workspace_root().to_string(),
        Arc::clone(fixture.plane.supervisor()),
        Some(connection),
    ))
    .await
    .expect("the client binds to the loaded adapter");

    // The union, not either half.
    assert!(
        client.supports_forced_tool_choice(),
        "the package manifest's capability survived"
    );
    assert!(
        client.output_budget_includes_reasoning(),
        "the adapter's reported `reasoningText` reached the client"
    );
    assert!(
        client.supports_anchored_minimal(),
        "the adapter's reported `anchoredMinimal` reached the client"
    );

    // One ordinary turn.
    let mut stream = client
        .create_message_stream(CreateMessageRequest::simple("fixture-pro", "hi"))
        .await
        .expect("the turn starts");
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event.expect("the turn streams without errors") {
            rebon_api::StreamEvent::ContentBlockDelta {
                delta: rebon_api::ContentBlockDelta::TextDelta { text: piece },
                ..
            } => text.push_str(&piece),
            rebon_api::StreamEvent::MessageStop => break,
            _ => {}
        }
    }
    assert_eq!(text, "你好");

    // A turn rebon stops reading: the adapter's own abort signal has to fire,
    // which only happens if the client sends a cancel on the way out.
    let dropped = client
        .create_message_stream(CreateMessageRequest::simple("fixture-slow", "hi"))
        .await
        .expect("the slow turn starts");
    drop(dropped);

    client.reset_session_state();
    client.end_turn();
    client.invalidate_previous_response_id();

    // The signals and the cancel are all fire-and-forget, so the adapter is
    // asked what it saw rather than the test guessing when they landed.
    let mut report = String::new();
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut stream = client
            .create_message_stream(CreateMessageRequest::simple("fixture-report", "hi"))
            .await
            .expect("the report turn starts");
        report.clear();
        while let Some(event) = stream.next().await {
            match event.expect("the report streams") {
                rebon_api::StreamEvent::ContentBlockDelta {
                    delta: rebon_api::ContentBlockDelta::TextDelta { text: piece },
                    ..
                } => report.push_str(&piece),
                rebon_api::StreamEvent::MessageStop => break,
                _ => {}
            }
        }
        let seen: Value = serde_json::from_str(&report).expect("the adapter reports JSON");
        let signals = seen["signals"].as_array().cloned().unwrap_or_default();
        if signals.len() == 3 && seen["aborted"] == Value::Bool(true) {
            break;
        }
    }
    let seen: Value = serde_json::from_str(&report).expect("the adapter reports JSON");
    // A set, not a sequence. The three signals are fire-and-forget — each is
    // spawned and none waits for the last — so asserting an order would be
    // asserting something the contract does not promise and the scheduler does
    // not deliver.
    let mut signals: Vec<String> = seen["signals"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect();
    signals.sort();
    assert_eq!(
        signals,
        vec![
            "endTurn".to_string(),
            "invalidate".to_string(),
            "reset".to_string()
        ],
        "all three control signals reached the adapter: {report}"
    );
    assert_eq!(
        seen["aborted"],
        Value::Bool(true),
        "dropping the stream cancelled the turn upstream: {report}"
    );
    assert_eq!(
        seen["apiKeys"][0],
        Value::String("sk-fixture".into()),
        "the connection settings rode the turn: {report}"
    );
    // Its own scope, not the plane's: that is what keeps two sessions binding
    // to one loaded adapter from sharing its state.
    let scopes = seen["scopes"].as_array().cloned().unwrap_or_default();
    assert_eq!(scopes.len(), 1, "one client, one scope: {report}");
    let scope = scopes[0].as_str().unwrap_or_default();
    assert!(
        scope.starts_with("provider:fixture-provider#"),
        "turns and signals travelled on the client's own scope, not `{}`: {report}",
        fixture.plane.scope()
    );

    fixture.plane.shutdown().await;
}

/// A plugin's slash commands and services, on the same seats a built-in uses.
///
/// The third cut of RFC kernel-plugins §8 from rebon's side, and the half the
/// first two left out: an external plugin could contribute tools and a model
/// route, but a command it registered reached nothing and a service it
/// registered was only ever a name in a routing table. What this asserts:
///
/// * all three command kinds reach the seat, with the spec the plugin wrote —
///   aliases, hint, category and surfaces included;
/// * a `prompt` command is answered by the plugin, and what it answers is
///   what the seat's handler returns;
/// * an `explain` and a `panel` answer without a round trip, because neither
///   has anything to ask;
/// * a service the plugin registered is callable on the entry's own scope;
/// * unloading takes every one of them out;
/// * a name a built-in already answers to is refused, and the refusal names
///   the collision rather than shadowing `/help`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_contributes_commands_and_services_to_the_seats() {
    use rebon_command_seat::{CommandArgs, CommandHandler, CommandSeatService, Surface};

    let _serial = serialised().await;
    // No composition: this plugin is not a composition entry, it is a plain
    // one on the shared host, which is what an installed package contributes.
    let Some(fixture) = boot(Vec::new(), &[]).await else {
        return;
    };
    let seat = fixture
        .kernel
        .context()
        .get::<CommandSeatService>()
        .expect("the core-commands plugin provides the seat");
    let builtins = seat.len();

    let root = repo().join("crates/rebon-harness/tests/fixtures/commands");
    let mut entry = ComposeEntry {
        id: "fixture-commands".into(),
        root: plain(&root),
        entry: "plugin.mjs".into(),
        services: vec!["fixture-report".into()],
        commands: vec![
            "fixture-echo".into(),
            "fixture-elsewhere".into(),
            "fixture-panel".into(),
            "fixture-silent".into(),
            "help".into(),
        ],
        ..ComposeEntry::default()
    };
    fixture
        .plane
        .load_standalone(&entry)
        .await
        .expect("the command plugin loads");

    assert_eq!(
        fixture.plane.registered_commands("fixture-commands"),
        vec![
            "fixture-echo".to_string(),
            "fixture-elsewhere".to_string(),
            "fixture-panel".to_string(),
            // The one that never answers; its own test drives it, and it is
            // listed here because every declared command lands on the seat
            // whether or not anyone ever calls it.
            "fixture-silent".to_string()
        ]
    );
    assert_eq!(
        fixture.plane.registered_services("fixture-commands"),
        vec!["fixture-report".to_string()]
    );
    assert_eq!(seat.len(), builtins + 4, "four commands joined the seat");

    // The spec the plugin wrote, as the seat holds it.
    let echo = seat.find("fixture-echo").expect("the command is findable");
    assert_eq!(
        echo.spec.description,
        "Echo the rest of the line back as a prompt"
    );
    assert_eq!(echo.spec.hint.as_deref(), Some("<text>"));
    assert!(seat.find("fx").is_some(), "an alias resolves to it");
    assert!(
        echo.spec.available_on(Surface::Tui) && echo.spec.available_on(Surface::Desktop),
        "the surfaces it declared: {:?}",
        echo.spec.surfaces
    );

    // A prompt command asks the plugin, and the answer is what the plugin said.
    let CommandHandler::Prompt(run) = &echo.handler else {
        panic!(
            "a prompt command carries a prompt handler: {:?}",
            echo.handler
        );
    };
    let expanded = run(&CommandArgs {
        raw: "/fixture-echo hello there".into(),
        rest: "hello there".into(),
        surface: Surface::Tui,
    })
    .expect("the plugin answered rather than failing");
    assert!(
        expanded.starts_with("fixture-echo(tui): hello there ["),
        "the plugin answered the invocation: {expanded}"
    );

    // The other two answered at registration.
    let elsewhere = seat.find("fixture-elsewhere").expect("registered");
    assert!(
        matches!(&elsewhere.handler, CommandHandler::Explain(text) if text.contains("desktop app")),
        "{:?}",
        elsewhere.handler
    );
    let panel = seat.find("fixture-panel").expect("registered");
    assert!(
        matches!(&panel.handler, CommandHandler::Panel(dialog) if dialog == "fixture-dialog"),
        "{:?}",
        panel.handler
    );

    // The service is callable on the entry's own scope, and answers.
    let answered = fixture
        .plane
        .call_service(
            "fixture-commands",
            fixture.plane.scope(),
            "fixture-report",
            serde_json::json!({"method": "ping", "params": {"n": 1}}),
        )
        .await
        .expect("the plugin service answers");
    assert_eq!(answered["saw"], "ping", "{answered}");

    // Unloading takes all of it out.
    fixture
        .plane
        .unload_entry("fixture-commands")
        .await
        .expect("the entry drains");
    assert_eq!(seat.len(), builtins, "the seat is back to the built-ins");
    assert!(seat.find("fixture-echo").is_none());
    assert!(seat.find("fx").is_none(), "the alias went with it");

    // A name a built-in already answers to is refused, and the entry with it.
    entry.config = serde_json::json!({ "takeHelp": true });
    let refused = fixture
        .plane
        .load_standalone(&entry)
        .await
        .expect_err("a plugin may not take a built-in's name");
    let message = refused.to_string();
    assert!(message.contains("[COMMAND_NAME_TAKEN]"), "{message}");
    assert!(message.contains("/help"), "{message}");
    // And the refusal left nothing behind: the built-in still answers, and
    // the three commands the same load registered before the clash are gone.
    assert_eq!(seat.len(), builtins, "{:?}", seat.all().len());
    assert!(seat.find("help").is_some(), "the built-in still answers");
    assert!(seat.find("fixture-echo").is_none());

    fixture.plane.shutdown().await;
}

/// A composition entry may be a plugin in rebon's own shape, not a Cordis one.
///
/// The composition loader routes a module by what it exports, and says so out
/// loud: `activate` goes to the host's own loader, `apply` mounts into the
/// Cordis realm. Only the first shape can register a command, and only the
/// second is one the control plugin can report on — so asking for a report and
/// reading its absence as a failure closed the composition to every plugin
/// that offers a command. That is not a small hole: `kernelPlugins` is the
/// only door an installed package comes through, and the one other place a
/// host-native module loads is reserved for model providers, which register
/// with `publish` off. Together they left the plugin command path with no
/// producer a real machine could reach.
///
/// So the report is asked for and its absence is allowed, exactly once and
/// only for the answer that means "not in the realm". Everything this entry
/// registered is in the ready report either way; what a mounted entry adds is
/// model catalogs and prompt sections, which this one has none of.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_native_composition_entry_loads_and_its_commands_reach_the_seat() {
    use rebon_command_seat::CommandSeatService;

    let _serial = serialised().await;
    let Some(fixture) = boot(Vec::new(), &[]).await else {
        return;
    };
    let seat = fixture
        .kernel
        .context()
        .get::<CommandSeatService>()
        .expect("the core-commands plugin provides the seat");
    let builtins = seat.len();

    let root = repo().join("crates/rebon-harness/tests/fixtures/commands");
    let entry = ComposeEntry {
        id: "fixture-commands".into(),
        root: plain(&root),
        entry: "plugin.mjs".into(),
        services: vec!["fixture-report".into()],
        commands: vec![
            "fixture-echo".into(),
            "fixture-elsewhere".into(),
            "fixture-panel".into(),
            "fixture-silent".into(),
        ],
        publish: true,
        ..ComposeEntry::default()
    };

    // `load_entry`, not `load_standalone`: this is the path `kernelPlugins`
    // takes, and it is the one that used to refuse the entry after loading it.
    fixture
        .plane
        .load_entry(&entry)
        .await
        .expect("a host-native entry is still a composition entry");

    assert_eq!(
        fixture.plane.registered_commands("fixture-commands"),
        vec![
            "fixture-echo".to_string(),
            "fixture-elsewhere".to_string(),
            "fixture-panel".to_string(),
            "fixture-silent".to_string()
        ]
    );
    assert_eq!(
        seat.len(),
        builtins + 4,
        "the commands reached the seat a person types at"
    );
    assert!(seat.find("fixture-echo").is_some());
    assert!(seat.find("fx").is_some(), "aliases came with them");
    assert_eq!(
        fixture.plane.registered_services("fixture-commands"),
        vec!["fixture-report".to_string()]
    );

    // The other half of the old failure: the entry used to be left running on
    // the host with nothing registered, so the same id could never be loaded
    // again. Unload and reload proves both sides agree about what is up.
    fixture
        .plane
        .unload_entry("fixture-commands")
        .await
        .expect("the entry drains");
    assert_eq!(seat.len(), builtins, "the seat is back to the built-ins");
    fixture
        .plane
        .load_entry(&entry)
        .await
        .expect("the same id loads again");
    assert_eq!(seat.len(), builtins + 4);

    fixture.plane.shutdown().await;
}

/// A package that declares a model provider its `activate` never registers
/// cannot serve a turn, and the plane has to say so at the load.
///
/// The load is what a session waits on, so this is where the person who
/// selected that provider still has somewhere to go. Bound instead, the
/// client looks healthy and the first turn dies with `[UNDECLARED_ADAPTER]`
/// after they already watched it start — with the package name nowhere in
/// sight. The refusal therefore names both halves: the package that made the
/// promise and the provider it promised.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_provider_a_plugin_declares_but_never_registers_is_refused_at_load() {
    let _serial = serialised().await;
    let Some(fixture) = boot(Vec::new(), &[]).await else {
        return;
    };

    let root = repo().join("crates/rebon-harness/tests/fixtures/silent-provider");
    let entry = ComposeEntry {
        id: "silent-provider".into(),
        root: plain(&root),
        entry: "provider.mjs".into(),
        llm_providers: vec!["silent-provider".into()],
        ..ComposeEntry::default()
    };

    let error = fixture
        .plane
        .load_standalone(&entry)
        .await
        .expect_err("a declared provider nothing registered must not load");
    let message = error.to_string();
    assert!(message.contains("[UNREGISTERED_PROVIDER]"), "{message}");
    assert!(message.contains("silent-provider"), "{message}");

    // Refused *and* undone. Were the plugin left up, the next attempt would
    // come back `already loaded` — a success — and the second session would
    // bind a provider the first one just proved is not there.
    let again = fixture
        .plane
        .load_standalone(&entry)
        .await
        .expect_err("the refusal is not remembered as a load");
    assert!(
        again.to_string().contains("[UNREGISTERED_PROVIDER]"),
        "{again}"
    );
    assert!(
        fixture
            .plane
            .registered_commands("silent-provider")
            .is_empty(),
        "nothing was registered for a plugin that was refused"
    );

    // Every test in this file ends here, and for the reason
    // `kernel_mainline_deepseek_js` spells out: the plane is a real Node child,
    // and a runtime with a task still reading that child's stdout does not
    // finish dropping. This one was written without it and hung the suite.
    fixture.plane.shutdown().await;
}

/// A `prompt` command whose plugin never answers has to come back anyway.
///
/// The proxy behind a plugin command is synchronous by contract: the terminal
/// calls it on the thread running its event loop and waits for a `String`. A
/// plugin that never resolves therefore does not fail one command — it stops
/// the loop that reads the keyboard, which is a frozen terminal with no way
/// out. The bound is what turns that into a sentence.
///
/// The plane is booted with a short deadline so the test watches the real path
/// without waiting out the real twenty seconds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_whose_plugin_never_answers_comes_back_as_a_refusal() {
    use rebon_command_seat::{CommandArgs, CommandHandler, CommandSeatService, Surface};

    let _serial = serialised().await;
    let bound = Duration::from_millis(200);
    let Some(fixture) = boot_with_bound(Vec::new(), &[], Some(bound)).await else {
        return;
    };

    let root = repo().join("crates/rebon-harness/tests/fixtures/commands");
    let entry = ComposeEntry {
        id: "fixture-silent-commands".into(),
        root: plain(&root),
        entry: "plugin.mjs".into(),
        services: vec!["fixture-report".into()],
        // Every command the fixture registers has to be declared, or the
        // loader refuses the first undeclared one and the whole load fails.
        commands: vec![
            "fixture-echo".into(),
            "fixture-elsewhere".into(),
            "fixture-panel".into(),
            "fixture-silent".into(),
        ],
        ..ComposeEntry::default()
    };
    fixture
        .plane
        .load_standalone(&entry)
        .await
        .expect("the fixture loads");

    let seat = fixture
        .kernel
        .context()
        .get::<CommandSeatService>()
        .expect("the command seat is up");
    let command = seat.find("fixture-silent").expect("the command registered");
    let CommandHandler::Prompt(expand) = command.handler else {
        panic!("fixture-silent is a prompt command");
    };

    // The call the terminal makes: synchronous, on this thread, and it must
    // return. `spawn_blocking` because that is where the TUI's event loop
    // lives, and because a `block_on` inside the proxy is illegal anywhere
    // else.
    let started = std::time::Instant::now();
    let outcome = tokio::task::spawn_blocking(move || {
        expand(&CommandArgs {
            raw: "/fixture-silent".to_string(),
            rest: String::new(),
            surface: Surface::Tui,
        })
    })
    .await
    .expect("the proxy thread does not panic");
    let elapsed = started.elapsed();

    // `Err`, not prompt text that happens to read like a failure: a front end
    // must be able to tell "here is what to send the model" from "here is what
    // to tell the person" without reading either.
    let text = outcome.expect_err("a command that never answered did not expand");
    assert!(
        text.contains("[HOST_UNANSWERED]"),
        "the refusal names the code: {text}"
    );
    assert!(
        text.contains("fixture-silent"),
        "the refusal names the command: {text}"
    );
    // Well inside the outer belt, which is the inner bound plus five seconds.
    assert!(
        elapsed < bound + Duration::from_secs(2),
        "the bound answered rather than the outer belt: {elapsed:?}"
    );

    fixture.plane.shutdown().await;
}

/// B8: the example package in `runtimes/node/plugins/examples/hello-command` is a real
/// package, loaded the way a user's `config.json` loads one.
///
/// It is here because documentation that nothing runs goes stale silently.
/// If the plane's shape moves, this is what says the example moved with it.
///
/// The declaration in `rebon-plugin.json` is the ceiling this loads with, not
/// a list retyped in the test — a package that grew a registration it never
/// declared would fail here, which is the same refusal a user would meet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_example_package_registers_its_command() {
    use rebon_command_seat::{CommandArgs, CommandHandler, CommandSeatService, Surface};
    use rebon_plugin_host::plugin_manifests::read_package_manifest;

    let _serial = serialised().await;
    let Some(fixture) = boot(Vec::new(), &[]).await else {
        return;
    };

    let module = repo().join("runtimes/node/plugins/examples/hello-command/plugin.mjs");
    let (declared_as, manifest) =
        read_package_manifest(&module).expect("the example package declares a kernel plugin");
    assert_eq!(declared_as, "hello-command");
    let entry = ComposeEntry {
        id: declared_as,
        root: plain(module.parent().expect("the package directory")),
        entry: manifest
            .entry
            .clone()
            .expect("the declaration names a module"),
        commands: manifest.commands.clone(),
        seats: manifest.seats.clone(),
        settings: manifest.settings.clone(),
        config: serde_json::json!({ "greeting": "Hello from the plugin plane" }),
        ..ComposeEntry::default()
    };
    fixture
        .plane
        .load_standalone(&entry)
        .await
        .expect("the example package loads");

    let seat = fixture
        .kernel
        .context()
        .get::<CommandSeatService>()
        .expect("the command seat is up");
    let command = seat.find("hello").expect("/hello reached the command seat");
    let CommandHandler::Prompt(expand) = command.handler else {
        panic!("/hello is a prompt command");
    };
    let say_hello = |expand: std::sync::Arc<
        dyn Fn(&CommandArgs) -> Result<String, String> + Send + Sync,
    >| async move {
        tokio::task::spawn_blocking(move || {
            expand(&CommandArgs {
                raw: "/hello".to_string(),
                rest: String::new(),
                surface: Surface::Tui,
            })
        })
        .await
        .expect("the proxy thread does not panic")
        .expect("/hello expands")
    };
    assert_eq!(
        say_hello(expand.clone()).await,
        "Hello from the plugin plane",
        "the manifest's declared default is what an untouched settings file reads as"
    );

    // A2: the same package's own settings namespace, end to end — a key the
    // manifest declared, written into the user settings file the seat writes,
    // read back by the plugin through the seat on its next invocation.
    rebon_config::save_plugin_settings_in_dir(
        fixture.config_dir(),
        "hello-command",
        serde_json::json!({ "greeting": "Bonjour" })
            .as_object()
            .expect("a patch object"),
    )
    .expect("the settings write lands");
    assert_eq!(
        say_hello(expand).await,
        "Bonjour",
        "the plugin reads its own namespace through the seat"
    );

    // The gates a plugin meets from the other side of the process boundary.
    // The plane stamps the caller's identity over whatever the params carried,
    // so naming somebody else here is naming yourself and being refused for it.
    let seat_write = |patch: serde_json::Value| {
        fixture.kernel.context().call_json(
            rebon_kernel::SETTINGS_SERVICE,
            "write",
            serde_json::json!({
                "callerPluginId": "hello-command",
                "patch": patch,
            }),
        )
    };
    let err = seat_write(serde_json::json!({ "undeclared": 1 }))
        .expect_err("a key the manifest never declared");
    assert!(err.to_string().contains("not a settings key"), "{err}");
    let err = seat_write(serde_json::json!({ "enabled": false })).expect_err("the kernel's switch");
    assert!(err.to_string().contains("not a settings key"), "{err}");
    assert_eq!(
        rebon_config::plugin_settings_in(
            fixture.config_dir(),
            fixture.config_dir(),
            "hello-command"
        )
        .get("greeting")
        .cloned(),
        Some(serde_json::json!("Bonjour")),
        "the refusals wrote nothing"
    );

    fixture.plane.shutdown().await;
}
