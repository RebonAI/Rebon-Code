use super::*;
use async_trait::async_trait;
use rebon_agent_core::prompt_executor::{PromptExecutor, PromptExecutorError, PromptRequest};
use rebon_proto::types::{error_code, ConfigOptionType, JsonRpcResponse, RequestId};
use rebon_session::session_storage::format_system_time_iso_ms;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::io::{duplex, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};

/// Build a fresh client/server pipe pair for in-memory tests.
///
/// Returns `(client_to_server, server_reader, server_writer, client_from_server)`.
/// The server end takes `server_reader` + `server_writer`; the test
/// drives the client end.
fn pipe_pair() -> (DuplexStream, DuplexStream, DuplexStream, DuplexStream) {
    let (client_to_server, server_reader) = duplex(4096);
    let (server_writer, client_from_server) = duplex(4096);
    (
        client_to_server,
        server_reader,
        server_writer,
        client_from_server,
    )
}

async fn drain(mut s: DuplexStream) -> Vec<u8> {
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    buf
}

fn first_ndjson_line(body: &[u8]) -> &[u8] {
    let i = body.iter().position(|&b| b == b'\n').unwrap_or(body.len());
    &body[..i]
}

#[tokio::test]
async fn tcp_listener_port_zero_accepts_initialize_frame() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let local_addr = listener.local_addr().unwrap();
    assert_ne!(local_addr.port(), 0);

    let serve_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, writer) = stream.into_split();
        serve(reader, writer, DefaultHandler::default())
            .await
            .unwrap();
    });

    let mut client = tokio::net::TcpStream::connect(local_addr).await.unwrap();
    let input = br#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#;
    client.write_all(input).await.unwrap();
    client.shutdown().await.unwrap();

    let mut out = Vec::new();
    client.read_to_end(&mut out).await.unwrap();
    serve_task.await.unwrap();

    let resp: JsonRpcResponse = serde_json::from_slice(first_ndjson_line(&out)).unwrap();
    assert_eq!(resp.id, Some(RequestId::Number(7)));
    assert!(resp.error.is_none());
    let result = resp.result.expect("expected result on initialize");
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["agentInfo"]["name"], "rebon");
}

#[tokio::test]
async fn initialize_returns_result() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let input = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":true}}}}
"#;
    client_in.write_all(input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });

    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let line = first_ndjson_line(&out);
    let resp: JsonRpcResponse = serde_json::from_slice(line).unwrap();
    assert_eq!(resp.id, Some(RequestId::Number(1)));
    assert!(resp.error.is_none());
    let result = resp.result.expect("expected result on initialize");
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["agentInfo"]["name"], "rebon");
    assert_eq!(result["agentInfo"]["title"], "Rebon");
    assert!(result["authMethods"].is_array());
    assert_eq!(result["authMethods"].as_array().unwrap().len(), 0);
    assert!(result["agentCapabilities"]["loadSession"]
        .as_bool()
        .unwrap());
    assert!(result["agentCapabilities"]["promptCapabilities"]["image"]
        .as_bool()
        .unwrap());
    assert!(result["agentCapabilities"]["mcpCapabilities"]["http"]
        .as_bool()
        .unwrap());
    assert!(result["agentCapabilities"]["mcpCapabilities"]["sse"]
        .as_bool()
        .unwrap());
}

#[tokio::test]
async fn initialize_clamps_to_max_protocol_version() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let input = br#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":999,"clientCapabilities":{}}}
"#;
    client_in.write_all(input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let resp: JsonRpcResponse = serde_json::from_slice(first_ndjson_line(&out)).unwrap();
    let result = resp.result.expect("result");
    assert_eq!(result["protocolVersion"], ACP_PROTOCOL_VERSION);
}

#[tokio::test]
async fn unknown_method_returns_method_not_found() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let input = br#"{"jsonrpc":"2.0","id":42,"method":"session/unknown","params":{}}
"#;
    client_in.write_all(input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let resp: JsonRpcResponse = serde_json::from_slice(first_ndjson_line(&out)).unwrap();
    assert_eq!(resp.id, Some(RequestId::Number(42)));
    assert!(resp.result.is_none());
    let err = resp.error.expect("expected method-not-found error");
    assert_eq!(err.code, error_code::METHOD_NOT_FOUND);
    assert!(err.message.contains("session/unknown"));
}

#[tokio::test]
async fn malformed_json_returns_parse_error() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    // Leading `{` triggers NDJSON detection; the body terminates at `\n`
    // and then JSON parse fails.
    let input = b"{not-json}\n";
    client_in.write_all(input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let resp: JsonRpcResponse = serde_json::from_slice(first_ndjson_line(&out)).unwrap();
    assert!(resp.id.is_none(), "parse error must use null id");
    let err = resp.error.expect("expected parse error");
    assert_eq!(err.code, error_code::PARSE_ERROR);
}

#[tokio::test]
async fn structurally_invalid_jsonrpc_returns_invalid_request() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    // Valid JSON, but neither method nor id is present.
    let input = br#"{"jsonrpc":"2.0"}
"#;
    client_in.write_all(input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let resp: JsonRpcResponse = serde_json::from_slice(first_ndjson_line(&out)).unwrap();
    assert!(resp.id.is_none());
    let err = resp.error.expect("expected invalid-request error");
    assert_eq!(err.code, error_code::INVALID_REQUEST);
}

#[tokio::test]
async fn notification_is_silently_ignored() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let input = br#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s1"}}
"#;
    client_in.write_all(input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    assert!(
        out.is_empty(),
        "notification must not produce any response, got {:?}",
        String::from_utf8_lossy(&out)
    );
}

#[tokio::test]
async fn empty_input_returns_clean() {
    let (client_in, server_in, server_out, client_out) = pipe_pair();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();
    assert!(out.is_empty());
}

#[tokio::test]
async fn content_length_framing_is_mirrored_in_response() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let body = br#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}"#;
    let mut input = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    input.extend_from_slice(body);
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    assert!(
        out.starts_with(b"Content-Length:"),
        "expected Content-Length framing in response, got: {:?}",
        String::from_utf8_lossy(&out)
    );

    // Pull the body out and confirm it parses as a successful response.
    let header_end = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("expected header terminator");
    let body_start = header_end + 4;
    let resp: JsonRpcResponse = serde_json::from_slice(&out[body_start..]).unwrap();
    assert_eq!(resp.id, Some(RequestId::Number(7)));
    assert!(resp.result.is_some());
}

#[tokio::test]
async fn handles_two_requests_in_sequence() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":2,"method":"session/unknown","params":{}}
"#,
    );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines: Vec<&[u8]> = out
        .split(|&b| b == b'\n')
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(lines.len(), 2);
    let r1: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    assert_eq!(r1.id, Some(RequestId::Number(1)));
    assert!(r1.result.is_some());
    assert_eq!(r2.id, Some(RequestId::Number(2)));
    assert_eq!(
        r2.error.as_ref().unwrap().code,
        error_code::METHOD_NOT_FOUND
    );
}

// ---- session/new ----

fn ndjson_lines(body: &[u8]) -> Vec<&[u8]> {
    body.split(|&b| b == b'\n')
        .filter(|s| !s.is_empty())
        .collect()
}

#[tokio::test]
async fn initialize_then_session_new_happy_path() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/work"}}
"#,
    );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let handler = DefaultHandler::default();
    let state = handler.state().clone();
    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);

    let r1: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    assert_eq!(r1.id, Some(RequestId::Number(1)));
    assert!(r1.error.is_none(), "initialize must succeed");

    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    assert_eq!(r2.id, Some(RequestId::Number(2)));
    assert!(r2.error.is_none(), "session/new must succeed after init");
    let result = r2.result.expect("session/new result");
    let sid = result["sessionId"]
        .as_str()
        .expect("sessionId must be a string");
    assert!(!sid.is_empty());

    // Sanity-check server state: the session exists and is stored under
    // the requested cwd.
    let record = state.get_session(sid).expect("session should be stored");
    assert_eq!(record.cwd, "/tmp/work");
    assert_eq!(state.session_count(), 1);
}

#[tokio::test]
async fn initialize_then_session_new_with_mcp_servers_succeeds_and_stores_config() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/work","mcpServers":[{"transport":"stdio","name":"fs","command":"node","args":["server.js"],"env":{"B":"2","A":"1"}},{"transport":"http","name":"remote","url":"https://example.test/mcp","headers":{"Authorization":"Bearer x"}},{"transport":"sse","name":"events","url":"https://example.test/sse"}]}}
"#,
        );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let handler = DefaultHandler::default();
    let state = handler.state().clone();
    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    assert_eq!(r2.id, Some(RequestId::Number(2)));
    assert!(r2.error.is_none(), "session/new should accept mcpServers");
    let sid = r2.result.unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let record = state.get_session(&sid).expect("session should be stored");
    assert_eq!(record.mcp_servers.len(), 3);
    assert_eq!(record.mcp_servers[0].name(), "fs");
    assert_eq!(state.session_count(), 1);
}

#[tokio::test]
async fn session_new_rejects_invalid_mcp_servers() {
    for (mcp_servers, expected) in [
        (
            r#"[{"transport":"stdio","name":" ","command":"node"}]"#,
            "non-blank name",
        ),
        (
            r#"[{"transport":"stdio","name":"dup","command":"node"},{"transport":"http","name":"dup","url":"https://example.test"}]"#,
            "duplicate server name: dup",
        ),
        (
            r#"[{"transport":"stdio","name":"fs","command":" "}]"#,
            "non-blank command",
        ),
        (
            r#"[{"transport":"http","name":"remote","url":" "}]"#,
            "non-blank url",
        ),
        (
            r#"[{"transport":"sse","name":"events","url":" "}]"#,
            "non-blank url",
        ),
    ] {
        let handler = DefaultHandler::default();
        handler
            .handle_request(
                "initialize",
                Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
            )
            .await
            .unwrap();
        let params = serde_json::json!({
            "cwd": "/tmp/work",
            "mcpServers": serde_json::from_str::<Value>(mcp_servers).unwrap(),
        });
        let err = handler
            .handle_request("session/new", Some(params))
            .await
            .unwrap_err();
        assert_eq!(err.code, error_code::INVALID_PARAMS);
        assert!(
            err.message.contains(expected),
            "expected `{expected}` in `{}`",
            err.message
        );
    }
}

#[tokio::test]
async fn session_new_before_initialize_errors() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let input = br#"{"jsonrpc":"2.0","id":42,"method":"session/new","params":{"cwd":"/tmp/x"}}
"#;
    client_in.write_all(input).await.unwrap();
    drop(client_in);

    let handler = DefaultHandler::default();
    let state = handler.state().clone();
    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 1);
    let resp: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    assert_eq!(resp.id, Some(RequestId::Number(42)));
    assert!(resp.result.is_none());
    let err = resp.error.expect("expected not-initialized error");
    assert_eq!(err.code, error_code::INVALID_REQUEST);
    assert!(
        err.message.contains("Not initialized"),
        "unexpected error message: {}",
        err.message
    );
    // Crucially, no session was stored.
    assert_eq!(state.session_count(), 0);
}

#[tokio::test]
async fn duplicate_initialize_errors() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r1: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    assert!(r1.error.is_none(), "first initialize must succeed");
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    let err = r2.error.expect("second initialize must error");
    assert_eq!(err.code, error_code::INVALID_REQUEST);
    assert!(err.message.contains("Already initialized"));
}

#[tokio::test]
async fn two_session_new_calls_return_distinct_ids() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/a"}}
"#,
    );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/tmp/b"}}
"#,
    );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let handler = DefaultHandler::default();
    let state = handler.state().clone();
    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 3);

    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    let r3: JsonRpcResponse = serde_json::from_slice(lines[2]).unwrap();
    let sid_a = r2.result.as_ref().unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let sid_b = r3.result.as_ref().unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(sid_a, sid_b, "session ids must differ");
    assert_eq!(state.session_count(), 2);
    assert_eq!(state.get_session(&sid_a).unwrap().cwd, "/tmp/a");
    assert_eq!(state.get_session(&sid_b).unwrap().cwd, "/tmp/b");
}

#[tokio::test]
async fn session_new_empty_cwd_falls_back_to_default() {
    // When params.cwd is empty, the handler should fall back to
    // `DefaultHandler::default_cwd` (the
    // `params.cwd || this.options.cwd || the current process directory` chain).
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":""}}
"#,
    );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let handler = DefaultHandler {
        default_cwd: Some("/fallback/dir".to_string()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();
    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    assert!(r2.error.is_none());
    let sid = r2.result.unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(state.get_session(&sid).unwrap().cwd, "/fallback/dir");
}

#[tokio::test]
async fn session_new_whitespace_only_cwd_is_preserved() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"   "}}
"#,
    );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let handler = DefaultHandler {
        default_cwd: Some("/fallback/dir".to_string()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();
    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    assert!(r2.error.is_none());
    let sid = r2.result.unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(state.get_session(&sid).unwrap().cwd, "   ");
}

// ---- session/prompt + session/cancel ----

#[tokio::test]
async fn initialize_then_session_new_then_session_prompt_happy_path() {
    // Full end-to-end: initialize → session/new → session/prompt.
    // Rather than string-replacing the freshly-minted session id into
    // a queued request (which would require async plumbing we don't
    // need), we drive the server in two passes sharing the same
    // `DefaultHandler` — the handler's `Arc<ServerState>` survives
    // even when the transport pipe does not.
    let handler = DefaultHandler::default();
    let state = handler.state().clone();

    // --- pass 1: initialize + session/new ---
    let sid = {
        let (mut client_in, server_in, server_out, client_out) = pipe_pair();
        let mut input = Vec::new();
        input.extend_from_slice(
                br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
            );
        input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/work"}}
"#,
        );
        client_in.write_all(&input).await.unwrap();
        drop(client_in);

        let h = handler.clone();
        let serve_task = tokio::spawn(async move {
            serve(server_in, server_out, h).await.unwrap();
        });
        let out = drain(client_out).await;
        serve_task.await.unwrap();

        let lines = ndjson_lines(&out);
        assert_eq!(lines.len(), 2);
        let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
        assert!(r2.error.is_none(), "session/new must succeed");
        r2.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // --- pass 2: session/prompt against the stored session id ---
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"{sid}","prompt":[{{"type":"text","text":"hello world"}}]}}}}
"#
    );
    client_in.write_all(req.as_bytes()).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 1);
    let resp: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    assert_eq!(resp.id, Some(RequestId::Number(3)));
    assert!(resp.error.is_none(), "session/prompt must succeed");
    let result = resp.result.expect("session/prompt result");
    assert_eq!(result["stopReason"], "end_turn");

    // Session state: the active-prompt slot and its potentially large
    // diagnostic payload are released after the executor completes.
    let rec = state.get_session(&sid).expect("session must still exist");
    assert!(rec.messages.is_empty());
    assert!(!state.is_prompt_active(&sid));
}

#[derive(Default)]
struct RecordingPromptExecutor {
    requests: Mutex<Vec<PromptRequest>>,
}

struct AbortThenSuccessExecutor {
    calls: std::sync::atomic::AtomicUsize,
    entered: tokio::sync::Notify,
}

#[async_trait]
impl PromptExecutor for AbortThenSuccessExecutor {
    async fn execute(
        &self,
        _request: PromptRequest,
    ) -> Result<rebon_agent_core::prompt_executor::PromptOutcome, PromptExecutorError> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            std::future::pending().await
        } else {
            Ok(rebon_agent_core::prompt_executor::PromptOutcome::end_turn())
        }
    }
}

struct CancelAwarePromptExecutor {
    entered: tokio::sync::Notify,
}

struct CancelThenSuccessPromptExecutor {
    calls: std::sync::atomic::AtomicUsize,
    first_entered: tokio::sync::Notify,
}

struct ReleaseThenSuccessPromptExecutor {
    calls: std::sync::atomic::AtomicUsize,
    first_entered: tokio::sync::Notify,
    release_first: tokio::sync::Notify,
    second_entered: tokio::sync::Notify,
    release_second: tokio::sync::Notify,
}

struct DelayedCancelHandler {
    inner: DefaultHandler,
    cancel_entered: Arc<tokio::sync::Notify>,
    release_cancel: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl RequestHandler for DelayedCancelHandler {
    async fn handle_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, rebon_proto::types::JsonRpcError> {
        self.inner.handle_request(method, params).await
    }

    fn permission_session_cwd(&self, session_id: &str) -> Option<String> {
        self.inner.permission_session_cwd(session_id)
    }

    fn apply_allow_always_rules(
        &self,
        session_id: &str,
        rules: &[rebon_permissions::PermissionRuleValue],
    ) -> Result<(), String> {
        self.inner.apply_allow_always_rules(session_id, rules)
    }

    async fn handle_notification(&self, method: &str, params: Option<Value>) {
        if method == "session/cancel" {
            self.cancel_entered.notify_one();
            self.release_cancel.notified().await;
        }
        self.inner.handle_notification(method, params).await;
    }
}

#[derive(Clone)]
struct NotificationRecordingHandler {
    inner: DefaultHandler,
    notifications: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl RequestHandler for NotificationRecordingHandler {
    async fn handle_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, rebon_proto::types::JsonRpcError> {
        self.inner.handle_request(method, params).await
    }

    fn permission_session_cwd(&self, session_id: &str) -> Option<String> {
        self.inner.permission_session_cwd(session_id)
    }

    fn apply_allow_always_rules(
        &self,
        session_id: &str,
        rules: &[rebon_permissions::PermissionRuleValue],
    ) -> Result<(), String> {
        self.inner.apply_allow_always_rules(session_id, rules)
    }

    async fn handle_notification(&self, method: &str, params: Option<Value>) {
        self.notifications.lock().unwrap().push(method.to_string());
        self.inner.handle_notification(method, params).await;
    }
}

#[async_trait]
impl PromptExecutor for CancelAwarePromptExecutor {
    async fn execute(
        &self,
        request: PromptRequest,
    ) -> Result<rebon_agent_core::prompt_executor::PromptOutcome, PromptExecutorError> {
        self.entered.notify_one();
        request.cancel.notified().await;
        Err(PromptExecutorError::Cancelled)
    }
}

#[async_trait]
impl PromptExecutor for CancelThenSuccessPromptExecutor {
    async fn execute(
        &self,
        request: PromptRequest,
    ) -> Result<rebon_agent_core::prompt_executor::PromptOutcome, PromptExecutorError> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            self.first_entered.notify_one();
            request.cancel.notified().await;
            Err(PromptExecutorError::Cancelled)
        } else {
            Ok(rebon_agent_core::prompt_executor::PromptOutcome::end_turn())
        }
    }
}

#[async_trait]
impl PromptExecutor for ReleaseThenSuccessPromptExecutor {
    async fn execute(
        &self,
        request: PromptRequest,
    ) -> Result<rebon_agent_core::prompt_executor::PromptOutcome, PromptExecutorError> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            self.first_entered.notify_one();
            self.release_first.notified().await;
            Ok(rebon_agent_core::prompt_executor::PromptOutcome::end_turn())
        } else {
            self.second_entered.notify_one();
            tokio::select! {
                () = request.cancel.notified() => Err(PromptExecutorError::Cancelled),
                () = self.release_second.notified() => {
                    Ok(rebon_agent_core::prompt_executor::PromptOutcome::end_turn())
                }
            }
        }
    }
}

struct FixedPromptExecutor(
    Result<rebon_agent_core::prompt_executor::PromptOutcome, PromptExecutorError>,
);

struct ReleaseAndPublishExecutor {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct PermissionWaitingExecutor;

#[async_trait]
impl PromptExecutor for PermissionWaitingExecutor {
    async fn execute(
        &self,
        request: PromptRequest,
    ) -> Result<rebon_agent_core::prompt_executor::PromptOutcome, PromptExecutorError> {
        request
            .permission_publisher
            .expect("test executor requires permission publisher")
            .request_permission(sample_permission_request_params())
            .await
            .map_err(|err| PromptExecutorError::Execution(err.to_string()))?;
        Ok(rebon_agent_core::prompt_executor::PromptOutcome::end_turn())
    }
}

#[async_trait]
impl PromptExecutor for ReleaseAndPublishExecutor {
    async fn execute(
        &self,
        request: PromptRequest,
    ) -> Result<rebon_agent_core::prompt_executor::PromptOutcome, PromptExecutorError> {
        self.entered.notify_one();
        self.release.notified().await;
        request
            .update_publisher
            .expect("test executor requires update publisher")
            .publish_to(
                &request.session_id,
                rebon_proto::types::SessionUpdate::AgentMessageChunk {
                    content: rebon_proto::types::ContentBlock::Text(
                        rebon_proto::types::TextContent {
                            text: "after eof".to_string(),
                            annotations: None,
                        },
                    ),
                },
            )
            .await;
        Ok(rebon_agent_core::prompt_executor::PromptOutcome::end_turn())
    }
}

#[async_trait]
impl PromptExecutor for FixedPromptExecutor {
    async fn execute(
        &self,
        _request: PromptRequest,
    ) -> Result<rebon_agent_core::prompt_executor::PromptOutcome, PromptExecutorError> {
        self.0.clone()
    }
}

async fn lifecycle_test_session(executor: Arc<dyn PromptExecutor>) -> (DefaultHandler, String) {
    let handler = DefaultHandler::default().with_prompt_executor(executor);
    handler
        .handle_request(
            "initialize",
            Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    let result = handler
        .handle_request(
            "session/new",
            Some(serde_json::json!({"cwd": r"C:\rebon-lifecycle-test"})),
        )
        .await
        .unwrap();
    let sid = result["sessionId"].as_str().unwrap().to_string();
    (handler, sid)
}

fn lifecycle_prompt_params(sid: &str, text: &str) -> Value {
    serde_json::json!({
        "sessionId": sid,
        "prompt": [{"type":"text","text":text}]
    })
}

fn assert_prompt_lifecycle_released(handler: &DefaultHandler, sid: &str) {
    let state = handler.state();
    assert!(!state.is_prompt_active(sid));
    assert!(state.get_session(sid).unwrap().messages.is_empty());
    assert_eq!(state.prompt_messages_capacity(sid), Some(0));
    assert!(!handler
        .active_cancels
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains_key(sid));
}

#[tokio::test]
async fn dropped_pending_prompt_cleans_lifecycle_and_allows_next_prompt() {
    let executor = Arc::new(AbortThenSuccessExecutor {
        calls: std::sync::atomic::AtomicUsize::new(0),
        entered: tokio::sync::Notify::new(),
    });
    let (handler, sid) = lifecycle_test_session(executor.clone()).await;
    let task_handler = handler.clone();
    let task_sid = sid.clone();
    let task = tokio::spawn(async move {
        task_handler
            .handle_request(
                "session/prompt",
                Some(lifecycle_prompt_params(&task_sid, &"x".repeat(64 * 1024))),
            )
            .await
    });

    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.entered.notified(),
    )
    .await
    .expect("pending executor was not entered");
    assert!(handler.state().is_prompt_active(&sid));
    assert!(handler.state().prompt_messages_capacity(&sid).unwrap() > 0);
    assert!(handler.active_cancels.lock().unwrap().contains_key(&sid));

    task.abort();
    tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("aborted prompt task did not finish")
        .expect_err("aborted prompt task unexpectedly completed");
    assert_prompt_lifecycle_released(&handler, &sid);

    let result = handler
        .handle_request(
            "session/prompt",
            Some(lifecycle_prompt_params(&sid, "next")),
        )
        .await
        .expect("replacement prompt should acquire the released slot");
    assert_eq!(result["stopReason"], "end_turn");
    assert_prompt_lifecycle_released(&handler, &sid);
}

#[tokio::test]
async fn cancellation_then_executor_completion_uses_guard_cleanup() {
    let executor = Arc::new(CancelAwarePromptExecutor {
        entered: tokio::sync::Notify::new(),
    });
    let (handler, sid) = lifecycle_test_session(executor.clone()).await;
    let task_handler = handler.clone();
    let task_sid = sid.clone();
    let task = tokio::spawn(async move {
        task_handler
            .handle_request(
                "session/prompt",
                Some(lifecycle_prompt_params(&task_sid, "cancel me")),
            )
            .await
    });

    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.entered.notified(),
    )
    .await
    .expect("cancel-aware executor was not entered");
    handler
        .handle_notification(
            "session/cancel",
            Some(serde_json::json!({"sessionId": sid})),
        )
        .await;

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("cancelled prompt did not finish")
        .expect("cancelled prompt task panicked")
        .expect("cancelled executor must remain a successful ACP response");
    assert_eq!(result["stopReason"], "cancelled");
    assert_eq!(handler.state().cancel_count(&sid), 1);
    assert_prompt_lifecycle_released(&handler, &sid);
}

