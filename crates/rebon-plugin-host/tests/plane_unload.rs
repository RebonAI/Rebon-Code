//! How an unload ends, on a real plane.
//!
//! `unload_entry` withdraws a plugin only once its in-flight calls have
//! finished, and one deadline covers both asking the host and waiting for the
//! drain. Past it the two kinds of host part ways: a contained plugin is ended
//! with its whole container (`Forced`, the container's other plugins with it);
//! one on the shared host, which cannot be ended for it, is withdrawn and its
//! id refused until restart (`Stuck`). Every load attempt opens a run before
//! the load is sent, and a run's terminal state is announced only once it is
//! true.
//!
//! Skips without `REBON_TEST_NODE`, like every test that drives a real child.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rebon_kernel::{Context, Kernel, LifecycleSink, PluginLifecycleChanged, SharedLifecycleSink};
use rebon_kernel_seats::kernel_compose_tools::ComposeToolRegistry;
use rebon_kernel_seats::kernel_prompt_sections::{ComposePromptSections, SYSTEM_PROMPT_SERVICE};
use rebon_plugin_host::container::ContainerSpec;
use rebon_plugin_host::plugin_plane::{
    default_exposed_seats, ComposeEntry, ComposeNode, PluginLifecycle, PluginPlane,
    PluginPlaneConfig, UnloadOutcome,
};
use rebon_plugin_protocol::Payload;
use rebon_plugin_supervisor::{
    ToolInvocation, ToolInvoker, ToolRefusal, HOST_UNANSWERED_CODE, PLUGIN_STUCK_CODE,
};
use serde_json::{json, Value};

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

fn plain(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/")
}

struct NoTools;

impl ToolInvoker for NoTools {
    fn invoke(
        &self,
        invocation: ToolInvocation,
    ) -> Pin<Box<dyn Future<Output = Result<Payload, ToolRefusal>> + Send + '_>> {
        Box::pin(async move {
            Err(ToolRefusal::new(
                "[NO_TOOLS]",
                format!("this fixture serves no tool ({})", invocation.tool),
            ))
        })
    }
}

/// A plugin in rebon's own shape with one service that sleeps for as long as
/// it is asked to.
const SLOW_PLUGIN: &str = r#"
export function activate(plugin) {
  plugin.service('sleep', async (request) => {
    await new Promise((resolve) => setTimeout(resolve, request?.ms ?? 0));
    return { slept: request?.ms ?? 0 };
  });
}
"#;

const ID: &str = "slow";
const SIBLING: &str = "slow-sibling";

fn package(into: &Path) -> String {
    let root = into.join("slow-plugin");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("index.mjs"), SLOW_PLUGIN).unwrap();
    plain(&root)
}

fn entry_named(id: &str, root: &str) -> ComposeEntry {
    ComposeEntry {
        id: id.into(),
        root: root.into(),
        entry: "index.mjs".into(),
        services: vec!["sleep".into()],
        ..ComposeEntry::default()
    }
}

fn entry(root: &str) -> ComposeEntry {
    entry_named(ID, root)
}

/// `id` in the one container every contained entry here shares.
fn contained_named(id: &str, root: &str, data: &Path) -> ComposeEntry {
    let mut entry = entry_named(id, root);
    entry.container = Some(ContainerSpec {
        id: "slow-box".into(),
        read: vec![root.into()],
        data_dir: plain(data),
        network: Vec::new(),
        env: Vec::new(),
    });
    entry
}

fn contained(root: &str, data: &Path) -> ComposeEntry {
    contained_named(ID, root, data)
}

/// A host script that is the real host with some of what rebon sends it
/// dropped on the floor, so a test can have a host that never answers one
/// method and behaves normally otherwise.
fn deaf_host(into: &Path, methods: &[&str]) -> PathBuf {
    let real = repo().join("runtimes/node/plugin-host/src/cli.mjs");
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
            real = serde_json::to_string(&plain(&real)).unwrap(),
        ),
    )
    .unwrap();
    script
}

