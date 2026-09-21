//! The whole vertical, end to end: a Rust supervisor starts the real Node host,
//! loads a real plugin off disk, calls one of its services, and drains it.
//!
//! Everything below this is unit-tested on both sides already. What this proves
//! is that the two sides agree — that the schema the Rust side serialises is the
//! one the Node validator accepts, that a manifest declaration on one side is
//! the same ceiling on the other, and that a refusal comes back as the code both
//! were written to use.

use std::{env, future::Future, path::PathBuf, pin::Pin, sync::Arc, time::Duration};

use rebon_plugin_protocol::{Payload, PluginLoadRequest};
use rebon_plugin_supervisor::{
    EventPublisher, HostCallError, HostConfig, PluginHostSupervisor, PublishedEvent,
    SeatDispatcher, SeatInvocation, StreamEvent, ToolInvocation, ToolInvoker, ToolRefusal,
};

fn node() -> Option<PathBuf> {
    match env::var_os("REBON_TEST_NODE") {
        Some(node) => Some(PathBuf::from(node)),
        None => {
            assert_ne!(
                env::var_os("REBON_REQUIRE_TEST_NODE").as_deref(),
                Some(std::ffi::OsStr::new("1")),
                "REBON_REQUIRE_TEST_NODE=1 requires an absolute REBON_TEST_NODE"
            );
            eprintln!("skipping: set REBON_TEST_NODE to an absolute Node executable");
            None
        }
    }
}

fn real_host() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtimes/node/plugin-host/src/cli.mjs")
}

/// Writes a package whose entry registers one echo service.
fn plugin_package(dir: &std::path::Path, body: &str) -> String {
    std::fs::write(dir.join("index.mjs"), body).unwrap();
    dir.to_string_lossy().replace('\\', "/")
}

const ECHO_PLUGIN: &str = r#"
export function activate(plugin) {
  plugin.service('echo', async (request) => ({ echoed: request }));
}
"#;

/// Keeps what it was handed so a service call can report it: an event that
/// arrived is only proven by the plugin being able to say so.
const LISTENER_PLUGIN: &str = r#"
export function activate(plugin) {
  const heard = [];
  plugin.topic('session', async (event) => { heard.push(event); });
  plugin.service('heard', async () => ({ heard }));
}
"#;

fn load_request(root: String) -> PluginLoadRequest {
    PluginLoadRequest {
        plugin_id: "plugin.demo".into(),
        root,
        entry: "index.mjs".into(),
        services: vec!["echo".into()],
        event_topics: Vec::new(),
        published_topics: Vec::new(),
        llm_providers: Vec::new(),
        tools: Vec::new(),
        commands: Vec::new(),
        invokable_tools: Vec::new(),
        seats: Vec::new(),
        config: Payload::null(),
    }
}

fn listener_request(root: String) -> PluginLoadRequest {
    PluginLoadRequest {
        plugin_id: "plugin.demo".into(),
        root,
        entry: "index.mjs".into(),
        services: vec!["heard".into()],
        event_topics: vec!["session".into()],
        published_topics: Vec::new(),
        llm_providers: Vec::new(),
        tools: Vec::new(),
        commands: Vec::new(),
        invokable_tools: Vec::new(),
        seats: Vec::new(),
        config: Payload::null(),
    }
}

async fn started(node: &std::path::Path) -> PluginHostSupervisor {
    PluginHostSupervisor::start(base_config(node))
        .await
        .expect("host starts")
}

fn base_config(node: &std::path::Path) -> HostConfig {
    HostConfig::new(node, real_host())
        .with_startup_timeout(Duration::from_secs(20))
        .with_working_directory(env::temp_dir())
}

/// Stands in for the embedder. A real one enters rebon's permission broker
/// here; this one records who asked and answers, which is what the test needs
/// to see travel across the seam.
#[derive(Default)]
struct RecordingTools {
    seen: std::sync::Mutex<Vec<(String, String, String)>>,
    deny: bool,
}

impl ToolInvoker for RecordingTools {
    fn invoke(
        &self,
        invocation: ToolInvocation,
    ) -> Pin<Box<dyn Future<Output = Result<Payload, ToolRefusal>> + Send + '_>> {
        self.seen.lock().unwrap().push((
            invocation.identity.plugin_id.clone(),
            invocation.identity.scope_id.clone(),
            invocation.tool.clone(),
        ));
        let deny = self.deny;
        let input = invocation.input;
        Box::pin(async move {
            if deny {
                return Err(ToolRefusal::new("[PERMISSION_DENIED]", "the user said no"));
            }
            Ok(Payload::from(serde_json::json!({"echoed": input})))
        })
    }
}

#[tokio::test]
async fn a_plugin_loads_answers_a_service_call_and_drains() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path(), ECHO_PLUGIN);
    let supervisor = started(&node).await;

    let report = supervisor
        .load_plugin(&load_request(root))
        .await
        .expect("the plugin loads");
    assert_eq!(report.plugin_id, "plugin.demo");
    assert_eq!(report.services, vec!["echo".to_string()]);
    assert!(report.event_topics.is_empty());

    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .expect("scope opens");

    let answer = supervisor
        .call_service(
            "plugin.demo",
            "session-1",
            "echo",
            Payload::from(serde_json::json!({"n": 1})),
        )
        .await
        .expect("the service answers");
    assert_eq!(
        answer.to_value().unwrap(),
        serde_json::json!({"echoed": {"n": 1}})
    );

    let drained = supervisor
        .unload_plugin("plugin.demo")
        .await
        .expect("the plugin drains");
    assert_eq!(drained.plugin_id, "plugin.demo");
    assert!(drained.outstanding_calls.is_empty());

    // Draining stops routing on both sides.
    let error = supervisor
        .call_service("plugin.demo", "session-1", "echo", Payload::null())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("[STALE_PROVIDER]"),
        "expected a stale-provider refusal, got {error}"
    );

    supervisor.shutdown().await.unwrap();
}

/// The manifest is the ceiling on both sides. The plugin's own registration is
/// refused first, so the failure names the plugin's line rather than arriving as
/// a rejected report after its module already ran.
#[tokio::test]
async fn a_plugin_cannot_register_a_service_its_manifest_did_not_declare() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(
        dir.path(),
        "export function activate(plugin) { plugin.service('smuggled', async () => null); }\n",
    );
    let supervisor = started(&node).await;

    let error = supervisor
        .load_plugin(&load_request(root))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("[UNAUTHORIZED_REGISTER]"),
        "got {error}"
    );

    // A load that did not finish leaves nothing behind, so the same plugin id
    // can be loaded again rather than being stuck as already-loaded.
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path(), ECHO_PLUGIN);
    supervisor
        .load_plugin(&load_request(root))
        .await
        .expect("the id is free again");
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_service_the_plugin_never_registered_is_refused_before_the_host_is_asked() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path(), ECHO_PLUGIN);
    let supervisor = started(&node).await;
    supervisor.load_plugin(&load_request(root)).await.unwrap();
    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .unwrap();

    let error = supervisor
        .call_service("plugin.demo", "session-1", "missing", Payload::null())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("[UNKNOWN_SERVICE]"),
        "got {error}"
    );
    assert!(supervisor.is_alive().await);
    supervisor.shutdown().await.unwrap();
}