#[tokio::test]
async fn transport_delivers_cancel_while_prompt_request_is_pending() {
    let executor = Arc::new(CancelAwarePromptExecutor {
        entered: tokio::sync::Notify::new(),
    });
    let (handler, sid) = lifecycle_test_session(executor.clone()).await;
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let serve_task = tokio::spawn(async move { serve(server_in, server_out, handler).await });

    let prompt = format!(
        r#"{{"jsonrpc":"2.0","id":91,"method":"session/prompt","params":{{"sessionId":"{sid}","prompt":[{{"type":"text","text":"cancel over transport"}}]}}}}
"#
    );
    client_in.write_all(prompt.as_bytes()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.entered.notified(),
    )
    .await
    .expect("transport did not start the prompt executor");

    let cancel = format!(
        r#"{{"jsonrpc":"2.0","method":"session/cancel","params":{{"sessionId":"{sid}"}}}}
"#
    );
    client_in.write_all(cancel.as_bytes()).await.unwrap();

    let mut client_out = BufReader::new(client_out);
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client_out.read_line(&mut line),
    )
    .await
    .expect("cancelled prompt response was blocked behind the pending request")
    .unwrap();
    let response: JsonRpcResponse = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(response.id, Some(RequestId::Number(91)));
    assert_eq!(response.result.unwrap()["stopReason"], "cancelled");

    drop(client_in);
    tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
        .await
        .expect("server did not exit after client half-close")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn prompt_and_cancel_in_same_write_preserve_registration_and_request_order() {
    let executor = Arc::new(CancelAwarePromptExecutor {
        entered: tokio::sync::Notify::new(),
    });
    let (handler, sid) = lifecycle_test_session(executor).await;
    let observed = handler.clone();
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let serve_task = tokio::spawn(async move { serve(server_in, server_out, handler).await });

    let input = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":87,\"method\":\"missing/one\"}}\n\
         {{\"jsonrpc\":\"2.0\",\"id\":88,\"method\":\"missing/two\"}}\n\
         {{\"jsonrpc\":\"2.0\",\"id\":89,\"method\":\"missing/three\"}}\n\
         {{\"jsonrpc\":\"2.0\",\"id\":90,\"method\":\"missing/four\"}}\n\
         {{\"jsonrpc\":\"2.0\",\"id\":91,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"cancel immediately\"}}]}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"id\":92,\"method\":\"session/list\",\"params\":{{}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{sid}\"}}}}\n"
    );
    client_in.write_all(input.as_bytes()).await.unwrap();
    drop(client_in);

    let out = tokio::time::timeout(std::time::Duration::from_secs(2), drain(client_out))
        .await
        .expect("same-write cancellation deadlocked behind queued requests");
    tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
        .await
        .expect("server did not drain same-write prompt cancellation")
        .unwrap()
        .unwrap();

    let responses = ndjson_lines(&out)
        .into_iter()
        .map(|line| serde_json::from_slice::<JsonRpcResponse>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        responses
            .iter()
            .map(|response| response.id.clone())
            .collect::<Vec<_>>(),
        vec![
            Some(RequestId::Number(87)),
            Some(RequestId::Number(88)),
            Some(RequestId::Number(89)),
            Some(RequestId::Number(90)),
            Some(RequestId::Number(91)),
            Some(RequestId::Number(92)),
        ]
    );
    assert_eq!(
        responses[4].result.as_ref().unwrap()["stopReason"],
        "cancelled"
    );
    assert!(responses[5].error.is_none());
    assert_eq!(observed.state().cancel_count(&sid), 1);
    assert_prompt_lifecycle_released(&observed, &sid);
}

#[tokio::test]
async fn same_session_queued_prompt_does_not_hide_active_prompt_cancel_barrier() {
    let executor = Arc::new(CancelThenSuccessPromptExecutor {
        calls: std::sync::atomic::AtomicUsize::new(0),
        first_entered: tokio::sync::Notify::new(),
    });
    let (handler, sid) = lifecycle_test_session(executor.clone()).await;
    let observed = handler.clone();
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let serve_task = tokio::spawn(async move { serve(server_in, server_out, handler).await });

    let first_prompt = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":101,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"first\"}}]}}}}\n"
    );
    client_in.write_all(first_prompt.as_bytes()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.first_entered.notified(),
    )
    .await
    .expect("first prompt executor was not entered");

    // Queue another prompt for the same session before cancelling. The cancel
    // must wait on the active prompt's ready barrier, not the queued prompt's
    // barrier (which cannot become ready until the active prompt exits).
    let queued_prompt_and_cancel = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":102,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"second\"}}]}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{sid}\"}}}}\n"
    );
    client_in
        .write_all(queued_prompt_and_cancel.as_bytes())
        .await
        .unwrap();
    drop(client_in);

    let out = tokio::time::timeout(std::time::Duration::from_secs(2), drain(client_out))
        .await
        .expect("queued same-session prompt hid the active prompt cancel barrier");
    tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
        .await
        .expect("server deadlocked while draining same-session prompts")
        .unwrap()
        .unwrap();

    let responses = ndjson_lines(&out)
        .into_iter()
        .map(|line| serde_json::from_slice::<JsonRpcResponse>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0].id, Some(RequestId::Number(101)));
    assert_eq!(
        responses[0].result.as_ref().unwrap()["stopReason"],
        "cancelled"
    );
    assert_eq!(responses[1].id, Some(RequestId::Number(102)));
    assert_eq!(
        responses[1].result.as_ref().unwrap()["stopReason"],
        "end_turn"
    );
    assert_eq!(executor.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(observed.state().cancel_count(&sid), 1);
    assert_prompt_lifecycle_released(&observed, &sid);
}

#[tokio::test]
async fn three_same_session_prompts_then_cancel_bind_to_active_prompt_only() {
    let executor = Arc::new(CancelThenSuccessPromptExecutor {
        calls: std::sync::atomic::AtomicUsize::new(0),
        first_entered: tokio::sync::Notify::new(),
    });
    let (handler, sid) = lifecycle_test_session(executor.clone()).await;
    let observed = handler.clone();
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let serve_task = tokio::spawn(async move { serve(server_in, server_out, handler).await });

    let first = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":141,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"first\"}}]}}}}\n"
    );
    client_in.write_all(first.as_bytes()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.first_entered.notified(),
    )
    .await
    .expect("first prompt executor was not entered");

    let tail = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":142,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"second\"}}]}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"id\":143,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"third\"}}]}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{sid}\"}}}}\n"
    );
    client_in.write_all(tail.as_bytes()).await.unwrap();
    drop(client_in);

    let out = tokio::time::timeout(std::time::Duration::from_secs(2), drain(client_out))
        .await
        .expect("three-prompt barrier queue deadlocked");
    tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
        .await
        .expect("server did not drain three same-session prompts")
        .unwrap()
        .unwrap();

    let responses = ndjson_lines(&out)
        .into_iter()
        .map(|line| serde_json::from_slice::<JsonRpcResponse>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0].id, Some(RequestId::Number(141)));
    assert_eq!(
        responses[0].result.as_ref().unwrap()["stopReason"],
        "cancelled"
    );
    for (response, id) in responses[1..].iter().zip([142, 143]) {
        assert_eq!(response.id, Some(RequestId::Number(id)));
        assert_eq!(response.result.as_ref().unwrap()["stopReason"], "end_turn");
    }
    assert_eq!(executor.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(observed.state().cancel_count(&sid), 1);
    assert_prompt_lifecycle_released(&observed, &sid);
}

#[tokio::test]
async fn delayed_cancel_bound_to_completed_prompt_cannot_cancel_next_prompt() {
    let executor = Arc::new(ReleaseThenSuccessPromptExecutor {
        calls: std::sync::atomic::AtomicUsize::new(0),
        first_entered: tokio::sync::Notify::new(),
        release_first: tokio::sync::Notify::new(),
        second_entered: tokio::sync::Notify::new(),
        release_second: tokio::sync::Notify::new(),
    });
    let (inner, sid) = lifecycle_test_session(executor.clone()).await;
    let observed = inner.clone();
    let cancel_entered = Arc::new(tokio::sync::Notify::new());
    let release_cancel = Arc::new(tokio::sync::Notify::new());
    let handler = DelayedCancelHandler {
        inner,
        cancel_entered: cancel_entered.clone(),
        release_cancel: release_cancel.clone(),
    };
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let serve_task = tokio::spawn(async move { serve(server_in, server_out, handler).await });

    let first = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":146,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"first\"}}]}}}}\n"
    );
    client_in.write_all(first.as_bytes()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.first_entered.notified(),
    )
    .await
    .expect("first prompt executor was not entered");

    let tail = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":147,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"second\"}}]}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{sid}\"}}}}\n"
    );
    client_in.write_all(tail.as_bytes()).await.unwrap();
    drop(client_in);
    tokio::time::timeout(std::time::Duration::from_secs(2), cancel_entered.notified())
        .await
        .expect("cancel notification did not reach delayed handler");

    // Complete the first prompt while its already-bound cancel remains
    // delayed. The request FIFO must not start the second prompt until that
    // cancel has been dispatched against the first prompt's generation (where
    // it is now a no-op).
    executor.release_first.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while observed.state().is_prompt_active(&sid) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first prompt did not complete");
    assert_eq!(executor.calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    release_cancel.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.second_entered.notified(),
    )
    .await
    .expect("second prompt did not start after bound cancel drained");
    executor.release_second.notify_one();

    let out = tokio::time::timeout(std::time::Duration::from_secs(2), drain(client_out))
        .await
        .expect("delayed bound cancel prevented EOF drain");
    tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
        .await
        .expect("server did not exit after delayed cancel test")
        .unwrap()
        .unwrap();
    let responses = ndjson_lines(&out)
        .into_iter()
        .map(|line| serde_json::from_slice::<JsonRpcResponse>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0].id, Some(RequestId::Number(146)));
    assert_eq!(
        responses[0].result.as_ref().unwrap()["stopReason"],
        "end_turn"
    );
    assert_eq!(responses[1].id, Some(RequestId::Number(147)));
    assert_eq!(
        responses[1].result.as_ref().unwrap()["stopReason"],
        "end_turn"
    );
    assert_eq!(observed.state().cancel_count(&sid), 1);
    assert_prompt_lifecycle_released(&observed, &sid);
}

#[tokio::test]
async fn parse_and_invalid_request_errors_follow_earlier_prompt_response_fifo() {
    let executor = Arc::new(CancelAwarePromptExecutor {
        entered: tokio::sync::Notify::new(),
    });
    let (handler, sid) = lifecycle_test_session(executor.clone()).await;
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let serve_task = tokio::spawn(async move { serve(server_in, server_out, handler).await });

    let prompt = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":151,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"first\"}}]}}}}\n"
    );
    client_in.write_all(prompt.as_bytes()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.entered.notified(),
    )
    .await
    .expect("prompt executor was not entered");

    let tail = format!(
        "{{not-json}}\n\
         {{\"jsonrpc\":\"2.0\",\"params\":{{}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{sid}\"}}}}\n"
    );
    client_in.write_all(tail.as_bytes()).await.unwrap();
    drop(client_in);

    let out = tokio::time::timeout(std::time::Duration::from_secs(2), drain(client_out))
        .await
        .expect("FIFO error responses blocked prompt cancellation");
    tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
        .await
        .expect("server did not drain FIFO error responses")
        .unwrap()
        .unwrap();

    let responses = ndjson_lines(&out)
        .into_iter()
        .map(|line| serde_json::from_slice::<JsonRpcResponse>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 3, "unexpected responses: {responses:?}");
    assert_eq!(responses[0].id, Some(RequestId::Number(151)));
    assert_eq!(
        responses[0].result.as_ref().unwrap()["stopReason"],
        "cancelled"
    );
    assert_eq!(responses[1].id, None);
    assert_eq!(
        responses[1].error.as_ref().unwrap().code,
        error_code::PARSE_ERROR
    );
    assert_eq!(responses[2].id, None);
    assert_eq!(
        responses[2].error.as_ref().unwrap().code,
        error_code::INVALID_REQUEST
    );
}

#[tokio::test]
async fn malformed_queued_prompt_does_not_poison_cancel_barrier_or_eof_drain() {
    let executor = Arc::new(CancelAwarePromptExecutor {
        entered: tokio::sync::Notify::new(),
    });
    let (handler, sid) = lifecycle_test_session(executor.clone()).await;
    let observed = handler.clone();
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let serve_task = tokio::spawn(async move { serve(server_in, server_out, handler).await });

    let first = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":111,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"first\"}}]}}}}\n"
    );
    client_in.write_all(first.as_bytes()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.entered.notified(),
    )
    .await
    .expect("first prompt executor was not entered");

    let tail = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":112,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":\"not-an-array\"}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{sid}\"}}}}\n"
    );
    client_in.write_all(tail.as_bytes()).await.unwrap();
    drop(client_in);

    let out = tokio::time::timeout(std::time::Duration::from_secs(2), drain(client_out))
        .await
        .expect("malformed queued prompt poisoned cancellation or EOF drain");
    tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
        .await
        .expect("server did not exit after malformed queued prompt and EOF")
        .unwrap()
        .unwrap();
    let responses = ndjson_lines(&out)
        .into_iter()
        .map(|line| serde_json::from_slice::<JsonRpcResponse>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0].id, Some(RequestId::Number(111)));
    assert_eq!(
        responses[0].result.as_ref().unwrap()["stopReason"],
        "cancelled"
    );
    assert_eq!(responses[1].id, Some(RequestId::Number(112)));
    assert!(responses[1].error.is_some());
    assert_prompt_lifecycle_released(&observed, &sid);
}

#[tokio::test]
async fn two_same_session_prompts_and_cancel_in_one_write_use_earliest_barrier() {
    let executor = Arc::new(CancelThenSuccessPromptExecutor {
        calls: std::sync::atomic::AtomicUsize::new(0),
        first_entered: tokio::sync::Notify::new(),
    });
    let (handler, sid) = lifecycle_test_session(executor).await;
    let observed = handler.clone();
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let serve_task = tokio::spawn(async move { serve(server_in, server_out, handler).await });
    let input = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":121,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"first\"}}]}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"id\":122,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"second\"}}]}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{sid}\"}}}}\n"
    );
    client_in.write_all(input.as_bytes()).await.unwrap();
    drop(client_in);

    let out = tokio::time::timeout(std::time::Duration::from_secs(2), drain(client_out))
        .await
        .expect("one-write prompt queue and cancel deadlocked");
    tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
        .await
        .expect("server did not drain one-write prompt queue")
        .unwrap()
        .unwrap();
    let responses = ndjson_lines(&out)
        .into_iter()
        .map(|line| serde_json::from_slice::<JsonRpcResponse>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0].id, Some(RequestId::Number(121)));
    assert_eq!(
        responses[0].result.as_ref().unwrap()["stopReason"],
        "cancelled"
    );
    assert_eq!(responses[1].id, Some(RequestId::Number(122)));
    assert_eq!(
        responses[1].result.as_ref().unwrap()["stopReason"],
        "end_turn"
    );
    assert_prompt_lifecycle_released(&observed, &sid);
}

#[tokio::test]
async fn different_session_prompt_barriers_and_cancels_remain_independent() {
    let executor = Arc::new(CancelAwarePromptExecutor {
        entered: tokio::sync::Notify::new(),
    });
    let (handler, first_sid) = lifecycle_test_session(executor).await;
    let second_sid = handler
        .handle_request(
            "session/new",
            Some(serde_json::json!({"cwd": r"C:\rebon-second-session"})),
        )
        .await
        .unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let observed = handler.clone();
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let serve_task = tokio::spawn(async move { serve(server_in, server_out, handler).await });
    let input = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":131,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{first_sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"first session\"}}]}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"id\":132,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{second_sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"second session\"}}]}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{first_sid}\"}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{second_sid}\"}}}}\n"
    );
    client_in.write_all(input.as_bytes()).await.unwrap();
    drop(client_in);

    let out = tokio::time::timeout(std::time::Duration::from_secs(2), drain(client_out))
        .await
        .expect("different-session cancellation barriers deadlocked");
    tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
        .await
        .expect("server did not drain different-session prompts")
        .unwrap()
        .unwrap();
    let responses = ndjson_lines(&out)
        .into_iter()
        .map(|line| serde_json::from_slice::<JsonRpcResponse>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0].id, Some(RequestId::Number(131)));
    assert_eq!(responses[1].id, Some(RequestId::Number(132)));
    assert!(responses
        .iter()
        .all(|response| { response.result.as_ref().unwrap()["stopReason"] == "cancelled" }));
    assert_prompt_lifecycle_released(&observed, &first_sid);
    assert_prompt_lifecycle_released(&observed, &second_sid);
}

#[tokio::test]
async fn queued_request_cannot_block_cancel_or_other_notifications() {
    let executor = Arc::new(CancelAwarePromptExecutor {
        entered: tokio::sync::Notify::new(),
    });
    let inner = DefaultHandler::default().with_prompt_executor(executor.clone());
    let state = inner.state().clone();
    let notifications = Arc::new(Mutex::new(Vec::new()));
    let handler = NotificationRecordingHandler {
        inner,
        notifications: notifications.clone(),
    };
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let serve_task = tokio::spawn(async move { serve(server_in, server_out, handler).await });
    let mut client_out = BufReader::new(client_out);

    client_in
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/ordered-cancel"}}
"#,
        )
        .await
        .unwrap();
    let mut line = String::new();
    client_out.read_line(&mut line).await.unwrap();
    let init: JsonRpcResponse = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(init.id, Some(RequestId::Number(1)));
    line.clear();
    client_out.read_line(&mut line).await.unwrap();
    let new_session: JsonRpcResponse = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(new_session.id, Some(RequestId::Number(2)));
    let sid = new_session.result.unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let commands = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"wait\"}}]}}}}\n"
    );
    client_in.write_all(commands.as_bytes()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.entered.notified(),
    )
    .await
    .expect("prompt executor was not entered");

    let queued = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"session/list\",\"params\":{{}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{sid}\"}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"method\":\"vendor/unknown\",\"params\":{{\"value\":1}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{sid}\"}}}}\n"
    );
    client_in.write_all(queued.as_bytes()).await.unwrap();
    drop(client_in);

    line.clear();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client_out.read_line(&mut line),
    )
    .await
    .expect("cancel did not bypass the queued list request")
    .unwrap();
    let prompt: JsonRpcResponse = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(prompt.id, Some(RequestId::Number(3)));
    assert_eq!(prompt.result.unwrap()["stopReason"], "cancelled");

    line.clear();
    client_out.read_line(&mut line).await.unwrap();
    let list: JsonRpcResponse = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(list.id, Some(RequestId::Number(4)));
    assert!(list.error.is_none());

    tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
        .await
        .expect("server deadlocked while draining queued commands")
        .unwrap()
        .unwrap();
    assert_eq!(state.cancel_count(&sid), 2);
    assert!(!state.is_prompt_active(&sid));
    assert!(state.get_session(&sid).unwrap().messages.is_empty());
    assert_eq!(state.prompt_messages_capacity(&sid), Some(0));
    assert_eq!(
        *notifications.lock().unwrap(),
        vec!["session/cancel", "vendor/unknown", "session/cancel"]
    );
}

#[tokio::test]
async fn eof_after_pending_prompt_drains_its_notification_and_response() {
    let executor = Arc::new(ReleaseAndPublishExecutor {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let (publisher, rx) = rebon_agent_core::publisher::ChannelSessionUpdatePublisher::new();
    let handler = DefaultHandler::default()
        .with_prompt_executor(executor.clone())
        .with_update_publisher(Arc::new(publisher));
    handler
        .handle_request(
            "initialize",
            Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    let sid = handler
        .handle_request(
            "session/new",
            Some(serde_json::json!({"cwd":"/tmp/eof-drain"})),
        )
        .await
        .unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let serve_task = tokio::spawn(async move {
        serve_with_publisher(server_in, server_out, handler, Some(rx)).await
    });
    let prompt = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":31,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"finish after eof\"}}]}}}}\n"
    );
    client_in.write_all(prompt.as_bytes()).await.unwrap();
    drop(client_in);

    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.entered.notified(),
    )
    .await
    .expect("half-closed prompt was not accepted");
    executor.release.notify_one();

    let out = tokio::time::timeout(std::time::Duration::from_secs(2), drain(client_out))
        .await
        .expect("server did not finish draining after EOF");
    tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
        .await
        .expect("serve did not return after draining prompt outputs")
        .unwrap()
        .unwrap();

    let messages: Vec<Value> = ndjson_lines(&out)
        .into_iter()
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(messages.len(), 2, "expected notification and response");
    assert!(messages.iter().any(|message| {
        message["method"] == "session/update"
            && message["params"]["update"]["content"]["text"] == "after eof"
    }));
    assert!(messages
        .iter()
        .any(|message| { message["id"] == 31 && message["result"]["stopReason"] == "end_turn" }));
}

#[tokio::test]
async fn half_close_drains_all_accepted_requests_in_receive_order() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    client_in
        .write_all(
            br#"{"jsonrpc":"2.0","id":41,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
{"jsonrpc":"2.0","id":42,"method":"missing/one"}
{"jsonrpc":"2.0","id":43,"method":"missing/two"}
{"jsonrpc":"2.0","id":44,"method":"session/list","params":{}}
"#,
        )
        .await
        .unwrap();
    drop(client_in);

    let serve_task =
        tokio::spawn(async move { serve(server_in, server_out, DefaultHandler::default()).await });
    let out = tokio::time::timeout(std::time::Duration::from_secs(2), drain(client_out))
        .await
        .expect("queued responses did not drain after half-close");
    serve_task.await.unwrap().unwrap();
    let ids: Vec<Option<RequestId>> = ndjson_lines(&out)
        .into_iter()
        .map(|line| serde_json::from_slice::<JsonRpcResponse>(line).unwrap().id)
        .collect();
    assert_eq!(
        ids,
        vec![
            Some(RequestId::Number(41)),
            Some(RequestId::Number(42)),
            Some(RequestId::Number(43)),
            Some(RequestId::Number(44)),
        ]
    );
}

