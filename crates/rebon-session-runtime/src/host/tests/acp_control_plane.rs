//! The control plane answered as ACP, over a real socket.
//!
//! The unit tests next to `wire_probe`, `acp_gate` and
//! `wire_errors` pin each rule on its own; these pin the thing none of them
//! can: that a real `BackgroundIpcServer`, on one port, answers a JSON-RPC
//! client *and* still answers the legacy envelope every shipped build sends.
//!
//! Every socket here carries a read timeout. A test that hangs tells nobody
//! anything, and the failure this guards against — a connection the server
//! kept open when it should have closed it, or closed when it should have kept
//! it — looks exactly like a hang.

#[path = "acp_prompt.rs"]
mod prompt_completion;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

use super::super::*;
use super::support::*;
use rebon_core::permission::{
    ChannelPermissionBroker, OutboundPermissionQuery, PermissionOptionKind, PermissionQueryOption,
};

/// Long enough that a loaded machine does not fail the suite, short enough
/// that a genuine hang is a failure rather than a coffee break.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn hosted_job() -> (
    tempfile::TempDir,
    BackgroundStore,
    BackgroundJobState,
    BackgroundIpcServer,
) {
    let (dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("session-under-test".into());
    let ipc = start_background_ipc_server(&store, state.job_id()).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();
    (dir, store, state, ipc)
}

/// A hand-written ACP client. Deliberately not the production client: this has
/// to be able to send things a correct client never would.
struct Peer {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    next_id: i64,
}

impl Peer {
    fn connect(ipc: &BackgroundIpcServer) -> Self {
        let address = format!("127.0.0.1:{}", ipc.owner().endpoint.port);
        let stream = TcpStream::connect(address).expect("the server is listening");
        stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        Self {
            stream,
            reader,
            next_id: 1,
        }
    }

    fn send(&mut self, method: &str, params: serde_json::Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut body = serde_json::to_vec(&line).unwrap();
        body.push(b'\n');
        self.stream.write_all(&body).unwrap();
        self.stream.flush().unwrap();
        id
    }

    /// The next response, or `None` when the server closed the connection.
    fn read(&mut self) -> Option<serde_json::Value> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(serde_json::from_str(&line).expect("the server writes JSON")),
            Err(error) => panic!("reading the answer failed: {error}"),
        }
    }

    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.send(method, params);
        let answer = self.read().expect("the server answered");
        assert_eq!(answer["id"], serde_json::json!(id), "answered the wrong id");
        answer
    }

    /// Answer a request the *server* asked, by its id.
    fn send_response(&mut self, id: serde_json::Value, result: serde_json::Value) {
        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        let mut body = serde_json::to_vec(&line).unwrap();
        body.push(b'\n');
        self.stream.write_all(&body).unwrap();
        self.stream.flush().unwrap();
    }

    /// Whether the server has closed this connection.
    fn is_closed(&mut self) -> bool {
        let mut byte = [0u8; 1];
        matches!(self.stream.read(&mut byte), Ok(0))
    }
}

/// Exercise the real command waiter, not just the error codec. Each row starts
/// on one wire and retries on both, so a legacy first answer must not erase the
/// category before an ACP reconnect reads the shared idempotency history.
fn command_business_error_roundtrip(legacy_first: bool, cancel: bool) {
    use rebon_session_host::{wire_errors, HostCallError};

    for run_command in [true, false] {
        let (_dir, _store, state, ipc) = hosted_job();
        let command_id = "business-error";
        let (method, mut params, request, command_name) = if run_command {
            (
                "_session/run_command",
                serde_json::json!({"name": "hooks", "args": []}),
                BackgroundIpcRequest::RunCommand {
                    name: "hooks".into(),
                    args: vec![],
                },
                "hooks",
            )
        } else {
            (
                "_session/rewind",
                serde_json::json!({"userMessageUuid": "message", "scope": "conversation"}),
                BackgroundIpcRequest::Rewind {
                    user_message_uuid: "message".into(),
                    scope: rebon_session_host::RewindScopeWire::Conversation,
                },
                "rewind",
            )
        };
        params["_meta"] = serde_json::json!({"rebon": {"commandId": command_id}});
        let timeout = rebon_session_host::command_response_timeout(command_name) + READ_TIMEOUT;
        let mut peer = Peer::connect(&ipc);
        peer.reader
            .get_ref()
            .set_read_timeout(Some(timeout))
            .unwrap();
        peer.call("initialize", meta_with_token(&token(&ipc)));
        let legacy_body = serde_json::to_vec(&BackgroundIpcEnvelope {
            protocol_version: rebon_session_host::BACKGROUND_IPC_PROTOCOL_VERSION,
            token: token(&ipc),
            job_id: Some(state.job_id().into()),
            session_id: state.session_id().map(str::to_owned),
            command_id: Some(command_id.into()),
            request,
        })
        .unwrap();
        let send_legacy = || {
            let mut socket = TcpStream::connect(("127.0.0.1", ipc.owner().endpoint.port)).unwrap();
            socket.set_read_timeout(Some(timeout)).unwrap();
            socket.write_all(&legacy_body).unwrap();
            socket.write_all(b"\n").unwrap();
            BufReader::new(socket)
        };
        let mut legacy = if legacy_first {
            Some(send_legacy())
        } else {
            peer.send(method, params.clone());
            None
        };
        // Receiving the command proves the request reached the server waiter.
        // Keep its sender alive without answering; a deadline must expire on
        // the server, not on a production client's shorter local budget.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let pending = runtime.block_on(async {
            tokio::time::timeout(READ_TIMEOUT, ipc.recv_command())
                .await
                .unwrap()
                .unwrap()
        });
        assert_eq!(pending.name, command_name);
        assert_eq!(ipc.calls_in_flight(), 1);
        if cancel {
            let mut controller = Peer::connect(&ipc);
            controller.call("initialize", meta_with_token(&token(&ipc)));
            let released = controller.call(
                "_session/cancel_call",
                serde_json::json!({"commandId": command_id}),
            );
            assert_eq!(released["result"]["released"], true);
        }
        let expected = if cancel {
            HostCallError::Cancelled
        } else {
            HostCallError::HostUnanswered
        };
        let assert_acp = |answer: serde_json::Value| {
            assert!(
                answer.get("result").is_none(),
                "business failure was a success: {answer}"
            );
            let error: rebon_proto::types::JsonRpcError =
                serde_json::from_value(answer["error"].clone()).unwrap();
            assert_eq!(
                error.code,
                wire_errors::to_json_rpc(&expected).code,
                "lost typed business category: {answer}"
            );
            assert_eq!(wire_errors::from_json_rpc(&error), expected);
        };
        let legacy_error = if cancel {
            "background command was cancelled by the client"
        } else {
            "background command timed out"
        };
        let expected_legacy = if run_command {
            format!("{{\"error\":\"{legacy_error}\"}}\n")
        } else {
            format!("{{\"ok\":false,\"error\":\"{legacy_error}\"}}\n")
        };
        if let Some(reader) = legacy.as_mut() {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, expected_legacy, "legacy bytes changed");
        } else {
            assert_acp(peer.read().unwrap());
        }
        assert_eq!(ipc.calls_in_flight(), 0);
        assert!(
            pending
                .response_tx
                .send(Ok(rebon_session_host::CommandOutput {
                    text: "too late".into(),
                    tone: "info".into(),
                }))
                .is_err(),
            "the abandoned waiter is still held"
        );
        // Reconnect: this must read the typed remembered answer, never execute.
        let mut replay_peer = Peer::connect(&ipc);
        replay_peer.call("initialize", meta_with_token(&token(&ipc)));
        assert_acp(replay_peer.call(method, params));
        let mut replay = String::new();
        send_legacy().read_line(&mut replay).unwrap();
        assert_eq!(replay, expected_legacy, "legacy replay bytes changed");
        assert!(
            ipc.try_recv_command().is_none(),
            "a replay ran the command twice"
        );
    }
}