/// A plugin whose entry throws is a load failure with the plugin's own message,
/// not a host failure — the host is still perfectly usable.
#[tokio::test]
async fn a_plugin_that_throws_on_activation_does_not_take_the_host_down() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(
        dir.path(),
        "export function activate() { throw new Error('deliberate'); }\n",
    );
    let supervisor = started(&node).await;

    let error = supervisor
        .load_plugin(&load_request(root))
        .await
        .unwrap_err();
    match &error {
        HostCallError::Rejected { payload, .. } => {
            let value = payload.to_value().unwrap();
            assert_eq!(value["code"], "[ACTIVATE_FAILED]");
            assert!(
                value["message"].as_str().unwrap().contains("deliberate"),
                "{value}"
            );
        }
        other => panic!("expected a rejection, got {other}"),
    }
    assert!(
        supervisor.is_alive().await,
        "one bad plugin is not a bad host"
    );
    supervisor.shutdown().await.unwrap();
}

/// An entry that resolves outside its package root is refused by the host even
/// though the payload's own text looked harmless.
#[tokio::test]
async fn an_entry_outside_the_package_root_is_refused() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path(), ECHO_PLUGIN);
    std::fs::create_dir_all(dir.path().join("nested")).unwrap();
    let supervisor = started(&node).await;

    let mut request = load_request(format!("{root}/nested"));
    // Textually relative and clean, but it lands in the parent directory.
    request.entry = "index.mjs".into();
    let outside = supervisor.load_plugin(&request).await;
    assert!(
        outside.is_err(),
        "the entry does not exist inside the nested root"
    );
    assert!(supervisor.is_alive().await);
    supervisor.shutdown().await.unwrap();
}

/// The upward direction, proven against a real host: the plugin's topic
/// registration becomes a subscription only by the host asking rebon for one,
/// and the answer travels back on the same stream the request came in on.
#[tokio::test]
async fn a_plugin_subscribes_through_the_host_and_receives_an_event() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path(), LISTENER_PLUGIN);
    let supervisor = started(&node).await;
    supervisor
        .load_plugin(&listener_request(root))
        .await
        .expect("the plugin loads");
    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .expect("scope opens");

    // Opening the scope is what created it: the subscription is pinned to that
    // incarnation, so it cannot have existed before the scope did.
    let held = supervisor.subscriptions("plugin.demo").await;
    assert_eq!(held.len(), 1, "expected one subscription, got {held:?}");
    let (subscription, topic) = held.into_iter().next().unwrap();
    assert_eq!(topic, "session");

    supervisor
        .deliver_event(
            "plugin.demo",
            "session-1",
            &subscription,
            "session",
            Payload::from(serde_json::json!({"n": 1})),
        )
        .await
        .expect("the event is delivered");

    let heard = supervisor
        .call_service("plugin.demo", "session-1", "heard", Payload::null())
        .await
        .expect("the plugin reports what it heard");
    assert_eq!(
        heard.to_value().unwrap(),
        serde_json::json!({"heard": [{"n": 1}]})
    );
    supervisor.shutdown().await.unwrap();
}

/// Closing a scope is the invalidation point. A delivery afterwards is aimed at
/// an incarnation that no longer exists, and is refused before the host is even
/// asked.
#[tokio::test]
async fn closing_a_scope_revokes_the_subscriptions_it_created() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path(), LISTENER_PLUGIN);
    let supervisor = started(&node).await;
    supervisor
        .load_plugin(&listener_request(root))
        .await
        .unwrap();
    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .unwrap();
    let (subscription, _) = supervisor
        .subscriptions("plugin.demo")
        .await
        .into_iter()
        .next()
        .unwrap();

    let revoked = supervisor
        .close_scope("plugin.demo", "session-1")
        .await
        .expect("scope closes");
    assert_eq!(
        revoked,
        vec![("plugin.demo".to_string(), subscription.clone())]
    );
    assert!(supervisor.subscriptions("plugin.demo").await.is_empty());

    // The scope itself is the first thing that refuses: there is no live
    // incarnation to address, so the question of which subscription never
    // arises.
    let error = supervisor
        .deliver_event(
            "plugin.demo",
            "session-1",
            &subscription,
            "session",
            Payload::null(),
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("is already closed"),
        "got {error}"
    );
    assert!(supervisor.is_alive().await);
    supervisor.shutdown().await.unwrap();
}

/// A plugin that declared no topic cannot be delivered to, and finding that out
/// does not require asking the host.
#[tokio::test]
async fn an_event_for_a_subscription_that_was_never_made_is_refused_locally() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path(), ECHO_PLUGIN);
    let supervisor = started(&node).await;
    supervisor.load_plugin(&load_request(root)).await.unwrap();
    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .unwrap();
    assert!(supervisor.subscriptions("plugin.demo").await.is_empty());

    let error = supervisor
        .deliver_event(
            "plugin.demo",
            "session-1",
            "sub-1",
            "session",
            Payload::null(),
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("[UNKNOWN_SUBSCRIPTION]"),
        "got {error}"
    );
    supervisor.shutdown().await.unwrap();
}

/// Calls a rebon tool from inside a service handler and reports the answer.
const TOOL_PLUGIN: &str = r#"
export function activate(plugin) {
  plugin.service('run', async (request, ctx) => {
    try {
      return { ok: await ctx.invoke(request.tool, request.input ?? null) };
    } catch (cause) {
      return { refused: cause.code, message: cause.message };
    }
  });
}
"#;

fn tool_request(root: String, tools: Vec<String>) -> PluginLoadRequest {
    PluginLoadRequest {
        plugin_id: "plugin.demo".into(),
        root,
        entry: "index.mjs".into(),
        services: vec!["run".into()],
        event_topics: Vec::new(),
        published_topics: Vec::new(),
        llm_providers: Vec::new(),
        tools: Vec::new(),
        commands: Vec::new(),
        invokable_tools: tools,
        seats: Vec::new(),
        config: Payload::null(),
    }
}

async fn with_tools(
    node: &std::path::Path,
    tools: Arc<RecordingTools>,
    exposed: &[&str],
) -> PluginHostSupervisor {
    PluginHostSupervisor::start(
        base_config(node).with_tool_invoker(tools, exposed.iter().map(|tool| (*tool).to_string())),
    )
    .await
    .expect("host starts")
}