/// What a test is handed: the plane, the context its events go out on, and a
/// scratch directory.
struct Rig {
    plane: Arc<PluginPlane>,
    ctx: Context,
    tmp: PathBuf,
}

/// Runs `body` against a fresh plane whose drains give up after `deadline`,
/// on a host that drops `host`'s methods (the real host when `None`), and
/// shuts the plane down whatever the body does.
async fn with_plane_on<F, Fut>(
    deadline: Duration,
    host: Option<&[&str]>,
    sink: Option<SharedLifecycleSink>,
    body: F,
) where
    F: FnOnce(Rig) -> Fut,
    Fut: Future<Output = ()>,
{
    let Some(node) = node() else { return };
    let repo = repo();
    let tmp = tempfile::tempdir().expect("temp dir");
    let host_script = match host {
        Some(dropped) => deaf_host(tmp.path(), dropped),
        None => repo.join("runtimes/node/plugin-host/src/cli.mjs"),
    };

    let kernel = Kernel::new();
    let ctx = kernel.context().fork_scoped("plane");
    let registry = ComposeToolRegistry::new(Vec::<String>::new());
    ctx.provide_json("tool-registry", registry.clone())
        .expect("the composition tool seat provides");
    ctx.provide_json(SYSTEM_PROMPT_SERVICE, ComposePromptSections::new())
        .expect("the prompt section seat provides");

    let plane = PluginPlane::start(
        PluginPlaneConfig {
            node,
            host_script: PathBuf::from(plain(&host_script)),
            loader: PathBuf::from(plain(
                &repo.join("runtimes/node/compose-runtime/src/index.mjs"),
            )),
            compose_root: PathBuf::from(plain(&repo.join("runtimes/node/compose-runtime"))),
            payload_dir: Some(PathBuf::from(plain(
                &repo.join("runtimes/node/compose-runtime/payload"),
            ))),
            structure: vec![
                ComposeNode {
                    id: ID.into(),
                    ..Default::default()
                },
                ComposeNode {
                    id: SIBLING.into(),
                    ..Default::default()
                },
            ],
            web: Value::Null,
            modules: BTreeMap::new(),
            exposed_tools: Vec::new(),
            exposed_seats: default_exposed_seats(),
            tool_catalog: json!([]),
            scope_id: None,
            working_directory: repo.clone(),
            unary_call_timeout: Some(Duration::from_secs(10)),
            drain_deadline: Some(deadline),
            lifecycle_sink: sink,
        },
        ctx.clone(),
        registry,
        Arc::new(NoTools) as Arc<dyn ToolInvoker>,
    )
    .await
    .expect("the plugin plane starts");

    let rig = Rig {
        plane: Arc::clone(&plane),
        ctx,
        tmp: tmp.path().to_path_buf(),
    };
    let outcome =
        futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(body(rig))).await;
    plane.shutdown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn with_plane<F, Fut>(deadline: Duration, body: F)
where
    F: FnOnce(Rig) -> Fut,
    Fut: Future<Output = ()>,
{
    with_plane_on(deadline, None, None, body).await
}