#[test]
fn business_error_cancel_acp_then_both_replays() {
    command_business_error_roundtrip(false, true);
}

#[test]
fn business_error_cancel_legacy_then_both_replays() {
    command_business_error_roundtrip(true, true);
}

#[test]
fn business_error_deadline_acp_then_both_replays() {
    command_business_error_roundtrip(false, false);
}

#[test]
fn business_error_deadline_legacy_then_both_replays() {
    command_business_error_roundtrip(true, false);
}

#[test]
fn business_error_command_disconnect_and_refusal_stay_typed() {
    use rebon_session_host::{wire_errors, HostCallError};
    let diagnostic = "unsupported token was cancelled while closing";
    for outcome in [
        None,
        Some(HostCallError::Refused(diagnostic.into())),
        Some(HostCallError::SessionFailure),
        Some(HostCallError::Cancelled),
        Some(HostCallError::HostUnanswered),
    ] {
        let (_dir, _store, _state, ipc) = hosted_job();
        let mut peer = Peer::connect(&ipc);
        peer.call("initialize", meta_with_token(&token(&ipc)));
        let params = serde_json::json!({
            "name": "hooks", "args": [],
            "_meta": {"rebon": {"commandId": "command-failure"}},
        });
        peer.send("_session/run_command", params.clone());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let pending = runtime.block_on(async {
            tokio::time::timeout(READ_TIMEOUT, ipc.recv_command())
                .await
                .unwrap()
                .unwrap()
        });
        let expected = if let Some(kind) = outcome {
            // Prose is not a protocol. Even a typed producer's cancellation or
            // session failure must keep its category through the turn channel.
            let response = if matches!(kind, HostCallError::Refused(_)) {
                crate::host::ipc::server::request_refused(diagnostic)
            } else {
                Err((kind.clone(), diagnostic.to_string()))
            };
            pending.response_tx.send(response).unwrap();
            kind
        } else {
            drop(pending);
            HostCallError::Transport("background command channel is closed".into())
        };
        for answer in [
            peer.read().unwrap(),
            peer.call("_session/run_command", params),
        ] {
            let error = serde_json::from_value(answer["error"].clone()).unwrap();
            assert_eq!(wire_errors::from_json_rpc(&error), expected);
        }
        assert!(ipc.try_recv_command().is_none());
    }
}

#[test]
fn business_error_refactor_preserves_successful_command_replays() {
    for legacy_first in [false, true] {
        for run_command in [false, true] {
            let (_dir, _store, state, ipc) = hosted_job();
            let (method, mut params, request) = if run_command {
                (
                    "_session/run_command",
                    serde_json::json!({"name": "hooks", "args": []}),
                    BackgroundIpcRequest::RunCommand {
                        name: "hooks".into(),
                        args: vec![],
                    },
                )
            } else {
                (
                    "_session/rewind",
                    serde_json::json!({"userMessageUuid": "message", "scope": "conversation"}),
                    BackgroundIpcRequest::Rewind {
                        user_message_uuid: "message".into(),
                        scope: rebon_session_host::RewindScopeWire::Conversation,
                    },
                )
            };
            params["_meta"] = serde_json::json!({"rebon": {"commandId": "success"}});
            let envelope = BackgroundIpcEnvelope {
                protocol_version: rebon_session_host::BACKGROUND_IPC_PROTOCOL_VERSION,
                token: token(&ipc),
                job_id: Some(state.job_id().into()),
                session_id: state.session_id().map(str::to_owned),
                command_id: Some("success".into()),
                request,
            };
            let send_legacy = || {
                let mut socket = TcpStream::connect(("127.0.0.1", ipc.port)).unwrap();
                socket.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
                serde_json::to_writer(&mut socket, &envelope).unwrap();
                socket.write_all(b"\n").unwrap();
                BufReader::new(socket)
            };
            let mut peer = Peer::connect(&ipc);
            peer.call("initialize", meta_with_token(&token(&ipc)));
            let mut legacy = if legacy_first {
                Some(send_legacy())
            } else {
                peer.send(method, params.clone());
                None
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let pending = runtime.block_on(async {
                tokio::time::timeout(READ_TIMEOUT, ipc.recv_command())
                    .await
                    .unwrap()
                    .unwrap()
            });
            pending
                .response_tx
                .send(Ok(rebon_session_host::CommandOutput {
                    text: "one execution".into(),
                    tone: "info".into(),
                }))
                .unwrap();
            let expected_acp =
                serde_json::json!({"output": {"text": "one execution", "tone": "info"}});
            let expected_legacy = if run_command {
                "{\"output\":{\"text\":\"one execution\",\"tone\":\"info\"}}\n"
            } else {
                "{\"ok\":true,\"data\":{\"text\":\"one execution\",\"tone\":\"info\"}}\n"
            };
            if let Some(reader) = legacy.as_mut() {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line, expected_legacy);
            } else {
                assert_eq!(peer.read().unwrap()["result"], expected_acp);
            }
            assert_eq!(peer.call(method, params)["result"], expected_acp);
            let mut replay = String::new();
            send_legacy().read_line(&mut replay).unwrap();
            assert_eq!(replay, expected_legacy);
            assert!(
                ipc.try_recv_command().is_none(),
                "success was executed again"
            );
        }
    }
}

#[test]
fn business_error_shared_handlers_keep_generation_and_refusal_on_replay() {
    use rebon_session_host::{wire_errors, HostCallError};
    let (_dir, _store, state, ipc, mut receiver, query_id) =
        hosted_job_with_a_permission_in_flight();
    let mut peer = Peer::connect(&ipc);
    peer.call("initialize", meta_with_token(&token(&ipc)));
    let cases = [
        (
            "_session/answer_questions",
            serde_json::json!({
                "queryId": query_id, "turnGeneration": 0, "answers": [],
                "_meta": {"rebon": {"commandId": "stale-question"}},
            }),
            BackgroundIpcRequest::AnswerQuestions {
                query_id,
                turn_generation: 0,
                answers: vec![],
            },
            "stale-question",
            HostCallError::StaleGeneration,
            format!("question query {query_id} belongs to a different turn generation"),
        ),
        (
            "_session/set_option",
            serde_json::json!({
                "key": "token", "value": "value", "_meta": {"rebon": {"commandId": "refused-option"}},
            }),
            BackgroundIpcRequest::SetSessionOption {
                key: "token".into(),
                value: "value".into(),
            },
            "refused-option",
            HostCallError::Refused("unknown session option `token`".into()),
            "unknown session option `token`".into(),
        ),
    ];
    for (method, params, request, id, expected, diagnostic) in cases {
        for answer in [peer.call(method, params.clone()), peer.call(method, params)] {
            let error = serde_json::from_value(answer["error"].clone()).unwrap();
            assert_eq!(wire_errors::from_json_rpc(&error), expected);
        }
        let mut socket = TcpStream::connect(("127.0.0.1", ipc.port)).unwrap();
        socket.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        serde_json::to_writer(
            &mut socket,
            &BackgroundIpcEnvelope {
                protocol_version: rebon_session_host::BACKGROUND_IPC_PROTOCOL_VERSION,
                token: token(&ipc),
                job_id: Some(state.job_id().into()),
                session_id: state.session_id().map(str::to_owned),
                command_id: Some(id.into()),
                request,
            },
        )
        .unwrap();
        socket.write_all(b"\n").unwrap();
        let mut line = String::new();
        BufReader::new(socket).read_line(&mut line).unwrap();
        assert_eq!(
            line,
            format!(
                "{{\"ok\":false,\"error\":{}}}\n",
                serde_json::to_string(&diagnostic).unwrap()
            )
        );
    }
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
}

