//! A per-session agent loop, running on the plugin plane.
//!
//! `kernel_compose_agent_loop_js.rs` said over the new transport, and driven the
//! way rebon drives it: `LoopAgentSpec` in, `LoopHost` out, `kernelPlugins` read
//! from a real config file, the loop's model route served by the composition's
//! own adapter, and the session log arriving as published events.
//!
//! One loop, one host — the vendored dsh packages leave no choice, and the
//! module docs on `kernel_loop_plane` say why. The second test is the proof that
//! this is a real separation and not a happy accident.
//!
//! Skips without `REBON_TEST_NODE`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rebon_kernel::Kernel;
use rebon_kernel_seats::kernel_config_seats::{ConfigSeatsPlugin, CREDENTIALS_AUTHORIZE_EVENT};
use rebon_plugin_host::kernel_loop_plane::PlaneLoopHost;
use rebon_plugin_host::loop_host::{LoopAgentSpec, LoopHost};
use rebon_plugin_host::plugin_manifests::plain_path;
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

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root resolves")
}

/// Points `plane_paths` at this checkout, and hands it the Node the test runs
/// with. Process-global, so these tests run one at a time.
fn point_at_checkout(node: &std::path::Path) {
    let repo = repo();
    std::env::set_var("REBON_PLUGIN_NODE", node);
    std::env::set_var(
        "REBON_PLUGIN_HOST_JS",
        plain_path(&repo.join("runtimes/node/plugin-host/src/cli.mjs")),
    );
    std::env::set_var(
        "REBON_COMPOSE_LOADER_JS",
        plain_path(&repo.join("runtimes/node/compose-runtime/src/index.mjs")),
    );
    std::env::set_var(
        "REBON_KERNEL_JS_DIR",
        plain_path(&repo.join("runtimes/node/compose-runtime/payload")),
    );
}

/// One canned chat-completions reply per request, so a turn completes offline.
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
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"来了\"}}]}\n\n",
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

/// A config dir whose `kernelPlugins` names the composition's model adapter —
/// the ordinary way a user configures one.
fn config_dir(endpoint: std::net::SocketAddr) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("config dir");
    let config = serde_json::json!({
        "kernelPlugins": {
            "runtime": "node",
            "credentialGrants": ["REBON_TEST_LOOP_PLANE_KEY"],
            "plugins": [{
                "id": "llm-deepseek",
                "name": "@deepseek-ai/dsh-llm-deepseek",
                "config": {
                    "apiKeyEnv": "REBON_TEST_LOOP_PLANE_KEY",
                    "baseURL": format!("http://{endpoint}"),
                    "models": [{ "id": "deepseek-v4-flash", "name": "F", "contextWindow": 128000 }],
                },
            }],
        }
    });
    std::fs::write(
        dir.path().join("config.json"),
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .expect("config written");
    dir
}

fn spec(session: &str, config_dir: &std::path::Path) -> LoopAgentSpec {
    LoopAgentSpec {
        session_id: session.to_owned(),
        vendor: "dsh".into(),
        provider: "deepseek-official".into(),
        model: "deepseek-v4-flash".into(),
        workspace_root: repo(),
        config_dir: config_dir.to_path_buf(),
        prompt_section: None,
    }
}

fn kernel_with_seats(config_dir: &std::path::Path) -> Arc<Kernel> {
    let kernel = Kernel::new();
    kernel
        .load(vec![
            Box::new(ModelRouterPlugin),
            Box::new(ConfigSeatsPlugin::new(config_dir.to_path_buf())),
        ])
        .expect("kernel seats load");
    // The grant the composition's adapter needs. `credentialGrants` in the
    // config is the user's half; this is the authorizer that reads it.
    let authorizer = kernel.context().fork("authorizer");
    authorizer.wrap_json(CREDENTIALS_AUTHORIZE_EVENT, |payload, next| {
        if payload.get("env").and_then(|e| e.as_bool()) == Some(true) {
            serde_json::json!({ "allow": true })
        } else {
            next.call(payload)
        }
    });
    std::mem::forget(authorizer);
    kernel
}