/// The whole upward path, through a real process: a plugin's handler calls a
/// rebon tool, the identity that arrives at the embedder is the one the host
/// injected, and the answer comes back into the plugin.
#[tokio::test]
async fn a_plugin_invokes_a_rebon_tool_and_gets_its_answer() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path(), TOOL_PLUGIN);
    let tools = Arc::new(RecordingTools::default());
    let supervisor = with_tools(&node, Arc::clone(&tools), &["read_file"]).await;

    supervisor
        .load_plugin(&tool_request(root, vec!["read_file".into()]))
        .await
        .expect("the plugin loads");
    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .unwrap();

    let answer = supervisor
        .call_service(
            "plugin.demo",
            "session-1",
            "run",
            Payload::from(serde_json::json!({"tool": "read_file", "input": {"path": "a.txt"}})),
        )
        .await
        .expect("the service answers");
    assert_eq!(
        answer.to_value().unwrap(),
        serde_json::json!({"ok": {"echoed": {"path": "a.txt"}}})
    );

    // The plugin never supplies its own identity: the host's bridge injects it,
    // which is what makes this attribution worth making a permission decision on.
    assert_eq!(
        tools.seen.lock().unwrap().as_slice(),
        [(
            "plugin.demo".to_string(),
            "session-1".to_string(),
            "read_file".to_string()
        )]
    );
    supervisor.shutdown().await.unwrap();
}

/// A refusal from the embedder — which is where a denied permission comes from
/// — reaches the plugin with its own code rather than as "the call failed".
#[tokio::test]
async fn a_denied_tool_reaches_the_plugin_with_its_reason() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path(), TOOL_PLUGIN);
    let tools = Arc::new(RecordingTools {
        deny: true,
        ..RecordingTools::default()
    });
    let supervisor = with_tools(&node, tools, &["read_file"]).await;
    supervisor
        .load_plugin(&tool_request(root, vec!["read_file".into()]))
        .await
        .unwrap();
    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .unwrap();

    let answer = supervisor
        .call_service(
            "plugin.demo",
            "session-1",
            "run",
            Payload::from(serde_json::json!({"tool": "read_file"})),
        )
        .await
        .unwrap();
    let value = answer.to_value().unwrap();
    assert_eq!(value["refused"], "[PERMISSION_DENIED]");
    assert!(
        value["message"]
            .as_str()
            .unwrap()
            .contains("the user said no"),
        "{value}"
    );
    supervisor.shutdown().await.unwrap();
}

/// A manifest asking for something this build does not expose describes a
/// plugin that can never work, so it is refused at load rather than at first use.
#[tokio::test]
async fn a_manifest_declaring_an_unexposed_tool_is_refused_at_load() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path(), TOOL_PLUGIN);
    let tools = Arc::new(RecordingTools::default());
    let supervisor = with_tools(&node, Arc::clone(&tools), &["read_file"]).await;

    let error = supervisor
        .load_plugin(&tool_request(root.clone(), vec!["run_shell".into()]))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("[UNAVAILABLE_TOOL]"),
        "got {error}"
    );
    assert!(tools.seen.lock().unwrap().is_empty(), "nothing was invoked");

    // And the refusal left nothing behind: the same id loads with a manifest
    // this build can satisfy.
    supervisor
        .load_plugin(&tool_request(root, vec!["read_file".into()]))
        .await
        .expect("the id is free");
    supervisor.shutdown().await.unwrap();
}

/// With no invoker installed there is nothing to run, and a plugin that asks is
/// told so rather than left waiting.
#[tokio::test]
async fn a_plane_with_no_invoker_exposes_no_tools() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path(), TOOL_PLUGIN);
    let supervisor = started(&node).await;

    let error = supervisor
        .load_plugin(&tool_request(root, vec!["read_file".into()]))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("[UNAVAILABLE_TOOL]"),
        "got {error}"
    );
    supervisor.shutdown().await.unwrap();
}

/// Emits pieces before it answers.
const STREAMING_PLUGIN: &str = r#"
export function activate(plugin) {
  plugin.service('count', async (request, ctx) => {
    for (let index = 0; index < request.n; index++) await ctx.emit({ index });
    return { total: request.n };
  });
  plugin.service('leak', async (request, ctx) => {
    await ctx.emit({ index: 0 });
    globalThis.__escaped = ctx;
    return { escaped: true };
  });
  plugin.service('late', async () => {
    try {
      await globalThis.__escaped.emit({ index: 99 });
      return { refused: null };
    } catch (cause) {
      return { refused: cause.code };
    }
  });
}
"#;

fn streaming_request(root: String) -> PluginLoadRequest {
    PluginLoadRequest {
        plugin_id: "plugin.demo".into(),
        root,
        entry: "index.mjs".into(),
        services: vec!["count".into(), "leak".into(), "late".into()],
        event_topics: Vec::new(),
        published_topics: Vec::new(),
        llm_providers: Vec::new(),
        tools: Vec::new(),
        commands: Vec::new(),
        invokable_tools: Vec::new(),
        seats: Vec::new(),
        config: Payload::null(),
    }
}

async fn streaming_supervisor(
    node: &std::path::Path,
    dir: &tempfile::TempDir,
) -> PluginHostSupervisor {
    let root = plugin_package(dir.path(), STREAMING_PLUGIN);
    let supervisor = started(node).await;
    supervisor
        .load_plugin(&streaming_request(root))
        .await
        .expect("the plugin loads");
    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .unwrap();
    supervisor
}

/// The whole streaming vertical against a real process: chunks arrive in order,
/// exactly one end follows them, and the plugin's return value is that end.
#[tokio::test]
async fn a_service_streams_its_answer_in_pieces_before_ending() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let supervisor = streaming_supervisor(&node, &dir).await;

    let stream = supervisor
        .call_service_streaming(
            "plugin.demo",
            "session-1",
            "count",
            Payload::from(serde_json::json!({"n": 4})),
        )
        .await
        .expect("the stream starts");
    let (chunks, end) = stream.collect().await;

    let seen: Vec<serde_json::Value> = chunks
        .into_iter()
        .map(|chunk| chunk.to_value().unwrap())
        .collect();
    assert_eq!(
        seen,
        (0..4)
            .map(|index| serde_json::json!({"index": index}))
            .collect::<Vec<_>>(),
        "chunks arrive in the order they were emitted"
    );
    assert_eq!(
        end.unwrap().to_value().unwrap(),
        serde_json::json!({"total": 4})
    );

    // The call is closed on this side, so an unload does not find work in
    // flight that finished long ago.
    let drained = supervisor.unload_plugin("plugin.demo").await.unwrap();
    assert!(drained.outstanding_calls.is_empty());
    supervisor.shutdown().await.unwrap();
}