fn meta_with_token(token: &str) -> serde_json::Value {
    serde_json::json!({"_meta": {"rebon": {"token": token}}})
}

fn token(ipc: &BackgroundIpcServer) -> String {
    ipc.owner().endpoint.token
}

/// The point of the whole change: one connection, several requests. The legacy
/// protocol had to open a socket per request; this one does not.
#[test]
fn one_connection_answers_several_requests() {
    let (_dir, _store, state, ipc) = hosted_job();
    let mut peer = Peer::connect(&ipc);

    let hello = peer.call("initialize", meta_with_token(&token(&ipc)));
    assert_eq!(hello["result"]["protocolVersion"], serde_json::json!(1));
    // The extension advertises itself where steering does, so a client can
    // tell a worker that speaks `_session/*` from one that does not without
    // probing method by method.
    let advertised = hello["result"]["_meta"]["rebon"]["methods"]
        .as_array()
        .expect("the extension methods are advertised");
    assert!(advertised
        .iter()
        .any(|name| name == &serde_json::json!("_session/status")));

    let pong = peer.call("_session/ping", serde_json::json!({}));
    assert_eq!(pong["result"], serde_json::json!({}));
    assert!(pong.get("error").is_none());

    let status = peer.call("_session/status", serde_json::json!({}));
    assert_eq!(
        status["result"]["snapshot"]["jobId"],
        serde_json::json!(state.job_id()),
        "the snapshot is the owner's, read fresh"
    );
}

/// Fail-closed means closed: one error, then the socket ends. A peer that
/// guessed wrong pays for a new connection before it can guess again.
#[test]
fn a_wrong_token_is_refused_and_the_connection_ends() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = Peer::connect(&ipc);

    let refusal = peer.call("initialize", meta_with_token("not-the-token"));
    assert_eq!(refusal["error"]["code"], serde_json::json!(-32011));
    assert_eq!(
        refusal["error"]["data"]["kind"],
        serde_json::json!("unauthenticated")
    );
    assert!(refusal.get("result").is_none());
    assert!(
        peer.is_closed(),
        "a failed authentication must end the connection, not merely refuse one call"
    );
}

/// A missing token is answered exactly like a wrong one, over the wire and not
/// only in the gate's unit test.
#[test]
fn a_missing_token_is_refused_the_same_way() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = Peer::connect(&ipc);
    let refusal = peer.call("initialize", serde_json::json!({}));
    assert_eq!(refusal["error"]["code"], serde_json::json!(-32011));
    assert!(peer.is_closed());
}

/// ACP's own rule, and the reason the token can hang on that one moment. The
/// connection survives, because the peer has not claimed anything yet.
#[test]
fn nothing_is_answered_before_initialize() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = Peer::connect(&ipc);

    let early = peer.call("_session/status", serde_json::json!({}));
    assert_eq!(early["error"]["code"], serde_json::json!(-32600));

    let hello = peer.call("initialize", meta_with_token(&token(&ipc)));
    assert_eq!(hello["result"]["protocolVersion"], serde_json::json!(1));
    let pong = peer.call("_session/ping", serde_json::json!({}));
    assert_eq!(pong["result"], serde_json::json!({}));
}

/// An unknown method is method-not-found from an authenticated peer, and the
/// connection carries on. A later change turns most of these into real answers.
#[test]
fn an_unknown_method_is_method_not_found_and_the_connection_lives() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = Peer::connect(&ipc);
    peer.call("initialize", meta_with_token(&token(&ipc)));

    let unknown = peer.call("_session/nothing_like_this", serde_json::json!({}));
    assert_eq!(unknown["error"]["code"], serde_json::json!(-32601));

    let pong = peer.call("_session/ping", serde_json::json!({}));
    assert_eq!(pong["result"], serde_json::json!({}));
}

/// The compatibility guarantee, on one port: the same server that just
/// answered JSON-RPC still answers the envelope every shipped client sends.
#[test]
fn the_same_server_still_answers_a_legacy_envelope() {
    let (_dir, _store, state, ipc) = hosted_job();

    // First prove the ACP path works on this server.
    let mut peer = Peer::connect(&ipc);
    peer.call("initialize", meta_with_token(&token(&ipc)));
    assert_eq!(
        peer.call("_session/ping", serde_json::json!({}))["result"],
        serde_json::json!({})
    );
    drop(peer);

    // Then the legacy one, through the production client rather than a
    // hand-rolled envelope, so this pins what a real old build does.
    let answer = rebon_session_host::OwnerHandle::for_worker(
        state.session_id().unwrap_or_default(),
        Some(state.job_id()),
        &ipc.owner().endpoint,
    )
    .send(rebon_session_host::BackgroundIpcRequest::Ping, None);
    assert!(
        answer.is_ok(),
        "the legacy envelope stopped working: {answer:?}"
    );
}

/// Header framing is the other half of ACP's auto-detection, and the probe
/// resolves it by the first byte alone. Worth pinning on a socket, because it
/// is the one path where the probe must consume nothing at all.
#[test]
fn content_length_framing_is_answered_too() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let address = format!("127.0.0.1:{}", ipc.owner().endpoint.port);
    let mut stream = TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    let body = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "initialize",
        "params": {"_meta": {"rebon": {"token": token(&ipc)}}},
    }))
    .unwrap();
    let mut frame = rebon_proto::framing::content_length_header(body.len()).into_bytes();
    frame.extend_from_slice(&body);
    stream.write_all(&frame).unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut header = String::new();
    while reader.read_line(&mut header).unwrap() > 0 {
        if header.ends_with("\r\n\r\n") {
            break;
        }
    }
    let length: usize = header
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .expect("the answer is header framed")
        .trim()
        .parse()
        .unwrap();
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload).unwrap();
    let answer: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(answer["id"], serde_json::json!(7));
    assert_eq!(answer["result"]["protocolVersion"], serde_json::json!(1));
}

/// A second `initialize` must not be a second chance to present a token, and
/// must not break a connection that was already good.
#[test]
fn initialize_does_not_reopen() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = Peer::connect(&ipc);
    peer.call("initialize", meta_with_token(&token(&ipc)));

    let again = peer.call("initialize", meta_with_token("not-the-token"));
    assert_eq!(again["error"]["code"], serde_json::json!(-32600));

    let pong = peer.call("_session/ping", serde_json::json!({}));
    assert_eq!(
        pong["result"],
        serde_json::json!({}),
        "a refused second initialize must not close a connection that was fine"
    );
}

/// A request may carry rebon's own facts in `_meta.rebon`; these tests need
/// the command id, which is what makes a retry idempotent.
fn meta(fields: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "_meta": { "rebon": fields } })
}

/// An authenticated peer that has already said `initialize`.
fn attached(ipc: &BackgroundIpcServer) -> Peer {
    let mut peer = Peer::connect(ipc);
    let hello = peer.call("initialize", meta_with_token(&token(ipc)));
    assert!(hello.get("error").is_none(), "initialize failed: {hello}");
    peer
}

#[test]
fn standard_prompt_does_not_answer_before_execution_and_keeps_reading() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = attached(&ipc);
    let prompt = peer.send(
        "session/prompt",
        serde_json::json!({
            "sessionId": "session-under-test",
            "prompt": [{"type": "text", "text": "wait for my turn"}]
        }),
    );
    let ping = peer.send("_session/ping", serde_json::json!({}));
    let next = peer.read().expect("the reader must remain available");
    assert_eq!(
        next["id"], ping,
        "prompt {prompt} answered before executing: {next}"
    );
    ipc.stop();
}