/// Starts a `sleep` call of `ms` on `id` and returns once the plugin is
/// counting it.
async fn sleeping(
    plane: &Arc<PluginPlane>,
    id: &str,
    ms: u64,
) -> tokio::task::JoinHandle<Result<Value, rebon_plugin_supervisor::HostCallError>> {
    let caller = Arc::clone(plane);
    let target = id.to_owned();
    let call = tokio::spawn(async move {
        let scope = caller.scope().to_owned();
        caller
            .call_service(&target, &scope, "sleep", json!({ "ms": ms }))
            .await
    });
    for _ in 0..200 {
        if !plane.hosts().for_plugin(id).in_flight(id).await.is_empty() {
            return call;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the sleep call never started");
}

fn state_of(plane: &PluginPlane, id: &str) -> PluginLifecycle {
    plane.lifecycle(id).expect("the plane has a record").state
}

fn state(plane: &PluginPlane) -> PluginLifecycle {
    state_of(plane, ID)
}

fn generation_of(plane: &PluginPlane, id: &str) -> u64 {
    plane
        .lifecycle(id)
        .expect("the plane has a record")
        .incarnation
        .generation
}

fn generation(plane: &PluginPlane) -> u64 {
    generation_of(plane, ID)
}

/// Nothing running: unloaded at once, and the next load is a new run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quiet_unload_is_clean_and_the_next_load_is_a_new_run() {
    with_plane(
        Duration::from_secs(5),
        |Rig { plane, tmp, .. }| async move {
            let root = package(&tmp);
            assert!(plane.lifecycle(ID).is_none(), "nothing tried yet");

            plane.load_entry(&entry(&root)).await.expect("loads");
            assert_eq!(state(&plane), PluginLifecycle::Ready);
            assert_eq!(generation(&plane), 1);

            assert_eq!(plane.unload_entry(ID).await, UnloadOutcome::Clean);
            assert_eq!(state(&plane), PluginLifecycle::Unloaded);
            assert_eq!(
                generation(&plane),
                1,
                "an unload ends a run, it starts none"
            );
            assert!(plane.registered_services(ID).is_empty());

            plane.load_entry(&entry(&root)).await.expect("loads again");
            assert_eq!(state(&plane), PluginLifecycle::Ready);
            assert_eq!(generation(&plane), 2);
        },
    )
    .await;
}

/// A run says where its package came from: the entry's recorded source is on
/// every fact about the run, from the first one on, and an entry with none
/// gives runs with none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_carries_its_entry_source() {
    with_plane(
        Duration::from_secs(5),
        |Rig { plane, ctx, tmp }| async move {
            let seen = Arc::new(Mutex::new(Vec::new()));
            {
                let seen = Arc::clone(&seen);
                ctx.on::<PluginLifecycleChanged>(move |event| {
                    if event.incarnation.plugin_id == ID {
                        seen.lock().unwrap().push(event.incarnation.source.clone());
                    }
                });
            }
            let root = package(&tmp);
            let mut sourced = entry(&root);
            sourced.source = Some(r#"{"kind":"local","path":"/origin"}"#.into());
            plane.load_entry(&sourced).await.expect("loads");
            plane.unload_entry(ID).await;
            plane.load_entry(&entry(&root)).await.expect("loads again");

            // Loading, Ready, Draining and Unloaded of the first run; Loading
            // and Ready of the second.
            let mut expected = vec![sourced.source.clone(); 4];
            expected.extend([None, None]);
            assert_eq!(*seen.lock().unwrap(), expected);
            assert_eq!(plane.lifecycle(ID).unwrap().incarnation.source, None);
        },
    )
    .await;
}

/// A call in flight inside the deadline is waited on, and still answered by
/// the run it was made to.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unload_waits_for_the_call_in_flight() {
    with_plane(
        Duration::from_secs(10),
        |Rig { plane, tmp, .. }| async move {
            let root = package(&tmp);
            plane.load_entry(&entry(&root)).await.expect("loads");
            let call = sleeping(&plane, ID, 800).await;

            let started = Instant::now();
            assert_eq!(plane.unload_entry(ID).await, UnloadOutcome::Clean);
            assert!(
                started.elapsed() >= Duration::from_millis(300),
                "the unload waited for the call rather than withdrawing under it"
            );
            let answer = call.await.unwrap().expect("the call is answered");
            assert_eq!(answer["slept"], json!(800));
            assert_eq!(state(&plane), PluginLifecycle::Unloaded);
        },
    )
    .await;
}