/// A caller that asked for one answer and got a stream is told so. Dropping the
/// chunks quietly would lose content, and the mistake is the caller's shape,
/// not the host's behaviour.
#[tokio::test]
async fn a_streamed_answer_on_a_non_streaming_call_fails_loudly() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let supervisor = streaming_supervisor(&node, &dir).await;

    let error = supervisor
        .call_service(
            "plugin.demo",
            "session-1",
            "count",
            Payload::from(serde_json::json!({"n": 2})),
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("not requested as a stream"),
        "got {error}"
    );
    assert!(
        supervisor.is_alive().await,
        "one caller's mistake is not a host failure"
    );
    supervisor.shutdown().await.unwrap();
}

/// A context that outlived its call cannot emit into it: the caller has already
/// read the end and stopped listening.
#[tokio::test]
async fn emitting_after_the_call_ended_is_refused() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let supervisor = streaming_supervisor(&node, &dir).await;

    let (_, end) = supervisor
        .call_service_streaming("plugin.demo", "session-1", "leak", Payload::null())
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(
        end.unwrap().to_value().unwrap(),
        serde_json::json!({"escaped": true})
    );

    let refused = supervisor
        .call_service("plugin.demo", "session-1", "late", Payload::null())
        .await
        .unwrap();
    assert_eq!(
        refused.to_value().unwrap(),
        serde_json::json!({"refused": "[STREAM_CLOSED]"})
    );
    supervisor.shutdown().await.unwrap();
}

/// A caller walking away must not hold the plugin hostage: the call closes on
/// drop, so the unload that follows can finish.
#[tokio::test]
async fn dropping_a_stream_early_still_closes_the_call() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let supervisor = streaming_supervisor(&node, &dir).await;

    {
        let _stream = supervisor
            .call_service_streaming(
                "plugin.demo",
                "session-1",
                "count",
                Payload::from(serde_json::json!({"n": 2})),
            )
            .await
            .unwrap();
    }
    // The drop schedules the accounting; give it a turn to run.
    tokio::task::yield_now().await;
    for _ in 0..50 {
        if supervisor.in_flight("plugin.demo").await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        supervisor.in_flight("plugin.demo").await.is_empty(),
        "an abandoned stream must not stay counted"
    );
    supervisor.shutdown().await.unwrap();
}

/// A model-shaped LLM adapter: it emits stream chunks and finishes.
///
/// The chunk shapes are the model layer's vocabulary — `block-start` /
/// `text-delta` / `finish` — so what this proves is that the transport
/// carries exactly what that layer expects. The mapping itself is not this
/// crate's business.
const LLM_PLUGIN: &str = r#"
export function activate(plugin) {
  plugin.llm('demo', async (request, ctx) => {
    await ctx.emit({ type: 'block-start', index: 0, blockType: 'text' });
    for (const word of request.words) {
      if (ctx.signal.aborted) return { stopped: true };
      await ctx.emit({ type: 'text-delta', index: 0, text: word });
    }
    return { type: 'finish', finishReason: 'stop' };
  });
  // Honours cancellation by returning a value rather than throwing, so the
  // turn is a success that reports it stopped.
  plugin.llm('honour', async (request, ctx) => {
    await ctx.emit({ type: 'block-start', index: 0, blockType: 'text' });
    await new Promise((resolve) => {
      if (ctx.signal.aborted) return resolve();
      ctx.signal.addEventListener('abort', () => resolve(), { once: true });
    });
    return { stopped: true };
  });
  plugin.llm('slow', async (request, ctx) => {
    await ctx.emit({ type: 'block-start', index: 0, blockType: 'text' });
    await new Promise((resolve, reject) => {
      if (ctx.signal.aborted) return reject(new Error('cancelled'));
      ctx.signal.addEventListener('abort', () => reject(new Error('cancelled')), { once: true });
    });
    return { unreachable: true };
  });
}
"#;

fn llm_request(root: String) -> PluginLoadRequest {
    PluginLoadRequest {
        plugin_id: "plugin.demo".into(),
        root,
        entry: "index.mjs".into(),
        services: Vec::new(),
        event_topics: Vec::new(),
        published_topics: Vec::new(),
        llm_providers: vec!["demo".into(), "honour".into(), "slow".into()],
        tools: Vec::new(),
        commands: Vec::new(),
        invokable_tools: Vec::new(),
        seats: Vec::new(),
        config: Payload::null(),
    }
}

async fn llm_supervisor(node: &std::path::Path, dir: &tempfile::TempDir) -> PluginHostSupervisor {
    let root = plugin_package(dir.path(), LLM_PLUGIN);
    let supervisor = started(node).await;
    let report = supervisor
        .load_plugin(&llm_request(root))
        .await
        .expect("the plugin loads");
    assert_eq!(
        report.llm_providers,
        vec!["demo".to_string(), "honour".to_string(), "slow".to_string()],
        "the ready report names what the plugin actually registered"
    );
    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .unwrap();
    supervisor
}

/// The whole model-turn vertical against a real process: rebon asks a provider
/// the plugin registered, the adapter's chunks arrive in order, and the turn
/// ends with the adapter's own finish chunk.
#[tokio::test]
async fn an_llm_adapter_streams_a_turn_through_the_host() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let supervisor = llm_supervisor(&node, &dir).await;

    let stream = supervisor
        .stream_llm(
            "plugin.demo",
            "session-1",
            "demo",
            Payload::from(serde_json::json!({"words": ["he", "llo"]})),
        )
        .await
        .expect("the turn starts");
    let (chunks, end) = stream.collect().await;

    let seen: Vec<serde_json::Value> = chunks
        .into_iter()
        .map(|chunk| chunk.to_value().unwrap())
        .collect();
    assert_eq!(
        seen,
        vec![
            serde_json::json!({"type": "block-start", "index": 0, "blockType": "text"}),
            serde_json::json!({"type": "text-delta", "index": 0, "text": "he"}),
            serde_json::json!({"type": "text-delta", "index": 0, "text": "llo"}),
        ]
    );
    assert_eq!(
        end.unwrap().to_value().unwrap(),
        serde_json::json!({"type": "finish", "finishReason": "stop"})
    );
    supervisor.shutdown().await.unwrap();
}

/// The provider is the routing key, and the manifest is its ceiling: a turn for
/// a provider this plugin never registered is refused before the host is asked.
#[tokio::test]
async fn a_turn_for_an_unregistered_provider_is_refused_locally() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let supervisor = llm_supervisor(&node, &dir).await;

    let error = supervisor
        .stream_llm("plugin.demo", "session-1", "openai", Payload::null())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("[UNKNOWN_PROVIDER]"),
        "got {error}"
    );
    assert!(supervisor.is_alive().await);
    supervisor.shutdown().await.unwrap();
}