/// A lease taken over ACP shows up in the status every client reads, which is
/// the whole point of routing both protocols through one implementation: the
/// answer does not depend on which one you used.
#[test]
fn a_lease_taken_over_acp_is_visible_in_the_status() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = attached(&ipc);

    let taken = peer.call(
        "_session/lease",
        serde_json::json!({"clientId": "tui-1", "kind": "tui"}),
    );
    assert!(
        taken.get("error").is_none(),
        "the lease was refused: {taken}"
    );

    let status = peer.call("_session/status", serde_json::json!({}));
    let leases = status["result"]["snapshot"]["clientLeases"]
        .as_array()
        .expect("the snapshot carries the leases");
    assert!(
        leases.iter().any(|lease| lease["clientId"] == "tui-1"),
        "the lease is missing from {status}"
    );

    let released = peer.call(
        "_session/release_lease",
        serde_json::json!({"clientId": "tui-1", "deliberate": true}),
    );
    assert!(released.get("error").is_none(), "{released}");

    let after = peer.call("_session/status", serde_json::json!({}));
    let leases = after["result"]["snapshot"]["clientLeases"]
        .as_array()
        .map(|leases| leases.len())
        .unwrap_or(0);
    assert_eq!(leases, 0, "the lease outlived its release: {after}");
}

/// The permission mode a client sets is the one every other client then reads.
#[test]
fn a_permission_mode_set_over_acp_is_what_the_status_reports() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = attached(&ipc);

    let set = peer.call(
        "_session/set_permission_mode",
        serde_json::json!({"mode": "plan"}),
    );
    assert!(set.get("error").is_none(), "the mode was refused: {set}");

    let status = peer.call("_session/status", serde_json::json!({}));
    assert_eq!(
        status["result"]["snapshot"]["permissionMode"],
        serde_json::json!("plan")
    );
}

/// Giving up on a call that is not running is an answer, not a failure: it is
/// what a client that raced its own timeout needs to hear.
#[test]
fn cancelling_a_call_that_is_not_running_answers_that_there_was_nothing() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = attached(&ipc);
    let answer = peer.call(
        "_session/cancel_call",
        serde_json::json!({"commandId": "no-such-call"}),
    );
    assert_eq!(
        answer["result"],
        serde_json::json!({"released": false}),
        "{answer}"
    );
}

/// Params that will not decode are `-32602`, not `-32601`: the method exists,
/// the call was wrong. A client acts on those differently -- one is worth
/// fixing and resending, the other is worth falling back over.
#[test]
fn params_that_do_not_decode_are_invalid_params() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = attached(&ipc);
    let answer = peer.call(
        "_session/lease",
        serde_json::json!({"clientId": "tui-1", "kind": "not-a-surface"}),
    );
    assert_eq!(
        answer["error"]["code"],
        serde_json::json!(-32602),
        "{answer}"
    );
    // And the connection carries on, because a bad call is not a bad peer.
    let pong = peer.call("_session/ping", serde_json::json!({}));
    assert_eq!(pong["result"], serde_json::json!({}));
}

/// A method whose fields are all optional is callable with no params at all.
///
/// What is under test is the decode, not the operation: this fixture has no
/// task registry attached, so `_session/cancel_tasks` fails for its own
/// reasons whatever it is handed. The claim is narrower and exact -- absent
/// params are not a *params* error -- so that is what is asserted.
#[test]
fn a_method_with_only_optional_fields_needs_no_params() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = attached(&ipc);
    let id = peer.next_id;
    peer.next_id += 1;
    let line = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "_session/cancel_tasks",
    });
    let mut body = serde_json::to_vec(&line).unwrap();
    body.push(b'\n');
    peer.stream.write_all(&body).unwrap();
    peer.stream.flush().unwrap();
    let answer = peer.read().expect("the server answered");
    assert_ne!(
        answer["error"]["code"],
        serde_json::json!(-32602),
        "absent params were read as a params error: {answer}"
    );
}

/// Naming the wrong job is refused even on an authenticated connection. The
/// token says who you are; this says which worker you meant, and a client can
/// get that wrong on any single message after a worker was replaced.
#[test]
fn naming_the_wrong_job_is_refused() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = attached(&ipc);
    let answer = peer.call(
        "_session/ping",
        meta(serde_json::json!({"jobId": "some-other-job"})),
    );
    assert!(
        answer.get("result").is_none(),
        "a request for another job was answered: {answer}"
    );
    assert_eq!(
        answer["error"]["code"],
        serde_json::json!(-32012),
        "{answer}"
    );
}

/// A retry carrying an id already answered is answered from memory rather than
/// run again -- proven by sending a *different* request under the same id and
/// watching the first answer come back instead.
#[test]
fn a_repeated_command_id_replays_the_first_answer() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = attached(&ipc);

    let first = peer.call(
        "_session/cancel_call",
        merge(
            serde_json::json!({"commandId": "target"}),
            meta(serde_json::json!({"commandId": "retried-once"})),
        ),
    );
    assert_eq!(first["result"], serde_json::json!({"released": false}));

    // A release under the same id must not run: the answer is the remembered
    // one, and the lease taken below survives.
    peer.call(
        "_session/lease",
        serde_json::json!({"clientId": "tui-1", "kind": "tui"}),
    );
    let replayed = peer.call(
        "_session/release_lease",
        merge(
            serde_json::json!({"clientId": "tui-1"}),
            meta(serde_json::json!({"commandId": "retried-once"})),
        ),
    );
    assert_eq!(
        replayed["result"],
        serde_json::json!({"released": false}),
        "the id was not replayed: {replayed}"
    );

    let status = peer.call("_session/status", serde_json::json!({}));
    let leases = status["result"]["snapshot"]["clientLeases"]
        .as_array()
        .map(|leases| leases.len())
        .unwrap_or(0);
    assert_eq!(
        leases, 1,
        "the replayed request ran anyway and released the lease: {status}"
    );
}

/// One memory, both protocols. A command answered over ACP and retried over
/// the legacy envelope gets the same answer -- otherwise the idempotency
/// guarantee is per-protocol, which is to say not a guarantee.
#[test]
fn the_idempotency_memory_is_shared_across_the_two_protocols() {
    let (_dir, _store, state, ipc) = hosted_job();
    let mut peer = attached(&ipc);
    peer.call(
        "_session/lease",
        merge(
            serde_json::json!({"clientId": "tui-1", "kind": "tui"}),
            meta(serde_json::json!({"commandId": "shared-id"})),
        ),
    );
    drop(peer);

    // The same id over the legacy wire, carrying a request that would release
    // the lease if it ran.
    let answer = rebon_session_host::OwnerHandle::for_worker(
        state.session_id().unwrap_or_default(),
        Some(state.job_id()),
        &ipc.owner().endpoint,
    )
    .send(
        rebon_session_host::BackgroundIpcRequest::ReleaseLease {
            client_id: "tui-1".into(),
            deliberate: true,
        },
        Some("shared-id".to_string()),
    );
    assert!(answer.is_ok(), "{answer:?}");

    let status = rebon_session_host::OwnerHandle::for_worker(
        state.session_id().unwrap_or_default(),
        Some(state.job_id()),
        &ipc.owner().endpoint,
    )
    .status()
    .expect("the owner answers a status");
    assert_eq!(
        status.client_leases.len(),
        1,
        "an id first answered over ACP was run again over the envelope"
    );
}

/// Two objects, merged. The params and the `_meta` are written separately in
/// these tests because they are separate ideas.
fn merge(mut left: serde_json::Value, right: serde_json::Value) -> serde_json::Value {
    let (Some(left_object), Some(right_object)) = (left.as_object_mut(), right.as_object()) else {
        panic!("both sides must be objects");
    };
    for (key, value) in right_object {
        left_object.insert(key.clone(), value.clone());
    }
    left
}

