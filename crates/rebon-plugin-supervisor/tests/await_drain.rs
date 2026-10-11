//! Waiting for an unload to actually finish.
//!
//! `plugin/unload` answers when the drain *begins*. Whoever is about to
//! withdraw the plugin's registrations, or take its container down, needs the
//! moment it *ends* — and a bound on how long to wait for it, because a plugin
//! whose handler never returns would otherwise hold that caller forever.
//! [`PluginHostSupervisor::await_drain`] is that wait; these run it past the
//! real Node host through every way a drain can end.

use std::{
    env,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use rebon_plugin_protocol::{Payload, PluginLoadRequest};
use rebon_plugin_supervisor::{HostConfig, PluginHostSupervisor};

fn node() -> Option<PathBuf> {
    env::var_os("REBON_TEST_NODE").map(PathBuf::from)
}

fn real_host() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtimes/node/plugin-host/src/cli.mjs")
}

const PLUGIN: &str = r#"
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

const ID: &str = "plugin.drain";

fn package(dir: &std::path::Path) -> String {
    std::fs::write(dir.join("index.mjs"), PLUGIN).unwrap();
    dir.to_string_lossy().replace('\\', "/")
}

fn load_request(root: String) -> PluginLoadRequest {
    PluginLoadRequest {
        adapter: rebon_plugin_protocol::PluginAdapter {
            id: "native".into(),
            revision: 1,
        },
        plugin_id: ID.into(),
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

/// A host with the plugin loaded and a scope open, plus the package root so a
/// test can load it again.
async fn loaded(node: &std::path::Path) -> (Arc<PluginHostSupervisor>, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let root = package(dir.path());
    let supervisor = PluginHostSupervisor::start(
        HostConfig::new(node, real_host())
            .with_startup_timeout(Duration::from_secs(20))
            .with_working_directory(env::temp_dir()),
    )
    .await
    .expect("the host starts");
    supervisor
        .load_plugin(&load_request(root.clone()))
        .await
        .expect("the plugin loads");
    supervisor
        .open_scope(ID, "session-1", "C:/workspace")
        .await
        .unwrap();
    (Arc::new(supervisor), root, dir)
}

/// Starts the slow service and returns once this side counts it.
async fn slow_call_in_flight(
    supervisor: &Arc<PluginHostSupervisor>,
) -> tokio::task::JoinHandle<Result<Payload, rebon_plugin_supervisor::HostCallError>> {
    let caller = Arc::clone(supervisor);
    let call = tokio::spawn(async move {
        caller
            .call_service(ID, "session-1", "slow", Payload::null())
            .await
    });
    for _ in 0..100 {
        if !supervisor.in_flight(ID).await.is_empty() {
            return call;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the slow call never started");
}

/// Nothing running: the wait is over before it starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_unload_needs_no_wait() {
    let Some(node) = node() else {
        eprintln!("skipping: set REBON_TEST_NODE");
        return;
    };
    let (supervisor, _, _dir) = loaded(&node).await;
    supervisor.unload_plugin(ID).await.unwrap();

    let started = Instant::now();
    assert_eq!(
        supervisor.await_drain(ID, Duration::from_secs(5)).await,
        Ok(())
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    supervisor.shutdown().await.unwrap();
}

/// A call in flight finishes inside the deadline: the wait ends when it does,
/// and the call still gets the answer of the run it was made to.
#[tokio::test(flavor = "multi_thread")]
async fn the_wait_ends_when_the_last_call_does() {
    let Some(node) = node() else {
        eprintln!("skipping: set REBON_TEST_NODE");
        return;
    };
    let (supervisor, root, _dir) = loaded(&node).await;
    let call = slow_call_in_flight(&supervisor).await;

    let report = supervisor.unload_plugin(ID).await.unwrap();
    assert_eq!(
        report.outstanding_calls.len(),
        1,
        "the drain began with work"
    );
    assert_eq!(
        supervisor.await_drain(ID, Duration::from_secs(10)).await,
        Ok(())
    );
    assert!(supervisor.in_flight(ID).await.is_empty());

    let answer = call.await.unwrap().expect("the old run answers its call");
    assert_eq!(answer.to_value().unwrap()["late"], serde_json::json!(true));

    assert!(
        supervisor.load_plugin(&load_request(root)).await.is_ok(),
        "a finished drain frees the id"
    );
    supervisor.shutdown().await.unwrap();
}

/// The deadline passes first: the caller is told which calls are still
/// running, the plugin stays draining, and a later wait still sees it finish.
#[tokio::test(flavor = "multi_thread")]
async fn a_deadline_reports_what_is_still_running() {
    let Some(node) = node() else {
        eprintln!("skipping: set REBON_TEST_NODE");
        return;
    };
    let (supervisor, root, _dir) = loaded(&node).await;
    let call = slow_call_in_flight(&supervisor).await;
    supervisor.unload_plugin(ID).await.unwrap();

    let outstanding = supervisor
        .await_drain(ID, Duration::from_millis(200))
        .await
        .expect_err("the slow call outlasts the deadline");
    assert_eq!(outstanding, supervisor.in_flight(ID).await);
    assert_eq!(outstanding.len(), 1);

    // Timing out decided nothing: the call is still answered, and the drain
    // still ends when it does.
    assert!(call.await.unwrap().is_ok());
    assert_eq!(
        supervisor.await_drain(ID, Duration::from_secs(10)).await,
        Ok(())
    );
    assert!(supervisor.load_plugin(&load_request(root)).await.is_ok());
    supervisor.shutdown().await.unwrap();
}

/// A dead host runs nothing, so a drain waiting on it is over.
#[tokio::test(flavor = "multi_thread")]
async fn the_host_dying_ends_the_wait() {
    let Some(node) = node() else {
        eprintln!("skipping: set REBON_TEST_NODE");
        return;
    };
    let (supervisor, _, _dir) = loaded(&node).await;
    let _call = slow_call_in_flight(&supervisor).await;
    supervisor.unload_plugin(ID).await.unwrap();

    let waiter = Arc::clone(&supervisor);
    let wait = tokio::spawn(async move { waiter.await_drain(ID, Duration::from_secs(30)).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = Instant::now();
    supervisor.shutdown().await.unwrap();

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .expect("the wait ends with the host")
            .unwrap(),
        Ok(())
    );
    assert!(started.elapsed() < Duration::from_secs(10));
}

/// A stream the caller has not finished reading is a call this side still
/// counts. The wait sees the caller walking away, not only the host finishing.
#[tokio::test(flavor = "multi_thread")]
async fn an_unread_stream_holds_the_drain_until_dropped() {
    let Some(node) = node() else {
        eprintln!("skipping: set REBON_TEST_NODE");
        return;
    };
    let (supervisor, _, _dir) = loaded(&node).await;
    let stream = supervisor
        .call_service_streaming(
            ID,
            "session-1",
            "count",
            Payload::from(serde_json::json!({"n": 2})),
        )
        .await
        .expect("the stream starts");
    tokio::time::sleep(Duration::from_millis(300)).await;
    supervisor.unload_plugin(ID).await.unwrap();

    assert!(
        supervisor
            .await_drain(ID, Duration::from_millis(200))
            .await
            .is_err(),
        "the unread stream is still counted"
    );

    let waiter = Arc::clone(&supervisor);
    let wait = tokio::spawn(async move { waiter.await_drain(ID, Duration::from_secs(10)).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(stream);
    assert_eq!(wait.await.unwrap(), Ok(()));
    supervisor.shutdown().await.unwrap();
}

/// Nothing to wait for: an id never loaded, and a plugin that is not being
/// unloaded. Neither is a drain, so neither may block.
#[tokio::test(flavor = "multi_thread")]
async fn there_is_no_drain_to_wait_for() {
    let Some(node) = node() else {
        eprintln!("skipping: set REBON_TEST_NODE");
        return;
    };
    let (supervisor, _, _dir) = loaded(&node).await;

    assert_eq!(
        supervisor
            .await_drain("plugin.never", Duration::from_secs(5))
            .await,
        Ok(())
    );
    // Ready and routable: there is no unload in progress to wait out.
    assert_eq!(
        supervisor.await_drain(ID, Duration::from_secs(5)).await,
        Ok(())
    );
    supervisor.shutdown().await.unwrap();
}

/// The real host with some of what rebon sends it dropped on the floor, so a
/// test can have a host that never answers one method.
fn deaf_host(into: &std::path::Path, methods: &[&str]) -> PathBuf {
    let real = real_host().to_string_lossy().replace('\\', "/");
    let script = into.join("deaf-host.mjs");
    std::fs::write(
        &script,
        format!(
            r#"
import {{ PassThrough }} from 'node:stream';
import {{ createInterface }} from 'node:readline';
import {{ pathToFileURL }} from 'node:url';
const DROP = {drop};
const REAL = {real};
const filtered = new PassThrough();
createInterface({{ input: process.stdin, crlfDelay: Infinity }})
  .on('line', (line) => {{
    if (!DROP.some((method) => line.includes(JSON.stringify(method)))) filtered.write(line + '\n');
  }})
  .on('close', () => filtered.end());
Object.defineProperty(process, 'stdin', {{ value: filtered, configurable: true }});
process.argv[1] = REAL;
await import(pathToFileURL(REAL).href);
"#,
            drop = serde_json::to_string(methods).unwrap(),
            real = serde_json::to_string(&real).unwrap(),
        ),
    )
    .unwrap();
    script
}

async fn deaf(
    node: &std::path::Path,
    dir: &std::path::Path,
    methods: &[&str],
    shutdown_timeout: Duration,
) -> Arc<PluginHostSupervisor> {
    let mut config = HostConfig::new(node, deaf_host(dir, methods))
        .with_startup_timeout(Duration::from_secs(20))
        .with_working_directory(env::temp_dir());
    config.shutdown_timeout = shutdown_timeout;
    let supervisor = PluginHostSupervisor::start(config)
        .await
        .expect("the host starts");
    let root = package(dir);
    supervisor
        .load_plugin(&load_request(root))
        .await
        .expect("the plugin loads");
    supervisor
        .open_scope(ID, "session-1", "C:/workspace")
        .await
        .unwrap();
    Arc::new(supervisor)
}

/// A host that takes `platform/shutdown` and never answers: the request is
/// bounded by the shutdown window, the process is reaped anyway, and the
/// error is reported after — not instead of — the reaping.
#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_shutdown_is_bounded_and_still_reaps() {
    let Some(node) = node() else {
        eprintln!("skipping: set REBON_TEST_NODE");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let supervisor = deaf(
        &node,
        dir.path(),
        &["platform/shutdown"],
        Duration::from_millis(500),
    )
    .await;

    let started = Instant::now();
    let answer = supervisor.shutdown().await;
    assert!(started.elapsed() < Duration::from_secs(5), "bounded");
    let error = answer.expect_err("nobody answered the shutdown");
    assert!(
        error
            .to_string()
            .starts_with(rebon_plugin_supervisor::HOST_UNANSWERED_CODE),
        "{error}"
    );
    assert!(!supervisor.is_alive().await);
}

/// `terminate` does not ask: the host is gone when it returns, and a call it
/// was running is answered with that.
#[tokio::test(flavor = "multi_thread")]
async fn terminate_ends_the_host_and_answers_what_it_owed() {
    let Some(node) = node() else {
        eprintln!("skipping: set REBON_TEST_NODE");
        return;
    };
    let (supervisor, _, _dir) = loaded(&node).await;
    let call = slow_call_in_flight(&supervisor).await;

    let started = Instant::now();
    supervisor.terminate().await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "no grace window"
    );
    assert!(!supervisor.is_alive().await);
    assert_eq!(
        supervisor.failure().await.map(|failure| failure.reason),
        Some(rebon_plugin_supervisor::TERMINATED_REASON.to_owned())
    );
    let answer = tokio::time::timeout(Duration::from_secs(2), call)
        .await
        .expect("the call is answered")
        .unwrap();
    assert!(answer.is_err(), "the host that owed it is gone: {answer:?}");
}

/// A host that takes `plugin/unload` and never answers: the request is
/// bounded, and this side is draining anyway — nothing new is routed to the
/// plugin while the caller decides what to do about its host.
#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_unload_is_bounded_and_this_side_still_drains() {
    let Some(node) = node() else {
        eprintln!("skipping: set REBON_TEST_NODE");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let supervisor = deaf(
        &node,
        dir.path(),
        &["plugin/unload"],
        Duration::from_secs(2),
    )
    .await;

    let started = Instant::now();
    let error = supervisor
        .unload_plugin_within(ID, Duration::from_millis(300))
        .await
        .expect_err("nobody answered the unload");
    assert!(started.elapsed() < Duration::from_secs(3), "bounded");
    assert!(
        error
            .to_string()
            .starts_with(rebon_plugin_supervisor::HOST_UNANSWERED_CODE),
        "{error}"
    );
    let refused = supervisor
        .call_service(
            ID,
            "session-1",
            "count",
            Payload::from(serde_json::json!({"n": 1})),
        )
        .await
        .expect_err("a draining plugin takes no new calls");
    assert!(
        refused.to_string().contains("[STALE_PROVIDER]"),
        "{refused}"
    );
    supervisor.terminate().await;
}