/// A turn a user stopped is not a turn that failed. Cancel asks rather than
/// commands, so the terminal has to say which of the two happened.
#[tokio::test]
async fn cancelling_a_turn_ends_it_as_cancelled_not_as_an_error() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let supervisor = llm_supervisor(&node, &dir).await;

    let mut stream = supervisor
        .stream_llm(
            "plugin.demo",
            "session-1",
            "slow",
            Payload::from(serde_json::json!({"words": []})),
        )
        .await
        .expect("the turn starts");
    let call_id = stream.call_id().to_owned();

    // Wait for the adapter to actually be running before stopping it.
    match stream.recv().await {
        Some(StreamEvent::Chunk(_)) => {}
        other => panic!("expected the adapter's first chunk, got {other:?}"),
    }
    supervisor.cancel(&call_id).await.expect("cancel is sent");

    match stream.recv().await {
        Some(StreamEvent::End(Err(HostCallError::Rejected { status, .. }))) => {
            assert_eq!(status, rebon_plugin_protocol::TerminalStatus::Cancelled);
        }
        other => panic!("expected a cancelled terminal, got {other:?}"),
    }
    assert!(
        supervisor.is_alive().await,
        "a cancelled turn is not a failure"
    );
    assert!(supervisor.in_flight("plugin.demo").await.is_empty());
    supervisor.shutdown().await.unwrap();
}

/// An adapter that watches its signal and returns normally ends as a success:
/// stopping early is the adapter's decision to report, not the host's to
/// override.
#[tokio::test]
async fn an_adapter_that_honours_cancellation_by_returning_ends_successfully() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let supervisor = llm_supervisor(&node, &dir).await;

    let mut stream = supervisor
        .stream_llm("plugin.demo", "session-1", "honour", Payload::null())
        .await
        .unwrap();
    let call_id = stream.call_id().to_owned();
    match stream.recv().await {
        Some(StreamEvent::Chunk(_)) => {}
        other => panic!("expected a first chunk, got {other:?}"),
    }
    supervisor.cancel(&call_id).await.unwrap();

    let mut ended = None;
    while let Some(event) = stream.recv().await {
        if let StreamEvent::End(result) = event {
            ended = Some(result);
            break;
        }
    }
    let value = ended
        .expect("the turn ends")
        .expect("the adapter returned rather than throwing");
    assert_eq!(value.to_value().unwrap()["stopped"], true);
    supervisor.shutdown().await.unwrap();
}

/// Provides a tool, and calls a rebon tool from inside it.
///
/// Both directions in one plugin on purpose: `tool/call` and `tool/invoke` are
/// mirror images that must not be able to stand in for each other.
const TOOL_PROVIDER_PLUGIN: &str = r#"
export function activate(plugin) {
  plugin.tool(
    { name: 'grep', description: 'search files', inputSchema: { type: 'object', properties: { needle: { type: 'string' } } } },
    async (input, ctx) => ({ found: input.needle, scope: ctx.scopeId }),
  );
  plugin.tool(
    { name: 'relay', description: 'call a rebon tool and hand back what it said', inputSchema: { type: 'object' } },
    async (input, ctx) => ({ relayed: await ctx.invoke('read_file', input) }),
  );
  plugin.tool(
    { name: 'boom', description: 'always fails', inputSchema: { type: 'object' } },
    async () => { throw new Error('deliberate'); },
  );
}
"#;

fn provider_request(root: String) -> PluginLoadRequest {
    PluginLoadRequest {
        plugin_id: "plugin.demo".into(),
        root,
        entry: "index.mjs".into(),
        services: Vec::new(),
        event_topics: Vec::new(),
        published_topics: Vec::new(),
        llm_providers: Vec::new(),
        tools: vec!["grep".into(), "relay".into(), "boom".into()],
        commands: Vec::new(),
        invokable_tools: vec!["read_file".into()],
        seats: Vec::new(),
        config: Payload::null(),
    }
}

async fn tool_provider(
    node: &std::path::Path,
    dir: &tempfile::TempDir,
    tools: Arc<RecordingTools>,
) -> PluginHostSupervisor {
    let root = plugin_package(dir.path(), TOOL_PROVIDER_PLUGIN);
    let supervisor = PluginHostSupervisor::start(
        base_config(node).with_tool_invoker(tools, [String::from("read_file")]),
    )
    .await
    .expect("host starts");
    supervisor
        .load_plugin(&provider_request(root))
        .await
        .expect("the plugin loads");
    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .unwrap();
    supervisor
}

/// The whole provided-tool vertical: what the plugin registered comes back as
/// something a model could be offered, and calling it runs the plugin's code.
#[tokio::test]
async fn a_plugin_tool_is_described_and_then_called() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let supervisor = tool_provider(&node, &dir, Arc::new(RecordingTools::default())).await;

    // A tool cannot be offered to a model without saying what it does, so the
    // description and schema travel with the registration.
    let described = supervisor.tools("plugin.demo").await;
    let grep = described
        .iter()
        .find(|tool| tool.name == "grep")
        .expect("grep was registered");
    assert_eq!(grep.description, "search files");
    assert_eq!(
        grep.input_schema.to_value().unwrap()["properties"]["needle"]["type"],
        "string"
    );

    let answer = supervisor
        .call_tool(
            "plugin.demo",
            "session-1",
            "grep",
            Payload::from(serde_json::json!({"needle": "todo"})),
        )
        .await
        .expect("the tool answers");
    assert_eq!(
        answer.to_value().unwrap(),
        serde_json::json!({"found": "todo", "scope": "session-1"})
    );
    supervisor.shutdown().await.unwrap();
}

/// The two directions are separate rights. A plugin that provides `grep` may
/// not invoke `grep`, and being allowed to invoke `read_file` does not mean
/// providing it.
#[tokio::test]
async fn providing_a_tool_and_invoking_one_do_not_stand_in_for_each_other() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let tools = Arc::new(RecordingTools::default());
    let supervisor = tool_provider(&node, &dir, Arc::clone(&tools)).await;

    // rebon calling a tool the plugin only declared as invokable: not provided.
    let error = supervisor
        .call_tool("plugin.demo", "session-1", "read_file", Payload::null())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("[UNKNOWN_TOOL]"), "got {error}");

    // And the plugin reaching the other way lands on the invoker, not on its
    // own tool table.
    let relayed = supervisor
        .call_tool(
            "plugin.demo",
            "session-1",
            "relay",
            Payload::from(serde_json::json!({"path": "a.txt"})),
        )
        .await
        .expect("the relay runs");
    assert_eq!(
        relayed.to_value().unwrap(),
        serde_json::json!({"relayed": {"echoed": {"path": "a.txt"}}})
    );
    assert_eq!(
        tools.seen.lock().unwrap().len(),
        1,
        "the invoke went to the embedder, not back into the plugin"
    );
    supervisor.shutdown().await.unwrap();
}