/// The shared host cannot be ended for one plugin: past the deadline the
/// plugin is withdrawn, marked stuck, and its id is refused with a reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shared_host_plugin_that_will_not_drain_is_stuck() {
    with_plane(
        Duration::from_millis(300),
        |Rig { plane, tmp, .. }| async move {
            let root = package(&tmp);
            plane.load_entry(&entry(&root)).await.expect("loads");
            let call = sleeping(&plane, ID, 3_000).await;

            let outcome = plane.unload_entry(ID).await;
            let UnloadOutcome::Stuck { outstanding, .. } = outcome else {
                panic!("expected stuck, got {outcome:?}");
            };
            assert_eq!(outstanding.len(), 1);
            assert!(matches!(state(&plane), PluginLifecycle::Stuck { .. }));
            assert!(plane.registered_services(ID).is_empty(), "withdrawn anyway");
            assert!(!plane.loaded().contains(&ID.to_string()));

            let refused = plane.load_entry(&entry(&root)).await.unwrap_err();
            assert!(
                refused.to_string().starts_with(PLUGIN_STUCK_CODE),
                "{refused}"
            );
            // Refusing is not an attempt: the stuck run keeps its record.
            assert!(matches!(state(&plane), PluginLifecycle::Stuck { .. }));
            assert_eq!(generation(&plane), 1);

            // The old run was never cut off: it still answers the call it had.
            assert!(call.await.unwrap().is_ok());
        },
    )
    .await;
}

/// A host that takes `plugin/unload` and never answers is bounded by the same
/// deadline as the drain. On the shared host that is a stuck plugin — said
/// with the reason, and withdrawn — not an unload that hangs or one that
/// leaves its registrations behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unanswered_unload_on_the_shared_host_is_stuck_and_withdrawn() {
    with_plane_on(
        Duration::from_millis(500),
        Some(&["plugin/unload"]),
        None,
        |Rig { plane, tmp, .. }| async move {
            let root = package(&tmp);
            plane.load_entry(&entry(&root)).await.expect("loads");
            assert!(!plane.registered_services(ID).is_empty());

            let started = Instant::now();
            let outcome = plane.unload_entry(ID).await;
            assert!(started.elapsed() < Duration::from_secs(5), "bounded");
            let UnloadOutcome::Stuck { reason, .. } = outcome else {
                panic!("expected stuck, got {outcome:?}");
            };
            assert!(reason.contains(HOST_UNANSWERED_CODE), "{reason}");
            assert!(plane.registered_services(ID).is_empty(), "withdrawn");
            let PluginLifecycle::Stuck { reason, .. } = state(&plane) else {
                panic!("expected stuck, got {:?}", state(&plane));
            };
            assert!(reason.contains(HOST_UNANSWERED_CODE), "{reason}");
        },
    )
    .await;
}

/// A host that died runs nothing, so unloading a plugin on it is clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unload_on_a_dead_host_is_clean() {
    with_plane(
        Duration::from_secs(5),
        |Rig { plane, tmp, .. }| async move {
            let root = package(&tmp);
            plane.load_entry(&entry(&root)).await.expect("loads");
            plane.hosts().main().terminate().await;

            assert_eq!(plane.unload_entry(ID).await, UnloadOutcome::Clean);
            assert_eq!(state(&plane), PluginLifecycle::Unloaded);
            assert!(plane.registered_services(ID).is_empty());
        },
    )
    .await;
}

/// A contained plugin can be ended: past the deadline it goes with its
/// container, and its id is free for the next run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_contained_plugin_that_will_not_drain_is_forced_out() {
    with_plane(
        Duration::from_millis(300),
        |Rig { plane, tmp, .. }| async move {
            let root = package(&tmp);
            let data = tmp.join("data/slow");
            plane
                .load_entry(&contained(&root, &data))
                .await
                .expect("loads in its container");
            assert_eq!(
                plane.containers().await.get("slow-box"),
                Some(&vec![ID.to_string()])
            );
            let call = sleeping(&plane, ID, 5_000).await;

            let outcome = plane.unload_entry(ID).await;
            let UnloadOutcome::Forced {
                outstanding,
                taken_down,
                ..
            } = outcome
            else {
                panic!("expected forced, got {outcome:?}");
            };
            assert_eq!(outstanding.len(), 1);
            assert!(taken_down.is_empty(), "it was alone in its container");
            assert!(matches!(state(&plane), PluginLifecycle::Forced { .. }));
            assert!(!plane.containers().await.contains_key("slow-box"));

            // The container was ended, which ends the call: promptly, not after
            // the five seconds it asked for.
            let ended = tokio::time::timeout(Duration::from_secs(3), call)
                .await
                .expect("the call ends with its container")
                .unwrap();
            assert!(ended.is_err(), "nothing answered it: {ended:?}");

            plane
                .load_entry(&contained(&root, &data))
                .await
                .expect("a forced plugin's id is free again");
            assert_eq!(state(&plane), PluginLifecycle::Ready);
            assert_eq!(generation(&plane), 2);
        },
    )
    .await;
}