fn assert_writer_broken_pipe(result: anyhow::Result<()>) {
    let error = result.expect_err("closed writer peer must fail serve");
    let io_error = error
        .downcast_ref::<std::io::Error>()
        .expect("serve must preserve the original writer I/O error");
    assert_eq!(io_error.kind(), std::io::ErrorKind::BrokenPipe);
}

#[tokio::test]
async fn response_writer_failure_returns_io_error_without_hanging() {
    let executor = Arc::new(FixedPromptExecutor(Ok(
        rebon_agent_core::prompt_executor::PromptOutcome::end_turn(),
    )));
    let (handler, sid) = lifecycle_test_session(executor).await;
    let observed = handler.clone();
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    drop(client_out);
    let serve_task = tokio::spawn(async move { serve(server_in, server_out, handler).await });
    let prompt = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":51,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"response failure\"}}]}}}}\n"
    );
    client_in.write_all(prompt.as_bytes()).await.unwrap();

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
        .await
        .expect("response writer failure hung")
        .unwrap();
    assert_writer_broken_pipe(result);
    assert_prompt_lifecycle_released(&observed, &sid);
}

#[tokio::test]
async fn notification_writer_failure_aborts_pending_prompt_and_cleans_lifecycle() {
    let executor = Arc::new(CancelAwarePromptExecutor {
        entered: tokio::sync::Notify::new(),
    });
    let (publisher, rx) = rebon_agent_core::publisher::ChannelSessionUpdatePublisher::new();
    let handler = DefaultHandler::default()
        .with_prompt_executor(executor.clone())
        .with_update_publisher(Arc::new(publisher.clone()));
    handler
        .handle_request(
            "initialize",
            Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    let sid = handler
        .handle_request(
            "session/new",
            Some(serde_json::json!({"cwd":"/tmp/notification-write-failure"})),
        )
        .await
        .unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let observed = handler.clone();
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let serve_task = tokio::spawn(async move {
        serve_with_publisher(server_in, server_out, handler, Some(rx)).await
    });
    let prompt = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":52,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"pending\"}}]}}}}\n"
    );
    client_in.write_all(prompt.as_bytes()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.entered.notified(),
    )
    .await
    .expect("pending prompt did not start");
    drop(client_out);
    publisher
        .publish_to(
            &sid,
            rebon_proto::types::SessionUpdate::Plan {
                entries: Vec::new(),
            },
        )
        .await;
    drop(publisher);

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
        .await
        .expect("notification writer failure hung")
        .unwrap();
    assert_writer_broken_pipe(result);
    assert_prompt_lifecycle_released(&observed, &sid);
}

#[tokio::test]
async fn eof_releases_pending_permission_response_and_prompt_lifecycle() {
    let (permission_publisher, permission_rx) = ChannelPermissionRequestPublisher::new();
    let handler = DefaultHandler::default()
        .with_prompt_executor(Arc::new(PermissionWaitingExecutor))
        .with_permission_publisher(permission_publisher);
    handler
        .handle_request(
            "initialize",
            Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    let sid = handler
        .handle_request(
            "session/new",
            Some(serde_json::json!({"cwd":"/tmp/permission-eof"})),
        )
        .await
        .unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let observed = handler.clone();
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let serve_task = tokio::spawn(async move {
        serve_with_publishers(server_in, server_out, handler, None, Some(permission_rx)).await
    });
    let prompt = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":54,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"permission then eof\"}}]}}}}\n"
    );
    client_in.write_all(prompt.as_bytes()).await.unwrap();

    let mut client_out = BufReader::new(client_out);
    let mut reverse_request = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client_out.read_line(&mut reverse_request),
    )
    .await
    .expect("permission reverse request was not written")
    .unwrap();
    let reverse_request: Value = serde_json::from_str(reverse_request.trim_end()).unwrap();
    assert_eq!(reverse_request["method"], "session/request_permission");
    drop(client_in);

    let trailing = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        drain(client_out.into_inner()),
    )
    .await
    .expect("EOF left the permission-waiting prompt deadlocked");
    tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
        .await
        .expect("server did not exit after releasing permission waiter")
        .unwrap()
        .unwrap();

    let responses = ndjson_lines(&trailing);
    assert_eq!(
        responses.len(),
        1,
        "prompt must emit exactly one error response"
    );
    let response: JsonRpcResponse = serde_json::from_slice(responses[0]).unwrap();
    assert_eq!(response.id, Some(RequestId::Number(54)));
    assert!(response.result.is_none());
    assert!(response.error.as_ref().is_some_and(|error| error
        .message
        .contains("permission request response dropped")));
    assert_prompt_lifecycle_released(&observed, &sid);
}

#[tokio::test]
async fn permission_writer_failure_aborts_waiting_prompt_and_cleans_lifecycle() {
    let (permission_publisher, permission_rx) = ChannelPermissionRequestPublisher::new();
    let handler = DefaultHandler::default()
        .with_prompt_executor(Arc::new(PermissionWaitingExecutor))
        .with_permission_publisher(permission_publisher);
    handler
        .handle_request(
            "initialize",
            Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    let sid = handler
        .handle_request(
            "session/new",
            Some(serde_json::json!({"cwd":"/tmp/permission-write-failure"})),
        )
        .await
        .unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let observed = handler.clone();
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    drop(client_out);
    let serve_task = tokio::spawn(async move {
        serve_with_publishers(server_in, server_out, handler, None, Some(permission_rx)).await
    });
    let prompt = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":53,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"permission\"}}]}}}}\n"
    );
    client_in.write_all(prompt.as_bytes()).await.unwrap();

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
        .await
        .expect("permission writer failure hung")
        .unwrap();
    assert_writer_broken_pipe(result);
    assert_prompt_lifecycle_released(&observed, &sid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_cannot_cross_prompt_setup_without_seeing_registered_handle() {
    let executor = Arc::new(CancelAwarePromptExecutor {
        entered: tokio::sync::Notify::new(),
    });
    let (mut handler, sid) = lifecycle_test_session(executor.clone()).await;
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    handler.prompt_setup_hook = Some(Arc::new(move || {
        entered_tx.send(()).unwrap();
        release_rx.lock().unwrap().recv().unwrap();
    }));

    let prompt_handler = handler.clone();
    let prompt_sid = sid.clone();
    let prompt_task = tokio::spawn(async move {
        prompt_handler
            .handle_request(
                "session/prompt",
                Some(lifecycle_prompt_params(&prompt_sid, "gated setup")),
            )
            .await
    });
    tokio::task::spawn_blocking(move || entered_rx.recv())
        .await
        .unwrap()
        .unwrap();

    let cancel_started = Arc::new(tokio::sync::Notify::new());
    let cancel_started_task = cancel_started.clone();
    let cancel_handler = handler.clone();
    let cancel_sid = sid.clone();
    let cancel_task = tokio::spawn(async move {
        cancel_started_task.notify_one();
        cancel_handler
            .handle_notification(
                "session/cancel",
                Some(serde_json::json!({"sessionId": cancel_sid})),
            )
            .await;
    });
    cancel_started.notified().await;
    release_tx.send(()).unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(2), cancel_task)
        .await
        .expect("cancel remained blocked after prompt setup")
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), prompt_task)
        .await
        .expect("registered cancellation did not stop the executor")
        .unwrap()
        .unwrap();
    assert_eq!(result["stopReason"], "cancelled");
    assert_prompt_lifecycle_released(&handler, &sid);
}

#[tokio::test]
async fn setup_unwind_poison_recovers_and_releases_prompt_lifecycle() {
    let (mut handler, sid) = lifecycle_test_session(Arc::new(FixedPromptExecutor(Ok(
        rebon_agent_core::prompt_executor::PromptOutcome::end_turn(),
    ))))
    .await;
    handler.prompt_setup_hook = Some(Arc::new(|| panic!("deterministic setup unwind")));

    let panic_handler = handler.clone();
    let panic_sid = sid.clone();
    let task = tokio::spawn(async move {
        panic_handler
            .handle_request(
                "session/prompt",
                Some(lifecycle_prompt_params(&panic_sid, &"x".repeat(64 * 1024))),
            )
            .await
    });
    assert!(task.await.expect_err("setup hook must unwind").is_panic());
    assert_prompt_lifecycle_released(&handler, &sid);

    let mut recovery_handler = handler.clone();
    recovery_handler.prompt_setup_hook = None;
    let result = recovery_handler
        .handle_request(
            "session/prompt",
            Some(lifecycle_prompt_params(&sid, "after poison")),
        )
        .await
        .expect("poisoned lifecycle mutex must be recoverable");
    assert_eq!(result["stopReason"], "end_turn");
    assert_prompt_lifecycle_released(&recovery_handler, &sid);
}

#[tokio::test]
async fn normal_success_and_error_preserve_results_and_cleanup() {
    let (success_handler, success_sid) = lifecycle_test_session(Arc::new(FixedPromptExecutor(Ok(
        rebon_agent_core::prompt_executor::PromptOutcome::end_turn(),
    ))))
    .await;
    let success = success_handler
        .handle_request(
            "session/prompt",
            Some(lifecycle_prompt_params(&success_sid, "ok")),
        )
        .await
        .unwrap();
    assert_eq!(success["stopReason"], "end_turn");
    assert_prompt_lifecycle_released(&success_handler, &success_sid);

    let (error_handler, error_sid) = lifecycle_test_session(Arc::new(FixedPromptExecutor(Err(
        PromptExecutorError::Execution("deterministic failure".to_string()),
    ))))
    .await;
    let error = error_handler
        .handle_request(
            "session/prompt",
            Some(lifecycle_prompt_params(&error_sid, "fail")),
        )
        .await
        .expect_err("executor failure must remain a JSON-RPC error");
    assert_eq!(error.code, error_code::INTERNAL_ERROR);
    assert!(error.message.contains("deterministic failure"));
    assert_prompt_lifecycle_released(&error_handler, &error_sid);
}

#[tokio::test]
async fn stale_prompt_guard_cannot_clear_newer_generation() {
    use crate::server::handler::{ActivePromptCancel, PromptLifecycleGuard};

    let (handler, sid) = lifecycle_test_session(Arc::new(FixedPromptExecutor(Ok(
        rebon_agent_core::prompt_executor::PromptOutcome::end_turn(),
    ))))
    .await;
    let state = handler.state().clone();

    let old = state.begin_prompt_snapshot(&sid).unwrap();
    state.append_prompt_messages(
        &sid,
        serde_json::from_value(serde_json::json!([{"type":"text","text":"old"}])).unwrap(),
    );
    handler.active_cancels.lock().unwrap().insert(
        sid.clone(),
        ActivePromptCancel {
            generation: old.generation,
            cancel: rebon_agent_core::prompt_executor::PromptCancel::new(),
        },
    );
    let old_guard = PromptLifecycleGuard::new(
        state.clone(),
        handler.active_cancels.clone(),
        sid.clone(),
        old.generation,
    );

    // Simulate ownership moving on while the old guard is delayed.
    state.end_prompt(&sid);
    let newer = state.begin_prompt_snapshot(&sid).unwrap();
    state.append_prompt_messages(
        &sid,
        serde_json::from_value(serde_json::json!([{"type":"text","text":"new"}])).unwrap(),
    );
    handler.active_cancels.lock().unwrap().insert(
        sid.clone(),
        ActivePromptCancel {
            generation: newer.generation,
            cancel: rebon_agent_core::prompt_executor::PromptCancel::new(),
        },
    );
    let newer_guard = PromptLifecycleGuard::new(
        state.clone(),
        handler.active_cancels.clone(),
        sid.clone(),
        newer.generation,
    );

    drop(old_guard);
    assert!(state.is_prompt_active(&sid));
    assert_eq!(state.get_session(&sid).unwrap().messages.len(), 1);
    assert_eq!(
        handler
            .active_cancels
            .lock()
            .unwrap()
            .get(&sid)
            .unwrap()
            .generation,
        newer.generation
    );

    drop(newer_guard);
    assert_prompt_lifecycle_released(&handler, &sid);
}

#[async_trait]
impl PromptExecutor for RecordingPromptExecutor {
    async fn execute(
        &self,
        request: PromptRequest,
    ) -> Result<rebon_agent_core::prompt_executor::PromptOutcome, PromptExecutorError> {
        self.requests
            .lock()
            .expect("recording executor mutex poisoned")
            .push(request);
        Ok(rebon_agent_core::prompt_executor::PromptOutcome::end_turn())
    }
}

#[tokio::test]
async fn session_prompt_intercepts_every_advertised_read_only_command() {
    let executor = Arc::new(RecordingPromptExecutor::default());
    let updates = rebon_agent_core::publisher::MemorySessionUpdatePublisher::new();
    let kernel = rebon_kernel::Kernel::new();
    let _commands = provide_command_seat(&kernel);
    let handler = DefaultHandler::default()
        .with_prompt_executor(executor.clone())
        .with_kernel_scope(kernel.context().clone())
        .with_update_publisher(Arc::new(updates.clone()));
    handler
        .handle_request(
            "initialize",
            Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    let sid = handler
        .handle_request(
            "session/new",
            Some(serde_json::json!({"cwd":"/tmp/acp-read-only"})),
        )
        .await
        .unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let cases = [
        ("/status", "Session status"),
        ("/cost", "Cost estimate (local)"),
        ("/context", "Context Usage"),
        // Deliberately the substring both the populated report and the
        // "nothing discovered" fallback share: this test only proves the
        // command was intercepted. Whether the right files are listed is
        // `memory_command_reports_rules_includes_and_ancestor_instructions`.
        ("/memory", "instruction files"),
        ("/mcp", "MCP status"),
        ("/hooks PreToolUse", "Hooks: PreToolUse"),
        ("/doctor", "Doctor diagnostics (local-only)"),
    ];
    for (prompt, _) in cases {
        let result = handler
            .handle_request(
                "session/prompt",
                Some(serde_json::json!({
                    "sessionId": sid,
                    "prompt": [{"type":"text","text":prompt}],
                })),
            )
            .await
            .unwrap();
        assert_eq!(result["stopReason"], "end_turn", "prompt: {prompt}");
    }

    assert!(
        executor.requests.lock().unwrap().is_empty(),
        "read-only commands must not reach the prompt executor"
    );
    let published = updates.snapshot();
    assert_eq!(published.len(), cases.len());
    for (params, (prompt, expected)) in published.iter().zip(cases) {
        assert_eq!(params.session_id, sid);
        let SessionUpdate::AgentMessageChunk {
            content: ContentBlock::Text(text),
        } = &params.update
        else {
            panic!("{prompt} must publish one text chunk: {:?}", params.update);
        };
        assert!(
            text.text.contains(expected),
            "{prompt} output did not contain {expected:?}: {:?}",
            text.text
        );
    }
}

/// A prompt-shaped command expands into the turn, the way a terminal has
/// always run one.
///
/// Before this the ACP server sent `/hello world` to the model with the slash
/// still on it, so a skill or a plugin command typed in an editor asked the
/// model to guess what the command meant. The three cases below are the whole
/// contract: a handler that answers becomes the turn's prompt, a handler that
/// fails becomes a sentence and no turn, and a command this server cannot run
/// still reaches the model as typed.
#[tokio::test]
async fn session_prompt_expands_a_registered_prompt_command() {
    let executor = Arc::new(RecordingPromptExecutor::default());
    let updates = rebon_agent_core::publisher::MemorySessionUpdatePublisher::new();
    let kernel = rebon_kernel::Kernel::new();
    let commands = provide_command_seat(&kernel);
    let seat = kernel
        .context()
        .require::<rebon_command_seat::CommandSeatService>()
        .expect("provided above");
    seat.register(
        &commands,
        rebon_slash_commands::CommandSpec::new("hello", "Say hello")
            .aliases(["hi"])
            .surfaces(rebon_slash_commands::Surfaces::ACP_ONLY),
        rebon_command_seat::CommandHandler::Prompt(Arc::new(
            |args: &rebon_command_seat::CommandArgs| {
                Ok(format!("Please greet {} politely.", args.rest))
            },
        )),
    )
    .expect("/hello is free");
    seat.register(
        &commands,
        rebon_slash_commands::CommandSpec::new("silent", "Never answers")
            .surfaces(rebon_slash_commands::Surfaces::ACP_ONLY),
        rebon_command_seat::CommandHandler::Prompt(Arc::new(
            |_: &rebon_command_seat::CommandArgs| Err("the plugin never answered".to_string()),
        )),
    )
    .expect("/silent is free");

    let handler = DefaultHandler::default()
        .with_prompt_executor(executor.clone())
        .with_kernel_scope(kernel.context().clone())
        .with_update_publisher(Arc::new(updates.clone()));
    handler
        .handle_request(
            "initialize",
            Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    let sid = handler
        .handle_request(
            "session/new",
            Some(serde_json::json!({"cwd":"/tmp/acp-prompt-command"})),
        )
        .await
        .unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let prompt = |text: &str| {
        let handler = &handler;
        let sid = sid.clone();
        let text = text.to_string();
        async move {
            handler
                .handle_request(
                    "session/prompt",
                    Some(serde_json::json!({
                        "sessionId": sid,
                        "prompt": [{"type":"text","text": text}],
                    })),
                )
                .await
                .unwrap()
        }
    };

    // The alias resolves to the same command and hands over the same `rest`.
    prompt("/hi the reviewer").await;
    let sent = |index: usize| -> String {
        let requests = executor.requests.lock().unwrap();
        match &requests[index].prompt[0] {
            ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    };
    assert_eq!(executor.requests.lock().unwrap().len(), 1);
    assert_eq!(sent(0), "Please greet the reviewer politely.");

    // A handler that cannot answer is a sentence for the person, never a
    // prompt: sending the failure to the model would look like it worked.
    let result = prompt("/silent").await;
    assert_eq!(result["stopReason"], "end_turn");
    assert_eq!(
        executor.requests.lock().unwrap().len(),
        1,
        "a failed expansion must not start a turn"
    );
    let published = updates.snapshot();
    let SessionUpdate::AgentMessageChunk {
        content: ContentBlock::Text(text),
    } = &published.last().expect("one chunk").update
    else {
        panic!("a failure publishes one text chunk");
    };
    assert_eq!(text.text, "the plugin never answered");

    // A name nothing registered still goes to the model, which is what keeps
    // a user skill's `/name` reaching the engine that resolves it.
    prompt("/not-registered-anywhere go").await;
    assert_eq!(executor.requests.lock().unwrap().len(), 2);
    assert_eq!(sent(1), "/not-registered-anywhere go");
}

/// `/memory` reports the whole document list a session loaded, and it gets
/// that list off the `loaded-documents` seam rather than assembling one here.
/// The `REBON.md` chain alone was a silent under-report — a client saw
/// `.rebon/rules` files, `@` includes and the auto `MEMORY.md` as simply
/// absent — and which of those a session actually loaded is a question only
/// the `memory` plugin can answer.
#[tokio::test]
async fn memory_command_reports_every_document_the_seam_lists() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("repo");
    let cwd = root.join("crate");
    std::fs::create_dir_all(&cwd).unwrap();

    let listed = vec![
        root.join("REBON.md"),
        cwd.join("REBON.md"),
        cwd.join("included.md"),
        cwd.join(".rebon/rules/rule.md"),
    ];
    let kernel = rebon_kernel::Kernel::new();
    let scope = kernel.context().fork("memory");
    rebon_instructions::loaded_documents::provide(
        &scope,
        Arc::new(FixedLoadedDocuments(listed.clone())),
    )
    .expect("a fresh scope accepts the provider");
    let _commands = provide_command_seat(&kernel);

    let updates = rebon_agent_core::publisher::MemorySessionUpdatePublisher::new();
    let handler = DefaultHandler::default()
        .with_prompt_executor(Arc::new(RecordingPromptExecutor::default()))
        .with_kernel_scope(kernel.context().clone())
        .with_update_publisher(Arc::new(updates.clone()));
    handler
        .handle_request(
            "initialize",
            Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    let sid = handler
        .handle_request(
            "session/new",
            Some(serde_json::json!({"cwd": cwd.to_string_lossy()})),
        )
        .await
        .unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    handler
        .handle_request(
            "session/prompt",
            Some(serde_json::json!({
                "sessionId": sid,
                "prompt": [{"type":"text","text":"/memory"}],
            })),
        )
        .await
        .unwrap();

    let published = updates.snapshot();
    let SessionUpdate::AgentMessageChunk {
        content: ContentBlock::Text(text),
    } = &published[0].update
    else {
        panic!("/memory must publish one text chunk: {:?}", published[0]);
    };
    let report = text.text.replace('\\', "/");
    assert!(
        report.starts_with("Loaded memory/instruction files ("),
        "expected a populated report, got: {report}"
    );
    for path in &listed {
        let expected = path.to_string_lossy().replace('\\', "/");
        assert!(
            report.contains(&expected),
            "/memory did not list {expected}: {report}"
        );
    }
}

/// Stand in for `core-commands` and the plugins that register beside it.
///
/// The server reads typed slash commands off the `command-registry` seat, so
/// a test that types one has to say what is registered. The built-in table is
/// registered the way `core-commands` registers it — natively, under each
/// command's own name — plus `/memory`, whose owner is `plugins/memory` and
/// therefore out of this crate's reach.
///
/// The returned context owns the registrations: drop it and the commands are
/// gone, so callers bind it for the length of the test.
fn provide_command_seat(kernel: &rebon_kernel::Kernel) -> rebon_kernel::Context {
    let ctx = kernel.context().fork("core-commands");
    let seat = rebon_command_seat::CommandSeat::new();
    ctx.provide::<rebon_command_seat::CommandSeatService>(seat.clone())
        .expect("a fresh kernel accepts the seat");
    for spec in rebon_slash_commands::builtin_command_table() {
        let handler = rebon_command_seat::CommandHandler::Native(spec.name.clone());
        seat.register(&ctx, spec, handler)
            .expect("the built-in table has no duplicate spellings");
    }
    seat.register(
        &ctx,
        rebon_slash_commands::CommandSpec::new(
            "memory",
            "List loaded memory and instruction files",
        )
        .surfaces(rebon_slash_commands::Surfaces::ACP_ONLY),
        rebon_command_seat::CommandHandler::Native("memory".into()),
    )
    .expect("/memory is not a built-in name");
    ctx
}

/// Stands in for the `memory` plugin behind the `loaded-documents` seam:
/// this crate may not depend on it, and what discovery finds is tested
/// where discovery lives.
struct FixedLoadedDocuments(Vec<std::path::PathBuf>);

impl rebon_instructions::loaded_documents::LoadedDocuments for FixedLoadedDocuments {
    fn loaded_files(
        &self,
        _cwd: &str,
    ) -> Vec<rebon_instructions::loaded_documents::LoadedMemoryFile> {
        self.0
            .iter()
            .map(
                |path| rebon_instructions::loaded_documents::LoadedMemoryFile {
                    path: path.clone(),
                    bytes: 40,
                },
            )
            .collect()
    }
}

/// With no provider behind the seam — the `memory` plugin off, or a host
/// with no kernel — `/memory` still answers, and lists nothing rather than
/// the half of the set this crate could have found on its own.
#[test]
fn memory_command_lists_nothing_without_a_provider() {
    let record = read_only_command_session(std::path::Path::new("/repo"));
    let report = super::handler::format_acp_read_only_command(&record, "memory", "", None)
        .expect("/memory has a read-only report");
    assert!(report.contains("instruction files"), "{report}");
    assert!(!report.contains("REBON.md"), "{report}");
}

#[tokio::test]
async fn session_prompt_does_not_intercept_unadvertised_stateful_commands() {
    let executor = Arc::new(RecordingPromptExecutor::default());
    let updates = rebon_agent_core::publisher::MemorySessionUpdatePublisher::new();
    let handler = DefaultHandler::default()
        .with_prompt_executor(executor.clone())
        .with_update_publisher(Arc::new(updates.clone()));
    handler
        .handle_request(
            "initialize",
            Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    let sid = handler
        .handle_request(
            "session/new",
            Some(serde_json::json!({"cwd":"/tmp/acp-stateful"})),
        )
        .await
        .unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let result = handler
        .handle_request(
            "session/prompt",
            Some(serde_json::json!({
                "sessionId": sid,
                "prompt": [{"type":"text","text":"/compact preserve decisions"}],
            })),
        )
        .await
        .unwrap();
    assert_eq!(result["stopReason"], "end_turn");
    assert!(updates.is_empty());

    let requests = executor.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let [rebon_types::ContentBlock::Text(text)] = requests[0].prompt.as_slice() else {
        panic!("stateful command must reach the executor as one text block");
    };
    assert_eq!(text.text, "/compact preserve decisions");
}

#[tokio::test]
async fn session_prompt_passes_session_mcp_servers_to_executor() {
    let recorder = Arc::new(RecordingPromptExecutor::default());
    let handler = DefaultHandler::default().with_prompt_executor(recorder.clone());
    handler
        .handle_request(
            "initialize",
            Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    let new_result = handler
        .handle_request(
            "session/new",
            Some(serde_json::json!({
                "cwd": "/tmp/work",
                "mcpServers": [
                    {"transport":"stdio", "name":"fs", "command":"node"},
                    {"transport":"http", "name":"remote", "url":"https://example.test/mcp"}
                ]
            })),
        )
        .await
        .unwrap();
    let sid = new_result["sessionId"].as_str().unwrap().to_string();

    handler
        .handle_request(
            "session/prompt",
            Some(serde_json::json!({
                "sessionId": sid,
                "prompt": [{"type":"text","text":"use mcp"}]
            })),
        )
        .await
        .unwrap();

    let requests = recorder.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].mcp_servers.len(), 2);
    assert_eq!(requests[0].mcp_servers[0].name(), "fs");
    assert_eq!(requests[0].mcp_servers[1].name(), "remote");
}

#[tokio::test]
async fn session_prompt_before_initialize_errors() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let input = br#"{"jsonrpc":"2.0","id":9,"method":"session/prompt","params":{"sessionId":"sess-bogus","prompt":[]}}
"#;
    client_in.write_all(input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 1);
    let resp: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    assert_eq!(resp.id, Some(RequestId::Number(9)));
    let err = resp.error.expect("expected not-initialized error");
    assert_eq!(err.code, error_code::INVALID_REQUEST);
    assert!(
        err.message.contains("Not initialized"),
        "unexpected error message: {}",
        err.message
    );
}

#[tokio::test]
async fn session_prompt_unknown_session_errors() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"sess-does-not-exist","prompt":[{"type":"text","text":"hi"}]}}
"#,
        );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    assert_eq!(r2.id, Some(RequestId::Number(2)));
    let err = r2.error.expect("expected session-not-found error");
    assert_eq!(err.code, error_code::INVALID_PARAMS);
    assert!(
        err.message.contains("Session not found"),
        "unexpected error message: {}",
        err.message
    );
}

