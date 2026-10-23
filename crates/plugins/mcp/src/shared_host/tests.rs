//! The host end to end: real sessions (`StdioMcpClient`) against a real host
//! running a small Python server that says which process it is.

use std::time::Duration;

use serde_json::{json, Value};

use super::client::{list_in, stop_in};
use super::test_support::TestHost;
use super::StopReason;
use crate::mcp_stdio::{StdioMcpClient, StdioServerConfig};
use rebon_tool::{McpClient, McpToolCall};

fn python() -> Option<String> {
    static PYTHON: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    PYTHON
        .get_or_init(|| {
            for candidate in ["python3", "python", "py"] {
                let Ok(output) = std::process::Command::new(candidate)
                    .args(["-c", "import sys; print(sys.executable)"])
                    .stderr(std::process::Stdio::null())
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
        })
        .clone()
}

const SERVER: &str = r#"
import json, os, sys, time

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method, mid = msg.get("method"), msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "who", "version": "1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [{"name": "who"}]}})
    elif method == "tools/call":
        params = msg.get("params", {})
        name, args = params.get("name"), params.get("arguments", {})
        if name == "exit":
            sys.exit(0)
        if name == "slow":
            time.sleep(float(args.get("seconds", 0.3)))
        text = json.dumps({"pid": os.getpid(), "cwd": os.getcwd(), "args": args})
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "content": [{"type": "text", "text": text}], "isError": False}})
"#;

struct Fixture {
    host: TestHost,
    script: tempfile::NamedTempFile,
    python: String,
}

impl Fixture {
    async fn new(server_grace: Duration) -> Option<Self> {
        let Some(python) = python() else {
            eprintln!("skipping shared MCP host test: python not installed");
            return None;
        };
        use std::io::Write;
        let mut script = tempfile::Builder::new()
            .prefix("rebon-mcp-shared-")
            .suffix(".py")
            .tempfile()
            .unwrap();
        script.write_all(SERVER.as_bytes()).unwrap();
        script.flush().unwrap();
        Some(Self {
            host: TestHost::start(server_grace).await,
            script,
            python,
        })
    }

    fn config(&self) -> StdioServerConfig {
        StdioServerConfig::new(
            "who",
            self.python.clone(),
            vec![self.script.path().to_string_lossy().into_owned()],
        )
    }

    fn session(&self) -> StdioMcpClient {
        StdioMcpClient::sharing_through(self.host.path_buf())
    }

    async fn attached(&self, config: StdioServerConfig) -> StdioMcpClient {
        let session = self.session();
        session.add_server(config).await.expect("attach failed");
        session
    }
}

async fn ask(session: &StdioMcpClient, tool: &str, args: Value) -> Result<Value, String> {
    let result = session
        .call_tool(McpToolCall {
            server: "who".into(),
            name: tool.into(),
            arguments: args,
        })
        .await
        .map_err(|error| error.to_string())?;
    let text = result.content[0]["text"].as_str().unwrap().to_string();
    Ok(serde_json::from_str(&text).unwrap())
}

