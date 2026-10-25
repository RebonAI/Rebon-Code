use rebon_plugin_protocol::{
    CallIdentity, NdjsonCodec, WireEnvelope, WireMessage, PLATFORM_INITIALIZE_METHOD,
    PLATFORM_SHUTDOWN_METHOD, SCOPE_CLOSE_METHOD, SCOPE_OPEN_METHOD,
};
use serde_json::json;
use std::{
    env,
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
};

fn control(epoch: u64, id: &str, method: &str) -> WireEnvelope {
    WireEnvelope::new(
        CallIdentity::platform_control(epoch, id).unwrap(),
        WireMessage::Request {
            method: method.into(),
            payload: json!(null).into(),
        },
    )
}
fn scope(id: &str, generation: u64, method: &str, payload: serde_json::Value) -> WireEnvelope {
    WireEnvelope::new(
        CallIdentity {
            host_epoch: 7,
            plugin_id: "plugin.a".into(),
            scope_id: "scope.a".into(),
            scope_generation: generation,
            call_id: id.into(),
        },
        WireMessage::Request {
            method: method.into(),
            payload: payload.into(),
        },
    )
}
/// The runner the host runs on: `REBON_VERVE`, or the `rebon-verve` this
/// target directory built (`cargo build -p rebon-verve-host --bin rebon-verve`).
fn runner() -> PathBuf {
    if let Some(path) = env::var_os("REBON_VERVE") {
        return PathBuf::from(path);
    }
    let exe = env::current_exe().expect("the test knows its own path");
    let name = if cfg!(windows) {
        "rebon-verve.exe"
    } else {
        "rebon-verve"
    };
    let built = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("a test binary lives in target/<profile>/deps")
        .join(name);
    assert!(
        built.is_file(),
        "the process contract needs the runner: build it with          `cargo build -p rebon-verve-host --bin rebon-verve`, or set REBON_VERVE"
    );
    built
}

#[test]
fn rust_codec_drives_the_host_process() {
    let runner = runner();
    let cli = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtimes/node/plugin-host/src/cli.mjs");
    let mut child = Command::new(runner)
        .arg("--allow-all")
        .arg(cli)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let codec = NdjsonCodec::default();
    let frames = [
        control(7, "i", PLATFORM_INITIALIZE_METHOD),
        scope(
            "o",
            3,
            SCOPE_OPEN_METHOD,
            json!({"workspace_root":"C:/wire-corpus"}),
        ),
        scope("c", 4, SCOPE_CLOSE_METHOD, json!(null)),
        control(7, "s", PLATFORM_SHUTDOWN_METHOD),
    ];
    {
        let stdin = child.stdin.as_mut().unwrap();
        for frame in &frames {
            stdin.write_all(&codec.encode(frame).unwrap()).unwrap();
        }
    }
    drop(child.stdin.take());
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut decoder = NdjsonCodec::default();
    let replies = decoder.push(&stdout).unwrap();
    decoder.finish().unwrap();
    assert_eq!(replies.len(), 4);
    // Matched by call id rather than by position: the host handles frames
    // concurrently so that a handler can call back into rebon, which means a
    // quick request may finish ahead of a slower one that arrived first. The
    // contract is that every request is answered exactly once with its own
    // identity, not that answers come back in the order they were asked.
    for request in &frames {
        let reply = replies
            .iter()
            .find(|reply| reply.identity.call_id == request.identity.call_id)
            .unwrap_or_else(|| panic!("no answer for {}", request.identity.call_id));
        assert_eq!(reply.identity, request.identity);
        assert!(matches!(reply.message, WireMessage::Terminal { .. }));
    }
}