#[tokio::test]
async fn session_prompt_missing_params_returns_invalid_params() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":2,"method":"session/prompt"}
"#,
    );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    let err = r2.error.expect("expected invalid-params error");
    assert_eq!(err.code, error_code::INVALID_PARAMS);
}

#[tokio::test]
async fn session_cancel_notification_on_known_session_is_recorded() {
    // Create a session, then send `session/cancel` against it, and
    // verify that (a) no response is emitted and (b) the cancel was
    // recorded on the shared state.
    let handler = DefaultHandler::default();
    let state = handler.state().clone();

    // Pass 1: initialize + session/new.
    let sid = {
        let (mut client_in, server_in, server_out, client_out) = pipe_pair();
        let mut input = Vec::new();
        input.extend_from_slice(
                br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
            );
        input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/work"}}
"#,
        );
        client_in.write_all(&input).await.unwrap();
        drop(client_in);

        let h = handler.clone();
        let serve_task = tokio::spawn(async move {
            serve(server_in, server_out, h).await.unwrap();
        });
        let out = drain(client_out).await;
        serve_task.await.unwrap();

        let lines = ndjson_lines(&out);
        let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
        r2.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // Pass 2: session/cancel notification.
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let note = format!(
        r#"{{"jsonrpc":"2.0","method":"session/cancel","params":{{"sessionId":"{sid}"}}}}
"#
    );
    client_in.write_all(note.as_bytes()).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    assert!(
        out.is_empty(),
        "session/cancel notification must not produce a response, got {:?}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(state.cancel_count(&sid), 1);
}

#[tokio::test]
async fn session_cancel_notification_on_unknown_session_is_silent() {
    // Covers the `sessions.get(sessionId)` null-check in `cancelSession`:
    // unknown ids are silently no-ops and must not leave any cancel
    // record behind.
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let input = br#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"sess-nope"}}
"#;
    client_in.write_all(input).await.unwrap();
    drop(client_in);

    let handler = DefaultHandler::default();
    let state = handler.state().clone();
    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    assert!(out.is_empty());
    assert_eq!(state.cancel_count("sess-nope"), 0);
}

#[tokio::test]
async fn session_cancel_notification_malformed_params_does_not_crash() {
    // Malformed `params` (missing `sessionId`) must be dropped rather
    // than causing the server loop to bail: log and ignore.
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let input = br#"{"jsonrpc":"2.0","method":"session/cancel","params":{}}
"#;
    client_in.write_all(input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    assert!(out.is_empty(), "malformed notification must stay silent");
}

#[tokio::test]
async fn session_new_missing_params_returns_invalid_params() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    // `session/new` without `params` at all — after initialize has
    // already succeeded, so we know the error comes from params
    // validation and not the initialize guard.
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":2,"method":"session/new"}
"#,
    );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    let err = r2.error.expect("expected invalid-params error");
    assert_eq!(err.code, error_code::INVALID_PARAMS);
}

// ---- session/set_config_option ----

fn config_value<'a>(result: &'a Value, config_id: &str) -> Option<&'a str> {
    result["configOptions"]
        .as_array()?
        .iter()
        .find(|o| o["id"] == config_id)
        .and_then(|o| o["currentValue"].as_str())
}

#[tokio::test]
async fn session_set_config_option_before_initialize_errors() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let input = br#"{"jsonrpc":"2.0","id":1,"method":"session/set_config_option","params":{"sessionId":"sess-x","configId":"permissions","value":"plan"}}
"#;
    client_in.write_all(input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 1);
    let resp: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    let err = resp.error.expect("expected not-initialized error");
    assert_eq!(err.code, error_code::INVALID_REQUEST);
    assert!(err.message.contains("Not initialized"));
}

#[tokio::test]
async fn session_set_config_option_route_is_wired() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/set_config_option","params":{"sessionId":"sess-missing","configId":"permissions","value":"plan"}}
"#,
        );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let resp: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    assert!(
        resp.error.is_none(),
        "route must resolve, not method-not-found"
    );
    let result = resp.result.expect("expected result");
    assert_eq!(config_value(&result, "permissions"), Some("default"));
}

#[tokio::test]
async fn session_set_config_option_updates_existing_session_permission_mode() {
    let handler = DefaultHandler::default();
    let state = handler.state().clone();

    let sid = {
        let (mut client_in, server_in, server_out, client_out) = pipe_pair();
        let mut input = Vec::new();
        input.extend_from_slice(
                br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
            );
        input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/work"}}
"#,
        );
        client_in.write_all(&input).await.unwrap();
        drop(client_in);

        let h = handler.clone();
        let serve_task = tokio::spawn(async move {
            serve(server_in, server_out, h).await.unwrap();
        });
        let out = drain(client_out).await;
        serve_task.await.unwrap();
        let lines = ndjson_lines(&out);
        let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
        r2.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"session/set_config_option","params":{{"sessionId":"{sid}","configId":"permissions","value":"plan"}}}}
"#
    );
    client_in.write_all(req.as_bytes()).await.unwrap();
    drop(client_in);

    let h = handler.clone();
    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, h).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 1);
    let resp: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    assert!(resp.error.is_none());
    let result = resp.result.unwrap();
    assert_eq!(config_value(&result, "permissions"), Some("plan"));
    assert_eq!(state.get_session(&sid).unwrap().permission_mode, "plan");
    assert_eq!(
        handler
            .config_options_snapshot()
            .iter()
            .find(|option| option.id == "permissions")
            .unwrap()
            .current_value,
        "default"
    );
}

#[tokio::test]
async fn session_set_config_option_unknown_session_still_updates_shared_config() {
    let handler = DefaultHandler::default();

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/set_config_option","params":{"sessionId":"sess-missing","configId":"permissions","value":"dontAsk"}}
"#,
        );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/tmp/work"}}
"#,
    );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 3);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    assert!(r2.error.is_none());
    assert_eq!(
        config_value(&r2.result.unwrap(), "permissions"),
        Some("dontAsk")
    );

    let r3: JsonRpcResponse = serde_json::from_slice(lines[2]).unwrap();
    let result3 = r3.result.unwrap();
    assert_eq!(config_value(&result3, "permissions"), Some("dontAsk"));
}

#[tokio::test]
async fn session_new_inherits_explicit_startup_permission_mode() {
    for mode in ["plan", "bypassPermissions"] {
        let handler = DefaultHandler::default();
        handler.seed_startup_permission_mode(mode);
        let state = handler.state().clone();

        let (mut client_in, server_in, server_out, client_out) = pipe_pair();
        let input = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/work"}}
"#;
        client_in.write_all(input.as_bytes()).await.unwrap();
        drop(client_in);

        let serve_task = tokio::spawn(async move {
            serve(server_in, server_out, handler).await.unwrap();
        });
        let out = drain(client_out).await;
        serve_task.await.unwrap();

        let lines = ndjson_lines(&out);
        assert_eq!(lines.len(), 2, "mode={mode}");
        let response: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
        let result = response.result.as_ref().unwrap();
        let session_id = result["sessionId"].as_str().unwrap();
        assert_eq!(
            state.get_session(session_id).unwrap().permission_mode,
            mode,
            "mode={mode}"
        );
        assert_eq!(
            config_value(result, "permissions"),
            Some(mode),
            "mode={mode}"
        );
    }
}

#[tokio::test]
async fn session_new_does_not_inherit_session_scoped_permission_modes() {
    for mode in ["plan", "bypassPermissions"] {
        let handler = DefaultHandler::default();
        let state = handler.state().clone();

        let (mut client_in, server_in, server_out, client_out) = pipe_pair();
        let input = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
{"jsonrpc":"2.0","id":2,"method":"session/set_config_option","params":{"sessionId":"sess-missing","configId":"permissions","value":"__MODE__"}}
{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/tmp/work"}}
"#
        .replace("__MODE__", mode);
        client_in.write_all(input.as_bytes()).await.unwrap();
        drop(client_in);

        let serve_task = tokio::spawn(async move {
            serve(server_in, server_out, handler).await.unwrap();
        });
        let out = drain(client_out).await;
        serve_task.await.unwrap();

        let lines = ndjson_lines(&out);
        assert_eq!(lines.len(), 3, "mode={mode}");
        let r3: JsonRpcResponse = serde_json::from_slice(lines[2]).unwrap();
        let sid = r3.result.as_ref().unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            state.get_session(&sid).unwrap().permission_mode,
            "default",
            "mode={mode}"
        );
        assert_eq!(
            config_value(r3.result.as_ref().unwrap(), "permissions"),
            Some("default"),
            "mode={mode}"
        );
    }
}

// ---- session/load ----

use rebon_session::session_storage::project_dir_component;
/// Build a unique temp directory for a session/load test.
///
/// `tempfile` picks the unique name and deletes the tree when the
/// returned guard drops, so callers must keep it bound for the whole
/// test — `tmp.path()` borrows from it.
fn fresh_tempdir(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("rebon-acp-test-{tag}-"))
        .tempdir()
        .unwrap()
}

/// Drop a one-line-per-entry JSONL transcript into
/// `${projects_root}/${sanitize(cwd)}/${session_id}.jsonl`.
///
/// `entries` is a list of (uuid, parent_uuid, type, timestamp)
/// tuples — enough to drive the chain walker. The `message` payload
/// is stubbed to `null` because the loader only inspects the chain
/// metadata.
fn write_fixture_transcript(
    projects_root: &std::path::Path,
    cwd: &str,
    session_id: &str,
    entries: &[(&str, Option<&str>, &str, &str)],
) {
    let project_dir = projects_root.join(project_dir_component(cwd));
    std::fs::create_dir_all(&project_dir).unwrap();
    let file = project_dir.join(format!("{session_id}.jsonl"));
    let mut body = String::new();
    for (uuid, parent, ty, ts) in entries {
        let parent_json = match parent {
            Some(p) => format!("\"{p}\""),
            None => "null".into(),
        };
        body.push_str(&format!(
                "{{\"type\":\"{ty}\",\"uuid\":\"{uuid}\",\"parentUuid\":{parent_json},\"timestamp\":\"{ts}\",\"message\":null}}\n"
            ));
    }
    std::fs::write(file, body).unwrap();
}

#[tokio::test]
async fn initialize_then_session_load_happy_path() {
    // Write a 2-message transcript, drive initialize + session/load
    // against a handler whose projects_root points at the tempdir,
    // and verify the response + restored in-memory state.
    let tmp = fresh_tempdir("load-happy");
    let cwd = "/tmp/work";
    let sid = "sess-abc-123";
    write_fixture_transcript(
        tmp.path(),
        cwd,
        sid,
        &[
            ("u1", None, "user", "2025-01-01T00:00:00Z"),
            ("a1", Some("u1"), "assistant", "2025-01-01T00:00:01Z"),
        ],
    );

    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    let load_req = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/load","params":{{"sessionId":"{sid}","cwd":"{cwd}"}}}}
"#
    );
    input.extend_from_slice(load_req.as_bytes());
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r1: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    assert!(r1.error.is_none(), "initialize must succeed");
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    assert_eq!(r2.id, Some(RequestId::Number(2)));
    assert!(
        r2.error.is_none(),
        "session/load must succeed; got error {:?}",
        r2.error
    );
    let result = r2.result.expect("session/load result");
    assert_eq!(result["sessionId"], sid);

    // Shared state was populated: the loaded session is now known,
    // the cwd matches the request, and the transcript chain has the
    // two fixture entries in oldest-first order.
    let rec = state.get_session(sid).expect("session must be registered");
    assert_eq!(rec.id, sid);
    assert_eq!(rec.cwd, cwd);
    assert_eq!(rec.loaded_transcript.len(), 2);
    assert_eq!(rec.loaded_transcript[0].uuid, "u1");
    assert_eq!(rec.loaded_transcript[1].uuid, "a1");
    assert!(
        rec.messages.is_empty(),
        "loaded sessions start without prompt blocks"
    );
    assert!(rec.title.is_none());
}

#[tokio::test]
async fn session_load_inherits_explicit_startup_permission_mode() {
    let tmp = fresh_tempdir("load-startup-permission");
    let cwd = "/tmp/work";
    let sid = "sess-startup-permission";
    write_fixture_transcript(
        tmp.path(),
        cwd,
        sid,
        &[("u1", None, "user", "2025-01-01T00:00:00Z")],
    );

    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    handler.seed_startup_permission_mode("bypassPermissions");
    handler
        .handle_request(
            "initialize",
            Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();

    let result = handler
        .handle_request(
            "session/load",
            Some(serde_json::json!({"sessionId":sid,"cwd":cwd})),
        )
        .await
        .unwrap();

    assert_eq!(
        handler.state().get_session(sid).unwrap().permission_mode,
        "bypassPermissions"
    );
    assert_eq!(
        config_value(&result, "permissions"),
        Some("bypassPermissions")
    );
}

#[tokio::test]
async fn initialize_then_session_load_with_mcp_servers_succeeds() {
    let tmp = fresh_tempdir("load-mcp-servers");
    let cwd = "/tmp/work";
    let sid = "sess-mcp";
    write_fixture_transcript(
        tmp.path(),
        cwd,
        sid,
        &[
            ("u1", None, "user", "2024-01-01T00:00:00.000Z"),
            ("a1", Some("u1"), "assistant", "2024-01-01T00:00:01.000Z"),
        ],
    );
    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"sess-mcp","cwd":"/tmp/work","mcpServers":[{"transport":"stdio","name":"fs","command":"node"}]}}
"#,
        );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    assert!(r2.error.is_none(), "session/load should accept mcpServers");
    assert_eq!(state.get_session(sid).unwrap().mcp_servers.len(), 1);
}

#[tokio::test]
async fn session_load_empty_mcp_servers_replaces_existing_config() {
    let tmp = fresh_tempdir("load-mcp-servers");
    let cwd = "/tmp/work";
    let sid = "sess-mcp";
    write_fixture_transcript(
        tmp.path(),
        cwd,
        sid,
        &[
            ("u1", None, "user", "2024-01-01T00:00:00.000Z"),
            ("a1", Some("u1"), "assistant", "2024-01-01T00:00:01.000Z"),
        ],
    );
    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"sess-mcp","cwd":"/tmp/work","mcpServers":[{"transport":"stdio","name":"fs","command":"node"}]}}
"#,
        );
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":3,"method":"session/load","params":{"sessionId":"sess-mcp","cwd":"/tmp/work","mcpServers":[]}}
"#,
        );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 3);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    assert!(r2.error.is_none(), "session/load should accept mcpServers");
    let r3: JsonRpcResponse = serde_json::from_slice(lines[2]).unwrap();
    assert!(
        r3.error.is_none(),
        "empty session/load mcpServers should replace config"
    );
    assert!(state.get_session(sid).unwrap().mcp_servers.is_empty());
}

#[tokio::test]
async fn session_load_before_initialize_errors() {
    // No initialize call — must produce INVALID_REQUEST and leave
    // the session map untouched.
    let tmp = fresh_tempdir("load-before-init");
    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let req = br#"{"jsonrpc":"2.0","id":9,"method":"session/load","params":{"sessionId":"sess-x","cwd":"/tmp/x"}}
"#;
    client_in.write_all(req).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 1);
    let resp: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    assert_eq!(resp.id, Some(RequestId::Number(9)));
    let err = resp.error.expect("expected not-initialized error");
    assert_eq!(err.code, error_code::INVALID_REQUEST);
    assert!(
        err.message.contains("Not initialized"),
        "unexpected error message: {}",
        err.message
    );
    assert_eq!(state.session_count(), 0);
}

#[tokio::test]
async fn session_load_unknown_session_errors() {
    // Initialize succeeds, then session/load asks for a session id
    // that has no JSONL file on disk. Must return INVALID_PARAMS
    // "Session not found: …".
    let tmp = fresh_tempdir("load-missing");
    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"sess-missing","cwd":"/tmp/x"}}
"#,
        );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    let err = r2.error.expect("expected session-not-found error");
    assert_eq!(err.code, error_code::INVALID_PARAMS);
    assert!(
        err.message.contains("Session not found"),
        "unexpected error message: {}",
        err.message
    );
    assert!(err.message.contains("sess-missing"));
    // No session was inserted.
    assert_eq!(state.session_count(), 0);
}

#[tokio::test]
async fn session_load_empty_transcript_returns_session_not_found() {
    // JSONL file exists but contains only blank lines + garbage, so the
    // parsed message map ends up empty and there is no leaf to return.
    let tmp = fresh_tempdir("load-empty");
    let cwd = "/tmp/work";
    let sid = "sess-empty";
    let project_dir = tmp.path().join(project_dir_component(cwd));
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(project_dir.join(format!("{sid}.jsonl")), "\n\n{not-json}\n").unwrap();

    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/load","params":{{"sessionId":"{sid}","cwd":"{cwd}"}}}}
"#
    );
    input.extend_from_slice(req.as_bytes());
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    let err = r2.error.expect("expected session-not-found error");
    assert_eq!(err.code, error_code::INVALID_PARAMS);
    assert!(err.message.contains("Session not found"));
}

#[tokio::test]
async fn session_load_missing_params_returns_invalid_params() {
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":2,"method":"session/load"}
"#,
    );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    let err = r2.error.expect("expected invalid-params error");
    assert_eq!(err.code, error_code::INVALID_PARAMS);
}

#[tokio::test]
async fn session_load_malformed_params_returns_invalid_params() {
    // `params` present but `sessionId` is the wrong type (number
    // instead of string). Serde must reject it before we ever touch
    // the filesystem.
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":42,"cwd":"/tmp/x"}}
"#,
        );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, DefaultHandler::default())
            .await
            .unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    let err = r2.error.expect("expected invalid-params error");
    assert_eq!(err.code, error_code::INVALID_PARAMS);
    assert!(
        err.message.contains("session/load params"),
        "unexpected error message: {}",
        err.message
    );
}

#[tokio::test]
async fn session_load_existing_in_memory_session_short_circuits_disk() {
    // Create a session via session/new, then issue session/load
    // against the same id. The in-memory short-circuit branch should
    // preserve its canonical cwd and return the existing record without
    // touching disk, so no fixture file is needed.
    let tmp = fresh_tempdir("load-cached");
    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    // Pass 1: initialize + session/new → grab the minted session id.
    let sid = {
        let (mut client_in, server_in, server_out, client_out) = pipe_pair();
        let mut input = Vec::new();
        input.extend_from_slice(
                br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
            );
        input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/original"}}
"#,
        );
        client_in.write_all(&input).await.unwrap();
        drop(client_in);

        let h = handler.clone();
        let serve_task = tokio::spawn(async move {
            serve(server_in, server_out, h).await.unwrap();
        });
        let out = drain(client_out).await;
        serve_task.await.unwrap();
        let lines = ndjson_lines(&out);
        let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
        r2.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // Pass 2: session/load with the same id but a different cwd. The
    // handler returns the existing record without changing its storage cwd;
    // the fixture file is intentionally absent to exercise the no-disk path.
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"session/load","params":{{"sessionId":"{sid}","cwd":"/tmp/updated"}}}}
"#
    );
    client_in.write_all(req.as_bytes()).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 1);
    let resp: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    assert!(
        resp.error.is_none(),
        "session/load on an in-memory session must succeed"
    );
    assert_eq!(resp.result.unwrap()["sessionId"], sid);

    let rec = state.get_session(&sid).expect("session must still exist");
    assert_eq!(rec.cwd, "/tmp/original");
    assert_eq!(state.session_count(), 1);
}

#[tokio::test]
async fn initialize_then_session_load_then_session_prompt_happy_path() {
    // Compatibility check: a loaded session must be promptable.
    // Verifies that session/load → session/prompt flows through
    // the same `begin_prompt` / `append_prompt_messages` path used
    // by `session/new` → `session/prompt`, without trampling
    // `loaded_transcript`.
    let tmp = fresh_tempdir("load-then-prompt");
    let cwd = "/tmp/work";
    let sid = "sess-load-prompt";
    write_fixture_transcript(
        tmp.path(),
        cwd,
        sid,
        &[
            ("u1", None, "user", "2025-06-06T00:00:00Z"),
            ("a1", Some("u1"), "assistant", "2025-06-06T00:00:01Z"),
        ],
    );

    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    let load = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/load","params":{{"sessionId":"{sid}","cwd":"{cwd}"}}}}
"#
    );
    input.extend_from_slice(load.as_bytes());
    let prompt = format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"{sid}","prompt":[{{"type":"text","text":"continue"}}]}}}}