/// A prompt sent as standard ACP lands as the pending prompt the session's
/// turn loop picks up -- the same place a legacy `Reply` puts it. This is the
/// end-to-end version of the translation test: not "it became the right
/// request" but "the job record changed".
#[test]
fn enqueue_acknowledges_the_pending_prompt_without_running_it() {
    let (_dir, store, state, ipc) = hosted_job();
    let mut peer = attached(&ipc);

    let sent = peer.call(
        "_session/enqueue",
        serde_json::json!({
            "sessionId": "session-under-test",
            "prompt": [{"type": "text", "text": "carry on then"}],
        }),
    );
    assert!(
        sent.get("error").is_none(),
        "the prompt was refused: {sent}"
    );

    let after = store
        .read_state(state.job_id())
        .expect("the job is readable");
    assert_eq!(
        pending_text(&after),
        Some("carry on then"),
        "the prompt did not reach the job record"
    );
}

/// Naming another session in the standard field is refused. A plain ACP client
/// says which session it means there rather than in `_meta`, so reading it is
/// what makes that field fence instead of decorate.
#[test]
fn a_prompt_for_another_session_is_refused() {
    let (_dir, store, state, ipc) = hosted_job();
    let mut peer = attached(&ipc);

    let refused = peer.call(
        "session/prompt",
        serde_json::json!({
            "sessionId": "some-other-session",
            "prompt": [{"type": "text", "text": "not for you"}],
        }),
    );
    assert_eq!(
        refused["error"]["code"],
        serde_json::json!(-32012),
        "{refused}"
    );
    let after = store
        .read_state(state.job_id())
        .expect("the job is readable");
    assert_eq!(
        pending_text(&after),
        None,
        "a prompt for another session was queued anyway"
    );
}

/// A cancel notification cancels, and answers nothing.
///
/// Both halves matter and each would pass on its own for the wrong reason: a
/// notification that is silently dropped also writes no reply, and a cancel
/// that answered would also have cancelled. So this asserts the job record
/// moved *and* that the next thing on the wire is the next request's answer.
#[test]
fn a_cancel_notification_cancels_and_answers_nothing() {
    let (_dir, store, state, ipc) = hosted_job();
    let mut peer = attached(&ipc);

    let before = store
        .read_state(state.job_id())
        .expect("the job is readable");
    assert_ne!(
        before.process.status,
        BackgroundJobStatus::Idle,
        "this test needs a job that is not already idle"
    );

    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": {"sessionId": "session-under-test"},
    });
    let mut body = serde_json::to_vec(&notification).unwrap();
    body.push(b'\n');
    peer.stream.write_all(&body).unwrap();
    peer.stream.flush().unwrap();

    // The next request's answer must be the next thing on the wire. If the
    // notification had produced one, this would read that instead -- and a
    // client matching answers to requests by id would be one behind for the
    // rest of the connection.
    let pong = peer.call("_session/ping", serde_json::json!({}));
    assert_eq!(pong["result"], serde_json::json!({}), "{pong}");

    let after = store
        .read_state(state.job_id())
        .expect("the job is readable");
    assert_eq!(
        after.process.status,
        BackgroundJobStatus::Idle,
        "the cancel was not carried out"
    );
    assert_eq!(after.outcome.summary.as_deref(), Some("turn cancelled"));
}

/// A cancel that names another session is dropped. It is a notification, so
/// there is nothing to refuse it with -- which makes the fence the only thing
/// standing between a stray cancel and somebody else's turn.
#[test]
fn a_cancel_for_another_session_does_not_cancel_this_one() {
    let (_dir, store, state, ipc) = hosted_job();
    let mut peer = attached(&ipc);

    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": {"sessionId": "some-other-session"},
    });
    let mut body = serde_json::to_vec(&notification).unwrap();
    body.push(b'\n');
    peer.stream.write_all(&body).unwrap();
    peer.stream.flush().unwrap();

    // Ordered behind the notification on the same connection, so by the time
    // this is answered the notification has been handled.
    peer.call("_session/ping", serde_json::json!({}));

    let after = store
        .read_state(state.job_id())
        .expect("the job is readable");
    assert_ne!(
        after.process.status,
        BackgroundJobStatus::Idle,
        "a cancel for another session cancelled this one"
    );
}

/// A notification before `initialize` is dropped rather than refused: there is
/// no id to put a refusal against, and a peer that cannot receive an error is
/// not being told anything by one.
#[test]
fn a_notification_before_initialize_is_dropped_and_the_connection_lives() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = Peer::connect(&ipc);

    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": {"sessionId": "session-under-test"},
    });
    let mut body = serde_json::to_vec(&notification).unwrap();
    body.push(b'\n');
    peer.stream.write_all(&body).unwrap();
    peer.stream.flush().unwrap();

    let hello = peer.call("initialize", meta_with_token(&token(&ipc)));
    assert_eq!(
        hello["result"]["protocolVersion"],
        serde_json::json!(1),
        "the connection did not survive an early notification: {hello}"
    );
}

/// What one connection is worth, in milliseconds.
///
/// Not a benchmark that guards anything -- `#[ignore]`d, prints rather than
/// asserts. It exists because "is the ACP change worth it" and "why did a
/// hosted test time out" turn out to be the same question, and the answer is a
/// number rather than an argument.
///
/// Run with `cargo test -p rebon-session-runtime -- --ignored --nocapture
/// connection_cost`.
#[test]
#[ignore = "a measurement, not an assertion"]
fn connection_cost() {
    let (_dir, _store, _state, ipc) = hosted_job();

    // One connection, many requests: what ACP makes possible.
    let mut peer = attached(&ipc);
    for _ in 0..20 {
        peer.call("_session/ping", serde_json::json!({}));
    }
    let rounds = 200;
    let started = std::time::Instant::now();
    for _ in 0..rounds {
        peer.call("_session/ping", serde_json::json!({}));
    }
    let elapsed = started.elapsed();
    println!(
        "acp ping on one connection: {rounds} calls in {:?} = {:?} each",
        elapsed,
        elapsed / rounds
    );

    // A connection per request: what the legacy protocol has to do, measured
    // through the same server so the two numbers are comparable.
    let rounds = 20;
    let started = std::time::Instant::now();
    for _ in 0..rounds {
        let mut fresh = attached(&ipc);
        fresh.call("_session/ping", serde_json::json!({}));
    }
    let elapsed = started.elapsed();
    println!(
        "acp ping, a fresh connection each time: {rounds} calls in {:?} = {:?} each",
        elapsed,
        elapsed / rounds
    );
}