/// A container is one process. Ending it for one plugin ends every plugin in
/// it, and each of them is recorded as forced, naming why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forcing_a_shared_container_takes_its_other_plugins_with_it() {
    with_plane(
        Duration::from_millis(300),
        |Rig { plane, tmp, .. }| async move {
            let root = package(&tmp);
            let data = tmp.join("data/slow");
            plane
                .load_entry(&contained(&root, &data))
                .await
                .expect("the first loads");
            plane
                .load_entry(&contained_named(SIBLING, &root, &data))
                .await
                .expect("the second joins the same container");
            let mut members = plane.containers().await["slow-box"].clone();
            members.sort();
            assert_eq!(members, vec![ID.to_string(), SIBLING.to_string()]);
            let _call = sleeping(&plane, ID, 5_000).await;

            let outcome = plane.unload_entry(ID).await;
            let UnloadOutcome::Forced { taken_down, .. } = outcome else {
                panic!("expected forced, got {outcome:?}");
            };
            assert_eq!(taken_down, vec![SIBLING.to_string()]);
            assert!(!plane.containers().await.contains_key("slow-box"));
            let PluginLifecycle::Forced { reason, .. } = state_of(&plane, SIBLING) else {
                panic!("the sibling was forced too");
            };
            assert!(
                reason.contains(&format!("because {ID} had to be ended: ")),
                "{reason}"
            );
            assert!(plane.registered_services(SIBLING).is_empty());
            assert!(!plane.loaded().contains(&SIBLING.to_string()));
        },
    )
    .await;
}

/// A reload that forces a container brings back the plugins in it that the
/// composition still wants, in a new container.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reload_restarts_what_it_took_down_and_still_wants() {
    with_plane(
        Duration::from_millis(300),
        |Rig { plane, tmp, .. }| async move {
            let root = package(&tmp);
            let data = tmp.join("data/slow");
            let both = [
                contained(&root, &data),
                contained_named(SIBLING, &root, &data),
            ];
            let up = plane.reload(&both).await.unwrap();
            assert!(up.failed.is_empty(), "{:?}", up.failed);
            let _call = sleeping(&plane, ID, 5_000).await;

            let down = plane
                .reload(&[contained_named(SIBLING, &root, &data)])
                .await
                .unwrap();
            assert_eq!(down.removed, vec![ID.to_string()]);
            let mut forced = down.forced.clone();
            forced.sort();
            assert_eq!(forced, vec![ID.to_string(), SIBLING.to_string()]);
            assert!(down.failed.is_empty(), "{:?}", down.failed);
            // Restarted, so not left running untouched.
            assert!(down.unchanged.is_empty(), "{:?}", down.unchanged);

            assert_eq!(state_of(&plane, SIBLING), PluginLifecycle::Ready);
            assert_eq!(generation_of(&plane, SIBLING), 2, "a new run");
            assert_eq!(
                plane.containers().await.get("slow-box"),
                Some(&vec![SIBLING.to_string()])
            );
        },
    )
    .await;
}

/// A reload says which entries it left stuck, and a stuck entry coming back
/// fails with the reason rather than as "already loaded".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reload_reports_what_did_not_drain() {
    with_plane(
        Duration::from_millis(300),
        |Rig { plane, tmp, .. }| async move {
            let root = package(&tmp);
            let up = plane.reload(&[entry(&root)]).await.unwrap();
            assert_eq!(up.added, vec![ID.to_string()]);
            let _call = sleeping(&plane, ID, 3_000).await;

            let down = plane.reload(&[]).await.unwrap();
            assert_eq!(down.removed, vec![ID.to_string()]);
            assert_eq!(down.stuck, vec![ID.to_string()]);
            assert!(down.forced.is_empty());
            assert!(down.failed.is_empty(), "stuck is not a failed unload");

            let back = plane.reload(&[entry(&root)]).await.unwrap();
            let (failed, why) = back.failed.first().expect("the stuck id cannot load");
            assert_eq!(failed, ID);
            assert!(why.contains(PLUGIN_STUCK_CODE), "{why}");
        },
    )
    .await;
}