"#
    );
    input.extend_from_slice(prompt.as_bytes());
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 3);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    assert!(r2.error.is_none(), "session/load must succeed");
    let r3: JsonRpcResponse = serde_json::from_slice(lines[2]).unwrap();
    assert!(
        r3.error.is_none(),
        "session/prompt against loaded session must succeed; got {:?}",
        r3.error
    );
    assert_eq!(r3.result.unwrap()["stopReason"], "end_turn");

    // The loaded chain remains available for presentation while the completed
    // prompt's duplicate diagnostic payload has been released.
    let rec = state.get_session(sid).unwrap();
    assert_eq!(rec.loaded_transcript.len(), 2);
    assert!(rec.messages.is_empty());
    assert!(!state.is_prompt_active(sid));
}

#[tokio::test]
async fn initialize_then_session_load_then_session_cancel_notification() {
    // session/cancel against a loaded session must increment the
    // cancel counter for a known id. Regression guard for "cancel only
    // works against session/new sessions".
    let tmp = fresh_tempdir("load-then-cancel");
    let cwd = "/tmp/work";
    let sid = "sess-load-cancel";
    write_fixture_transcript(
        tmp.path(),
        cwd,
        sid,
        &[("only", None, "user", "2025-06-06T00:00:00Z")],
    );

    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    let load = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/load","params":{{"sessionId":"{sid}","cwd":"{cwd}"}}}}
"#
    );
    input.extend_from_slice(load.as_bytes());
    let cancel = format!(
        r#"{{"jsonrpc":"2.0","method":"session/cancel","params":{{"sessionId":"{sid}"}}}}
"#
    );
    input.extend_from_slice(cancel.as_bytes());
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    // Two responses (initialize, session/load) — the cancel
    // notification is silent, just like for sessions created via
    // session/new.
    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    assert_eq!(state.cancel_count(sid), 1);
}

#[tokio::test]
async fn session_load_idempotent_second_call_returns_same_id() {
    // Issuing session/load twice with the same id and cwd must
    // succeed both times — the second call hits the in-memory
    // short-circuit. We delete the fixture file between calls to
    // prove the second one doesn't reach disk.
    let tmp = fresh_tempdir("load-idempotent");
    let cwd = "/tmp/work";
    let sid = "sess-twice";
    write_fixture_transcript(
        tmp.path(),
        cwd,
        sid,
        &[("u1", None, "user", "2025-07-07T00:00:00Z")],
    );

    // Pass 1: initialize + first load.
    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    {
        let (mut client_in, server_in, server_out, client_out) = pipe_pair();
        let mut input = Vec::new();
        input.extend_from_slice(
                br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
            );
        let load = format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"session/load","params":{{"sessionId":"{sid}","cwd":"{cwd}"}}}}
"#
        );
        input.extend_from_slice(load.as_bytes());
        client_in.write_all(&input).await.unwrap();
        drop(client_in);

        let h = handler.clone();
        let serve_task = tokio::spawn(async move {
            serve(server_in, server_out, h).await.unwrap();
        });
        let out = drain(client_out).await;
        serve_task.await.unwrap();
        let lines = ndjson_lines(&out);
        assert_eq!(lines.len(), 2);
        let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
        assert!(r2.error.is_none());
    }

    // Yank the file so any disk lookup would fail.
    let pdir = tmp.path().join(project_dir_component(cwd));
    std::fs::remove_file(pdir.join(format!("{sid}.jsonl"))).unwrap();

    // Pass 2: second load — must still succeed via in-memory
    // short-circuit.
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let load = format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"session/load","params":{{"sessionId":"{sid}","cwd":"{cwd}"}}}}
"#
    );
    client_in.write_all(load.as_bytes()).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 1);
    let resp: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    assert!(resp.error.is_none(), "second load must succeed");
    assert_eq!(resp.result.unwrap()["sessionId"], sid);
}

#[tokio::test]
async fn session_load_two_distinct_sessions_in_one_connection() {
    // Two unrelated sessions loaded back-to-back. Both must succeed
    // and the state map must hold both records — guard against any
    // refactor that accidentally caches or globally-singletons the
    // session map.
    let tmp = fresh_tempdir("load-two-distinct");
    let cwd = "/tmp/work";
    let sid_a = "sess-aaa";
    let sid_b = "sess-bbb";
    write_fixture_transcript(
        tmp.path(),
        cwd,
        sid_a,
        &[("u1", None, "user", "2025-08-08T00:00:00Z")],
    );
    write_fixture_transcript(
        tmp.path(),
        cwd,
        sid_b,
        &[("v1", None, "user", "2025-08-08T00:00:01Z")],
    );

    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
            format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"session/load","params":{{"sessionId":"{sid_a}","cwd":"{cwd}"}}}}
"#
            )
            .as_bytes(),
        );
    input.extend_from_slice(
            format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"session/load","params":{{"sessionId":"{sid_b}","cwd":"{cwd}"}}}}
"#
            )
            .as_bytes(),
        );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 3);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    let r3: JsonRpcResponse = serde_json::from_slice(lines[2]).unwrap();
    assert_eq!(r2.result.unwrap()["sessionId"], sid_a);
    assert_eq!(r3.result.unwrap()["sessionId"], sid_b);
    assert_eq!(state.session_count(), 2);
    assert!(state.get_session(sid_a).is_some());
    assert!(state.get_session(sid_b).is_some());
}

#[tokio::test]
async fn session_load_over_content_length_framing() {
    // Compatibility / framing invariant: session/load works over
    // Content-Length framing the same way it does over NDJSON.
    // Matches `content_length_framing_is_mirrored_in_response`,
    // but for the new method.
    let tmp = fresh_tempdir("load-content-length");
    let cwd = "/tmp/work";
    let sid = "sess-clf";
    write_fixture_transcript(
        tmp.path(),
        cwd,
        sid,
        &[("only", None, "user", "2025-09-09T00:00:00Z")],
    );

    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let init = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}"#;
    let load_body = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/load","params":{{"sessionId":"{sid}","cwd":"{cwd}"}}}}"#
    );
    let load_bytes = load_body.as_bytes();
    let mut framed = format!("Content-Length: {}\r\n\r\n", init.len()).into_bytes();
    framed.extend_from_slice(init);
    framed.extend_from_slice(format!("Content-Length: {}\r\n\r\n", load_bytes.len()).as_bytes());
    framed.extend_from_slice(load_bytes);
    client_in.write_all(&framed).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    // Two Content-Length-framed responses concatenated.
    assert!(
        out.starts_with(b"Content-Length:"),
        "expected Content-Length framing, got {:?}",
        String::from_utf8_lossy(&out)
    );

    // Pull both bodies out of the framed stream.
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    let mut cursor = 0usize;
    while cursor < out.len() {
        // Find the next "Content-Length:" header.
        let header_start = cursor;
        let header_end = out[header_start..]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| header_start + i)
            .expect("expected header terminator");
        let header_str = std::str::from_utf8(&out[header_start..header_end]).unwrap();
        let len: usize = header_str
            .trim_start_matches("Content-Length:")
            .trim()
            .parse()
            .unwrap();
        let body_start = header_end + 4;
        let body_end = body_start + len;
        bodies.push(out[body_start..body_end].to_vec());
        cursor = body_end;
    }
    assert_eq!(bodies.len(), 2);

    let r1: JsonRpcResponse = serde_json::from_slice(&bodies[0]).unwrap();
    assert!(r1.error.is_none(), "initialize must succeed");
    let r2: JsonRpcResponse = serde_json::from_slice(&bodies[1]).unwrap();
    assert!(r2.error.is_none(), "session/load over CL must succeed");
    assert_eq!(r2.result.unwrap()["sessionId"], sid);
}

#[tokio::test]
async fn session_load_response_restores_command_and_config_metadata() {
    // A restored client needs the same command/config metadata that session/new
    // returned; transcript internals and cwd/title still stay off the wire.
    let tmp = fresh_tempdir("load-shape");
    let cwd = "/tmp/work";
    let sid = "sess-shape";
    write_fixture_transcript(
        tmp.path(),
        cwd,
        sid,
        &[("u1", None, "user", "2025-10-10T00:00:00Z")],
    );

    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    let load = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/load","params":{{"sessionId":"{sid}","cwd":"{cwd}"}}}}
"#
    );
    input.extend_from_slice(load.as_bytes());
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    let result = r2.result.expect("result");
    assert_eq!(result["sessionId"], sid);
    let commands = result["slashCommands"]
        .as_array()
        .expect("slashCommands must be restored");
    // The compiled-in table; a booted server adds the memory plugin's
    // `/memory` (see `acp_advertised_commands_are_exactly_the_implemented_set`).
    assert_eq!(commands.len(), 8);
    assert!(commands.iter().any(|command| command["name"] == "status"));
    assert!(commands.iter().any(|command| command["name"] == "doctor"));
    assert!(commands.iter().any(|command| {
        command["name"] == "ultrawork" && command["aliases"] == serde_json::json!(["ulw"])
    }));
    let options = result["configOptions"]
        .as_array()
        .expect("configOptions must be restored");
    assert!(options.iter().any(|option| option["id"] == "permissions"));
    let obj = result.as_object().expect("result must be an object");
    let keys = obj
        .keys()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        keys,
        std::collections::HashSet::from(["sessionId", "configOptions", "slashCommands"]),
        "session/load must not leak transcript internals: {obj:?}"
    );
}

#[tokio::test]
async fn session_load_cwd_with_special_chars_routes_to_sanitized_dir() {
    // End-to-end check that the dispatch path actually composes
    // ${root}/${sanitize(cwd)}/${sid}.jsonl. The fixture is
    // written under the sanitized name, and the cwd in the request
    // contains characters that the regex must rewrite.
    let tmp = fresh_tempdir("load-sanitize");
    let cwd = "/path with space/foo:bar";
    let sid = "sess-special";
    write_fixture_transcript(
        tmp.path(),
        cwd,
        sid,
        &[("u1", None, "user", "2025-11-11T00:00:00Z")],
    );
    // Sanity-check the fixture really landed under the sanitized
    // component name (the helper does the same `project_dir_component`
    // call the loader will do, so this is a self-consistency assertion).
    assert!(tmp
        .path()
        .join("-path-with-space-foo-bar")
        .join(format!("{sid}.jsonl"))
        .exists());

    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    // Embed the cwd via serde_json::to_string to dodge any escaping
    // landmines in the literal raw-string form.
    let params = serde_json::json!({
        "sessionId": sid,
        "cwd": cwd,
    });
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/load","params":{}}}
"#,
        serde_json::to_string(&params).unwrap()
    );
    input.extend_from_slice(req.as_bytes());
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    assert!(
        r2.error.is_none(),
        "session/load with sanitized cwd must succeed; got {:?}",
        r2.error
    );
    assert_eq!(r2.result.unwrap()["sessionId"], sid);
}

#[tokio::test]
async fn mixed_session_lifecycle_new_load_prompt_cancel() {
    // Comprehensive smoke test that all four ACP session methods
    // coexist on a single dispatcher instance:
    //
    //   1. initialize
    //   2. session/new          → mints sid_new
    //   3. session/load sid_lod (different cwd, fixture file)
    //   4. session/prompt against sid_new
    //   5. session/cancel against sid_lod
    //
    // Regression guard for "new method's state plumbing accidentally
    // breaks the old methods' state plumbing".
    let tmp = fresh_tempdir("mixed");
    let cwd_load = "/tmp/loaded";
    let sid_lod = "sess-loaded-mix";
    write_fixture_transcript(
        tmp.path(),
        cwd_load,
        sid_lod,
        &[("rl", None, "user", "2025-12-12T00:00:00Z")],
    );

    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    // Pass 1: initialize + session/new + session/load → grab sid_new.
    let sid_new = {
        let (mut client_in, server_in, server_out, client_out) = pipe_pair();
        let mut input = Vec::new();
        input.extend_from_slice(
                br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
            );
        input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/fresh"}}
"#,
        );
        input.extend_from_slice(
                format!(
                    r#"{{"jsonrpc":"2.0","id":3,"method":"session/load","params":{{"sessionId":"{sid_lod}","cwd":"{cwd_load}"}}}}
"#
                )
                .as_bytes(),
            );
        client_in.write_all(&input).await.unwrap();
        drop(client_in);

        let h = handler.clone();
        let serve_task = tokio::spawn(async move {
            serve(server_in, server_out, h).await.unwrap();
        });
        let out = drain(client_out).await;
        serve_task.await.unwrap();
        let lines = ndjson_lines(&out);
        assert_eq!(lines.len(), 3);
        let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
        let r3: JsonRpcResponse = serde_json::from_slice(lines[2]).unwrap();
        assert!(r2.error.is_none());
        assert!(r3.error.is_none());
        assert_eq!(r3.result.as_ref().unwrap()["sessionId"], sid_lod);
        r2.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // Pass 2: prompt against sid_new + cancel against sid_lod.
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let prompt = format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{{"sessionId":"{sid_new}","prompt":[{{"type":"text","text":"hi"}}]}}}}
"#
    );
    let cancel = format!(
        r#"{{"jsonrpc":"2.0","method":"session/cancel","params":{{"sessionId":"{sid_lod}"}}}}
"#
    );
    let mut input = Vec::new();
    input.extend_from_slice(prompt.as_bytes());
    input.extend_from_slice(cancel.as_bytes());
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    // One response (prompt). Cancel is silent.
    assert_eq!(lines.len(), 1);
    let r4: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    assert!(r4.error.is_none(), "prompt on sid_new must succeed");
    assert_eq!(r4.result.unwrap()["stopReason"], "end_turn");

    assert_eq!(state.session_count(), 2);
    let new_rec = state.get_session(&sid_new).unwrap();
    assert_eq!(new_rec.cwd, "/tmp/fresh");
    assert!(
        new_rec.messages.is_empty(),
        "completed prompt snapshot released"
    );
    assert!(new_rec.loaded_transcript.is_empty());

    let lod_rec = state.get_session(sid_lod).unwrap();
    assert_eq!(lod_rec.cwd, cwd_load);
    assert_eq!(lod_rec.loaded_transcript.len(), 1);
    assert!(lod_rec.messages.is_empty());
    assert_eq!(state.cancel_count(sid_lod), 1);
    assert_eq!(state.cancel_count(&sid_new), 0);
}

#[tokio::test]
async fn session_load_picks_latest_timestamp_leaf() {
    // Fixture with two concurrent leaves (different parents). The
    // loader must pick the lexicographically-latest timestamp leaf
    // and walk from there. `u2` wins because its timestamp is later.
    let tmp = fresh_tempdir("load-latest-leaf");
    let cwd = "/tmp/work";
    let sid = "sess-latest";
    write_fixture_transcript(
        tmp.path(),
        cwd,
        sid,
        &[
            ("root", None, "user", "2025-01-01T00:00:00Z"),
            ("a1", Some("root"), "assistant", "2025-01-01T00:00:01Z"),
            ("u2", Some("a1"), "user", "2025-01-01T00:00:02Z"),
            // Dangling branch off `root` — older timestamp, should lose.
            (
                "branch",
                Some("root"),
                "assistant",
                "2025-01-01T00:00:00.500Z",
            ),
        ],
    );

    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/load","params":{{"sessionId":"{sid}","cwd":"{cwd}"}}}}
"#
    );
    input.extend_from_slice(req.as_bytes());
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    assert!(r2.error.is_none(), "load must succeed");

    let rec = state.get_session(sid).unwrap();
    let uuids: Vec<_> = rec
        .loaded_transcript
        .iter()
        .map(|e| e.uuid.clone())
        .collect();
    assert_eq!(uuids, vec!["root", "a1", "u2"]);
}

#[tokio::test]
async fn session_load_walks_past_trailing_system_terminal_end_to_end() {
    // Dispatch-level regression guard for The behavioral leaf-walk
    // fix: a transcript whose only terminal is a `system` line —
    // with a user/assistant ancestor chain behind it — must still
    // load successfully, returning the user+assistant pair as the
    // restored transcript. Before the fix the Rust handler rejected
    // this fixture as `Session not found`, because the leaf
    // selector only considered terminals whose own type was
    // `user`/`assistant`.
    //
    // Topology matches the concrete divergent fixture called out by
    // the verifier:
    //
    //   u1 (user, root)
    //    └── a1 (assistant, parent=u1)
    //         └── sys1 (system, parent=a1)   <- only terminal
    let tmp = fresh_tempdir("load-trailing-system");
    let cwd = "/tmp/work";
    let sid = "sess-trailing-sys";
    write_fixture_transcript(
        tmp.path(),
        cwd,
        sid,
        &[
            ("u1", None, "user", "2025-07-07T00:00:00Z"),
            ("a1", Some("u1"), "assistant", "2025-07-07T00:00:01Z"),
            ("sys1", Some("a1"), "system", "2025-07-07T00:00:02Z"),
        ],
    );

    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/load","params":{{"sessionId":"{sid}","cwd":"{cwd}"}}}}
"#
    );
    input.extend_from_slice(req.as_bytes());
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    assert!(
        r2.error.is_none(),
        "session/load must succeed for a transcript whose only terminal is system; got {:?}",
        r2.error
    );
    assert_eq!(r2.result.unwrap()["sessionId"], sid);

    let rec = state.get_session(sid).expect("session must be registered");
    let uuids: Vec<_> = rec
        .loaded_transcript
        .iter()
        .map(|e| e.uuid.clone())
        .collect();
    assert_eq!(
            uuids,
            vec!["u1", "a1"],
            "loaded chain must walk past the trailing system terminal to the nearest user/assistant ancestor"
        );
    // The system terminal must not have leaked into the restored
    // chain, even though it seeded the walk.
    assert!(rec
        .loaded_transcript
        .iter()
        .all(|e| e.entry_type != "system"));
}

#[tokio::test]
async fn session_load_empty_cwd_falls_back_to_default_cwd() {
    // When params.cwd is blank, `session/load` must fall back through
    // `DefaultHandler::default_cwd` → process cwd, mirroring
    // `session/new`. The fixture transcript lives under the sanitized
    // fallback cwd; with the old unchanged-empty-path behaviour the
    // handler would look under `${root}/<empty>/<sid>.jsonl` and miss
    // it entirely.
    let tmp = fresh_tempdir("load-empty-cwd");
    let fallback = "/tmp/fallback-work";
    let sid = "sess-empty-cwd";
    write_fixture_transcript(
        tmp.path(),
        fallback,
        sid,
        &[
            ("u1", None, "user", "2025-08-08T00:00:00Z"),
            ("a1", Some("u1"), "assistant", "2025-08-08T00:00:01Z"),
        ],
    );

    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        default_cwd: Some(fallback.to_string()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/load","params":{{"sessionId":"{sid}","cwd":""}}}}
"#
    );
    input.extend_from_slice(req.as_bytes());
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 2);
    let r1: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    assert!(r1.error.is_none(), "initialize must succeed");
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    assert!(
        r2.error.is_none(),
        "session/load with empty cwd must fall back to default_cwd; got {:?}",
        r2.error
    );
    assert_eq!(r2.result.unwrap()["sessionId"], sid);

    let rec = state.get_session(sid).expect("session must be registered");
    // The persisted cwd is the fallback.
    assert_eq!(rec.cwd, fallback);
    assert_eq!(rec.loaded_transcript.len(), 2);
}

#[tokio::test]
async fn session_load_empty_cwd_on_existing_session_preserves_prior_cwd() {
    // The in-memory short-circuit uses the RAW request cwd for the
    // update check, so an
    // empty-string cwd must NOT overwrite the existing session's
    // cwd — even though the new disk-path fallback would otherwise
    // resolve the empty string to something non-empty.
    //
    // Regression guard against a subtle bug path: if the handler
    // pre-resolved the cwd *before* the existence check, every
    // empty-cwd reload would clobber the live record's cwd with
    // `default_cwd`.
    let tmp = fresh_tempdir("load-empty-cwd-existing");
    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        default_cwd: Some("/tmp/fallback".to_string()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    // Pass 1: initialize + session/new (with an explicit cwd) to
    // populate the in-memory map.
    let sid = {
        let (mut client_in, server_in, server_out, client_out) = pipe_pair();
        let mut input = Vec::new();
        input.extend_from_slice(
                br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
            );
        input.extend_from_slice(
                br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/original-work"}}
"#,
            );
        client_in.write_all(&input).await.unwrap();
        drop(client_in);

        let h = handler.clone();
        let serve_task = tokio::spawn(async move {
            serve(server_in, server_out, h).await.unwrap();
        });
        let out = drain(client_out).await;
        serve_task.await.unwrap();
        let lines = ndjson_lines(&out);
        let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
        r2.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // Pass 2: session/load with an empty-string cwd against the
    // same id. The handler must NOT touch the live cwd.
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"session/load","params":{{"sessionId":"{sid}","cwd":""}}}}
"#
    );
    client_in.write_all(req.as_bytes()).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 1);
    let resp: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    assert!(
        resp.error.is_none(),
        "empty-cwd reload of an in-memory session must succeed"
    );
    assert_eq!(resp.result.unwrap()["sessionId"], sid);

    let rec = state.get_session(&sid).expect("session must still exist");
    assert_eq!(
        rec.cwd, "/tmp/original-work",
        "empty-string cwd must not overwrite the existing session cwd"
    );
    assert_eq!(state.session_count(), 1);
}

/// A browser opening the session a terminal is writing is the exact failure
/// this refusal exists for: before it, both sides appended to one transcript
/// and the chain forked on disk.
#[tokio::test]
async fn session_load_refuses_a_session_another_process_owns() {
    let tmp = fresh_tempdir("load-owned-elsewhere");
    let cwd = "/tmp/owned";
    let sid = "sess-owned-elsewhere";
    write_fixture_transcript(
        tmp.path(),
        cwd,
        sid,
        &[("u1", None, "user", "2025-01-01T00:00:00Z")],
    );
    // Stand in for the other process: hold the session's active lock for the
    // duration of the request.
    let _held =
        rebon_session::session_storage::try_acquire_session_active_lock(tmp.path(), cwd, sid)
            .unwrap()
            .expect("the fixture session starts free");

    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    }
    .with_session_ownership();

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
        format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"session/load","params":{{"sessionId":"{sid}","cwd":"{cwd}"}}}}
"#
        )
        .as_bytes(),
    );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    let err = r2.error.expect("session/load must be refused");
    assert_eq!(err.code, error_code::SESSION_OWNED_ELSEWHERE);
    assert_eq!(err.data.as_ref().unwrap()["owner"]["sessionId"], sid);
}