/// A job with a permission genuinely in flight: a tool is waiting on the
/// answer, which is what makes answering it mean anything.
///
/// Written through the broker rather than by writing a snapshot into the job
/// record, because the record is only half of a pending permission -- the
/// other half is the waiter the answer releases, and a test that skipped it
/// would be asserting against a question nobody asked.
fn hosted_job_with_a_permission_in_flight() -> (
    tempfile::TempDir,
    BackgroundStore,
    BackgroundJobState,
    BackgroundIpcServer,
    tokio::sync::oneshot::Receiver<rebon_core::permission::PermissionAnswer>,
    u64,
) {
    let (dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 1;
    state.identity.session_id = Some("session-under-test".into());
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    install_ipc_owner(&mut state, &ipc);
    store.write_state(&state).unwrap();

    let (broker, receiver) = ChannelPermissionBroker::new("session-under-test");
    ipc.attach_permission_receiver(1, receiver);
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    broker.forward_direct(OutboundPermissionQuery {
        id: 91,
        tool_name: "Bash".into(),
        tool_call_id: "call-1".into(),
        session_id: "session-under-test".into(),
        title: "Run a command".into(),
        message: "rm -rf /tmp/x".into(),
        tool_input: None,
        metadata: None,
        options: vec![PermissionQueryOption {
            option_id: "allow_once".into(),
            label: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        }],
        response_tx,
    });
    let raised = wait_until(2_000, || {
        store
            .read_state(&state.identity.job_id)
            .map(|state| state.outcome.pending_permission.is_some())
            .unwrap_or(false)
    });
    assert!(
        raised,
        "the permission was never forwarded to the job record"
    );
    // The broker numbers the query, not the caller: the id in the record is
    // the one the answer has to name, so it is read back rather than assumed.
    let query_id = store
        .read_state(&state.identity.job_id)
        .unwrap()
        .outcome
        .pending_permission
        .expect("raised")
        .query_id;
    (dir, store, state, ipc, response_rx, query_id)
}

/// Subscribing answers, then starts sending: the snapshot first, because a
/// client cannot interpret a delta against a state it has not seen.
#[test]
fn subscribing_answers_and_then_sends_hello() {
    let (_dir, _store, state, ipc) = hosted_job();
    let mut peer = attached(&ipc);

    let answered = peer.call("_session/subscribe", serde_json::json!({}));
    assert!(answered.get("error").is_none(), "{answered}");

    let hello = peer.read().expect("hello follows the answer");
    assert_eq!(hello["method"], serde_json::json!("_session/hello"));
    assert_eq!(
        hello["params"]["status"]["jobId"],
        serde_json::json!(state.job_id())
    );
    assert!(
        hello["params"]["epoch"].as_u64().is_some(),
        "the cursor's numbering has to travel with it: {hello}"
    );
}

/// A delta carries its cursor in `_meta.rebon.cursor`, which is what the
/// client de-duplicates on. Standard `session/update` has nowhere else to put
/// it, and a delta that lost it would be applied twice after a reconnect.
#[test]
fn a_streamed_update_carries_its_cursor_where_the_watermark_reads_it() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = attached(&ipc);
    peer.call("_session/subscribe", serde_json::json!({}));
    let hello = peer.read().expect("hello");
    let attached_at = hello["params"]["cursor"].as_u64().expect("a cursor");

    wait_for_subscriber(&ipc);
    ipc.events
        .publish_turn(rebon_session_host::TurnStreamState::Running, None);

    let turn = peer.read().expect("the turn is streamed");
    assert_eq!(turn["method"], serde_json::json!("_session/turn"));
    assert_eq!(turn["params"]["state"], serde_json::json!("running"));

    let update = serde_json::from_value(serde_json::json!({
        "sessionId": "session-under-test",
        "update": {"sessionUpdate": "agent_message_chunk",
                   "content": {"type": "text", "text": "hi"}},
    }))
    .expect("a session update");
    ipc.events.publish_update_for_turn(&update, 7);

    let streamed = peer.read().expect("the update is streamed");
    assert_eq!(streamed["method"], serde_json::json!("session/update"));
    assert_eq!(streamed["params"]["turnGeneration"], serde_json::json!(7));
    let cursor = streamed["params"]["_meta"]["rebon"]["cursor"]
        .as_u64()
        .expect("the cursor rides in _meta.rebon");
    assert!(
        cursor > attached_at,
        "a delta's cursor must be past where the subscriber attached: {cursor} <= {attached_at}"
    );
    // The standard half is untouched, because the client reads it with the
    // same code it uses against any ACP agent.
    assert_eq!(
        streamed["params"]["update"]["sessionUpdate"],
        serde_json::json!("agent_message_chunk")
    );
}

/// A cursor the owner cannot reach back to is a gap, not a silent skip. The
/// client re-reads the transcript for what is between the two numbers, so both
/// ends travel.
#[test]
fn resuming_from_a_cursor_the_owner_never_reached_is_a_gap() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut peer = attached(&ipc);

    // A cursor from a previous incarnation of this owner: it restarted and
    // numbers from one again, so this is beyond anything it has published.
    peer.call("_session/subscribe", serde_json::json!({"since": 9_999}));
    let hello = peer.read().expect("hello");
    assert_eq!(hello["method"], serde_json::json!("_session/hello"));

    let gap = peer.read().expect("a gap follows");
    assert_eq!(gap["method"], serde_json::json!("_session/gap"));
    assert_eq!(gap["params"]["from"], serde_json::json!(9_999));
    assert!(
        gap["params"]["to"].as_u64().is_some(),
        "a gap names both ends: {gap}"
    );
}

/// The question and the answer travel in opposite directions on one socket at
/// the same time.
///
/// This is what the writer thread exists for. The client is sent a permission,
/// and *before answering it* asks something of its own; both get their own
/// correct answer, and neither is read as the other. A connection that stopped
/// reading while it had a question outstanding would deadlock here, and one
/// with two write handles would tear a frame.
#[test]
fn an_inbound_request_is_answered_while_an_outbound_one_is_outstanding() {
    let (_dir, store, state, ipc, answered, query_id) = hosted_job_with_a_permission_in_flight();
    let mut peer = attached(&ipc);

    peer.call("_session/subscribe", serde_json::json!({}));
    let hello = peer.read().expect("hello");
    assert_eq!(hello["method"], serde_json::json!("_session/hello"));

    // The pending permission, restated because a fresh subscriber gets no
    // replay. It is a request, so it carries an id this side chose.
    let asked = peer.read().expect("the pending permission is asked");
    assert_eq!(
        asked["method"],
        serde_json::json!("session/request_permission")
    );
    assert_eq!(
        asked["id"],
        serde_json::json!(format!("perm-session-under-test-{query_id}"))
    );
    assert_eq!(asked["params"]["toolName"], serde_json::json!("Bash"));

    // Now interleave: ask something before answering.
    let ping_id = peer.send("_session/ping", serde_json::json!({}));
    let pong = peer
        .read()
        .expect("the ping is answered while we owe an answer");
    assert_eq!(pong["id"], serde_json::json!(ping_id), "{pong}");
    assert_eq!(pong["result"], serde_json::json!({}));

    // And only then answer the permission, by responding to its id.
    peer.send_response(
        asked["id"].clone(),
        serde_json::json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}),
    );

    let cleared = wait_until(2_000, || {
        store
            .read_state(state.job_id())
            .map(|state| state.outcome.pending_permission.is_none())
            .unwrap_or(false)
    });
    assert!(cleared, "the answer never reached the owner");
    // And the tool that was waiting was actually released, which is the whole
    // point: clearing the record without releasing the waiter would leave the
    // turn blocked on a question that no longer exists.
    assert!(
        answered.blocking_recv().is_ok(),
        "the tool waiting on this permission was never released"
    );
}

/// A response naming an id this connection never issued is ignored. Acting on
/// it would let any response shape a permission decision.
#[test]
fn a_response_to_an_id_we_never_issued_changes_nothing() {
    let (_dir, store, state, ipc, _answered, _query_id) = hosted_job_with_a_permission_in_flight();
    let mut peer = attached(&ipc);
    peer.call("_session/subscribe", serde_json::json!({}));
    peer.read().expect("hello");
    peer.read().expect("the permission");

    peer.send_response(
        serde_json::json!("perm-session-under-test-999"),
        serde_json::json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}),
    );
    // Ordered behind it on the same connection, so by the time this is
    // answered the response has been handled.
    peer.call("_session/ping", serde_json::json!({}));

    let after = store.read_state(state.job_id()).expect("readable");
    assert!(
        after.outcome.pending_permission.is_some(),
        "a response for an id we never issued answered the permission"
    );
}

/// Wait for the owner to register a subscriber, so a test that publishes
/// asserts delivery rather than racing the attach.
fn wait_for_subscriber(ipc: &BackgroundIpcServer) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while ipc.events.subscriber_count() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "subscriber never attached"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_until(deadline_ms: u64, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(deadline_ms);
    while std::time::Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    ready()
}