async fn pid_of(session: &StdioMcpClient) -> u64 {
    ask(session, "who", json!({})).await.unwrap()["pid"]
        .as_u64()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sessions_use_one_process() {
    let Some(fx) = Fixture::new(Duration::from_secs(60)).await else {
        return;
    };
    let a = fx.attached(fx.config()).await;
    let b = fx.attached(fx.config()).await;
    let pid = pid_of(&a).await;
    assert_eq!(pid_of(&b).await, pid);
    assert_ne!(pid, u64::from(std::process::id()));

    let listed = list_in(fx.host.path()).unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].name, "who");
    assert_eq!(listed[0].sessions, 2);
    assert_eq!(listed[0].pid.map(u64::from), Some(pid));
    assert_eq!(a.list_tools("who").await, Some(vec!["who".to_string()]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_session_gets_its_own_answers_when_ids_collide() {
    let Some(fx) = Fixture::new(Duration::from_secs(60)).await else {
        return;
    };
    let a = fx.attached(fx.config()).await;
    let b = fx.attached(fx.config()).await;
    // Both sessions number their requests from 1, so the host sees the same
    // ids from each, and a slow answer for one is still in flight while the
    // other is answered.
    let slow = ask(&a, "slow", json!({ "who": "a", "seconds": 0.4 }));
    let quick = ask(&b, "who", json!({ "who": "b" }));
    let (slow, quick) = tokio::join!(slow, quick);
    assert_eq!(slow.unwrap()["args"]["who"], "a");
    assert_eq!(quick.unwrap()["args"]["who"], "b");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_working_directories_get_different_processes() {
    let Some(fx) = Fixture::new(Duration::from_secs(60)).await else {
        return;
    };
    let one = tempfile::tempdir().unwrap();
    let two = tempfile::tempdir().unwrap();
    let mut first = fx.config();
    first.cwd = Some(one.path().to_string_lossy().into_owned());
    let mut second = fx.config();
    second.cwd = Some(two.path().to_string_lossy().into_owned());
    let a = fx.attached(first).await;
    let b = fx.attached(second).await;
    assert_ne!(pid_of(&a).await, pid_of(&b).await);
    let cwd = ask(&b, "who", json!({})).await.unwrap()["cwd"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        std::fs::canonicalize(cwd).unwrap(),
        std::fs::canonicalize(two.path()).unwrap()
    );
    assert_eq!(list_in(fx.host.path()).unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_marked_not_shared_is_started_by_the_session() {
    let Some(fx) = Fixture::new(Duration::from_secs(60)).await else {
        return;
    };
    let mut config = fx.config();
    config.shared = false;
    let a = fx.attached(config.clone()).await;
    let b = fx.attached(config).await;
    assert_ne!(pid_of(&a).await, pid_of(&b).await);
    assert!(list_in(fx.host.path()).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_a_server_off_takes_it_out_of_every_session() {
    let Some(fx) = Fixture::new(Duration::from_secs(60)).await else {
        return;
    };
    let a = fx.attached(fx.config()).await;
    let b = fx.attached(fx.config()).await;
    pid_of(&a).await;
    assert_eq!(
        stop_in(fx.host.path(), "who", StopReason::Disabled).unwrap(),
        1
    );
    for session in [&a, &b] {
        let mut gone = false;
        for _ in 0..100 {
            if session.server_names().is_empty() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "a stopped server still lists its tools");
        let error = ask(session, "who", json!({})).await.unwrap_err();
        assert!(error.contains("turned off"), "{error}");
    }
    assert!(list_in(fx.host.path()).unwrap().is_empty());
    // An explicit reconnect brings it back.
    assert!(a.reconnect_server("who").await.unwrap());
    pid_of(&a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_gives_sessions_a_fresh_process_on_their_next_call() {
    let Some(fx) = Fixture::new(Duration::from_secs(60)).await else {
        return;
    };
    let a = fx.attached(fx.config()).await;
    let b = fx.attached(fx.config()).await;
    let before = pid_of(&a).await;
    assert_eq!(
        stop_in(fx.host.path(), "who", StopReason::Restart).unwrap(),
        1
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(a.server_names(), vec!["who".to_string()]);
    let after = pid_of(&a).await;
    assert_ne!(after, before);
    assert_eq!(pid_of(&b).await, after);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_exits_is_started_again_on_the_next_call() {
    let Some(fx) = Fixture::new(Duration::from_secs(60)).await else {
        return;
    };
    let a = fx.attached(fx.config()).await;
    let before = pid_of(&a).await;
    assert!(ask(&a, "exit", json!({})).await.is_err());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_ne!(pid_of(&a).await, before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_nobody_uses_is_kept_for_the_grace_then_stopped() {
    let Some(fx) = Fixture::new(Duration::from_millis(400)).await else {
        return;
    };
    let a = fx.attached(fx.config()).await;
    let pid = pid_of(&a).await;
    a.shutdown().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let listed = list_in(fx.host.path()).unwrap();
    assert_eq!(listed.len(), 1, "kept warm for the next session");
    assert_eq!(listed[0].sessions, 0);
    // The next session within the grace gets the same process.
    let b = fx.attached(fx.config()).await;
    assert_eq!(pid_of(&b).await, pid);
    b.shutdown().await;
    let mut stopped = false;
    for _ in 0..100 {
        if list_in(fx.host.path()).unwrap().is_empty() {
            stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(stopped, "an idle server outlived its grace");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_will_not_start_fails_the_attach_without_a_second_try() {
    let Some(fx) = Fixture::new(Duration::from_secs(60)).await else {
        return;
    };
    let config = StdioServerConfig::new("who", "rebon-no-such-mcp-server-anywhere", vec![]);
    let error = fx.session().add_server(config).await.unwrap_err();
    assert!(error.to_string().contains("spawn failed"), "{error}");
    assert!(list_in(fx.host.path()).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_host_the_session_starts_the_server_itself() {
    let Some(python) = python() else {
        return;
    };
    use std::io::Write;
    let mut script = tempfile::Builder::new().suffix(".py").tempfile().unwrap();
    script.write_all(SERVER.as_bytes()).unwrap();
    let empty = tempfile::tempdir().unwrap();
    let session = StdioMcpClient::sharing_through(empty.path().to_path_buf());
    session
        .add_server(StdioServerConfig::new(
            "who",
            python,
            vec![script.path().to_string_lossy().into_owned()],
        ))
        .await
        .expect("a missing host must not fail the server");
    pid_of(&session).await;
    session.shutdown().await;
}