/// The same fact, offered before the client tries: a list entry says whether
/// the session is being written and whether this server is the one writing it.
#[tokio::test]
async fn session_list_reports_a_session_another_process_owns_as_active() {
    let tmp = fresh_tempdir("list-owner-meta");
    let cwd = "/tmp/list-owner";
    let sid = "sess-list-owner";
    write_empty_session_file(tmp.path(), cwd, sid);
    let _held =
        rebon_session::session_storage::try_acquire_session_active_lock(tmp.path(), cwd, sid)
            .unwrap()
            .expect("the fixture session starts free");

    let (_r1, r2) = drive_list(tmp.path(), &format!(r#"{{"cwd":"{cwd}"}}"#), None).await;

    let result = r2.result.unwrap();
    let session = &result["sessions"][0];
    assert_eq!(session["sessionId"], sid);
    assert_eq!(session["_meta"]["rebon"]["owner"]["active"], true);
    assert_eq!(session["_meta"]["rebon"]["owner"]["heldHere"], false);
}

// ---- session/list ----

/// Drop an empty `.jsonl` file under the sanitized cwd directory so
/// the scanner has something to pick up. Returns the file path so
/// tests that care about mtime can stat it.
fn write_empty_session_file(
    projects_root: &std::path::Path,
    cwd: &str,
    sid: &str,
) -> std::path::PathBuf {
    let project_dir = projects_root.join(project_dir_component(cwd));
    std::fs::create_dir_all(&project_dir).unwrap();
    let p = project_dir.join(format!("{sid}.jsonl"));
    std::fs::write(&p, b"").unwrap();
    p
}

/// Extract the `sessionId` strings from a parsed `session/list` result.
fn list_result_ids(result: &Value) -> std::collections::HashSet<String> {
    result["sessions"]
        .as_array()
        .expect("sessions must be an array")
        .iter()
        .map(|s| s["sessionId"].as_str().unwrap().to_string())
        .collect()
}

/// Drive initialize + `session/list` with the given params JSON
/// literal (or `"null"` to omit params entirely) against a
/// freshly-constructed handler whose projects_root points at `tmp`.
/// Returns the parsed session/list response's result value.
async fn drive_list(
    tmp: &std::path::Path,
    params_literal: &str,
    default_cwd: Option<&str>,
) -> (JsonRpcResponse, JsonRpcResponse) {
    let handler = DefaultHandler {
        projects_root: Some(tmp.to_path_buf()),
        default_cwd: default_cwd.map(|s| s.to_string()),
        ..DefaultHandler::default()
    };
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    let req = if params_literal == "null" {
        br#"{"jsonrpc":"2.0","id":2,"method":"session/list"}
"#
        .to_vec()
    } else {
        format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"session/list","params":{params_literal}}}
"#
        )
        .into_bytes()
    };
    input.extend_from_slice(&req);
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(
        lines.len(),
        2,
        "expected initialize + session/list responses, got {}",
        String::from_utf8_lossy(&out)
    );
    let r1: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    (r1, r2)
}

#[tokio::test]
async fn session_list_before_initialize_errors() {
    // Without a prior initialize, session/list must return
    // INVALID_REQUEST "Not initialized". No side effects on state.
    let tmp = fresh_tempdir("list-before-init");
    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let req = br#"{"jsonrpc":"2.0","id":7,"method":"session/list","params":{}}
"#;
    client_in.write_all(req).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 1);
    let resp: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    assert_eq!(resp.id, Some(RequestId::Number(7)));
    let err = resp.error.expect("expected not-initialized error");
    assert_eq!(err.code, error_code::INVALID_REQUEST);
    assert!(err.message.contains("Not initialized"));
    assert_eq!(state.session_count(), 0);
}

#[tokio::test]
async fn session_list_with_omitted_params_returns_empty_array() {
    // `session/list` without a `params` field at all is valid
    // (both fields are optional). With no in-memory sessions and
    // no disk fixtures, the result is an empty list.
    let tmp = fresh_tempdir("list-null-params");
    let (r1, r2) = drive_list(tmp.path(), "null", None).await;
    assert!(r1.error.is_none(), "initialize must succeed");
    assert!(r2.error.is_none(), "session/list must succeed");
    let result = r2.result.expect("result");
    assert!(result["sessions"].as_array().unwrap().is_empty());
    // nextCursor must be OMITTED from the wire entirely.
    let obj = result.as_object().unwrap();
    assert!(!obj.contains_key("nextCursor"));
}

#[tokio::test]
async fn session_list_with_empty_object_params_returns_empty_array() {
    let tmp = fresh_tempdir("list-empty-params");
    let (_r1, r2) = drive_list(tmp.path(), "{}", None).await;
    let result = r2.result.expect("result");
    assert!(result["sessions"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn session_list_with_explicit_cwd_filters_in_memory_sessions() {
    // Create one session via a prior pass, then list with an
    // explicit cwd that matches it. Only matching sessions appear.
    let tmp = fresh_tempdir("list-filter");
    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };

    let sid_match = {
        let (mut client_in, server_in, server_out, client_out) = pipe_pair();
        let mut input = Vec::new();
        input.extend_from_slice(
                br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
            );
        input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/want"}}
"#,
        );
        input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/tmp/other"}}
"#,
        );
        input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":4,"method":"session/list","params":{"cwd":"/tmp/want"}}
"#,
        );
        client_in.write_all(&input).await.unwrap();
        drop(client_in);

        let serve_task = tokio::spawn(async move {
            serve(server_in, server_out, handler).await.unwrap();
        });
        let out = drain(client_out).await;
        serve_task.await.unwrap();

        let lines = ndjson_lines(&out);
        assert_eq!(lines.len(), 4);
        let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
        let r4: JsonRpcResponse = serde_json::from_slice(lines[3]).unwrap();
        let wanted_sid = r2.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        let list_result = r4.result.unwrap();
        let ids = list_result_ids(&list_result);
        assert_eq!(ids.len(), 1, "explicit cwd must filter out other cwd");
        assert!(ids.contains(&wanted_sid));
        wanted_sid
    };
    assert!(!sid_match.is_empty());
}

#[tokio::test]
async fn session_list_disk_only_fixtures_list_correctly() {
    // Drop a few empty jsonl files and assert that all of them come
    // back through session/list. Disk-only placeholders carry the
    // effective cwd and an ISO updatedAt timestamp.
    let tmp = fresh_tempdir("list-disk-only");
    let cwd = "/tmp/disk";
    write_empty_session_file(tmp.path(), cwd, "d1");
    write_empty_session_file(tmp.path(), cwd, "d2");
    write_empty_session_file(tmp.path(), cwd, "d3");

    let (_r1, r2) = drive_list(tmp.path(), r#"{"cwd":"/tmp/disk"}"#, None).await;
    let result = r2.result.expect("result");
    let ids = list_result_ids(&result);
    assert_eq!(ids.len(), 3);
    assert!(ids.contains("d1"));
    assert!(ids.contains("d2"));
    assert!(ids.contains("d3"));

    for s in result["sessions"].as_array().unwrap() {
        assert_eq!(s["cwd"], cwd);
        // updatedAt is present (derived from file mtime) and ISO-shaped.
        let ts = s["updatedAt"].as_str().expect("updatedAt must be string");
        assert!(
            ts.len() >= "1970-01-01T00:00:00.000Z".len() && ts.ends_with('Z') && ts.contains('T'),
            "unexpected updatedAt format: {ts}"
        );
        // title must be absent from the wire; `_meta` now always carries the
        // owner probe so a client can tell "already open elsewhere" from
        // "resumable".
        assert!(
            s.get("title").is_none(),
            "title must be omitted for disk placeholders"
        );
        assert_eq!(s["_meta"]["rebon"]["owner"]["active"], false);
        assert_eq!(s["_meta"]["rebon"]["owner"]["heldHere"], false);
    }
}

#[tokio::test]
async fn session_list_merges_in_memory_and_disk_sessions() {
    // In one connection: initialize, session/new under cwd X, then
    // session/list with cwd X — we should see both the fresh
    // in-memory session AND any on-disk placeholder for X.
    let tmp = fresh_tempdir("list-merge");
    let cwd = "/tmp/merged";
    write_empty_session_file(tmp.path(), cwd, "disk-entry");

    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/merged"}}
"#,
    );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":3,"method":"session/list","params":{"cwd":"/tmp/merged"}}
"#,
    );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 3);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    let sid_new = r2.result.unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let r3: JsonRpcResponse = serde_json::from_slice(lines[2]).unwrap();
    let result = r3.result.unwrap();
    let ids = list_result_ids(&result);
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&sid_new));
    assert!(ids.contains("disk-entry"));
}

#[tokio::test]
async fn session_list_in_memory_wins_on_id_collision() {
    // Create an in-memory session, then drop a disk jsonl with the
    // SAME stem. The returned list must contain exactly one entry
    // with that id — and it must be the in-memory one, identifiable
    // because the scanner would otherwise have used the empty file's
    // mtime (not the session's created_at) for updatedAt.
    let tmp = fresh_tempdir("list-collision");
    let cwd = "/tmp/collide";
    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };

    let sid = {
        let (mut client_in, server_in, server_out, client_out) = pipe_pair();
        let mut input = Vec::new();
        input.extend_from_slice(
                br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
            );
        input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/collide"}}
"#,
        );
        client_in.write_all(&input).await.unwrap();
        drop(client_in);

        let h = handler.clone();
        let serve_task = tokio::spawn(async move {
            serve(server_in, server_out, h).await.unwrap();
        });
        let out = drain(client_out).await;
        serve_task.await.unwrap();
        let lines = ndjson_lines(&out);
        let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
        r2.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string()
    };
    write_empty_session_file(tmp.path(), cwd, &sid);

    // Pass 2: session/list with explicit cwd → single entry for sid.
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"session/list","params":{{"cwd":"{cwd}"}}}}
"#
    );
    client_in.write_all(req.as_bytes()).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 1);
    let resp: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    let result = resp.result.unwrap();
    let sessions = result["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "in-memory session must win on collision");
    assert_eq!(sessions[0]["sessionId"], sid);
}

#[tokio::test]
async fn session_list_ignores_non_jsonl_files() {
    let tmp = fresh_tempdir("list-non-jsonl");
    let cwd = "/tmp/mixed";
    let project_dir = tmp.path().join(project_dir_component(cwd));
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(project_dir.join("keep.jsonl"), b"").unwrap();
    std::fs::write(project_dir.join("ignore.txt"), b"").unwrap();
    std::fs::write(project_dir.join("ignore.json"), b"{}").unwrap();
    std::fs::write(project_dir.join("sess.jsonl.bak"), b"").unwrap();

    let (_r1, r2) = drive_list(tmp.path(), r#"{"cwd":"/tmp/mixed"}"#, None).await;
    let result = r2.result.unwrap();
    let ids = list_result_ids(&result);
    assert_eq!(ids, std::iter::once("keep".to_string()).collect());
}

#[tokio::test]
async fn session_list_missing_project_dir_degrades_to_empty() {
    // The projects_root exists but there's no subdirectory for the
    // requested cwd. The scanner must degrade to "no disk entries"
    // and return an empty list (no error).
    let tmp = fresh_tempdir("list-missing-dir");
    let (r1, r2) = drive_list(tmp.path(), r#"{"cwd":"/tmp/never-created"}"#, None).await;
    assert!(r1.error.is_none());
    assert!(r2.error.is_none(), "missing dir must NOT error");
    let result = r2.result.unwrap();
    assert!(result["sessions"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn session_list_malformed_cwd_type_returns_invalid_params() {
    // `{ "cwd": 42 }` fails serde → INVALID_PARAMS with a useful
    // `session/list params: ...` prefix.
    let tmp = fresh_tempdir("list-malformed");
    let (_r1, r2) = drive_list(tmp.path(), r#"{"cwd":42}"#, None).await;
    let err = r2.error.expect("expected invalid-params error");
    assert_eq!(err.code, error_code::INVALID_PARAMS);
    assert!(
        err.message.contains("session/list params"),
        "unexpected error message: {}",
        err.message
    );
}

#[tokio::test]
async fn session_list_cursor_is_ignored_and_next_cursor_is_omitted() {
    // Pass a cursor value — it must be silently ignored and the
    // response must STILL omit nextCursor entirely.
    let tmp = fresh_tempdir("list-cursor-ignored");
    write_empty_session_file(tmp.path(), "/tmp/pagi", "one");
    write_empty_session_file(tmp.path(), "/tmp/pagi", "two");

    let (_r1, r2) = drive_list(
        tmp.path(),
        r#"{"cwd":"/tmp/pagi","cursor":"deadbeef"}"#,
        None,
    )
    .await;
    let result = r2.result.unwrap();
    let ids = list_result_ids(&result);
    assert_eq!(ids.len(), 2);
    let obj = result.as_object().unwrap();
    assert!(
        !obj.contains_key("nextCursor"),
        "nextCursor must be omitted even when cursor is supplied"
    );
}

#[tokio::test]
async fn session_list_response_shape_for_disk_placeholder() {
    // Field-level wire shape assertions for a single disk-only
    // placeholder: sessionId present, cwd present, updatedAt
    // present and ISO-shaped, title/_meta absent.
    let tmp = fresh_tempdir("list-shape-disk");
    write_empty_session_file(tmp.path(), "/tmp/shape", "only");

    let (_r1, r2) = drive_list(tmp.path(), r#"{"cwd":"/tmp/shape"}"#, None).await;
    let result = r2.result.unwrap();
    let sessions = result["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    let s = &sessions[0];
    let obj = s.as_object().unwrap();
    assert_eq!(s["sessionId"], "only");
    assert_eq!(s["cwd"], "/tmp/shape");
    assert!(obj.contains_key("updatedAt"));
    assert!(!obj.contains_key("title"));
    assert_eq!(s["_meta"]["rebon"]["owner"]["active"], false);
}

#[tokio::test]
async fn session_list_sanitized_cwd_routing_finds_fixture() {
    // A cwd with spaces / colons must sanitize to the same dir
    // the fixture writer uses.
    let tmp = fresh_tempdir("list-sanitize");
    let cwd = "/path with space/foo:bar";
    write_empty_session_file(tmp.path(), cwd, "sanitized-sess");

    let params = serde_json::json!({ "cwd": cwd });
    let params_literal = serde_json::to_string(&params).unwrap();
    let (_r1, r2) = drive_list(tmp.path(), &params_literal, None).await;
    let result = r2.result.unwrap();
    let ids = list_result_ids(&result);
    assert!(ids.contains("sanitized-sess"));
}

#[tokio::test]
async fn session_list_over_content_length_framing() {
    // session/list must work over Content-Length framing too.
    let tmp = fresh_tempdir("list-content-length");
    write_empty_session_file(tmp.path(), "/tmp/cl", "clfsess");

    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let init = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}"#;
    let list_body =
        br#"{"jsonrpc":"2.0","id":2,"method":"session/list","params":{"cwd":"/tmp/cl"}}"#;
    let mut framed = format!("Content-Length: {}\r\n\r\n", init.len()).into_bytes();
    framed.extend_from_slice(init);
    framed.extend_from_slice(format!("Content-Length: {}\r\n\r\n", list_body.len()).as_bytes());
    framed.extend_from_slice(list_body);
    client_in.write_all(&framed).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    assert!(out.starts_with(b"Content-Length:"));

    // Parse both bodies out of the framed stream.
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    let mut cursor = 0usize;
    while cursor < out.len() {
        let header_start = cursor;
        let header_end = out[header_start..]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| header_start + i)
            .expect("header terminator");
        let header_str = std::str::from_utf8(&out[header_start..header_end]).unwrap();
        let len: usize = header_str
            .trim_start_matches("Content-Length:")
            .trim()
            .parse()
            .unwrap();
        let body_start = header_end + 4;
        let body_end = body_start + len;
        bodies.push(out[body_start..body_end].to_vec());
        cursor = body_end;
    }
    assert_eq!(bodies.len(), 2);
    let r2: JsonRpcResponse = serde_json::from_slice(&bodies[1]).unwrap();
    assert!(r2.error.is_none());
    let result = r2.result.unwrap();
    let ids = list_result_ids(&result);
    assert!(ids.contains("clfsess"));
}

#[tokio::test]
async fn session_list_includes_cancelled_session() {
    // Cancelling a session only increments the cancel tally; the
    // session itself stays in the map. list_sessions must still
    // report it.
    let tmp = fresh_tempdir("list-cancelled");
    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    let sid = {
        let (mut client_in, server_in, server_out, client_out) = pipe_pair();
        let mut input = Vec::new();
        input.extend_from_slice(
                br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
            );
        input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/cancelled"}}
"#,
        );
        client_in.write_all(&input).await.unwrap();
        drop(client_in);

        let h = handler.clone();
        let serve_task = tokio::spawn(async move {
            serve(server_in, server_out, h).await.unwrap();
        });
        let out = drain(client_out).await;
        serve_task.await.unwrap();
        let lines = ndjson_lines(&out);
        let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
        r2.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // Cancel it.
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let note = format!(
        r#"{{"jsonrpc":"2.0","method":"session/cancel","params":{{"sessionId":"{sid}"}}}}
"#
    );
    let list = r#"{"jsonrpc":"2.0","id":3,"method":"session/list","params":{"cwd":"/tmp/cancelled"}}
"#;
    let mut input = Vec::new();
    input.extend_from_slice(note.as_bytes());
    input.extend_from_slice(list.as_bytes());
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 1);
    let resp: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    let result = resp.result.unwrap();
    let ids = list_result_ids(&result);
    assert!(ids.contains(&sid), "cancelled session must still be listed");
    assert_eq!(state.cancel_count(&sid), 1);
}

#[tokio::test]
async fn session_list_field_shape_for_in_memory_session() {
    // In-memory session: sessionId, cwd, updatedAt present. Title
    // and _meta absent (we never set either). updatedAt is the
    // formatted SystemTime.
    let tmp = fresh_tempdir("list-shape-mem");
    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    };
    let state = handler.state().clone();

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/shape-mem"}}
"#,
    );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":3,"method":"session/list","params":{"cwd":"/tmp/shape-mem"}}
"#,
    );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 3);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    let sid = r2.result.unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let r3: JsonRpcResponse = serde_json::from_slice(lines[2]).unwrap();
    let result = r3.result.unwrap();
    let sessions = result["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    let s = &sessions[0];
    let obj = s.as_object().unwrap();
    assert_eq!(s["sessionId"], sid);
    assert_eq!(s["cwd"], "/tmp/shape-mem");
    let updated_at = s["updatedAt"].as_str().expect("updatedAt must be string");
    assert!(updated_at.ends_with('Z'));
    assert!(updated_at.contains('T'));
    // Matches the formatter's output for the stored created_at.
    let rec = state.get_session(&sid).unwrap();
    let expected = format_system_time_iso_ms(rec.created_at);
    assert_eq!(updated_at, expected);
    assert!(!obj.contains_key("title"));
    assert_eq!(s["_meta"]["rebon"]["owner"]["active"], false);
}

#[tokio::test]
async fn session_list_omitted_cwd_returns_all_in_memory_regardless_of_cwd() {
    // With omitted cwd, every in-memory session is returned —
    // regardless of their own cwds. This asymmetry is intentional.
    let tmp = fresh_tempdir("list-omit-mem");
    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        default_cwd: Some("/tmp/default".to_string()),
        ..DefaultHandler::default()
    };

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/a"}}
"#,
    );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/tmp/b"}}
"#,
    );
    // Omit params entirely.
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":4,"method":"session/list"}
"#,
    );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);

    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, handler).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();

    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 4);
    let r2: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    let r3: JsonRpcResponse = serde_json::from_slice(lines[2]).unwrap();
    let sid_a = r2.result.unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let sid_b = r3.result.unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let r4: JsonRpcResponse = serde_json::from_slice(lines[3]).unwrap();
    let ids = list_result_ids(&r4.result.unwrap());
    assert!(ids.contains(&sid_a));
    assert!(ids.contains(&sid_b));
}

#[tokio::test]
async fn session_list_omitted_cwd_scans_disk_under_default_cwd() {
    // With omitted cwd + an explicit default_cwd, the disk scan
    // lands under `default_cwd`. Disk entries under other
    // directories must NOT appear.
    let tmp = fresh_tempdir("list-omit-disk");
    write_empty_session_file(tmp.path(), "/tmp/default-work", "under-default");
    write_empty_session_file(tmp.path(), "/tmp/other-work", "under-other");

    let (_r1, r2) = drive_list(tmp.path(), "null", Some("/tmp/default-work")).await;
    let result = r2.result.unwrap();
    let ids = list_result_ids(&result);
    assert!(ids.contains("under-default"));
    assert!(
        !ids.contains("under-other"),
        "disk entries under unrelated cwds must not appear when cwd is omitted"
    );
}

// -----------------------------------------------------------------------
// Outbound `session/update` notifications via
// `serve_with_publisher` + `ChannelSessionUpdatePublisher`.
//
// Each test exercises the full notification path through a
// `tokio::io::duplex` pipe pair: a producer task drives a publisher
// (clone), `serve_with_publisher` pumps the receiver into the writer,
// and the test reads the wire bytes from the client side and asserts
// both the JSON-RPC envelope shape and the inner `session/update`
// payload shape.
//
// The prompt-turn emit sequence is:
//   - initial `tool_call` notification
//   - `tool_call_update` (in_progress)
//   - `agent_message_chunk` from
//     `content_block_delta`
//   - completion `tool_call_update`
// -----------------------------------------------------------------------

use rebon_agent_core::publisher::{
    make_permission_result_response, ChannelPermissionRequestPublisher,
    ChannelSessionUpdatePublisher, SessionUpdatePublisher,
};
use rebon_proto::types::{
    ContentBlock, PermissionOption, PermissionOptionKind, PermissionOutcome,
    RequestPermissionParams, RequestPermissionResult, SessionUpdate, ToolCallReference,
    ToolCallStatus, ToolKind,
};

/// Read NDJSON lines from a duplex stream until either `expected`
/// lines have arrived or EOF. Returns the parsed JSON values.
async fn read_ndjson_messages(
    stream: &mut DuplexStream,
    expected: usize,
) -> Vec<serde_json::Value> {
    let mut buf = Vec::new();
    let mut messages = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        // Try to parse complete lines from the buffer first.
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).take(pos).collect();
            if line.is_empty() {
                continue;
            }
            let v: serde_json::Value = serde_json::from_slice(&line).unwrap_or_else(|e| {
                panic!(
                    "invalid JSON line: {e}: {:?}",
                    String::from_utf8_lossy(&line)
                )
            });
            messages.push(v);
            if messages.len() >= expected {
                return messages;
            }
        }
        // Need more data.
        match stream.read(&mut chunk).await {
            Ok(0) => return messages,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return messages,
        }
    }
}

#[tokio::test]
async fn serve_with_publisher_emits_notification_through_duplex_wire() {
    // Build a duplex pair, attach a ChannelSessionUpdatePublisher
    // to serve_with_publisher, send one notification through the
    // publisher, and assert the JSON-RPC envelope + payload that
    // appears on the client-side reader.
    let (_client_to_server, server_in, server_out, mut client_from_server) = pipe_pair();
    let (publisher, rx) = ChannelSessionUpdatePublisher::new();

    let serve_task = tokio::spawn(async move {
        serve_with_publisher(server_in, server_out, DefaultHandler::default(), Some(rx))
            .await
            .unwrap();
    });

    // Producer publishes one agent_message_chunk.
    publisher
        .publish_to(
            &"sess-1".to_string(),
            SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text(rebon_proto::types::TextContent {
                    text: "delta".into(),
                    annotations: None,
                }),
            },
        )
        .await;

    // Drop the publisher (closing the channel) so the dispatcher
    // degrades to reader-only mode and then exits on EOF (the
    // client-side `_client_to_server` is dropped at function exit).
    drop(publisher);
    drop(_client_to_server);

    let messages = read_ndjson_messages(&mut client_from_server, 1).await;
    serve_task.await.unwrap();

    assert_eq!(messages.len(), 1, "expected one notification on the wire");
    let m = &messages[0];

    // JSON-RPC 2.0 notification envelope: jsonrpc + method + params,
    // and crucially NO `id` field.
    assert_eq!(m["jsonrpc"], "2.0");
    assert_eq!(m["method"], "session/update");
    assert!(
        m.as_object().unwrap().get("id").is_none(),
        "JSON-RPC notifications must not have an `id` field, got {m}"
    );

    // Inner SessionUpdateParams envelope.
    let params = &m["params"];
    assert_eq!(params["sessionId"], "sess-1");

    // Inner SessionUpdate payload — `sessionUpdate` discriminator.
    let update = &params["update"];
    assert_eq!(update["sessionUpdate"], "agent_message_chunk");
    assert_eq!(update["content"]["type"], "text");
    assert_eq!(update["content"]["text"], "delta");
}