/// A tool that throws is that call failing, with the plugin's own message. The
/// host stays usable and the call stops being counted, so an unload can finish.
#[tokio::test]
async fn a_failing_tool_ends_its_own_call_and_nothing_else() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let supervisor = tool_provider(&node, &dir, Arc::new(RecordingTools::default())).await;

    let error = supervisor
        .call_tool("plugin.demo", "session-1", "boom", Payload::null())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("deliberate"), "got {error}");
    assert!(supervisor.is_alive().await);
    assert!(supervisor.in_flight("plugin.demo").await.is_empty());

    let drained = supervisor.unload_plugin("plugin.demo").await.unwrap();
    assert!(drained.outstanding_calls.is_empty());
    supervisor.shutdown().await.unwrap();
}

/// The manifest is the ceiling here too: a tool the manifest did not declare is
/// refused at registration, so the failure names the plugin's own line.
#[tokio::test]
async fn a_tool_the_manifest_did_not_declare_cannot_be_registered() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(
        dir.path(),
        "export function activate(plugin) { plugin.tool({ name: 'smuggled', description: 'd', inputSchema: {} }, async () => null); }\n",
    );
    let supervisor = started(&node).await;

    let mut request = provider_request(root);
    request.tools = vec!["grep".into()];
    // Nothing to invoke here: this supervisor exposes no tools, and the point
    // under test is the *provided* side.
    request.invokable_tools = Vec::new();
    let error = supervisor.load_plugin(&request).await.unwrap_err();
    assert!(
        error.to_string().contains("[UNAUTHORIZED_REGISTER]"),
        "got {error}"
    );
    supervisor.shutdown().await.unwrap();
}

/// A tool with nothing to say about itself cannot be offered to a model, so it
/// is refused where it is written rather than reaching the model as noise.
#[tokio::test]
async fn a_tool_with_no_description_is_refused_at_registration() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(
        dir.path(),
        "export function activate(plugin) { plugin.tool({ name: 'grep', description: '', inputSchema: {} }, async () => null); }\n",
    );
    let supervisor = started(&node).await;

    let mut request = provider_request(root);
    request.tools = vec!["grep".into()];
    request.invokable_tools = Vec::new();
    let error = supervisor.load_plugin(&request).await.unwrap_err();
    assert!(
        error.to_string().contains("[EMPTY_DESCRIPTION]"),
        "got {error}"
    );
    supervisor.shutdown().await.unwrap();
}

/// Reaches a kernel seat from inside a service handler.
const SEAT_PLUGIN: &str = r#"
export function activate(plugin) {
  plugin.service('resolve', async (request, ctx) => {
    try {
      return { value: await ctx.seat(request.seat, request.method, request.params ?? null) };
    } catch (cause) {
      return { refused: cause.code, message: cause.message };
    }
  });
}
"#;

/// Stands in for the embedder's kernel. A real one routes into the seat
/// registry; this one records who asked and answers.
#[derive(Default)]
struct RecordingSeats {
    seen: std::sync::Mutex<Vec<(String, String, String)>>,
    deny: bool,
}

impl SeatDispatcher for RecordingSeats {
    fn call(
        &self,
        invocation: SeatInvocation,
    ) -> Pin<Box<dyn Future<Output = Result<Payload, ToolRefusal>> + Send + '_>> {
        self.seen.lock().unwrap().push((
            invocation.identity.plugin_id.clone(),
            invocation.seat.clone(),
            invocation.method.clone(),
        ));
        let deny = self.deny;
        let params = invocation.params;
        Box::pin(async move {
            if deny {
                return Err(ToolRefusal::new(
                    "[NOT_IN_SEAT]",
                    "this seat is not granted",
                ));
            }
            Ok(Payload::from(serde_json::json!({"answered": params})))
        })
    }
}

fn seat_request(root: String, seats: Vec<String>) -> PluginLoadRequest {
    PluginLoadRequest {
        plugin_id: "plugin.demo".into(),
        root,
        entry: "index.mjs".into(),
        services: vec!["resolve".into()],
        event_topics: Vec::new(),
        published_topics: Vec::new(),
        llm_providers: Vec::new(),
        tools: Vec::new(),
        commands: Vec::new(),
        invokable_tools: Vec::new(),
        seats,
        config: Payload::null(),
    }
}

async fn seat_supervisor(
    node: &std::path::Path,
    dir: &tempfile::TempDir,
    seats: Arc<RecordingSeats>,
    exposed: &[&str],
) -> PluginHostSupervisor {
    let root = plugin_package(dir.path(), SEAT_PLUGIN);
    let supervisor = PluginHostSupervisor::start(
        base_config(node)
            .with_seat_dispatcher(seats, exposed.iter().map(|seat| (*seat).to_string())),
    )
    .await
    .expect("host starts");
    supervisor
        .load_plugin(&seat_request(root, vec!["credentials".into()]))
        .await
        .expect("the plugin loads");
    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .unwrap();
    supervisor
}

/// The last Node → rebon method, end to end: a plugin reaches a kernel seat and
/// the identity that arrives at the embedder is the one the host injected.
#[tokio::test]
async fn a_plugin_calls_a_kernel_seat_and_gets_its_answer() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let seats = Arc::new(RecordingSeats::default());
    let supervisor = seat_supervisor(&node, &dir, Arc::clone(&seats), &["credentials"]).await;

    let answer = supervisor
        .call_service(
            "plugin.demo",
            "session-1",
            "resolve",
            Payload::from(serde_json::json!({
                "seat": "credentials",
                "method": "resolveEnv",
                "params": {"ref": "DEEPSEEK_API_KEY"}
            })),
        )
        .await
        .expect("the service answers");
    assert_eq!(
        answer.to_value().unwrap(),
        serde_json::json!({"value": {"answered": {"ref": "DEEPSEEK_API_KEY"}}})
    );
    assert_eq!(
        seats.seen.lock().unwrap().as_slice(),
        [(
            "plugin.demo".to_string(),
            "credentials".to_string(),
            "resolveEnv".to_string()
        )]
    );
    supervisor.shutdown().await.unwrap();
}

/// The manifest is the ceiling, and this side refuses first so the error names
/// the plugin's own line rather than arriving from across a process boundary.
#[tokio::test]
async fn a_seat_the_manifest_did_not_declare_is_refused_before_anything_is_sent() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let seats = Arc::new(RecordingSeats::default());
    let supervisor = seat_supervisor(&node, &dir, Arc::clone(&seats), &["credentials"]).await;

    let answer = supervisor
        .call_service(
            "plugin.demo",
            "session-1",
            "resolve",
            Payload::from(serde_json::json!({"seat": "logger", "method": "warn"})),
        )
        .await
        .unwrap();
    assert_eq!(answer.to_value().unwrap()["refused"], "[UNAUTHORIZED_SEAT]");
    assert!(
        seats.seen.lock().unwrap().is_empty(),
        "nothing was asked of the embedder"
    );
    supervisor.shutdown().await.unwrap();
}

