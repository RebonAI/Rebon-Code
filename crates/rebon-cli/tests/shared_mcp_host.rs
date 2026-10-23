//! The shared MCP host as sessions meet it: the real `rebon __mcp-host`,
//! started by the first session that needs a server, used by the next, and
//! gone once nobody does.
//!
//! One test in this binary on purpose: it points the process's environment at
//! a host directory of its own.

use std::path::PathBuf;
use std::time::Duration;

use rebon_plugin_mcp::shared_host::{self, HostLauncher};
use rebon_plugin_mcp::{StdioMcpClient, StdioServerConfig};
use rebon_tool::{McpClient, McpToolCall};
use serde_json::{json, Value};

const SERVER: &str = r#"
import json, os, sys
for line in sys.stdin:
    msg = json.loads(line)
    method, mid = msg.get("method"), msg.get("id")
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                  "serverInfo": {"name": "who", "version": "1"}}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": json.dumps({"pid": os.getpid()})}]}
    elif mid is not None:
        result = {}
    else:
        continue
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()
"#;

fn python() -> Option<String> {
    for candidate in ["python3", "python", "py"] {
        let Ok(output) = std::process::Command::new(candidate)
            .args(["-c", "import sys; print(sys.executable)"])
            .output()
        else {
            continue;
        };
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if output.status.success() && std::path::Path::new(&path).is_file() {
            return Some(path);
        }
    }
    None
}

async fn pid_of(session: &StdioMcpClient) -> u64 {
    let result = session
        .call_tool(McpToolCall {
            server: "who".into(),
            name: "who".into(),
            arguments: json!({}),
        })
        .await
        .expect("tool call");
    let text = result.content[0]["text"].as_str().unwrap();
    serde_json::from_str::<Value>(text).unwrap()["pid"]
        .as_u64()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_session_starts_the_host_and_the_next_one_shares_it() {
    let Some(python) = python() else {
        eprintln!("skipping: python not installed");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("who.py");
    std::fs::write(&script, SERVER).unwrap();
    let host_dir = dir.path().join("host");
    std::env::set_var("REBON_MCP_HOST_DIR", &host_dir);
    std::env::set_var("REBON_MCP_HOST_IDLE_SECONDS", "1");
    std::env::set_var("REBON_MCP_SERVER_GRACE_SECONDS", "1");
    std::env::remove_var("REBON_MCP_NO_SHARE");
    shared_host::install_launcher(HostLauncher {
        program: PathBuf::from(env!("CARGO_BIN_EXE_rebon")),
        args: vec!["__mcp-host".into()],
    });
    let config = StdioServerConfig::new("who", python, vec![script.to_string_lossy().into_owned()]);

    let first = StdioMcpClient::new();
    first
        .add_server(config.clone())
        .await
        .expect("first session");
    let second = StdioMcpClient::new();
    second.add_server(config).await.expect("second session");
    let pid = pid_of(&first).await;
    assert_eq!(
        pid_of(&second).await,
        pid,
        "the second session shares the process"
    );
    assert_ne!(pid, u64::from(std::process::id()));

    let listed = tokio::task::spawn_blocking(shared_host::list_shared_servers)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].sessions, 2);
    assert_eq!(listed[0].pid.map(u64::from), Some(pid));

    first.shutdown().await;
    second.shutdown().await;
    // Grace, then idle: the server goes and the host after it.
    let info = host_dir.join("host-v1.json");
    let mut gone = false;
    for _ in 0..200 {
        if !info.exists() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "the host outlived its last server");
}