/// A load that fails is a run too: recorded with its reason, and the next
/// attempt is the next generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_load_is_recorded_as_an_attempt() {
    with_plane(
        Duration::from_secs(5),
        |Rig { plane, tmp, .. }| async move {
            let root = package(&tmp);
            let mut broken = entry(&root);
            broken.entry = "missing.mjs".into();
            assert!(plane.load_entry(&broken).await.is_err());
            let PluginLifecycle::Failed { reason } = state(&plane) else {
                panic!("expected failed, got {:?}", state(&plane));
            };
            assert!(!reason.is_empty());
            assert_eq!(generation(&plane), 1);

            plane
                .load_entry(&entry(&root))
                .await
                .expect("the fixed entry loads");
            assert_eq!(state(&plane), PluginLifecycle::Ready);
            assert_eq!(generation(&plane), 2);
        },
    )
    .await;
}

/// What a listener on the kernel's event plane sees, and what was true at the
/// moment it saw it.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    generation: u64,
    from: Option<PluginLifecycle>,
    to: PluginLifecycle,
    /// Whether the plugin still had registrations on rebon's seats.
    registered: bool,
}

/// A run opens before its load is sent, moves through its states one event at
/// a time, and announces its ending only after its registrations are gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_change_is_announced_and_an_ending_only_once_true() {
    with_plane(
        Duration::from_secs(5),
        |Rig { plane, ctx, tmp }| async move {
            let seen = Arc::new(Mutex::new(Vec::<Seen>::new()));
            {
                let seen = Arc::clone(&seen);
                let watched = Arc::clone(&plane);
                ctx.on::<PluginLifecycleChanged>(move |event| {
                    if event.incarnation.plugin_id != ID {
                        return;
                    }
                    seen.lock().unwrap().push(Seen {
                        generation: event.incarnation.generation,
                        from: event.from.clone(),
                        to: event.to.clone(),
                        registered: !watched.registered_services(ID).is_empty(),
                    });
                });
            }
            let root = package(&tmp);
            plane.load_entry(&entry(&root)).await.expect("loads");
            plane.unload_entry(ID).await;
            let mut broken = entry(&root);
            broken.entry = "missing.mjs".into();
            assert!(plane.load_entry(&broken).await.is_err());

            let seen = seen.lock().unwrap().clone();
            let steps: Vec<_> = seen
                .iter()
                .map(|s| (s.generation, s.from.clone(), s.to.clone()))
                .collect();
            assert_eq!(
                steps[..4],
                [
                    (1, None, PluginLifecycle::Loading),
                    (1, Some(PluginLifecycle::Loading), PluginLifecycle::Ready),
                    (1, Some(PluginLifecycle::Ready), PluginLifecycle::Draining),
                    (
                        1,
                        Some(PluginLifecycle::Draining),
                        PluginLifecycle::Unloaded
                    ),
                ]
            );
            // Loading is announced before anything is registered; Ready and
            // Draining while the registrations are up; Unloaded after they
            // are gone.
            assert_eq!(
                seen[..4].iter().map(|s| s.registered).collect::<Vec<_>>(),
                vec![false, true, true, false]
            );
            assert_eq!(steps[4], (2, None, PluginLifecycle::Loading));
            assert!(matches!(
                steps[5],
                (
                    2,
                    Some(PluginLifecycle::Loading),
                    PluginLifecycle::Failed { .. }
                )
            ));
            assert_eq!(steps.len(), 6);
        },
    )
    .await;
}