/// The client-side probe, against a real server rather than a fixture.
///
/// The probe itself is unit-tested in `rebon-session-host` against the exact
/// bytes a legacy-only worker answers with, but that crate sits below the server
/// and cannot start one. This is the other half: a live owner, asked the way
/// a client asks, answers as ACP.
#[test]
fn the_client_probe_recognises_a_real_owner_as_acp() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let endpoint = ipc.owner().endpoint;
    rebon_session_host::protocol_probe::forget(&endpoint);
    assert_eq!(
        rebon_session_host::protocol_probe::owner_protocol(&endpoint),
        rebon_session_host::protocol_probe::OwnerProtocol::Acp
    );
    rebon_session_host::protocol_probe::forget(&endpoint);
}

/// And a port with nothing on it reads as legacy rather than hanging or
/// claiming the new protocol.
#[test]
fn the_client_probe_reads_a_dead_endpoint_as_legacy() {
    let (_dir, _store, _state, ipc) = hosted_job();
    let mut endpoint = ipc.owner().endpoint;
    ipc.stop();
    // The port is closed once the accept thread returns; either way nothing
    // there answers JSON-RPC.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while ipc.accept_thread_running() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    endpoint.token = format!("{}-after-stop", endpoint.token);
    assert_eq!(
        rebon_session_host::protocol_probe::owner_protocol(&endpoint),
        rebon_session_host::protocol_probe::OwnerProtocol::Legacy
    );
    rebon_session_host::protocol_probe::forget(&endpoint);
}

/// The production client talks to a real owner over ACP, and reuses one
/// connection to do it.
///
/// The rest of this file drives the wire by hand. This drives it through
/// `OwnerHandle`, which is what every surface actually calls.
///
/// The observable is the link's own call count, and it has to be: an answer
/// looks the same over either protocol -- that is the whole point of the
/// translation -- so "the status came back" proves nothing about which wire
/// carried it. A first version of this test asserted exactly that and passed
/// with the ACP path removed.
#[test]
fn the_production_client_reuses_one_connection_over_acp() {
    let (_dir, _store, state, ipc) = hosted_job();
    let endpoint = ipc.owner().endpoint;
    rebon_session_host::protocol_probe::forget(&endpoint);
    rebon_session_host::acp_link::close_link(&endpoint);

    // Open the link first, so what the client does can be watched on it.
    let link = rebon_session_host::acp_link::link_to(&endpoint).expect("a link");
    assert_eq!(link.calls_made(), 1, "the handshake is one call");

    let owner = rebon_session_host::OwnerHandle::for_worker(
        state.session_id().unwrap_or_default(),
        Some(state.job_id()),
        &endpoint,
    );
    let snapshot = owner.status().expect("a status over ACP");
    assert_eq!(snapshot.job_id, state.job_id());
    assert_eq!(
        link.calls_made(),
        2,
        "the client did not send its request on this link"
    );

    owner.status().expect("a second status");
    assert_eq!(
        link.calls_made(),
        3,
        "the second call did not reuse the link"
    );
    let again = rebon_session_host::acp_link::link_to(&endpoint).expect("the same link");
    assert!(
        std::sync::Arc::ptr_eq(&link, &again),
        "a second connection was opened for the same endpoint"
    );

    rebon_session_host::acp_link::close_link(&endpoint);
    rebon_session_host::protocol_probe::forget(&endpoint);
}

/// A session with nothing to say does not end its subscription.
///
/// The handshake reads with a deadline; the stream after it must not. When the
/// deadline was cleared on a duplicated handle instead of the reader's own, the
/// reader kept it — and the first half-second in which the owner had nothing to
/// say read as the owner hanging up. `serve` answered that the way it answers a
/// real disconnect: released its lease, slept, resubscribed. About once a
/// second, for as long as a tab was open (26 seconds, 31 rounds, on a real
/// worker).
///
/// The read has to be **in flight** while nothing is published, which is why
/// this reads from another thread rather than sleeping and then reading.
#[test]
fn a_quiet_owner_does_not_end_the_subscription() {
    let (_dir, store, mut state, ipc) = hosted_job();
    state.process.status = BackgroundJobStatus::Running;
    store.write_state(&state).unwrap();
    let endpoint = ipc.owner().endpoint;
    rebon_session_host::protocol_probe::forget(&endpoint);
    rebon_session_host::acp_link::close_link(&endpoint);

    let owner = rebon_session_host::OwnerHandle::for_worker(
        "session-under-test",
        Some(state.job_id()),
        &endpoint,
    );
    let mut stream = owner.subscribe(None).expect("a subscription");
    assert!(
        stream.speaks_acp(),
        "the subscription fell back to the legacy stream, which never had this bug"
    );
    stream.next().expect("hello arrives first");
    wait_for_subscriber(&ipc);

    // Blocked in the read while the owner says nothing.
    let reading = std::thread::spawn(move || stream.next());
    // Longer than the handshake's own deadline, which is what used to end it.
    std::thread::sleep(std::time::Duration::from_millis(1_200));
    ipc.events
        .publish_update(&rebon_types::SessionUpdateParams {
            session_id: "session-under-test".to_string(),
            update: rebon_types::SessionUpdate::AgentMessageChunk {
                content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: "spoken after a long silence".to_string(),
                    annotations: None,
                }),
            },
        });

    let event = reading.join().expect("the reader thread finished");
    assert!(
        event.is_some(),
        "a quiet second ended the subscription; the owner never hung up"
    );
}

