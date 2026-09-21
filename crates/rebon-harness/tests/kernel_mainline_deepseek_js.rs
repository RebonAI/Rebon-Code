//! Production-shape acceptance for a plugin model route: nothing but
//! user-facing configuration drives the whole chain. `config.json` names
//! the composition (`kernelPlugins` with the real llm-deepseek plugin and a
//! `credentialGrants` entry) and the active provider (`deepseek-official`,
//! which no `customProviders` entry knows), and `resolve_runtime_model`
//! does the rest: native resolution fails, the kernel fallback boots the
//! composition, the grant authorizes the environment reference, and a full
//! turn streams against a local fake endpoint.
//!
//! Lives in its own test binary on purpose: `REBON_CONFIG_DIR` must be set
//! before the process kernel and the compose-host singleton first
//! initialize, and both are process-wide.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use rebon_api::{ContentBlockDelta, StopReason, StreamEvent};

/// Minimal HTTP/1.1 server speaking one canned chat-completions SSE reply.
async fn spawn_fake_deepseek_endpoint() -> (std::net::SocketAddr, Arc<Mutex<Option<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fake endpoint binds");
    let addr = listener.local_addr().expect("bound address");
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let capture = captured.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let capture = capture.clone();
            tokio::spawn(async move {
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 4096];
                let header_end = loop {
                    match socket.read(&mut tmp).await {
                        Ok(0) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(_) => return,
                    }
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.trim()
                            .eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                while buf.len() < header_end + content_length {
                    match socket.read(&mut tmp).await {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(_) => return,
                    }
                }
                *capture.lock().unwrap() = Some(String::from_utf8_lossy(&buf).to_string());

                let sse = concat!(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"配置直达\"}}]}\n\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n",
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
    (addr, captured)
}

/// The Node the plane runs on, and the checkout it loads from.
///
/// The composition this test boots is a real Node process now, so the test
/// needs the same three paths every other plane test needs. It used to boot an
/// isolate compiled into the binary and needed none of them.
fn point_at_checkout() -> bool {
    let Some(node) = std::env::var_os("REBON_TEST_NODE") else {
        assert_ne!(
            std::env::var_os("REBON_REQUIRE_TEST_NODE").as_deref(),
            Some(std::ffi::OsStr::new("1")),
            "REBON_REQUIRE_TEST_NODE=1 requires an absolute REBON_TEST_NODE"
        );
        eprintln!("skipping: set REBON_TEST_NODE to an absolute Node executable");
        return false;
    };
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
    true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn config_driven_mainline_serves_a_full_turn() {
    if !point_at_checkout() {
        return;
    }
    let (addr, captured) = spawn_fake_deepseek_endpoint().await;

    // The entire setup a real user performs: one config.json.
    let config_dir = tempfile::tempdir().expect("config dir");
    std::fs::write(
        config_dir.path().join("config.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "activeCustomProvider": "deepseek-official",
            "kernelPlugins": {
                "plugins": [
                    {
                        "id": "llm-deepseek",
                        "name": "@deepseek-ai/dsh-llm-deepseek",
                        "config": {
                            "apiKeyEnv": "REBON_TEST_MAINLINE_KEY",
                            "baseURL": format!("http://{addr}"),
                            "models": [
                                { "id": "deepseek-v4-flash", "contextWindow": 128000 },
                            ],
                        },
                    },
                ],
                "credentialGrants": ["REBON_TEST_MAINLINE_KEY"],
            },
        }))
        .expect("config renders"),
    )
    .expect("config written");
    std::env::set_var("REBON_CONFIG_DIR", config_dir.path());
    std::env::set_var("REBON_TEST_MAINLINE_KEY", "sk-mainline-key");

    // Main-session resolution: `deepseek-official` matches no
    // customProviders entry, so the kernel fallback boots the configured
    // composition and serves the runtime from the plugin route.
    let runtime = tokio::time::timeout(
        Duration::from_secs(30),
        rebon_harness::resolve_runtime_model(&rebon_harness::HarnessOverrides::default()),
    )
    .await
    .expect("resolution inside the timeout")
    .expect("config-driven kernel route resolves");
    assert_eq!(runtime.provider_name, "deepseek-official");
    assert_eq!(runtime.model, "deepseek-v4-flash");

    let mut stream = runtime
        .client
        .create_message_stream(rebon_api::CreateMessageRequest::simple(
            "deepseek-v4-flash",
            "你好",
        ))
        .await
        .expect("stream starts");
    let events = tokio::time::timeout(Duration::from_secs(30), async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.expect("turn streams without errors"));
        }
        events
    })
    .await
    .expect("turn finishes");

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ContentBlockDelta {
                delta: ContentBlockDelta::TextDelta { text },
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "配置直达");
    let delta = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::MessageDelta { delta } => Some(delta),
            _ => None,
        })
        .expect("message delta present");
    assert_eq!(delta.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(delta.usage.input_tokens, 7);

    // The wire request carried the granted key — proof the config grant
    // (not a test-local authorizer) opened the credentials seam.
    let request = captured
        .lock()
        .unwrap()
        .clone()
        .expect("endpoint saw the request");
    assert!(
        request
            .to_lowercase()
            .contains("authorization: bearer sk-mainline-key"),
        "granted bearer key missing:\n{request}"
    );

    // Stop what the resolution started. The composition is a real Node child
    // held in a process-global, and a multi-threaded runtime does not finish
    // dropping while a task is still reading that child's stdout — so without
    // this the test passes and then the process never exits.
    if let Some(plane) = rebon_plugin_host::plugin_boot::ensure_process_plugin_plane(
        &rebon_harness::kernel_bootstrap::process_plugin_registry(),
    )
    .await
    {
        plane.shutdown().await;
    }

    std::env::remove_var("REBON_TEST_MAINLINE_KEY");
    std::env::remove_var("REBON_CONFIG_DIR");
}