/// Two plugins in one container, both being removed in one reload. The first
/// one forced ends the container and the second with it; the second's turn in
/// the same loop must not rewrite that ending as a clean drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sibling_forced_earlier_in_the_same_reload_stays_forced() {
    with_plane(
        Duration::from_millis(300),
        |Rig { plane, ctx, tmp }| async move {
            let root = package(&tmp);
            let data = tmp.join("data/slow");
            // Removals go in reverse id order, so `SIBLING` goes first.
            let up = plane
                .reload(&[
                    contained_named(SIBLING, &root, &data),
                    contained(&root, &data),
                ])
                .await
                .unwrap();
            assert!(up.failed.is_empty(), "{:?}", up.failed);
            let first_events = Arc::new(Mutex::new(Vec::new()));
            {
                let first_events = Arc::clone(&first_events);
                ctx.on::<PluginLifecycleChanged>(move |event| {
                    if event.incarnation.plugin_id == ID {
                        first_events.lock().unwrap().push(event.to.clone());
                    }
                });
            }
            let _call = sleeping(&plane, SIBLING, 5_000).await;

            let down = plane.reload(&[]).await.unwrap();
            let mut forced = down.forced.clone();
            forced.sort();
            assert_eq!(forced, vec![ID.to_string(), SIBLING.to_string()]);
            assert!(matches!(
                state_of(&plane, ID),
                PluginLifecycle::Forced { .. }
            ));
            let seen = first_events.lock().unwrap().clone();
            assert_eq!(seen.len(), 1, "one ending, not a second one: {seen:?}");
            assert!(matches!(seen[0], PluginLifecycle::Forced { .. }));
        },
    )
    .await;
}

/// A host that died takes every plugin on it down. Unloading one of them is
/// clean — nothing of it runs — and the others are recorded as failed rather
/// than left reading as ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_container_fails_its_other_plugins() {
    with_plane(
        Duration::from_secs(5),
        |Rig { plane, tmp, .. }| async move {
            let root = package(&tmp);
            let data = tmp.join("data/slow");
            plane
                .load_entry(&contained(&root, &data))
                .await
                .expect("loads");
            plane
                .load_entry(&contained_named(SIBLING, &root, &data))
                .await
                .expect("joins");
            plane.hosts().for_plugin(ID).terminate().await;

            assert_eq!(plane.unload_entry(ID).await, UnloadOutcome::Clean);
            assert_eq!(state(&plane), PluginLifecycle::Unloaded);
            let PluginLifecycle::Failed { reason } = state_of(&plane, SIBLING) else {
                panic!("expected failed, got {:?}", state_of(&plane, SIBLING));
            };
            assert!(reason.contains("host"), "{reason}");
            assert!(plane.registered_services(SIBLING).is_empty());
            assert!(plane.containers().await.is_empty());
        },
    )
    .await;
}

/// A reload looks for hosts that died before it diffs, so the plugins that
/// were on one come back up — as new runs — rather than being left for dead
/// behind a table that still says they are loaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reload_restarts_plugins_whose_host_died() {
    with_plane(
        Duration::from_secs(5),
        |Rig { plane, tmp, .. }| async move {
            let root = package(&tmp);
            let data = tmp.join("data/slow");
            let both = [
                contained(&root, &data),
                contained_named(SIBLING, &root, &data),
            ];
            plane.reload(&both).await.unwrap();
            plane.hosts().for_plugin(ID).terminate().await;

            let again = plane.reload(&both).await.unwrap();
            assert!(again.failed.is_empty(), "{:?}", again.failed);
            let mut added = again.added.clone();
            added.sort();
            assert_eq!(added, vec![ID.to_string(), SIBLING.to_string()]);
            for id in [ID, SIBLING] {
                assert_eq!(state_of(&plane, id), PluginLifecycle::Ready);
                assert_eq!(generation_of(&plane, id), 2);
            }
        },
    )
    .await;
}