/// The whole chain, through the production client, against a real owner.
///
/// Subscribe, read `hello`, send a prompt, see the delta with its cursor, meet
/// a permission, answer it, and watch the tool that was waiting get released.
/// Each half of that is pinned elsewhere; what this adds is that they compose
/// -- a client that subscribes and then answers is using two connections to
/// one owner at once, and the answer has to reach the question.
#[test]
fn the_client_drives_a_whole_turn_over_acp() {
    let (_dir, store, mut state, ipc) = hosted_job();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 1;
    store.write_state(&state).unwrap();
    let endpoint = ipc.owner().endpoint;
    rebon_session_host::protocol_probe::forget(&endpoint);
    rebon_session_host::acp_link::close_link(&endpoint);

    let owner = rebon_session_host::OwnerHandle::for_worker(
        "session-under-test",
        Some(state.job_id()),
        &endpoint,
    );

    // Subscribe, and read the snapshot every delta is interpreted against.
    let mut stream = owner.subscribe(None).expect("a subscription");
    assert!(
        stream.speaks_acp(),
        "the subscription fell back to the legacy stream, so nothing below \
         this line is testing what it says it is"
    );
    let hello = stream.next().expect("hello arrives first");
    let epoch = match hello {
        rebon_session_host::SessionEvent::Hello { epoch, status, .. } => {
            assert_eq!(status.job_id, state.job_id());
            epoch
        }
        other => panic!("expected hello, got {other:?}"),
    };
    assert_ne!(epoch, 0, "a cursor without its numbering is not comparable");
    wait_for_subscriber(&ipc);

    // A prompt, over the standard method, landing where the turn loop reads it.
    owner
        .send(
            rebon_session_host::BackgroundIpcRequest::Reply {
                message: "carry on then".into(),
                images: Vec::new(),
            },
            None,
        )
        .expect("the prompt is accepted");
    let queued = wait_until(2_000, || {
        store
            .read_state(state.job_id())
            .map(|state| pending_text(&state) == Some("carry on then"))
            .unwrap_or(false)
    });
    assert!(queued, "the prompt never reached the job record");

    // A delta, carrying the cursor the watermark reads.
    let update = serde_json::from_value(serde_json::json!({
        "sessionId": "session-under-test",
        "update": {"sessionUpdate": "agent_message_chunk",
                   "content": {"type": "text", "text": "hi"}},
    }))
    .expect("a session update");
    ipc.events.publish_update(&update);
    let streamed = next_matching(&mut stream, |event| {
        matches!(
            event,
            rebon_session_host::SessionEvent::SessionUpdate { .. }
        )
    })
    .expect("the delta is streamed");
    match streamed {
        rebon_session_host::SessionEvent::SessionUpdate { cursor, update } => {
            assert!(cursor > 0, "a delta arrived without its cursor");
            assert_eq!(
                update["update"]["sessionUpdate"],
                serde_json::json!("agent_message_chunk")
            );
        }
        other => panic!("expected a delta, got {other:?}"),
    }

    // A permission, raised for real so answering it releases something.
    let (broker, receiver) = ChannelPermissionBroker::new("session-under-test");
    ipc.attach_permission_receiver(1, receiver);
    let (response_tx, answered) = tokio::sync::oneshot::channel();
    broker.forward_direct(OutboundPermissionQuery {
        id: 1,
        tool_name: "Bash".into(),
        tool_call_id: "call-1".into(),
        session_id: "session-under-test".into(),
        title: "Run a command".into(),
        message: "rm -rf /tmp/x".into(),
        tool_input: None,
        metadata: None,
        options: vec![PermissionQueryOption {
            option_id: "allow_once".into(),
            label: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        }],
        response_tx,
    });

    let asked = next_matching(&mut stream, |event| {
        matches!(event, rebon_session_host::SessionEvent::Permission { .. })
    })
    .expect("the permission reaches the subscriber");
    let query_id = match asked {
        rebon_session_host::SessionEvent::Permission { query, .. } => {
            assert_eq!(query.tool.as_deref(), Some("Bash"));
            assert_eq!(query.options.len(), 1);
            query.query_id
        }
        other => panic!("expected a permission, got {other:?}"),
    };

    // Answered on the subscription, because that is where it was asked. The
    // owner releases the tool either way, so the witness that it went out
    // *here* is this side's own note of the question.
    let answerer =
        rebon_session_host::acp_subscription::answerer_for(&endpoint).expect("a subscription");
    assert!(
        answerer.owes_an_answer(query_id),
        "the question was not recorded against the subscription that asked it"
    );
    owner
        .send(
            rebon_session_host::BackgroundIpcRequest::PermissionAnswer {
                query_id,
                turn_generation: 1,
                option_id: Some("allow_once".into()),
                extra_text: None,
                updated_input: None,
            },
            None,
        )
        .expect("the answer is accepted");
    assert!(
        !answerer.owes_an_answer(query_id),
        "the answer did not go out on the subscription that was asked"
    );

    assert!(
        answered.blocking_recv().is_ok(),
        "the tool waiting on this permission was never released"
    );

    drop(stream);
    rebon_session_host::acp_link::close_link(&endpoint);
    rebon_session_host::protocol_probe::forget(&endpoint);
}

/// The next event this test cares about, skipping the ones it does not.
///
/// A live owner also publishes status changes as the record moves, and a test
/// that asserted on "the next event" would be asserting on the owner's
/// bookkeeping rather than on what it asked for.
fn next_matching(
    stream: &mut rebon_session_host::SessionEventStream,
    mut wanted: impl FnMut(&rebon_session_host::SessionEvent) -> bool,
) -> Option<rebon_session_host::SessionEvent> {
    for _ in 0..64 {
        let event = stream.next()?;
        if wanted(&event) {
            return Some(event);
        }
    }
    None
}

/// The same whole chain, over the legacy envelope, against the same real
/// owner.
///
/// The other row of the matrix, and the one that says the compatibility period
/// is real: a client that has decided this owner predates ACP drives a whole
/// turn through it anyway, because the owner serves both protocols for the
/// whole release.
///
/// An owner that actually predates ACP cannot be started in this process, so
/// the verdict is written down rather than measured -- which is exactly what
/// the probe would have written after meeting one. What is under test is the
/// client's legacy path and the owner's legacy path, both of which are the
/// real ones.
#[test]
fn the_client_drives_a_whole_turn_over_the_legacy_envelope() {
    let (_dir, store, mut state, ipc) = hosted_job();
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 1;
    store.write_state(&state).unwrap();
    let endpoint = ipc.owner().endpoint;
    rebon_session_host::acp_link::close_link(&endpoint);
    rebon_session_host::protocol_probe::remember(
        &endpoint,
        rebon_session_host::protocol_probe::OwnerProtocol::Legacy,
    );

    let owner = rebon_session_host::OwnerHandle::for_worker(
        "session-under-test",
        Some(state.job_id()),
        &endpoint,
    );

    let mut stream = owner.subscribe(None).expect("a subscription");
    assert!(
        !stream.speaks_acp(),
        "this row is the legacy one; an ACP subscription here tests nothing"
    );
    match stream.next().expect("hello arrives first") {
        rebon_session_host::SessionEvent::Hello { status, .. } => {
            assert_eq!(status.job_id, state.job_id());
        }
        other => panic!("expected hello, got {other:?}"),
    }
    wait_for_subscriber(&ipc);

    owner
        .send(
            rebon_session_host::BackgroundIpcRequest::Reply {
                message: "carry on then".into(),
                images: Vec::new(),
            },
            None,
        )
        .expect("the prompt is accepted");
    assert!(
        wait_until(2_000, || {
            store
                .read_state(state.job_id())
                .map(|state| pending_text(&state) == Some("carry on then"))
                .unwrap_or(false)
        }),
        "the prompt never reached the job record"
    );

    let update = serde_json::from_value(serde_json::json!({
        "sessionId": "session-under-test",
        "update": {"sessionUpdate": "agent_message_chunk",
                   "content": {"type": "text", "text": "hi"}},
    }))
    .expect("a session update");
    ipc.events.publish_update(&update);
    match next_matching(&mut stream, |event| {
        matches!(
            event,
            rebon_session_host::SessionEvent::SessionUpdate { .. }
        )
    })
    .expect("the delta is streamed")
    {
        rebon_session_host::SessionEvent::SessionUpdate { cursor, .. } => {
            assert!(cursor > 0, "a delta arrived without its cursor");
        }
        other => panic!("expected a delta, got {other:?}"),
    }

    let (broker, receiver) = ChannelPermissionBroker::new("session-under-test");
    ipc.attach_permission_receiver(1, receiver);
    let (response_tx, answered) = tokio::sync::oneshot::channel();
    broker.forward_direct(OutboundPermissionQuery {
        id: 1,
        tool_name: "Bash".into(),
        tool_call_id: "call-1".into(),
        session_id: "session-under-test".into(),
        title: "Run a command".into(),
        message: "rm -rf /tmp/x".into(),
        tool_input: None,
        metadata: None,
        options: vec![PermissionQueryOption {
            option_id: "allow_once".into(),
            label: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        }],
        response_tx,
    });

    let query_id = match next_matching(&mut stream, |event| {
        matches!(event, rebon_session_host::SessionEvent::Permission { .. })
    })
    .expect("the permission reaches the subscriber")
    {
        rebon_session_host::SessionEvent::Permission { query, .. } => query.query_id,
        other => panic!("expected a permission, got {other:?}"),
    };

    // No ACP subscription exists, so there is no question recorded here and
    // the answer takes the envelope -- which is the whole of this row.
    assert!(
        rebon_session_host::acp_subscription::answerer_for(&endpoint).is_none(),
        "a legacy subscription registered itself as one that can answer over ACP"
    );
    owner
        .send(
            rebon_session_host::BackgroundIpcRequest::PermissionAnswer {
                query_id,
                turn_generation: 1,
                option_id: Some("allow_once".into()),
                extra_text: None,
                updated_input: None,
            },
            None,
        )
        .expect("the answer is accepted");
    assert!(
        answered.blocking_recv().is_ok(),
        "the tool waiting on this permission was never released"
    );

    drop(stream);
    rebon_session_host::protocol_probe::forget(&endpoint);
}