#[tokio::test]
async fn serve_with_publisher_emits_full_tool_call_sequence_in_order() {
    // The canonical prompt-turn emission sequence:
    //   1. tool_call (status=pending)
    //   2. tool_call_update (status=in_progress)
    //   3. agent_message_chunk (text delta)
    //   4. tool_call_update (status=completed, with content)
    //
    // Asserts both the in-order delivery and the per-message wire
    // shape — this is the regression fixture the real
    // session/prompt implementation plugs into.
    let (_client_to_server, server_in, server_out, mut client_from_server) = pipe_pair();
    let (publisher, rx) = ChannelSessionUpdatePublisher::new();

    let serve_task = tokio::spawn(async move {
        serve_with_publisher(server_in, server_out, DefaultHandler::default(), Some(rx))
            .await
            .unwrap();
    });

    let session_id = "sess-1".to_string();

    // 1. Initial tool_call.
    let mut raw_input = std::collections::HashMap::new();
    raw_input.insert("file_path".into(), serde_json::json!("foo.rs"));
    publisher
        .publish_to(
            &session_id,
            SessionUpdate::ToolCall {
                tool_call_id: "toolu_01".into(),
                title: "Read foo.rs".into(),
                kind: ToolKind::Read,
                status: ToolCallStatus::Pending,
                content: None,
                locations: None,
                raw_input: Some(raw_input),
                raw_output: None,
            },
        )
        .await;

    // 2. tool_call_update -> in_progress.
    publisher
        .publish_to(
            &session_id,
            SessionUpdate::ToolCallUpdate {
                tool_call_id: "toolu_01".into(),
                status: Some(ToolCallStatus::InProgress),
                title: None,
                content: None,
                locations: None,
                raw_output: None,
            },
        )
        .await;

    // 3. agent_message_chunk between tool calls.
    publisher
        .publish_to(
            &session_id,
            SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text(rebon_proto::types::TextContent {
                    text: "thinking...".into(),
                    annotations: None,
                }),
            },
        )
        .await;

    // 4. tool_call_update -> completed with content.
    publisher
        .publish_to(
            &session_id,
            SessionUpdate::ToolCallUpdate {
                tool_call_id: "toolu_01".into(),
                status: Some(ToolCallStatus::Completed),
                title: None,
                content: Some(vec![rebon_proto::types::ToolCallContent::Content(
                    rebon_proto::types::RegularContent {
                        content: ContentBlock::Text(rebon_proto::types::TextContent {
                            text: "fn main() {}".into(),
                            annotations: None,
                        }),
                    },
                )]),
                locations: Some(vec![rebon_proto::types::ToolCallLocation {
                    path: "foo.rs".into(),
                    line: Some(1),
                }]),
                raw_output: None,
            },
        )
        .await;

    drop(publisher);
    drop(_client_to_server);

    let messages = read_ndjson_messages(&mut client_from_server, 4).await;
    serve_task.await.unwrap();

    assert_eq!(
        messages.len(),
        4,
        "expected exactly 4 notifications, got {messages:?}"
    );

    // (1) tool_call
    assert_eq!(messages[0]["method"], "session/update");
    assert_eq!(messages[0]["params"]["sessionId"], "sess-1");
    let u0 = &messages[0]["params"]["update"];
    assert_eq!(u0["sessionUpdate"], "tool_call");
    assert_eq!(u0["toolCallId"], "toolu_01");
    assert_eq!(u0["title"], "Read foo.rs");
    assert_eq!(u0["kind"], "read");
    assert_eq!(u0["status"], "pending");
    assert_eq!(u0["rawInput"]["file_path"], "foo.rs");

    // (2) tool_call_update -> in_progress
    let u1 = &messages[1]["params"]["update"];
    assert_eq!(u1["sessionUpdate"], "tool_call_update");
    assert_eq!(u1["toolCallId"], "toolu_01");
    assert_eq!(u1["status"], "in_progress");
    // Only sessionUpdate + toolCallId + status -> 3 keys.
    assert_eq!(u1.as_object().unwrap().len(), 3);

    // (3) agent_message_chunk
    let u2 = &messages[2]["params"]["update"];
    assert_eq!(u2["sessionUpdate"], "agent_message_chunk");
    assert_eq!(u2["content"]["type"], "text");
    assert_eq!(u2["content"]["text"], "thinking...");

    // (4) tool_call_update -> completed
    let u3 = &messages[3]["params"]["update"];
    assert_eq!(u3["sessionUpdate"], "tool_call_update");
    assert_eq!(u3["status"], "completed");
    let content_arr = u3["content"].as_array().unwrap();
    assert_eq!(content_arr.len(), 1);
    assert_eq!(content_arr[0]["type"], "content");
    assert_eq!(content_arr[0]["content"]["type"], "text");
    assert_eq!(content_arr[0]["content"]["text"], "fn main() {}");
    let loc_arr = u3["locations"].as_array().unwrap();
    assert_eq!(loc_arr.len(), 1);
    assert_eq!(loc_arr[0]["path"], "foo.rs");
    assert_eq!(loc_arr[0]["line"], 1);
}

#[tokio::test]
async fn serve_with_publisher_interleaves_inbound_request_and_outbound_notification() {
    // Mixed traffic: an `initialize` request flows in while a
    // notification is published. Both must reach the wire — the
    // notification as a JSON-RPC notification (no id) and the
    // initialize as a JSON-RPC response (with id). The relative
    // order is biased toward outbound notifications by the
    // `tokio::select!` arrangement, but both must be present.
    let (mut client_to_server, server_in, server_out, mut client_from_server) = pipe_pair();
    let (publisher, rx) = ChannelSessionUpdatePublisher::new();

    let serve_task = tokio::spawn(async move {
        serve_with_publisher(server_in, server_out, DefaultHandler::default(), Some(rx))
            .await
            .unwrap();
    });

    // Publish first so the channel branch fires.
    publisher
        .publish_to(
            &"sess-1".to_string(),
            SessionUpdate::Plan { entries: vec![] },
        )
        .await;

    // Then send the initialize request.
    let init = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#;
    client_to_server.write_all(init).await.unwrap();

    // Close everything so the dispatcher exits cleanly.
    drop(publisher);
    drop(client_to_server);

    let messages = read_ndjson_messages(&mut client_from_server, 2).await;
    serve_task.await.unwrap();

    assert_eq!(
        messages.len(),
        2,
        "expected one notification + one response, got {messages:?}"
    );

    // Find each by shape — the order depends on select! biasing
    // and the channel-vs-reader race; both orderings are valid as
    // long as both messages reach the wire intact.
    let mut saw_notification = false;
    let mut saw_response = false;
    for m in &messages {
        if m.get("id").is_some() {
            // JSON-RPC response (initialize).
            assert_eq!(m["id"], 1);
            assert_eq!(m["jsonrpc"], "2.0");
            assert!(m["result"]["protocolVersion"].is_number());
            saw_response = true;
        } else {
            // JSON-RPC notification (session/update).
            assert_eq!(m["method"], "session/update");
            assert_eq!(m["params"]["sessionId"], "sess-1");
            assert_eq!(m["params"]["update"]["sessionUpdate"], "plan");
            saw_notification = true;
        }
    }
    assert!(saw_notification, "did not see session/update notification");
    assert!(saw_response, "did not see initialize response");
}

#[tokio::test]
async fn serve_with_publisher_degrades_when_publisher_closed_first() {
    // Edge case: publisher dropped before any notification is sent.
    // The dispatcher must immediately degrade to reader-only mode
    // and continue to handle inbound traffic until reader EOF.
    let (mut client_to_server, server_in, server_out, mut client_from_server) = pipe_pair();
    let (publisher, rx) = ChannelSessionUpdatePublisher::new();
    // Drop publisher immediately — channel has no senders.
    drop(publisher);

    let serve_task = tokio::spawn(async move {
        serve_with_publisher(server_in, server_out, DefaultHandler::default(), Some(rx))
            .await
            .unwrap();
    });

    let init = br#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#;
    client_to_server.write_all(init).await.unwrap();
    drop(client_to_server);

    let messages = read_ndjson_messages(&mut client_from_server, 1).await;
    serve_task.await.unwrap();

    assert_eq!(messages.len(), 1, "expected just the initialize response");
    assert_eq!(messages[0]["id"], 7);
    assert!(messages[0]["result"]["protocolVersion"].is_number());
}

fn sample_permission_request_params() -> RequestPermissionParams {
    RequestPermissionParams {
        session_id: "sess-1".into(),
        tool_call: ToolCallReference {
            tool_call_id: "toolu_01".into(),
        },
        options: vec![PermissionOption {
            option_id: "allow_once".into(),
            name: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        }],
        title: Some("Review workflow demo".into()),
        message: None,
        tool_name: None,
        tool_input: None,
        metadata: None,
    }
}

#[tokio::test]
async fn serve_with_publishers_emits_permission_request_and_routes_response() {
    let (mut client_to_server, server_in, server_out, mut client_from_server) = pipe_pair();
    let (publisher, rx) = ChannelPermissionRequestPublisher::new();

    let serve_task = tokio::spawn(async move {
        serve_with_publishers(
            server_in,
            server_out,
            DefaultHandler::default(),
            None,
            Some(rx),
        )
        .await
        .unwrap();
    });

    let mut params = sample_permission_request_params();
    params.metadata = Some(serde_json::json!({
        "kind": "workflowReview",
        "audit": {
            "title": "Workflow/RunWorkflow permission review"
        }
    }));

    let requester =
        tokio::spawn(async move { publisher.request_permission(params).await.unwrap() });

    let messages = read_ndjson_messages(&mut client_from_server, 1).await;
    assert_eq!(
        messages.len(),
        1,
        "expected one reverse request on the wire"
    );
    let outbound = &messages[0];
    assert_eq!(outbound["jsonrpc"], "2.0");
    assert_eq!(outbound["method"], "session/request_permission");
    assert_eq!(outbound["params"]["sessionId"], "sess-1");
    assert_eq!(outbound["params"]["toolCall"]["toolCallId"], "toolu_01");
    assert_eq!(outbound["params"]["title"], "Review workflow demo");
    assert_eq!(outbound["params"]["options"][0]["optionId"], "allow_once");
    assert_eq!(outbound["params"]["metadata"]["kind"], "workflowReview");
    assert_eq!(
        outbound["params"]["metadata"]["audit"]["title"],
        "Workflow/RunWorkflow permission review"
    );
    let request_id = outbound["id"].as_i64().expect("numeric request id");

    let response = make_permission_result_response(
        RequestId::Number(request_id),
        RequestPermissionResult {
            outcome: PermissionOutcome::Selected,
            option_id: Some("allow_once".into()),
            updated_input: None,
        },
    );
    let response_bytes = serde_json::to_vec(&response).unwrap();
    client_to_server.write_all(&response_bytes).await.unwrap();
    client_to_server.write_all(b"\n").await.unwrap();
    drop(client_to_server);

    let result = requester.await.unwrap();
    serve_task.await.unwrap();
    assert_eq!(result.outcome, PermissionOutcome::Selected);
    assert_eq!(result.option_id.as_deref(), Some("allow_once"));
}

#[tokio::test]
async fn serve_with_publishers_ignores_duplicate_permission_response() {
    let (mut client_to_server, server_in, server_out, mut client_from_server) = pipe_pair();
    let (publisher, rx) = ChannelPermissionRequestPublisher::new();

    let serve_task = tokio::spawn(async move {
        serve_with_publishers(
            server_in,
            server_out,
            DefaultHandler::default(),
            None,
            Some(rx),
        )
        .await
        .unwrap();
    });

    let requester = tokio::spawn(async move {
        publisher
            .request_permission(sample_permission_request_params())
            .await
            .unwrap()
    });

    let messages = read_ndjson_messages(&mut client_from_server, 1).await;
    let outbound = &messages[0];
    let request_id = outbound["id"].as_i64().expect("numeric request id");

    let ok_response = make_permission_result_response(
        RequestId::Number(request_id),
        RequestPermissionResult {
            outcome: PermissionOutcome::Selected,
            option_id: Some("allow_once".into()),
            updated_input: None,
        },
    );
    let ok_bytes = serde_json::to_vec(&ok_response).unwrap();
    client_to_server.write_all(&ok_bytes).await.unwrap();
    client_to_server.write_all(b"\n").await.unwrap();

    let result = requester.await.unwrap();
    assert_eq!(result.outcome, PermissionOutcome::Selected);
    assert_eq!(result.option_id.as_deref(), Some("allow_once"));

    let duplicate = make_permission_result_response(
        RequestId::Number(request_id),
        RequestPermissionResult {
            outcome: PermissionOutcome::Cancelled,
            option_id: None,
            updated_input: None,
        },
    );
    let duplicate_bytes = serde_json::to_vec(&duplicate).unwrap();
    client_to_server.write_all(&duplicate_bytes).await.unwrap();
    client_to_server.write_all(b"\n").await.unwrap();
    drop(client_to_server);

    let trailing = read_ndjson_messages(&mut client_from_server, 1).await;
    serve_task.await.unwrap();
    assert!(
        trailing.is_empty(),
        "duplicate permission responses must not produce outbound traffic: {trailing:?}"
    );
}

// ---- sub_agents ConfigOption -----------------------------------




#[test]
fn session_scoped_permission_config_updates_do_not_change_shared_default() {
    for mode in ["plan", "bypassPermissions"] {
        let mut handler = DefaultHandler::default();
        handler.seed_config_option_value("permissions", "auto");
        let session = handler.state().create_session_with_permission_mode(
            "/tmp/work".into(),
            Vec::new(),
            "auto",
        );
        let apply_count = Arc::new(Mutex::new(0));
        let apply_count_for_callback = Arc::clone(&apply_count);
        handler.config_option_applier = Some(Arc::new(move |_, _| {
            *apply_count_for_callback.lock().unwrap() += 1;
        }));

        let options = handler.apply_config_option_local(&session.id, "permissions", mode);

        assert_eq!(
            options
                .iter()
                .find(|option| option.id == "permissions")
                .unwrap()
                .current_value,
            mode
        );
        assert_eq!(
            handler
                .config_options_snapshot()
                .iter()
                .find(|option| option.id == "permissions")
                .unwrap()
                .current_value,
            "auto",
            "mode={mode}"
        );
        assert_eq!(
            handler
                .state()
                .get_session(&session.id)
                .unwrap()
                .permission_mode,
            mode
        );
        assert_eq!(*apply_count.lock().unwrap(), 0, "mode={mode}");
    }
}

#[test]
fn seed_config_option_value_ignores_session_scoped_permission_defaults() {
    let handler = DefaultHandler::default();
    handler.seed_config_option_value("permissions", "auto");

    for mode in ["plan", "bypassPermissions"] {
        handler.seed_config_option_value("permissions", mode);

        let options = handler.config_options_snapshot();
        let permissions = options
            .iter()
            .find(|option| option.id == "permissions")
            .unwrap();
        assert_eq!(permissions.current_value, "auto", "mode={mode}");
    }
}

#[test]
fn seed_config_option_value_updates_current_value() {
    let handler = DefaultHandler::default();
    handler.seed_config_option_value("auto_compact", "off");
    let options = handler.config_options_snapshot();
    let auto_compact = options.iter().find(|o| o.id == "auto_compact").unwrap();
    assert_eq!(auto_compact.current_value, "off");
}

#[test]
fn seed_config_option_value_ignores_unknown_value() {
    let handler = DefaultHandler::default();
    handler.seed_config_option_value("auto_compact", "garbage");
    let options = handler.config_options_snapshot();
    let auto_compact = options.iter().find(|o| o.id == "auto_compact").unwrap();
    assert_eq!(auto_compact.current_value, "on");
}

/// The handler's own rows are the session's; everything else arrives from the
/// `config-options` seat, which is how a plugin gets a row at all.
#[test]
fn the_handlers_own_rows_are_the_session_scoped_ones() {
    let handler = DefaultHandler::default();
    let ids: Vec<String> = handler
        .config_options_snapshot()
        .into_iter()
        .map(|option| option.id)
        .collect();
    // No kernel has booted in this test, so the seat is absent and the list is
    // the session rows alone — which is also the fallback a composition
    // without the Core plugin gets.
    assert_eq!(
        ids,
        vec!["permissions", "model", "context_prune", "auto_compact"],
        "a row backed by the config file belongs to whoever owns the setting"
    );
}

#[test]
fn seed_config_option_value_ignores_unknown_id() {
    let handler = DefaultHandler::default();
    handler.seed_config_option_value("does_not_exist", "on");
    // No panic, other options untouched.
    let options = handler.config_options_snapshot();
    assert!(options.iter().any(|o| o.id == "permissions"));
    assert!(options.iter().any(|o| o.id == "auto_compact"));
}

/// The wider commands — plan mode, the settings surfaces, anything that needs
/// a session someone is sitting in front of — stay off the wire.
#[test]
fn local_only_commands_are_not_advertised_over_acp() {
    let commands = crate::server::commands::acp_advertised_slash_commands();
    let advertised: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
    for local in ["grill", "ultraplan", "settings", "theme", "rewind", "new"] {
        assert!(
            !advertised.contains(&local),
            "/{local} is advertised but the server does not intercept it"
        );
    }
}

#[test]
fn ultrawork_reminder_accepts_leading_command_token_whitespace_boundaries() {
    let cases = [
        ("long exact", "/ultrawork"),
        ("short exact", "/ulw"),
        ("space", "/ultrawork task"),
        ("tab", "/ulw\ttask"),
        ("newline", "/ultrawork\ntask"),
        ("leading whitespace", " \t\n/ulw task"),
    ];

    for (case, input) in cases {
        let prompt = vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: input.to_string(),
            annotations: None,
        })];
        let (transformed, detected) = acp_prompt_with_ultrawork_reminder(prompt);
        assert!(detected, "case {case}");
        let rebon_types::ContentBlock::Text(text) = &transformed[0] else {
            panic!("case {case}: expected text block");
        };
        assert!(
            text.text.starts_with("<system-reminder>\n"),
            "case {case}: {:?}",
            text.text
        );
        assert!(text.text.ends_with(input), "case {case}: {:?}", text.text);
    }
}

#[test]
fn ultrawork_reminder_rejects_prefix_collisions_and_nonleading_commands() {
    let cases = [
        ("long prefix", "/ultraworker"),
        ("short prefix", "/ulwfoo"),
        ("path suffix", "/ultrawork/task"),
        ("nonleading command", "please /ultrawork"),
        ("empty", ""),
        ("whitespace only", " \t\n"),
        ("ordinary text", "do the task"),
    ];

    for (case, input) in cases {
        let prompt = vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: input.to_string(),
            annotations: None,
        })];
        let (transformed, detected) = acp_prompt_with_ultrawork_reminder(prompt);
        assert!(!detected, "case {case}");
        assert_eq!(transformed.len(), 1, "case {case}");
        let rebon_types::ContentBlock::Text(text) = &transformed[0] else {
            panic!("case {case}: expected text block");
        };
        assert_eq!(text.text, input, "case {case}");
        assert!(text.annotations.is_none(), "case {case}");
    }
}

#[test]
fn ultrawork_reminder_preserves_policy_across_fresh_and_saved_user_resume() {
    let cases = [
        ("long exact", "/ultrawork", true),
        ("short exact", "/ulw", true),
        ("long space boundary", "/ultrawork task", true),
        ("short tab boundary", "/ulw\ttask", true),
        ("long newline boundary", "/ultrawork\ntask", true),
        ("short leading whitespace", " \n\t/ulw task", true),
        ("long prefix collision", "/ultraworker task", false),
        ("short prefix collision", "/ulwfoo task", false),
        ("slash suffix collision", "/ultrawork/task", false),
        ("nonleading command", "please /ulw", false),
    ];

    for resume_saved_user in [false, true] {
        for (case, input, expected_policy) in cases {
            let durable_ultrawork_requested = starts_with_ultrawork_command(input);
            let prompt = if resume_saved_user {
                Vec::new()
            } else {
                vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: input.to_string(),
                    annotations: None,
                })]
            };
            let (transformed, injected_reminder) = acp_prompt_with_ultrawork_reminder(prompt);
            let ultrawork_requested = if resume_saved_user {
                durable_ultrawork_requested
            } else {
                injected_reminder
            };
            let execution_policy =
                ultrawork_requested.then(rebon_types::ExecutionPolicy::workflow_controller);

            assert_eq!(
                execution_policy.is_some(),
                expected_policy,
                "case {case}, resume_saved_user={resume_saved_user}"
            );
            if resume_saved_user {
                assert!(
                    transformed.is_empty(),
                    "case {case}: saved user block must not be reinjected"
                );
                assert!(
                    !injected_reminder,
                    "case {case}: reminder must not be injected without a prompt block"
                );
                continue;
            }

            let rebon_types::ContentBlock::Text(text) = &transformed[0] else {
                panic!("case {case}: expected text block");
            };
            if expected_policy {
                assert!(
                    text.text.starts_with("<system-reminder>\n"),
                    "case {case}: {:?}",
                    text.text
                );
                assert!(text.text.ends_with(input), "case {case}: {:?}", text.text);
            } else {
                assert_eq!(text.text, input, "case {case}");
            }
        }
    }
}


// --- `_session/steering` ---

#[derive(Default)]
struct TestSteeringSink {
    queues: Mutex<std::collections::HashMap<String, Vec<SteeringMessage>>>,
}

impl TestSteeringSink {
    fn pending_len(&self, session_id: &str) -> usize {
        self.queues
            .lock()
            .unwrap()
            .get(session_id)
            .map(Vec::len)
            .unwrap_or(0)
    }
}

impl SteeringSink for TestSteeringSink {
    fn enqueue(&self, session_id: &str, messages: Vec<SteeringMessage>) {
        self.queues
            .lock()
            .unwrap()
            .entry(session_id.to_string())
            .or_default()
            .extend(messages);
    }

    fn drain_pending(&self, session_id: &str) -> Vec<SteeringMessage> {
        self.queues
            .lock()
            .unwrap()
            .remove(session_id)
            .unwrap_or_default()
    }
}

/// First call records its prompt, signals entry, and blocks until released;
/// every later call records, signals, and finishes immediately. This is the
/// smallest shape that exercises both steering paths: inject-while-running
/// and the end-of-turn leftover respawn.
#[derive(Default)]
struct SteeringScriptExecutor {
    calls: std::sync::atomic::AtomicUsize,
    first_entered: tokio::sync::Notify,
    release_first: tokio::sync::Notify,
    later_entered: tokio::sync::Notify,
    prompts: Mutex<Vec<Vec<rebon_types::ContentBlock>>>,
    uuids: Mutex<Vec<Option<String>>>,
}

