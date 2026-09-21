//! Can a plugin that was unloaded be loaded again?
//!
//! Both of these run past the real Node host the unload/reload question the
//! protocol poses, and the second is the counterexample that made the answer
//! two-sided. It failed until the drain gained its second ending: rebon used
//! to finish a drain at one place only — the
//! unload reply — and only on the *host's* count of what was running, so a call
//! this side still counted stranded the plugin in Draining for the life of the
//! process, with its id unusable. Now whoever closes the last call finishes the
//! drain, which is the rule the host has always followed in the `finally` of
//! every handler.

use std::{env, path::PathBuf, time::Duration};

use rebon_plugin_protocol::{Payload, PluginLoadRequest};
use rebon_plugin_supervisor::{HostConfig, PluginHostSupervisor};

fn node() -> Option<PathBuf> {
    env::var_os("REBON_TEST_NODE").map(PathBuf::from)
}

fn real_host() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtimes/node/plugin-host/src/cli.mjs")
}

const STREAMING_PLUGIN: &str = r#"
export function activate(plugin) {
  plugin.service('count', async (request, ctx) => {
    for (let index = 0; index < request.n; index++) await ctx.emit({ index });
    return { total: request.n };
  });
  plugin.service('slow', async () => {
    await new Promise((resolve) => setTimeout(resolve, 1500));
    return { late: true };
  });
}
"#;

fn plugin_package(dir: &std::path::Path) -> String {
    std::fs::write(dir.join("index.mjs"), STREAMING_PLUGIN).unwrap();
    dir.to_string_lossy().replace('\\', "/")
}

fn load_request(root: String) -> PluginLoadRequest {
    PluginLoadRequest {
        plugin_id: "plugin.demo".into(),
        root,
        entry: "index.mjs".into(),
        services: vec!["count".into(), "slow".into()],
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

async fn started(node: &std::path::Path) -> PluginHostSupervisor {
    PluginHostSupervisor::start(
        HostConfig::new(node, real_host())
            .with_startup_timeout(Duration::from_secs(20))
            .with_working_directory(env::temp_dir()),
    )
    .await
    .expect("the host starts")
}

/// Baseline: nothing in flight, so both sides finish the drain and the same id
/// loads again.
#[tokio::test(flavor = "multi_thread")]
async fn a_reload_after_a_quiet_unload_succeeds() {
    let Some(node) = node() else {
        eprintln!("skipping: set REBON_TEST_NODE");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path());
    let supervisor = started(&node).await;

    supervisor
        .load_plugin(&load_request(root.clone()))
        .await
        .expect("first load");
    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .unwrap();
    let drained = supervisor.unload_plugin("plugin.demo").await.unwrap();
    assert!(drained.outstanding_calls.is_empty());

    let again = supervisor.load_plugin(&load_request(root)).await;
    assert!(
        again.is_ok(),
        "a quiet unload must permit a reload: {again:?}"
    );
    supervisor.shutdown().await.unwrap();
}

/// The host has finished the call, but this side still counts it because the
/// caller has not drained the stream — a window the caller holds open for as
/// long as it likes. An unload landing inside it has to survive being told by
/// the host that nothing is running, and still finish when the caller finally
/// walks away.
///
/// This is the shape of the counterexample that made the second ending
/// necessary: the host has finished, this side has not, and only the caller
/// walking away closes the gap.
#[tokio::test(flavor = "multi_thread")]
async fn an_unload_while_a_finished_stream_is_unread() {
    let Some(node) = node() else {
        eprintln!("skipping: set REBON_TEST_NODE");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path());
    let supervisor = started(&node).await;

    supervisor
        .load_plugin(&load_request(root.clone()))
        .await
        .expect("first load");
    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .unwrap();

    let stream = supervisor
        .call_service_streaming(
            "plugin.demo",
            "session-1",
            "count",
            Payload::from(serde_json::json!({"n": 2})),
        )
        .await
        .expect("the stream starts");

    // Let the host finish the handler: chunks emitted, terminal written. The
    // host's in-flight set is empty from here; this side's is not, because the
    // stream has not been read.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        supervisor.in_flight("plugin.demo").await.len(),
        1,
        "this side still counts the unread stream"
    );

    supervisor.unload_plugin("plugin.demo").await.unwrap();

    // The caller now walks away, which is what closes the call on this side.
    drop(stream);
    for _ in 0..100 {
        if supervisor.in_flight("plugin.demo").await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        supervisor.in_flight("plugin.demo").await.is_empty(),
        "the abandoned stream must stop being counted"
    );

    // Everything is quiet on both sides. Can the id be loaded again?
    let again = supervisor.load_plugin(&load_request(root)).await;
    assert!(
        again.is_ok(),
        "a plugin whose calls all finished must be loadable again, got {again:?}"
    );
    supervisor.shutdown().await.unwrap();
}

/// The caller of a call that answers once stops waiting for the answer.
///
/// The same window the test above covers for streams, on the path that does not
/// stream — and the one production actually walks into, because every plugin
/// tool call is wrapped in a timeout and every turn can be cancelled. Admitting
/// the call counts it; only closing it counts it back out, and the close used to
/// live on a line the dropped future never reached. The call then stayed counted
/// forever, which is a slow way of saying the plugin could never be unloaded and
/// its id could never be used again.
#[tokio::test(flavor = "multi_thread")]
async fn an_abandoned_one_shot_call_stops_being_counted() {
    let Some(node) = node() else {
        eprintln!("skipping: set REBON_TEST_NODE");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_package(dir.path());
    let supervisor = started(&node).await;

    supervisor
        .load_plugin(&load_request(root.clone()))
        .await
        .expect("first load");
    supervisor
        .open_scope("plugin.demo", "session-1", "C:/workspace")
        .await
        .unwrap();

    // What the embedder does to a plugin tool that takes too long, and
    // what a cancelled turn does to one that has not answered yet: the future is
    // dropped with the call still in flight.
    let abandoned = tokio::time::timeout(
        Duration::from_millis(100),
        supervisor.call_service("plugin.demo", "session-1", "slow", Payload::null()),
    )
    .await;
    assert!(abandoned.is_err(), "the slow service must outlast the wait");

    for _ in 0..100 {
        if supervisor.in_flight("plugin.demo").await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        supervisor.in_flight("plugin.demo").await.is_empty(),
        "a call nobody is waiting for must stop being counted"
    );

    // The host retires a plugin in the `finally` of its own last handler, and
    // that handler is still sleeping. Its side of the drain is a separate
    // promise from this one, so it is waited out rather than raced: what is
    // being tested here is that *this* side stopped counting.
    tokio::time::sleep(Duration::from_millis(2000)).await;

    supervisor.unload_plugin("plugin.demo").await.unwrap();
    let again = supervisor.load_plugin(&load_request(root)).await;
    assert!(
        again.is_ok(),
        "abandoning one call must not cost the plugin its id, got {again:?}"
    );
    supervisor.shutdown().await.unwrap();
}