/// Every host the plane spawns gets an epoch of its own, and none is the
/// default every host used to share.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_host_has_its_own_epoch() {
    with_plane(
        Duration::from_secs(5),
        |Rig { plane, tmp, .. }| async move {
            let root = package(&tmp);
            let data = tmp.join("data/slow");
            plane
                .load_entry(&entry_named(SIBLING, &root))
                .await
                .unwrap();
            plane.load_entry(&contained(&root, &data)).await.unwrap();
            let shared = plane.lifecycle(SIBLING).unwrap().incarnation.host_epoch;
            let boxed = plane.lifecycle(ID).unwrap().incarnation.host_epoch;
            assert_ne!(shared, boxed);
            assert_ne!(shared, 1);
            assert_ne!(boxed, 1);
            assert!(
                shared < (1 << 53) && boxed < (1 << 53),
                "safe JSON integers"
            );
        },
    )
    .await;
}

/// A sink that records what it is handed and what the plane's table said at
/// that moment, and refuses whichever state it is told to.
struct RecordingSink {
    plane: std::sync::OnceLock<std::sync::Weak<PluginPlane>>,
    refuse: Mutex<Option<&'static str>>,
    seen: Mutex<Vec<(PluginLifecycle, Option<PluginLifecycle>)>>,
}

impl LifecycleSink for RecordingSink {
    fn commit(&self, fact: &PluginLifecycleChanged) -> Result<(), String> {
        let table = self
            .plane
            .get()
            .and_then(std::sync::Weak::upgrade)
            .and_then(|plane| plane.lifecycle(&fact.incarnation.plugin_id))
            .map(|record| record.state);
        self.seen.lock().unwrap().push((fact.to.clone(), table));
        let phase = serde_json::to_value(&fact.to).unwrap()["phase"]
            .as_str()
            .unwrap()
            .to_owned();
        match *self.refuse.lock().unwrap() {
            Some(refused) if refused == phase => Err(format!("refusing {phase}")),
            _ => Ok(()),
        }
    }
}

/// A fact reaches the sink before the table changes. A start the sink
/// refuses is not started and costs no generation; a change that already
/// happened is kept, announced, and the refusal is reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn facts_are_committed_first_and_a_refusal_is_handled_by_kind() {
    let sink = Arc::new(RecordingSink {
        plane: std::sync::OnceLock::new(),
        refuse: Mutex::new(Some("loading")),
        seen: Mutex::new(Vec::new()),
    });
    let shared = SharedLifecycleSink(Arc::clone(&sink) as Arc<dyn LifecycleSink>);
    let watched = Arc::clone(&sink);
    with_plane_on(
        Duration::from_secs(5),
        None,
        Some(shared),
        |Rig { plane, tmp, .. }| async move {
            let _ = watched.plane.set(Arc::downgrade(&plane));
            let root = package(&tmp);

            let refused = plane.load_entry(&entry(&root)).await.unwrap_err();
            assert!(
                refused
                    .to_string()
                    .starts_with(rebon_plugin_supervisor::LIFECYCLE_UNRECORDED_CODE),
                "{refused}"
            );
            assert!(plane.lifecycle(ID).is_none(), "no run was opened");
            assert!(plane.registered_services(ID).is_empty(), "nothing loaded");

            *watched.refuse.lock().unwrap() = Some("unloaded");
            plane.load_entry(&entry(&root)).await.expect("loads now");
            assert_eq!(generation(&plane), 1, "the refused start cost nothing");
            assert_eq!(plane.unload_entry(ID).await, UnloadOutcome::Clean);
            assert_eq!(
                state(&plane),
                PluginLifecycle::Unloaded,
                "an unload that happened stays recorded"
            );
            assert_eq!(
                plane.lifecycle_sink_error().as_deref(),
                Some("refusing unloaded")
            );

            // Each commit saw the table as it was before that change.
            let seen = watched.seen.lock().unwrap().clone();
            assert_eq!(
                seen,
                vec![
                    (PluginLifecycle::Loading, None),
                    (PluginLifecycle::Loading, None),
                    (PluginLifecycle::Ready, Some(PluginLifecycle::Loading)),
                    (PluginLifecycle::Draining, Some(PluginLifecycle::Ready)),
                    (PluginLifecycle::Unloaded, Some(PluginLifecycle::Draining)),
                ]
            );
        },
    )
    .await;
}