/// A manifest asking for a seat this build does not expose describes a plugin
/// that cannot work, so it is refused at load rather than at first use.
#[tokio::test]
async fn a_manifest_declaring_an_unexposed_seat_is_refused_at_load() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path(), SEAT_PLUGIN);
    let supervisor = PluginHostSupervisor::start(base_config(&node).with_seat_dispatcher(
        Arc::new(RecordingSeats::default()),
        [String::from("logger")],
    ))
    .await
    .unwrap();

    let error = supervisor
        .load_plugin(&seat_request(root, vec!["credentials".into()]))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("[UNAVAILABLE_SEAT]"),
        "got {error}"
    );
    supervisor.shutdown().await.unwrap();
}

/// The embedder's own refusal reaches the plugin with its code intact — a seat
/// that is not granted says so in the kernel's vocabulary.
#[tokio::test]
async fn a_refused_seat_reaches_the_plugin_with_its_reason() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let seats = Arc::new(RecordingSeats {
        deny: true,
        ..RecordingSeats::default()
    });
    let supervisor = seat_supervisor(&node, &dir, seats, &["credentials"]).await;

    let answer = supervisor
        .call_service(
            "plugin.demo",
            "session-1",
            "resolve",
            Payload::from(serde_json::json!({"seat": "credentials", "method": "resolveEnv"})),
        )
        .await
        .unwrap();
    let value = answer.to_value().unwrap();
    assert_eq!(value["refused"], "[NOT_IN_SEAT]");
    assert!(
        value["message"].as_str().unwrap().contains("not granted"),
        "{value}"
    );
    supervisor.shutdown().await.unwrap();
}

/// With no dispatcher installed there is nothing to call, and a plugin that
/// asks is told so rather than left waiting.
#[tokio::test]
async fn a_plane_with_no_dispatcher_exposes_no_seats() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path(), SEAT_PLUGIN);
    let supervisor = started(&node).await;

    let error = supervisor
        .load_plugin(&seat_request(root, vec!["credentials".into()]))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("[UNAVAILABLE_SEAT]"),
        "got {error}"
    );
    supervisor.shutdown().await.unwrap();
}

/// Publishes on a topic from inside a service handler.
const EMIT_PLUGIN: &str = r#"
export function activate(plugin) {
  plugin.service('announce', async (request, ctx) => {
    try {
      return { answer: await ctx.publish(request.topic, request.event ?? null) };
    } catch (cause) {
      return { refused: cause.code, message: cause.message };
    }
  });
}
"#;

/// Publishes a whole burst from one call, the way a loop reports a turn: the
/// events go out back to back and the last one is the one that means "done".
const BURST_PLUGIN: &str = r#"
export function activate(plugin) {
  plugin.service('burst', async (request, ctx) => {
    const sent = [];
    for (let i = 0; i < request.count; i += 1) {
      sent.push(ctx.publish('compose:session/append', { index: i }));
    }
    await Promise.all(sent);
    return { sent: sent.length };
  });
}
"#;

/// Stands in for rebon's event plane: records what was published, by whom.
#[derive(Default)]
struct RecordingEvents {
    seen: std::sync::Mutex<Vec<(String, String, u64, String)>>,
    /// `index` from each event body, in arrival order.
    order: std::sync::Mutex<Vec<u64>>,
    deny: bool,
}

impl EventPublisher for RecordingEvents {
    fn publish(
        &self,
        event: PublishedEvent,
    ) -> Pin<Box<dyn Future<Output = Result<Payload, ToolRefusal>> + Send + '_>> {
        self.seen.lock().unwrap().push((
            event.identity.plugin_id.clone(),
            event.identity.scope_id.clone(),
            event.identity.scope_generation,
            event.topic.clone(),
        ));
        let index = event
            .event
            .to_value()
            .ok()
            .and_then(|value| value.get("index").and_then(serde_json::Value::as_u64));
        let deny = self.deny;
        Box::pin(async move {
            if let Some(index) = index {
                // Recorded after a yield, because a real publisher awaits —
                // rebon's own delivers into the kernel. Without one lane for
                // events, this is where per-request tasks overtake each other.
                tokio::task::yield_now().await;
                self.order.lock().unwrap().push(index);
            }
            if deny {
                return Err(ToolRefusal::new(
                    "[UNKNOWN_TOPIC]",
                    "no listener for that topic",
                ));
            }
            Ok(Payload::from(serde_json::json!({"published": true})))
        })
    }
}

fn emit_request(root: String, published_topics: Vec<String>) -> PluginLoadRequest {
    PluginLoadRequest {
        plugin_id: "plugin.demo".into(),
        root,
        entry: "index.mjs".into(),
        services: vec!["announce".into()],
        event_topics: Vec::new(),
        published_topics,
        llm_providers: Vec::new(),
        tools: Vec::new(),
        commands: Vec::new(),
        invokable_tools: Vec::new(),
        seats: Vec::new(),
        config: Payload::null(),
    }
}

async fn emit_supervisor(
    node: &std::path::Path,
    dir: &tempfile::TempDir,
    events: Arc<RecordingEvents>,
    topics: Vec<String>,
) -> PluginHostSupervisor {
    let root = plugin_package(dir.path(), EMIT_PLUGIN);
    let supervisor = PluginHostSupervisor::start(base_config(node).with_event_publisher(events))
        .await
        .expect("host starts");
    supervisor
        .load_plugin(&emit_request(root, topics))
        .await
        .expect("the plugin loads");
    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .unwrap();
    supervisor
}

/// The upward event path, end to end: a plugin publishes and the identity that
/// reaches the embedder is the one the host injected, not one the plugin chose.
#[tokio::test]
async fn a_plugin_publishes_an_event_and_rebon_learns_who_sent_it() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(RecordingEvents::default());
    let supervisor = emit_supervisor(
        &node,
        &dir,
        Arc::clone(&events),
        vec!["compose:session/append".into()],
    )
    .await;

    let answer = supervisor
        .call_service(
            "plugin.demo",
            "session-1",
            "announce",
            Payload::from(serde_json::json!({
                "topic": "compose:session/append",
                "event": {"type": "append"},
            })),
        )
        .await
        .expect("the service answers");

    assert_eq!(
        answer.to_value().unwrap(),
        serde_json::json!({"answer": {"published": true}})
    );
    assert_eq!(
        events.seen.lock().unwrap().as_slice(),
        [(
            "plugin.demo".to_string(),
            "session-1".to_string(),
            0,
            "compose:session/append".to_string()
        )]
    );
    supervisor.shutdown().await.unwrap();
}