#[async_trait]
impl PromptExecutor for SteeringScriptExecutor {
    async fn execute(
        &self,
        request: PromptRequest,
    ) -> Result<rebon_agent_core::prompt_executor::PromptOutcome, PromptExecutorError> {
        self.prompts.lock().unwrap().push(request.prompt.clone());
        self.uuids
            .lock()
            .unwrap()
            .push(request.user_message_uuid.clone());
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            self.first_entered.notify_one();
            self.release_first.notified().await;
        } else {
            self.later_entered.notify_one();
        }
        Ok(rebon_agent_core::prompt_executor::PromptOutcome::end_turn())
    }
}

async fn wait_until(what: &str, condition: impl Fn() -> bool) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting until {what}");
}

async fn steering_test_session(
    executor: Arc<dyn PromptExecutor>,
    sink: Arc<dyn SteeringSink>,
) -> (DefaultHandler, String) {
    let handler = DefaultHandler::default()
        .with_prompt_executor(executor)
        .with_steering_sink(sink);
    handler
        .handle_request(
            "initialize",
            Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    let result = handler
        .handle_request(
            "session/new",
            Some(serde_json::json!({"cwd": r"C:\rebon-steering-test"})),
        )
        .await
        .unwrap();
    let sid = result["sessionId"].as_str().unwrap().to_string();
    (handler, sid)
}

fn steering_params(sid: &str, text: &str) -> Option<Value> {
    Some(serde_json::json!({
        "sessionId": sid,
        "prompt": [{"type": "text", "text": text}],
    }))
}

fn prompt_texts(prompt: &[rebon_types::ContentBlock]) -> Vec<String> {
    prompt
        .iter()
        .filter_map(|block| match block {
            rebon_types::ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn steering_without_sink_is_method_not_found() {
    let (handler, sid) = lifecycle_test_session(Arc::new(FixedPromptExecutor(Ok(
        rebon_agent_core::prompt_executor::PromptOutcome::end_turn(),
    ))))
    .await;

    let err = handler
        .handle_request("_session/steering", steering_params(&sid, "hi"))
        .await
        .unwrap_err();

    assert_eq!(err.code, error_code::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn steering_before_initialize_is_invalid_request() {
    let handler =
        DefaultHandler::default().with_steering_sink(Arc::new(TestSteeringSink::default()));

    let err = handler
        .handle_request("_session/steering", steering_params("sess-x", "hi"))
        .await
        .unwrap_err();

    assert_eq!(err.code, error_code::INVALID_REQUEST);
}

#[tokio::test]
async fn initialize_advertises_steering_only_when_sink_is_wired() {
    let with_sink =
        DefaultHandler::default().with_steering_sink(Arc::new(TestSteeringSink::default()));
    let result = with_sink
        .handle_request(
            "initialize",
            Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    assert_eq!(result["_meta"]["steering"]["supported"], true);

    let without_sink = DefaultHandler::default();
    let result = without_sink
        .handle_request(
            "initialize",
            Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    assert!(
        result.get("_meta").is_none(),
        "no sink must mean no steering advertisement: {result}"
    );
}

#[tokio::test]
async fn steering_unknown_session_and_empty_prompt_are_invalid_params() {
    let sink = Arc::new(TestSteeringSink::default());
    let (handler, sid) =
        steering_test_session(Arc::new(SteeringScriptExecutor::default()), sink).await;

    let err = handler
        .handle_request("_session/steering", steering_params("sess-missing", "hi"))
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::INVALID_PARAMS);
    assert!(err.message.contains("Session not found"));

    let err = handler
        .handle_request(
            "_session/steering",
            Some(serde_json::json!({"sessionId": sid, "prompt": []})),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::INVALID_PARAMS);
    assert!(err.message.contains("non-empty prompt"));
}

#[tokio::test]
async fn steering_active_turn_returns_injected_and_leftovers_respawn_as_new_turn() {
    let executor = Arc::new(SteeringScriptExecutor::default());
    let sink = Arc::new(TestSteeringSink::default());
    let (handler, sid) = steering_test_session(executor.clone(), sink.clone()).await;
    let state = handler.state().clone();

    let prompt_handler = handler.clone();
    let prompt_sid = sid.clone();
    let prompt_task = tokio::spawn(async move {
        prompt_handler
            .handle_request(
                "session/prompt",
                Some(serde_json::json!({
                    "sessionId": prompt_sid,
                    "prompt": [{"type": "text", "text": "long task"}],
                })),
            )
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.first_entered.notified(),
    )
    .await
    .expect("prompt executor was not entered");

    let result = handler
        .handle_request("_session/steering", steering_params(&sid, "steer please"))
        .await
        .unwrap();
    assert_eq!(result["outcome"], "injected");
    assert_eq!(
        sink.pending_len(&sid),
        1,
        "injected message waits for the active turn's poller"
    );

    // The turn ends before any poller drained the queue: the leftover
    // must be handed to a fresh turn instead of being lost.
    executor.release_first.notify_one();
    let prompt_result = prompt_task.await.unwrap().unwrap();
    assert_eq!(prompt_result["stopReason"], "end_turn");

    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.later_entered.notified(),
    )
    .await
    .expect("leftover steering message did not respawn a turn");
    let prompts = executor.prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompt_texts(&prompts[1]), vec!["steer please"]);
    assert_eq!(sink.pending_len(&sid), 0);
    wait_until("respawned steering turn releases the prompt slot", || {
        !state.is_prompt_active(&sid)
    })
    .await;
}

#[tokio::test]
async fn steering_idle_session_starts_a_new_turn() {
    let executor = Arc::new(SteeringScriptExecutor::default());
    let sink = Arc::new(TestSteeringSink::default());
    let (handler, sid) = steering_test_session(executor.clone(), sink.clone()).await;
    let state = handler.state().clone();

    // Pre-release so the spawned turn's first executor call finishes
    // without an extra rendezvous.
    executor.release_first.notify_one();
    let result = handler
        .handle_request("_session/steering", steering_params(&sid, "fresh turn"))
        .await
        .unwrap();

    assert_eq!(result["outcome"], "startedNewTurn");
    assert_eq!(sink.pending_len(&sid), 0);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.first_entered.notified(),
    )
    .await
    .expect("steering-spawned turn did not reach the executor");
    let prompts = executor.prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompt_texts(&prompts[0]), vec!["fresh turn"]);
    wait_until("steering-spawned turn releases the prompt slot", || {
        !state.is_prompt_active(&sid)
    })
    .await;
}

#[tokio::test]
async fn steering_spawned_turn_is_cancellable_via_session_cancel() {
    let executor = Arc::new(CancelAwarePromptExecutor {
        entered: tokio::sync::Notify::new(),
    });
    let sink = Arc::new(TestSteeringSink::default());
    let (handler, sid) = steering_test_session(executor.clone(), sink).await;
    let state = handler.state().clone();

    let result = handler
        .handle_request("_session/steering", steering_params(&sid, "cancel me"))
        .await
        .unwrap();
    assert_eq!(result["outcome"], "startedNewTurn");
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.entered.notified(),
    )
    .await
    .expect("steering-spawned turn did not reach the executor");
    assert!(state.is_prompt_active(&sid));

    handler
        .handle_notification(
            "session/cancel",
            Some(serde_json::json!({"sessionId": sid})),
        )
        .await;

    wait_until("cancelled steering turn releases the prompt slot", || {
        !state.is_prompt_active(&sid)
    })
    .await;
    assert_eq!(state.cancel_count(&sid), 1);
}

#[tokio::test]
async fn a_prompt_queues_behind_a_steering_spawned_turn_instead_of_rejecting() {
    let executor = Arc::new(SteeringScriptExecutor::default());
    let sink = Arc::new(TestSteeringSink::default());
    let (handler, sid) = steering_test_session(executor.clone(), sink.clone()).await;

    let result = handler
        .handle_request("_session/steering", steering_params(&sid, "fresh turn"))
        .await
        .unwrap();
    assert_eq!(result["outcome"], "startedNewTurn");
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.first_entered.notified(),
    )
    .await
    .expect("steering-spawned turn did not reach the executor");

    let prompt_handler = handler.clone();
    let prompt_sid = sid.clone();
    let prompt_task = tokio::spawn(async move {
        prompt_handler
            .handle_request(
                "session/prompt",
                Some(serde_json::json!({
                    "sessionId": prompt_sid,
                    "prompt": [{"type": "text", "text": "queued prompt"}],
                })),
            )
            .await
    });

    // The slot is owned by the steering-spawned turn; the prompt must
    // park rather than resolve with INVALID_REQUEST.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !prompt_task.is_finished(),
        "session/prompt must queue behind the steering-spawned turn"
    );

    executor.release_first.notify_one();
    let response = tokio::time::timeout(std::time::Duration::from_secs(2), prompt_task)
        .await
        .expect("queued prompt never completed after the steering turn ended")
        .expect("prompt task panicked")
        .expect("queued prompt was rejected");
    assert_eq!(response["stopReason"], "end_turn");

    let prompts = executor.prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompt_texts(&prompts[0]), vec!["fresh turn"]);
    assert_eq!(prompt_texts(&prompts[1]), vec!["queued prompt"]);

    // The steered turn carries the uuid `_session/steering` minted; a
    // plain prompt leaves minting to the engine.
    let uuids = executor.uuids.lock().unwrap().clone();
    assert!(uuids[0].is_some(), "steered turn lost its promised uuid");
    assert!(uuids[1].is_none(), "plain prompt must not inherit a uuid");
}

#[tokio::test]
async fn steering_response_bypasses_an_in_flight_prompt_on_the_wire() {
    let executor = Arc::new(SteeringScriptExecutor::default());
    let sink = Arc::new(TestSteeringSink::default());
    let handler = DefaultHandler::default()
        .with_prompt_executor(executor.clone())
        .with_steering_sink(sink.clone());

    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let serve_task = tokio::spawn(async move { serve(server_in, server_out, handler).await });
    let mut client_out = BufReader::new(client_out);

    client_in
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/steering-wire"}}
"#,
        )
        .await
        .unwrap();
    let mut line = String::new();
    client_out.read_line(&mut line).await.unwrap();
    let init: JsonRpcResponse = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(init.id, Some(RequestId::Number(1)));
    assert_eq!(
        init.result.as_ref().unwrap()["_meta"]["steering"]["supported"],
        true,
        "the wire initialize response must advertise steering"
    );
    line.clear();
    client_out.read_line(&mut line).await.unwrap();
    let new_session: JsonRpcResponse = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(new_session.id, Some(RequestId::Number(2)));
    let sid = new_session.result.unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let prompt = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"long task\"}}]}}}}\n"
    );
    client_in.write_all(prompt.as_bytes()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.first_entered.notified(),
    )
    .await
    .expect("prompt executor was not entered");

    // With the prompt occupying the FIFO request worker, the steering
    // request must still get through and answer first.
    let steer = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"_session/steering\",\"params\":{{\"sessionId\":\"{sid}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"mid-turn note\"}}]}}}}\n"
    );
    client_in.write_all(steer.as_bytes()).await.unwrap();

    line.clear();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client_out.read_line(&mut line),
    )
    .await
    .expect("steering response did not bypass the in-flight prompt")
    .unwrap();
    let steering: JsonRpcResponse = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(steering.id, Some(RequestId::Number(4)));
    assert_eq!(steering.result.unwrap()["outcome"], "injected");
    assert_eq!(sink.pending_len(&sid), 1);

    executor.release_first.notify_one();
    line.clear();
    client_out.read_line(&mut line).await.unwrap();
    let prompt_response: JsonRpcResponse = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(prompt_response.id, Some(RequestId::Number(3)));
    assert_eq!(prompt_response.result.unwrap()["stopReason"], "end_turn");

    // The undelivered steering message respawns as a fire-and-forget
    // turn after the prompt's turn ends.
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.later_entered.notified(),
    )
    .await
    .expect("leftover steering message did not respawn a turn");
    let prompts = executor.prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompt_texts(&prompts[1]), vec!["mid-turn note"]);

    drop(client_in);
    tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
        .await
        .expect("server did not drain after EOF")
        .unwrap()
        .unwrap();
}

// ---- read-only slash commands -----------------------------------

/// A session rooted at `cwd`, as `session/new` would create it.
fn read_only_command_session(cwd: &std::path::Path) -> crate::session::SessionRecord {
    crate::session::ServerState::new().create_session_with_permission_mode(
        cwd.to_string_lossy().into_owned(),
        Vec::new(),
        "default",
    )
}

#[test]
fn acp_hooks_command_lists_every_event_and_reads_project_settings() {
    let cwd = tempfile::tempdir().unwrap();
    let rebon_dir = cwd.path().join(".rebon");
    std::fs::create_dir_all(&rebon_dir).unwrap();
    std::fs::write(
        rebon_dir.join("settings.json"),
        serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{ "type": "command", "command": "./scripts/audit.sh" }]
                }]
            }
        })
        .to_string(),
    )
    .unwrap();

    let record = read_only_command_session(cwd.path());
    let output = super::handler::format_acp_read_only_command(&record, "hooks", "", None)
        .expect("/hooks has a read-only report");

    assert!(
        output.contains(&format!(
            "Known hook events: {}",
            rebon_hooks::HOOK_EVENTS.len()
        )),
        "every hook event must be listed: {output}"
    );
    assert!(
        output.contains("./scripts/audit.sh"),
        "configured project hooks must not be reported as absent: {output}"
    );
}

#[test]
fn acp_hooks_command_for_one_event_reports_its_configured_entry() {
    let cwd = tempfile::tempdir().unwrap();
    let rebon_dir = cwd.path().join(".rebon");
    std::fs::create_dir_all(&rebon_dir).unwrap();
    std::fs::write(
        rebon_dir.join("settings.json"),
        serde_json::json!({
            "hooks": {
                "SessionStart": [{
                    "hooks": [{ "type": "command", "command": "./scripts/start.sh" }]
                }]
            }
        })
        .to_string(),
    )
    .unwrap();

    let record = read_only_command_session(cwd.path());
    let output =
        super::handler::format_acp_read_only_command(&record, "hooks", "SessionStart", None)
            .expect("/hooks has a read-only report");

    assert!(output.starts_with("Hooks: SessionStart"), "{output}");
    assert!(output.contains("configured: yes (1)"), "{output}");
    assert!(output.contains("./scripts/start.sh"), "{output}");
}

#[test]
fn acp_status_command_never_reports_unmeasured_counters_as_zero() {
    let cwd = tempfile::tempdir().unwrap();
    let record = read_only_command_session(cwd.path());
    let output = super::handler::format_acp_read_only_command(&record, "status", "", None)
        .expect("/status has a read-only report");

    assert!(output.contains("active agents: unavailable"), "{output}");
    assert!(
        output.contains("active background tasks: unavailable"),
        "{output}"
    );
    assert!(
        !output.contains("vim mode: disabled"),
        "ACP has no editor mode to report as disabled: {output}"
    );
}

#[test]
fn acp_cost_command_never_reports_unknown_token_counts_as_zero() {
    let cwd = tempfile::tempdir().unwrap();
    let record = read_only_command_session(cwd.path());
    let output = super::handler::format_acp_read_only_command(&record, "cost", "", None)
        .expect("/cost has a read-only report");

    // `session duration` is genuinely measured; every token counter is not.
    for line in output.lines().filter(|line| !line.contains("duration")) {
        assert!(
            !line.contains(" 0"),
            "a hard-coded zero reads as a measured zero: {output}"
        );
    }
    assert!(
        !output.contains("$0.000000"),
        "unpriced usage must not render as a zero bill: {output}"
    );
    assert!(output.contains("total input: unavailable"), "{output}");
}

// ---- session-scoped config options ----

/// A session-scoped option is applied per session, read back per session,
/// and a refusal from the host reaches the client as invalid params.
#[tokio::test]
async fn session_scoped_config_option_is_per_session_and_can_be_refused() {
    let values: Arc<Mutex<std::collections::HashMap<String, String>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let apply_values = values.clone();
    let current_values = values.clone();
    let handler = DefaultHandler::default().with_session_config_options(SessionConfigOptions {
        options: vec![rebon_proto::types::ConfigOption {
            id: "agent".to_string(),
            name: "Agent".to_string(),
            description: None,
            category: Some("agent".to_string()),
            option_type: ConfigOptionType::Select,
            current_value: "local".to_string(),
            options: vec![
                rebon_proto::types::ConfigOptionValue {
                    value: "local".to_string(),
                    name: "Local".to_string(),
                    description: None,
                },
                rebon_proto::types::ConfigOptionValue {
                    value: "kernel:dsh".to_string(),
                    name: "dsh".to_string(),
                    description: None,
                },
            ],
        }],
        apply: Arc::new(move |session_id, config_id, value| {
            assert_eq!(config_id, "agent");
            if value == "kernel:dsh" || value == "local" {
                apply_values
                    .lock()
                    .unwrap()
                    .insert(session_id.to_string(), value.to_string());
                Ok(())
            } else {
                Err(format!("no agent named {value:?}"))
            }
        }),
        current: Arc::new(move |session_id| {
            vec![(
                "agent".to_string(),
                current_values
                    .lock()
                    .unwrap()
                    .get(session_id)
                    .cloned()
                    .unwrap_or_else(|| "local".to_string()),
            )]
        }),
    });

    // Two sessions.
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let mut input = Vec::new();
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
    );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/work"}}
"#,
    );
    input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/tmp/work"}}
"#,
    );
    client_in.write_all(&input).await.unwrap();
    drop(client_in);
    let h = handler.clone();
    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, h).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();
    let lines = ndjson_lines(&out);
    let session_of = |line: &[u8]| -> (String, Value) {
        let resp: JsonRpcResponse = serde_json::from_slice(line).unwrap();
        let result = resp.result.unwrap();
        (result["sessionId"].as_str().unwrap().to_string(), result)
    };
    let (a, a_new) = session_of(lines[1]);
    let (b, _) = session_of(lines[2]);
    assert_eq!(
        config_value(&a_new, "agent"),
        Some("local"),
        "session/new advertises the session-scoped option"
    );

    // Switch A, refuse a bad value on B, then read both back.
    let (mut client_in, server_in, server_out, client_out) = pipe_pair();
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"session/set_config_option","params":{{"sessionId":"{a}","configId":"agent","value":"kernel:dsh"}}}}
{{"jsonrpc":"2.0","id":5,"method":"session/set_config_option","params":{{"sessionId":"{b}","configId":"agent","value":"nope"}}}}
{{"jsonrpc":"2.0","id":6,"method":"session/set_config_option","params":{{"sessionId":"{b}","configId":"permissions","value":"plan"}}}}
{{"jsonrpc":"2.0","id":7,"method":"session/set_config_option","params":{{"sessionId":"ghost","configId":"agent","value":"local"}}}}
"#
    );
    client_in.write_all(req.as_bytes()).await.unwrap();
    drop(client_in);
    let h = handler.clone();
    let serve_task = tokio::spawn(async move {
        serve(server_in, server_out, h).await.unwrap();
    });
    let out = drain(client_out).await;
    serve_task.await.unwrap();
    let lines = ndjson_lines(&out);
    assert_eq!(lines.len(), 4);

    let switched: JsonRpcResponse = serde_json::from_slice(lines[0]).unwrap();
    assert!(switched.error.is_none(), "{switched:?}");
    assert_eq!(
        config_value(&switched.result.unwrap(), "agent"),
        Some("kernel:dsh")
    );

    let refused: JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
    let err = refused.error.expect("the host's refusal is an error");
    assert_eq!(err.code, error_code::INVALID_PARAMS);
    assert!(err.message.contains("no agent named"), "{}", err.message);

    // B's other options still work, and B's agent is still local — A's
    // switch did not leak into the shared list.
    let other: JsonRpcResponse = serde_json::from_slice(lines[2]).unwrap();
    let other = other.result.unwrap();
    assert_eq!(config_value(&other, "permissions"), Some("plan"));
    assert_eq!(config_value(&other, "agent"), Some("local"));

    let ghost: JsonRpcResponse = serde_json::from_slice(lines[3]).unwrap();
    assert_eq!(ghost.error.unwrap().code, error_code::INVALID_PARAMS);

    assert_eq!(
        config_value(
            &serde_json::json!({"configOptions": handler.config_options_for_session(&a)}),
            "agent"
        ),
        Some("kernel:dsh")
    );
    assert_eq!(
        config_value(
            &serde_json::json!({"configOptions": handler.config_options_for_session(&b)}),
            "agent"
        ),
        Some("local")
    );
    assert_eq!(
        handler
            .config_options_snapshot()
            .iter()
            .find(|option| option.id == "agent")
            .unwrap()
            .current_value,
        "local",
        "the shared list never learns a per-session value"
    );
}

#[test]
#[should_panic(expected = "already a server-wide option")]
fn a_session_scoped_option_may_not_shadow_a_shared_one() {
    let _ = DefaultHandler::default().with_session_config_options(SessionConfigOptions {
        options: vec![rebon_proto::types::ConfigOption {
            id: "permissions".to_string(),
            name: "Permissions".to_string(),
            description: None,
            category: None,
            option_type: ConfigOptionType::Select,
            current_value: "default".to_string(),
            options: Vec::new(),
        }],
        apply: Arc::new(|_, _, _| Ok(())),
        current: Arc::new(|_| Vec::new()),
    });
}

/// `session/load` restores a session for the engine and tells the client
/// nothing about what the session said.
///
/// This surprises people, and the work that followed assumed the opposite,
/// so it is pinned rather than left to be
/// rediscovered. The loaded transcript becomes the model's context for the
/// next turn; not one `session/update` reaches the client, so an ACP peer
/// that resumes a session shows an empty screen unless it reads the
/// transcript itself.
///
/// Making it replay would be a new wire behaviour, not a refactor: the rows
/// exist (`rebon_render::transcript_replay::replayed_rows` produces exactly
/// what the terminal draws) but `SessionUpdate` has no variant for a
/// committed user, system, attachment or folded row, so carrying them needs
/// a shape decision this test deliberately does not prejudge.
#[tokio::test]
async fn session_load_restores_the_transcript_without_replaying_it_to_the_client() {
    let tmp = fresh_tempdir("load-no-replay");
    let cwd = "/tmp/work";
    let sid = "sess-no-replay";
    write_fixture_transcript(
        tmp.path(),
        cwd,
        sid,
        &[
            ("root", None, "user", "2026-09-06T00:00:00Z"),
            ("a1", Some("root"), "assistant", "2026-09-06T00:00:01Z"),
        ],
    );

    let updates = rebon_agent_core::publisher::MemorySessionUpdatePublisher::new();
    let handler = DefaultHandler {
        projects_root: Some(tmp.path().to_path_buf()),
        ..DefaultHandler::default()
    }
    .with_update_publisher(Arc::new(updates.clone()));

    handler
        .handle_request(
            "initialize",
            Some(serde_json::json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    let result = handler
        .handle_request(
            "session/load",
            Some(serde_json::json!({"sessionId": sid, "cwd": cwd})),
        )
        .await
        .unwrap();

    assert_eq!(result["sessionId"], sid);
    let record = handler.state().get_session(sid).unwrap();
    assert_eq!(
        record.loaded_transcript.len(),
        2,
        "the engine gets the transcript"
    );
    assert!(
        updates.is_empty(),
        "and the client gets none of it: {:?}",
        updates.snapshot()
    );
}