/// Reads the loop's published session log until the turn ends.
async fn turn_end(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
) -> (Vec<String>, Value) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    let mut kinds = Vec::new();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), events.recv()).await {
            Ok(Some(event)) => {
                if let Some(kind) = event.get("type").and_then(Value::as_str) {
                    kinds.push(kind.to_string());
                    if kind == "turn/end" {
                        let data = event.get("data").cloned().unwrap_or(Value::Null);
                        return (kinds, data);
                    }
                }
            }
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    (kinds, Value::Null)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_loop_runs_a_turn_on_a_host_of_its_own() {
    let Some(node) = node() else { return };
    let endpoint = fake_deepseek().await;
    point_at_checkout(&node);
    std::env::set_var("REBON_TEST_LOOP_PLANE_KEY", "sk-loop-plane");
    let dir = config_dir(endpoint);
    let kernel = kernel_with_seats(dir.path());

    let host = PlaneLoopHost::spawn(kernel.context(), spec("session-a", dir.path()))
        .await
        .expect("the loop comes up on a host of its own");

    // dsh names an agent by the configured id plus the session it runs in.
    assert!(
        host.agent_id().starts_with("session-a"),
        "{}",
        host.agent_id()
    );
    let status = host.status().await.expect("the loop answers status");
    assert!(!status.is_empty(), "{status}");

    let mut events = host.take_events().expect("the event stream is taken once");
    host.followup("你好")
        .await
        .expect("the loop accepts a followup");
    let (kinds, ended) = turn_end(&mut events).await;
    assert!(
        kinds.iter().any(|k| k == "turn/end"),
        "the turn never ended; saw {kinds:?}"
    );
    assert!(kinds.iter().any(|k| k == "turn/start"), "{kinds:?}");
    // The model route the loop named was served by the composition's own
    // adapter, in the same host — so the turn completed rather than ending in
    // the loop's own NO_ADAPTER or MISSING_CREDENTIAL diagnosis.
    assert_eq!(
        ended
            .get("reason")
            .and_then(|r| r.get("kind"))
            .and_then(Value::as_str),
        Some("completed"),
        "{ended}"
    );

    host.shutdown();
    assert!(host.has_exited());
    assert!(
        host.status().await.is_err(),
        "a shut-down loop is unusable, not merely idle"
    );
    std::env::remove_var("REBON_TEST_LOOP_PLANE_KEY");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sessions_get_two_hosts_and_do_not_see_each_other() {
    let Some(node) = node() else { return };
    let endpoint = fake_deepseek().await;
    point_at_checkout(&node);
    std::env::set_var("REBON_TEST_LOOP_PLANE_KEY", "sk-loop-plane");
    let dir = config_dir(endpoint);
    let kernel = kernel_with_seats(dir.path());

    let first = PlaneLoopHost::spawn(kernel.context(), spec("session-a", dir.path()))
        .await
        .expect("the first loop comes up");
    let second = PlaneLoopHost::spawn(kernel.context(), spec("session-b", dir.path()))
        .await
        .expect("the second loop comes up on its own host");

    assert_ne!(first.agent_id(), second.agent_id());
    assert!(first.status().await.is_ok());
    assert!(second.status().await.is_ok());

    // Each loop hears only its own session log.
    let mut events = second.take_events().expect("the second stream is taken");
    first
        .followup("给 A 的")
        .await
        .expect("the first loop accepts a followup");
    assert!(
        tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .is_err(),
        "one session's turn must not reach another's stream"
    );

    // And taking one down leaves the other running.
    first.shutdown();
    assert!(second.status().await.is_ok());
    second.shutdown();

    let sink: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
    let _ = sink;
    std::env::remove_var("REBON_TEST_LOOP_PLANE_KEY");
}