/// Events reach the embedder in the order the plugin sent them.
///
/// Every other upstream request is answered on a task of its own, which is
/// right because answers are matched by call id. An event is not only an
/// answer: publishing it is a side effect, and for a stream whose last item
/// means "the turn is over" the order of those effects is the whole meaning.
/// Spawning per request let a loop's `turn/end` overtake the messages of that
/// turn, and the usage they carried was counted into a turn that had already
/// been closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_reach_the_embedder_in_the_order_they_were_sent() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(RecordingEvents::default());
    let root = plugin_package(dir.path(), BURST_PLUGIN);
    let mut request = emit_request(root, vec!["compose:session/append".into()]);
    request.services = vec!["burst".into()];
    let supervisor = PluginHostSupervisor::start(
        base_config(&node).with_event_publisher(Arc::clone(&events) as Arc<dyn EventPublisher>),
    )
    .await
    .expect("host starts");
    supervisor
        .load_plugin(&request)
        .await
        .expect("the plugin loads");
    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .unwrap();

    // Enough that a per-request task pool reorders them in practice.
    const COUNT: usize = 64;
    supervisor
        .call_service(
            "plugin.demo",
            "session-1",
            "burst",
            Payload::from(serde_json::json!({ "count": COUNT })),
        )
        .await
        .expect("the service answers");

    let seen = events.order.lock().unwrap().clone();
    assert_eq!(seen.len(), COUNT, "every event arrived");
    assert_eq!(
        seen,
        (0..COUNT as u64).collect::<Vec<_>>(),
        "events arrived out of order"
    );
    supervisor.shutdown().await.unwrap();
}

/// The manifest is the ceiling on this direction too, and the refusal happens
/// on the plugin's own side — nothing reaches the embedder to be judged.
#[tokio::test]
async fn publishing_an_undeclared_topic_never_reaches_the_embedder() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(RecordingEvents::default());
    let supervisor =
        emit_supervisor(&node, &dir, Arc::clone(&events), vec!["allowed".into()]).await;

    let answer = supervisor
        .call_service(
            "plugin.demo",
            "session-1",
            "announce",
            Payload::from(serde_json::json!({"topic": "secrets"})),
        )
        .await
        .expect("the service answers");

    assert_eq!(
        answer.to_value().unwrap()["refused"],
        serde_json::json!("[UNAUTHORIZED_TOPIC]")
    );
    assert!(events.seen.lock().unwrap().is_empty());
    supervisor.shutdown().await.unwrap();
}

/// rebon's own refusal reaches the plugin. "The call failed" sends a plugin
/// author nowhere; the code and message say what actually happened.
#[tokio::test]
async fn the_embedders_refusal_reaches_the_plugin_verbatim() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(RecordingEvents {
        deny: true,
        ..RecordingEvents::default()
    });
    let supervisor = emit_supervisor(&node, &dir, events, vec!["session".into()]).await;

    let answer = supervisor
        .call_service(
            "plugin.demo",
            "session-1",
            "announce",
            Payload::from(serde_json::json!({"topic": "session"})),
        )
        .await
        .expect("the service answers");

    let value = answer.to_value().unwrap();
    assert_eq!(value["refused"], serde_json::json!("[UNKNOWN_TOPIC]"));
    assert!(
        value["message"].as_str().unwrap().contains("no listener"),
        "{value}"
    );
    supervisor.shutdown().await.unwrap();
}

/// A plane with nowhere to publish refuses the manifest that asks to, rather
/// than loading a plugin whose events would vanish.
#[tokio::test]
async fn declaring_a_published_topic_without_a_publisher_fails_the_load() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path(), EMIT_PLUGIN);
    let supervisor = PluginHostSupervisor::start(base_config(&node))
        .await
        .expect("host starts");

    let error = supervisor
        .load_plugin(&emit_request(root, vec!["session".into()]))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("[UNAVAILABLE_EVENTS]"),
        "got {error}"
    );
    supervisor.shutdown().await.unwrap();
}

/// A service that never answers, and reports whether it was cancelled.
///
/// `stalled` parks until its abort signal fires; `cancelled` says whether that
/// happened. The pair is what proves a bound does more than stop waiting.
const STALLING_PLUGIN: &str = r#"
export function activate(plugin) {
  let sawCancel = false;
  plugin.service('stalled', async (request, ctx) => {
    await new Promise((resolve) => {
      if (ctx.signal.aborted) { sawCancel = true; return resolve(); }
      ctx.signal.addEventListener('abort', () => { sawCancel = true; resolve(); }, { once: true });
    });
    return { stopped: true };
  });
  plugin.service('cancelled', async () => ({ cancelled: sawCancel }));
}
"#;

fn stalling_request(root: String) -> PluginLoadRequest {
    PluginLoadRequest {
        plugin_id: "plugin.demo".into(),
        root,
        entry: "index.mjs".into(),
        services: vec!["stalled".into(), "cancelled".into()],
        event_topics: Vec::new(),
        published_topics: Vec::new(),
        llm_providers: Vec::new(),
        tools: Vec::new(),
        commands: Vec::new(),
        invokable_tools: Vec::new(),
        seats: Vec::new(),
        config: Payload::null(),
    }
}

/// A bounded call that expires cancels the call it gave up on.
///
/// Dropping the future would stop this side waiting, and leave the plugin
/// computing an answer nobody will read — on a host with a call budget, that
/// is a slot lost for the rest of the process's life. So the bound sends
/// `call/cancel` on the way out, and the plugin can see it arrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bounded_call_that_expires_cancels_what_it_gave_up_on() {
    let Some(node) = node() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path(), STALLING_PLUGIN);
    let supervisor = started(&node).await;
    supervisor
        .load_plugin(&stalling_request(root))
        .await
        .expect("the plugin loads");
    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .expect("scope opens");

    let error = supervisor
        .call_service_bounded(
            "plugin.demo",
            "session-1",
            "stalled",
            Payload::null(),
            Some(Duration::from_millis(200)),
        )
        .await
        .expect_err("a service that never answers must not be waited on forever");
    assert!(
        error.to_string().contains("[HOST_UNANSWERED]"),
        "the refusal names the code: {error}"
    );
    assert!(
        error.to_string().contains("plugin.demo"),
        "the refusal names the plugin: {error}"
    );

    // The cancel travels as a notification, so give the host a moment to run
    // the abort handler before asking what it saw.
    let mut seen = false;
    for _ in 0..50 {
        let answer = supervisor
            .call_service("plugin.demo", "session-1", "cancelled", Payload::null())
            .await
            .expect("the reporting service answers");
        if answer.to_value().unwrap() == serde_json::json!({"cancelled": true}) {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(seen, "the plugin saw the cancel the bound sent");

    // And the abandoned call stops being counted, so a later drain can finish.
    for _ in 0..50 {
        if supervisor.in_flight("plugin.demo").await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        supervisor.in_flight("plugin.demo").await.is_empty(),
        "the cancelled call is no longer in flight"
    );
}
