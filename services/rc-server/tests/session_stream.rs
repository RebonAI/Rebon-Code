//! The session stream end to end: the real server and the real
//! `rebon-bridge` WebSocket client, over a real socket.
//!
//! Every test that expects a frame waits with a deadline, so a routing
//! bug fails as a timeout rather than a hung test run. Nothing here
//! starts the 1 Hz sweeper; the tests that depend on it call
//! `RcState::cleanup` themselves, which keeps them deterministic.

mod support;

use std::path::Path;
use std::time::Duration;

use rebon_bridge::api_client::BridgeApiError;
use rebon_bridge::config::PermissionResponseBody;
use rebon_bridge::control_request::{
    plan_server_control_response, ControlEffect, ControlVerdict, ServerControlRequestPlanInput,
    ServerControlRequestSubtype,
};
use rebon_bridge::session_stream::{ControlResponseBody, QuestionAnswer, SessionFrame};
// `RawMessage` is the client's raw message type, for the tests that must
// put something on the wire the envelope would never produce.
use rebon_bridge::stream_client::{
    CloseReason, RawMessage as Message, SessionFrameSink, SessionStream, SessionStreamError,
    SessionStreamOptions, SessionStreamRx, SessionStreamTx,
};
use rebon_bridge::work_secret::WorkSecret;
use rebon_rc_server::{auth, ids};
use serde_json::{json, Value};
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Upper bound on any single wait. Generous, because a CI box can be
/// slow; a correct run never gets near it.
const WAIT: Duration = Duration::from_secs(10);

// ─── Fixtures ─────────────────────────────────────────────────────────

/// One environment with one leased session.
struct Leased {
    device: Device,
    environment_id: String,
    secret: String,
    work_id: String,
    session_id: String,
    session_token: String,
    ingress_url: String,
}

/// Bootstrap, register, queue a session and lease it.
async fn leased(server: &Server) -> Leased {
    let (device, environment_id, secret) = bootstrapped_environment(server).await;
    lease_on(server, device, environment_id, secret).await
}

/// Queue and lease one more session on an environment that already has
/// its device and secret.
async fn lease_on(
    server: &Server,
    device: Device,
    environment_id: String,
    secret: String,
) -> Leased {
    let (work_id, session_id) =
        enqueue(server, &device.access_token, &environment_id, "stream test").await;
    let work = poll(server, &environment_id, &secret, 2_000)
        .await
        .expect("the queued work is handed out");
    assert_eq!(work["id"], work_id.as_str());
    let decoded = WorkSecret::decode(work["secret"].as_str().expect("secret")).expect("decodes");
    assert_eq!(decoded.session_id.as_deref(), Some(session_id.as_str()));
    Leased {
        device,
        environment_id,
        secret,
        work_id,
        session_id,
        session_token: decoded.session_token,
        ingress_url: decoded.ingress_url,
    }
}

async fn attach(url: &str, token: &str) -> (SessionStreamTx, SessionStreamRx) {
    SessionStream::connect(&SessionStreamOptions::new(url, token))
        .await
        .expect("the stream accepts this credential")
        .split()
}

async fn refusal(url: &str, token: &str) -> SessionStreamError {
    SessionStream::connect(&SessionStreamOptions::new(url, token))
        .await
        .expect_err("the stream refuses this credential")
}

fn status_of(error: &SessionStreamError) -> u16 {
    match error {
        SessionStreamError::Rejected { status, .. } => *status,
        other => panic!("expected a refused upgrade, got {other:?}"),
    }
}

/// The next frame, or fail.
async fn next(rx: &mut SessionStreamRx) -> SessionFrame {
    timeout(WAIT, rx.recv())
        .await
        .expect("a frame arrives in time")
        .expect("the stream is still open")
        .expect("the frame parses")
}

/// Read until the stream ends and report why. Frames still in flight are
/// discarded; the count of them is returned for the tests that care.
async fn closed(rx: &mut SessionStreamRx) -> (CloseReason, usize) {
    let drained = timeout(WAIT, async {
        let mut drained = 0usize;
        while rx.recv().await.is_some() {
            drained += 1;
        }
        drained
    })
    .await
    .expect("the stream closes in time");
    (rx.close_reason().cloned().expect("a close reason"), drained)
}

/// Nothing arrives for a short while.
async fn quiet(rx: &mut SessionStreamRx) {
    if let Ok(Some(frame)) = timeout(Duration::from_millis(300), rx.recv()).await {
        panic!("unexpected frame {frame:?}");
    }
}

fn message(seq: usize) -> SessionFrame {
    SessionFrame::SessionMessage {
        message_id: None,
        message: json!({ "seq": seq }),
    }
}

fn seq_of(frame: &SessionFrame) -> u64 {
    match frame {
        SessionFrame::SessionMessage { message, .. } => message["seq"].as_u64().expect("seq"),
        other => panic!("expected a session message, got {other:?}"),
    }
}

/// A raw upgrade request, for the cases the client refuses to build —
/// no `Authorization` header at all.
async fn raw_upgrade_status(server: &Server, path: &str, authorization: Option<&str>) -> u16 {
    let address = server.base.trim_start_matches("http://");
    let mut tcp = TcpStream::connect(address).await.expect("connect");
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
    );
    if let Some(value) = authorization {
        request.push_str(&format!("Authorization: {value}\r\n"));
    }
    request.push_str("\r\n");
    tcp.write_all(request.as_bytes()).await.expect("write");
    let mut buffer = vec![0u8; 1024];
    let read = timeout(WAIT, tcp.read(&mut buffer))
        .await
        .expect("a response in time")
        .expect("read");
    let head = String::from_utf8_lossy(&buffer[..read]);
    head.split(' ')
        .nth(1)
        .and_then(|status| status.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {head:?}"))
}

async fn stop_work(server: &Server, leased: &Leased) {
    let response = client()
        .post(format!(
            "{}/v1/environments/{}/work/{}/stop",
            server.base, leased.environment_id, leased.work_id
        ))
        .bearer_auth(&leased.device.access_token)
        .json(&json!({"force": false}))
        .send()
        .await
        .expect("stop");
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
}

fn events_of(server: &Server, session_id: &str) -> Vec<Value> {
    server
        .state
        .store()
        .session_events(session_id)
        .expect("session events")
        .into_iter()
        .map(|event| serde_json::from_str(&event.payload_json).expect("stored json"))
        .collect()
}

// ─── Routing ──────────────────────────────────────────────────────────

#[tokio::test]
async fn a_worker_message_reaches_every_attached_controller() {
    let server = server().await;
    let leased = leased(&server).await;
    let (worker, _worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    let (_a, mut a) = attach(&leased.ingress_url, &leased.device.access_token).await;
    let (_b, mut b) = attach(&leased.ingress_url, &leased.device.access_token).await;

    let frame = SessionFrame::SessionMessage {
        message_id: None,
        message: json!({"role": "assistant", "text": "leases expire after 90 s"}),
    };
    worker.send(&frame).await.expect("send");
    assert_eq!(next(&mut a).await, frame);
    assert_eq!(next(&mut b).await, frame);

    let state = SessionFrame::SessionState {
        state: "running".into(),
        detail: Some("turn 1".into()),
    };
    worker.send(&state).await.expect("send");
    assert_eq!(next(&mut a).await, state);
    assert_eq!(next(&mut b).await, state);

    // Persisted verbatim, in order.
    let stored = events_of(&server, &leased.session_id);
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0]["type"], "session_message");
    assert_eq!(stored[1]["type"], "session_state");
}

#[tokio::test]
async fn a_controller_prompt_reaches_the_worker_and_is_mirrored_to_the_other_controllers() {
    let server = server().await;
    let leased = leased(&server).await;
    let (_worker, mut worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    let (sender, mut sender_rx) = attach(&leased.ingress_url, &leased.device.access_token).await;
    let (_other, mut other_rx) = attach(&leased.ingress_url, &leased.device.access_token).await;

    let prompt = SessionFrame::Prompt {
        text: "explain the sweeper".into(),
        attachments: vec![json!({"path": "services/rc-server/src/lib.rs"})],
    };
    // Through the object-safe sink, the way a runner holds it.
    let sink: std::sync::Arc<dyn SessionFrameSink> = std::sync::Arc::new(sender);
    sink.send_frame(&prompt).await.expect("send");

    assert_eq!(next(&mut worker_rx).await, prompt);
    assert_eq!(next(&mut other_rx).await, prompt);
    // Never echoed to the controller that sent it.
    quiet(&mut sender_rx).await;

    sink.send_frame(&SessionFrame::Cancel)
        .await
        .expect("cancel");
    assert_eq!(next(&mut worker_rx).await, SessionFrame::Cancel);
    assert_eq!(next(&mut other_rx).await, SessionFrame::Cancel);
}

#[tokio::test]
async fn a_worker_streams_with_nobody_listening_and_a_late_controller_gets_it_all() {
    // The product depends on this: a phone that attaches later finds the
    // content already on the server, without waking the machine.
    let server = server().await;
    let leased = leased(&server).await;
    let worker = SessionStream::connect(&SessionStreamOptions::new(
        &leased.ingress_url,
        &leased.session_token,
    ))
    .await
    .expect("attach");
    for seq in 0..5 {
        worker.send(&message(seq)).await.expect("send");
    }
    worker
        .send(&SessionFrame::SessionState {
            state: "idle".into(),
            detail: None,
        })
        .await
        .expect("send");
    // `close` waits for the server's answer, which it only sends after
    // every earlier frame has been handled.
    worker.close().await.expect("close");
    assert_eq!(events_of(&server, &leased.session_id).len(), 6);

    // The session can even be archived before anyone looks.
    let response = client()
        .post(format!(
            "{}/v1/sessions/{}/archive",
            server.base, leased.session_id
        ))
        .bearer_auth(&leased.device.access_token)
        .send()
        .await
        .expect("archive");
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);

    let (_controller, mut rx) = attach(&leased.ingress_url, &leased.device.access_token).await;
    for seq in 0..5 {
        assert_eq!(seq_of(&next(&mut rx).await), seq as u64);
    }
    assert_eq!(
        next(&mut rx).await,
        SessionFrame::SessionState {
            state: "idle".into(),
            detail: None
        }
    );
    quiet(&mut rx).await;
}

#[tokio::test]
async fn a_permission_request_goes_out_and_the_decision_comes_back() {
    let server = server().await;
    let leased = leased(&server).await;
    let (worker, mut worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    let (deciding, mut deciding_rx) =
        attach(&leased.ingress_url, &leased.device.access_token).await;
    let (_watching, mut watching_rx) =
        attach(&leased.ingress_url, &leased.device.access_token).await;

    let request = SessionFrame::PermissionRequest {
        request_id: "perm-1".into(),
        request: json!({"tool": "Bash", "input": {"command": "cargo test"}}),
    };
    worker.send(&request).await.expect("send");
    assert_eq!(next(&mut deciding_rx).await, request);
    assert_eq!(next(&mut watching_rx).await, request);

    let decision = SessionFrame::PermissionResponse {
        response: PermissionResponseBody::success("perm-1", json!({"behavior": "allow"})),
    };
    deciding.send(&decision).await.expect("send");
    assert_eq!(next(&mut worker_rx).await, decision);
    // The other surface learns the prompt was answered.
    assert_eq!(next(&mut watching_rx).await, decision);
}

#[tokio::test]
async fn a_question_goes_out_and_the_answers_come_back() {
    let server = server().await;
    let leased = leased(&server).await;
    let (worker, mut worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    let (answering, mut answering_rx) =
        attach(&leased.ingress_url, &leased.device.access_token).await;
    let (_watching, mut watching_rx) =
        attach(&leased.ingress_url, &leased.device.access_token).await;

    let request = SessionFrame::PermissionRequest {
        request_id: "perm-q".into(),
        request: json!({
            "questions": [{"question": "Which?", "multiSelect": false, "options": []}],
            "_meta": {"rebonRc": {"kind": "question", "answerable": true, "answerWith": "question_response"}}
        }),
    };
    worker.send(&request).await.expect("send");
    assert_eq!(next(&mut answering_rx).await, request);
    assert_eq!(next(&mut watching_rx).await, request);

    let answers = SessionFrame::QuestionResponse {
        request_id: "perm-q".into(),
        answers: vec![QuestionAnswer::options([1]), QuestionAnswer::text("later")],
    };
    answering.send(&answers).await.expect("send");
    assert_eq!(next(&mut worker_rx).await, answers);
    // The other surface learns the question was answered.
    assert_eq!(next(&mut watching_rx).await, answers);
    quiet(&mut answering_rx).await;

    // Stored as sent, every copy: a controller may answer again after a
    // refusal.
    answering.send(&answers).await.expect("send");
    assert_eq!(next(&mut worker_rx).await, answers);
    let stored = events_of(&server, &leased.session_id);
    let kinds: Vec<&str> = stored
        .iter()
        .map(|event| event["type"].as_str().expect("type"))
        .collect();
    assert_eq!(
        kinds,
        vec![
            "permission_request",
            "question_response",
            "question_response"
        ]
    );
    assert_eq!(stored[1]["answers"][1]["other_text"], "later");
}

#[tokio::test]
async fn a_question_response_with_nobody_to_run_it_is_refused() {
    let server = server().await;
    let leased = leased(&server).await;
    let (controller, mut rx) = attach(&leased.ingress_url, &leased.device.access_token).await;
    controller
        .send(&SessionFrame::QuestionResponse {
            request_id: "perm-q".into(),
            answers: vec![QuestionAnswer::options([0])],
        })
        .await
        .expect("send");
    match next(&mut rx).await {
        SessionFrame::StreamError { code, .. } => assert_eq!(code, "no_worker"),
        other => panic!("expected a stream error, got {other:?}"),
    }
    assert!(events_of(&server, &leased.session_id).is_empty());
}

#[tokio::test]
async fn a_control_request_reaches_the_worker_and_its_response_fans_out() {
    let server = server().await;
    let leased = leased(&server).await;
    let (worker, mut worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    let (controller, mut controller_rx) =
        attach(&leased.ingress_url, &leased.device.access_token).await;
    let (_other, mut other_rx) = attach(&leased.ingress_url, &leased.device.access_token).await;

    for (request_id, subtype, params) in [
        ("ctl-init", "initialize", Value::Null),
        ("ctl-model", "set_model", json!({"model": "opus"})),
        ("ctl-mode", "set_permission_mode", json!({"mode": "plan"})),
        ("ctl-think", "set_max_thinking_tokens", json!({"max": 1024})),
        ("ctl-stop", "interrupt", Value::Null),
    ] {
        controller
            .send(&SessionFrame::ControlRequest {
                request_id: request_id.into(),
                subtype: subtype.into(),
                params: params.clone(),
            })
            .await
            .expect("send");

        // The worker receives it intact and plans a reply with the
        // existing pure planner.
        let received = next(&mut worker_rx).await;
        let parsed = received.control_subtype().expect("a control request");
        assert_eq!(parsed, ServerControlRequestSubtype::parse(subtype));
        let SessionFrame::ControlRequest {
            request_id: received_id,
            params: received_params,
            ..
        } = &received
        else {
            panic!("expected a control request, got {received:?}");
        };
        assert_eq!(received_id, request_id);
        assert_eq!(received_params, &params);
        // The mirror.
        assert_eq!(next(&mut other_rx).await, received);

        // What a Rebon runner would decide: the model applies from the
        // next turn, an interrupt at once, and there is no handler for a
        // permission mode or a thinking budget here.
        let verdict = match subtype {
            "set_model" => Some(ControlVerdict::Applied(ControlEffect::NextTurn)),
            "interrupt" => Some(ControlVerdict::Applied(ControlEffect::Now)),
            _ => None,
        };
        let plan = plan_server_control_response(ServerControlRequestPlanInput {
            request_id: received_id,
            subtype: parsed,
            outbound_only: false,
            verdict,
            pid: 4242,
        });
        let response = SessionFrame::ControlResponse {
            response: ControlResponseBody::from(plan),
        };
        worker.send(&response).await.expect("respond");
        assert_eq!(next(&mut controller_rx).await, response);
        assert_eq!(next(&mut other_rx).await, response);

        let SessionFrame::ControlResponse { response } = response else {
            unreachable!()
        };
        assert_eq!(response.request_id, request_id);
        match subtype {
            "initialize" => {
                assert_eq!(response.subtype, "success");
                assert_eq!(response.response.expect("body")["pid"], 4242);
            }
            "set_model" => assert_eq!(response.applied(), Some(ControlEffect::NextTurn)),
            "interrupt" => assert_eq!(response.applied(), Some(ControlEffect::Now)),
            // No verdict, so the planner answers an error rather than
            // claiming a change it did not make.
            _ => {
                assert_eq!(response.subtype, "error");
                assert_eq!(
                    response.error.as_deref(),
                    Some(format!("{subtype} is not supported by this bridge").as_str())
                );
            }
        }
    }
}

#[tokio::test]
async fn a_permission_event_posted_over_http_reaches_attached_controllers() {
    let server = server().await;
    let leased = leased(&server).await;
    let (_controller, mut rx) = attach(&leased.ingress_url, &leased.device.access_token).await;

    let response = client()
        .post(format!(
            "{}/v1/sessions/{}/events",
            server.base, leased.session_id
        ))
        .bearer_auth(&leased.session_token)
        .json(&json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": "perm-http", "response": {"behavior": "deny"}}
        }))
        .send()
        .await
        .expect("post event");
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);

    let SessionFrame::ControlResponse { response } = next(&mut rx).await else {
        panic!("expected a control response");
    };
    assert_eq!(response.request_id, "perm-http");
    assert_eq!(response.response, Some(json!({"behavior": "deny"})));
}

#[tokio::test]
async fn an_unknown_frame_type_from_the_worker_is_relayed_verbatim() {
    let server = server().await;
    let leased = leased(&server).await;
    let (worker, _worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    let (_controller, mut rx) = attach(&leased.ingress_url, &leased.device.access_token).await;

    let raw = r#"{"type":"telemetry","cpu":0.25,"nested":{"a":[1,2]}}"#;
    worker
        .send_raw(Message::Text(raw.into()))
        .await
        .expect("send");
    let frame = next(&mut rx).await;
    assert_eq!(frame.frame_type(), "telemetry");
    assert_eq!(
        serde_json::to_value(&frame).expect("serializes"),
        serde_json::from_str::<Value>(raw).expect("raw")
    );
    assert_eq!(
        events_of(&server, &leased.session_id)[0]["type"],
        "telemetry"
    );
}

// ─── Replay ───────────────────────────────────────────────────────────

#[tokio::test]
async fn a_late_controller_is_replayed_the_newest_events_in_order() {
    let server = server_with(Options {
        session_replay_events: 3,
        ..Options::default()
    })
    .await;
    let leased = leased(&server).await;
    let (worker, _worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    let (_early, mut early) = attach(&leased.ingress_url, &leased.device.access_token).await;
    for seq in 0..5 {
        worker.send(&message(seq)).await.expect("send");
    }
    for seq in 0..5 {
        assert_eq!(seq_of(&next(&mut early).await), seq);
    }

    // The newest three, oldest first — then live frames after them.
    let (_late, mut late) = attach(&leased.ingress_url, &leased.device.access_token).await;
    for seq in 2..5 {
        assert_eq!(seq_of(&next(&mut late).await), seq);
    }
    worker.send(&message(5)).await.expect("send");
    assert_eq!(seq_of(&next(&mut late).await), 5);
    quiet(&mut late).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_controller_attaching_mid_stream_misses_nothing_and_sees_nothing_twice() {
    // The worker keeps writing while the controller attaches, so frames
    // are persisted before, during and after the backlog read. Whatever
    // the interleaving, the controller must see every frame exactly once
    // and in order. The step-by-step version of the same interleaving is
    // the unit test next to `already_replayed`.
    const TOTAL: usize = 600;
    let server = server_with(Options {
        session_replay_events: 2_000,
        ..Options::default()
    })
    .await;
    let leased = leased(&server).await;
    let (worker, _worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;

    let (halfway_tx, halfway_rx) = tokio::sync::oneshot::channel();
    let writer = tokio::spawn(async move {
        let mut halfway = Some(halfway_tx);
        for seq in 0..TOTAL {
            worker.send(&message(seq)).await.expect("send");
            if seq == TOTAL / 4 {
                halfway.take().expect("once").send(()).expect("signal");
            }
        }
        worker
    });
    halfway_rx.await.expect("the writer got going");
    let (_controller, mut rx) = attach(&leased.ingress_url, &leased.device.access_token).await;
    let _worker = writer.await.expect("writer");

    let mut seen = Vec::with_capacity(TOTAL);
    while seen.len() < TOTAL {
        seen.push(seq_of(&next(&mut rx).await));
    }
    assert_eq!(seen, (0..TOTAL as u64).collect::<Vec<_>>());
    quiet(&mut rx).await;
}

// ─── Topology ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_second_worker_supersedes_the_first() {
    let server = server().await;
    let leased = leased(&server).await;
    let (_first, mut first_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    let (_second, mut second_rx) = attach(&leased.ingress_url, &leased.session_token).await;

    let (reason, _) = closed(&mut first_rx).await;
    assert_eq!(reason, CloseReason::Superseded);
    assert!(!reason.is_reconnectable());

    // Controller frames now reach only the new worker.
    let (controller, _controller_rx) =
        attach(&leased.ingress_url, &leased.device.access_token).await;
    controller
        .send(&SessionFrame::prompt("who is there"))
        .await
        .expect("send");
    assert_eq!(
        next(&mut second_rx).await,
        SessionFrame::prompt("who is there")
    );
}

#[tokio::test]
async fn a_controller_frame_with_no_worker_is_refused_and_not_recorded() {
    let server = server().await;
    let leased = leased(&server).await;
    let (controller, mut rx) = attach(&leased.ingress_url, &leased.device.access_token).await;

    for _ in 0..2 {
        controller
            .send(&SessionFrame::prompt("anyone?"))
            .await
            .expect("send");
        match next(&mut rx).await {
            SessionFrame::StreamError { code, .. } => assert_eq!(code, "no_worker"),
            other => panic!("expected a stream error, got {other:?}"),
        }
    }
    // The socket survived both refusals, and nothing entered the history.
    assert!(events_of(&server, &leased.session_id).is_empty());

    // Once a worker arrives, the same controller is routed normally.
    let (_worker, mut worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    controller
        .send(&SessionFrame::prompt("now?"))
        .await
        .expect("send");
    assert_eq!(next(&mut worker_rx).await, SessionFrame::prompt("now?"));
    assert_eq!(events_of(&server, &leased.session_id).len(), 1);
}

#[tokio::test]
async fn a_worker_whose_lease_is_stopped_is_hung_up_on() {
    let server = server().await;
    let leased = leased(&server).await;
    let (_worker, mut worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    let (_controller, mut controller_rx) =
        attach(&leased.ingress_url, &leased.device.access_token).await;

    // Nothing happens until the sweeper runs.
    server.state.cleanup();
    quiet(&mut worker_rx).await;

    stop_work(&server, &leased).await;
    server.state.cleanup();
    let (reason, _) = closed(&mut worker_rx).await;
    assert_eq!(reason, CloseReason::LeaseGone);
    assert!(SessionStreamError::Closed(reason).is_lease_gone());

    // The controllers are not the ones who lost anything.
    quiet(&mut controller_rx).await;
    assert!(controller_rx.close_reason().is_none());
}

// ─── Auth ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_missing_or_malformed_credential_gets_no_socket() {
    let server = server().await;
    let leased = leased(&server).await;
    let path = format!("/v1/sessions/{}/stream", leased.session_id);

    assert_eq!(raw_upgrade_status(&server, &path, None).await, 401);
    assert_eq!(
        raw_upgrade_status(&server, &path, Some("Bearer not-a-token")).await,
        401
    );
    assert_eq!(
        raw_upgrade_status(
            &server,
            &path,
            Some(&format!("Basic {}", leased.session_token))
        )
        .await,
        401
    );
    // A well-formed token nobody issued.
    let error = refusal(&leased.ingress_url, &ids::generate_token()).await;
    assert_eq!(status_of(&error), 401);
    assert!(matches!(
        error,
        SessionStreamError::Rejected {
            error: BridgeApiError::Unauthorized(_),
            ..
        }
    ));
    // Another class never crosses over.
    assert_eq!(
        status_of(&refusal(&leased.ingress_url, &leased.secret).await),
        401
    );
    assert_eq!(
        status_of(&refusal(&leased.ingress_url, &leased.device.refresh_token).await),
        401
    );
}

#[tokio::test]
async fn a_session_token_only_opens_its_own_session() {
    let server = server().await;
    let first = leased(&server).await;
    let second = lease_on(
        &server,
        first.device.clone(),
        first.environment_id.clone(),
        first.secret.clone(),
    )
    .await;
    assert_ne!(first.session_id, second.session_id);

    // Right token, wrong session.
    let error = refusal(&second.ingress_url, &first.session_token).await;
    assert_eq!(status_of(&error), 403);
    assert!(!error.is_lease_gone());
    // And an unknown session is not confirmed to exist — the credential
    // is checked before the id.
    let unknown = second
        .ingress_url
        .replace(&second.session_id, &ids::generate_id("sess"));
    assert_eq!(
        status_of(&refusal(&unknown, &first.session_token).await),
        403
    );
}

#[tokio::test]
async fn a_worker_whose_lease_was_stopped_gets_no_socket() {
    let server = server().await;
    let leased = leased(&server).await;
    stop_work(&server, &leased).await;

    let error = refusal(&leased.ingress_url, &leased.session_token).await;
    assert_eq!(status_of(&error), 409);
    assert!(error.is_lease_gone());
    assert!(!error.is_transient());
}

#[tokio::test]
async fn a_controller_from_another_account_is_told_nothing() {
    let directory = tempfile::tempdir().expect("tempdir");
    let database = directory.path().join("rc.sqlite3");
    let server = server_with(Options {
        database_path: Some(database.clone()),
        ..Options::default()
    })
    .await;
    let leased = leased(&server).await;
    let intruder = device_on_another_account(&server, &database);

    assert_eq!(
        status_of(&refusal(&leased.ingress_url, &intruder).await),
        404
    );
    // Exactly what an unknown session gets, so the two are indistinguishable.
    let unknown = leased
        .ingress_url
        .replace(&leased.session_id, &ids::generate_id("sess"));
    assert_eq!(status_of(&refusal(&unknown, &intruder).await), 404);
    assert_eq!(
        status_of(&refusal(&unknown, &leased.device.access_token).await),
        404
    );
}

/// RC holds one account until OIDC lands, so a second one is written
/// straight into the database; its device then goes through the store's
/// ordinary issuance path, and its token through the ordinary auth.
fn device_on_another_account(server: &Server, database: &Path) -> String {
    let account = ids::generate_id("acc");
    rusqlite::Connection::open(database)
        .expect("open database")
        .execute(
            "INSERT INTO accounts (account_id, issuer, subject, created_at_unix)
             VALUES (?1, NULL, NULL, 0)",
            [&account],
        )
        .expect("insert account");
    let access = ids::generate_token();
    let refresh = ids::generate_token();
    let key = Options::default().hmac_key;
    let now = ids::now_unix();
    server
        .state
        .store()
        .issue_device(
            &account,
            "test",
            "intruder",
            &ids::domain_digest(
                &key,
                auth::DOMAIN_DEVICE_REFRESH,
                &ids::token_bytes(&refresh),
            ),
            &ids::domain_digest(&key, auth::DOMAIN_DEVICE_ACCESS, &ids::token_bytes(&access)),
            now + 3_600,
            now,
        )
        .expect("issue device");
    access
}

// ─── Protocol and limits ──────────────────────────────────────────────

#[tokio::test]
async fn a_frame_from_the_wrong_side_closes_the_sender() {
    let server = server().await;
    let leased = leased(&server).await;

    // A worker may not prompt, nor answer its own question.
    for frame in [
        SessionFrame::prompt("hijack"),
        SessionFrame::QuestionResponse {
            request_id: "perm-q".into(),
            answers: vec![QuestionAnswer::options([0])],
        },
    ] {
        let (worker, mut worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
        worker.send(&frame).await.expect("send");
        assert_eq!(closed(&mut worker_rx).await.0, CloseReason::ProtocolError);
    }

    // A controller may not speak for the worker, nor for the server.
    for frame in [
        message(0),
        SessionFrame::stream_error("no_worker", "forged"),
    ] {
        let (controller, mut rx) = attach(&leased.ingress_url, &leased.device.access_token).await;
        controller.send(&frame).await.expect("send");
        assert_eq!(closed(&mut rx).await.0, CloseReason::ProtocolError);
    }
    assert!(events_of(&server, &leased.session_id).is_empty());
}

#[tokio::test]
async fn malformed_and_binary_frames_close_the_sender() {
    let server = server().await;
    let leased = leased(&server).await;
    for raw in [
        Message::Text("not json".into()),
        Message::Text(r#"{"type":"prompt"}"#.into()),
        Message::Text(r#"{"type":"question_response","request_id":"perm-q"}"#.into()),
        Message::Text(r#"{"no_type":true}"#.into()),
        Message::Binary(vec![1, 2, 3]),
    ] {
        let (controller, mut rx) = attach(&leased.ingress_url, &leased.device.access_token).await;
        controller.send_raw(raw).await.expect("send");
        let (reason, _) = closed(&mut rx).await;
        assert_eq!(reason, CloseReason::ProtocolError);
        assert!(!reason.is_reconnectable());
    }
}

#[tokio::test]
async fn an_oversized_frame_closes_the_sender_with_the_size_code() {
    const LIMIT: usize = 4_096;
    let server = server_with(Options {
        max_body_bytes: LIMIT,
        ..Options::default()
    })
    .await;
    let leased = leased(&server).await;

    // One byte over is caught by the application check; far over is
    // caught by the WebSocket layer. Both must say "too big".
    for size in [LIMIT + 1, LIMIT * 8] {
        let (worker, mut rx) = attach(&leased.ingress_url, &leased.session_token).await;
        let padding = "x".repeat(size - r#"{"type":"session_message","message":""}"#.len());
        let text = format!(r#"{{"type":"session_message","message":"{padding}"}}"#);
        assert_eq!(text.len(), size);
        worker.send_raw(Message::Text(text)).await.expect("send");
        let (reason, _) = closed(&mut rx).await;
        assert_eq!(reason, CloseReason::ResourceLimit, "a {size}-byte frame");
    }
    // A frame exactly at the limit is fine.
    let (worker, _rx) = attach(&leased.ingress_url, &leased.session_token).await;
    let (_controller, mut controller_rx) =
        attach(&leased.ingress_url, &leased.device.access_token).await;
    let padding = "x".repeat(LIMIT - r#"{"type":"session_message","message":""}"#.len());
    let text = format!(r#"{{"type":"session_message","message":"{padding}"}}"#);
    worker.send_raw(Message::Text(text)).await.expect("send");
    assert_eq!(
        next(&mut controller_rx).await.frame_type(),
        "session_message"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_controller_that_stops_reading_is_cut_off_without_stalling_the_others() {
    // Enough bytes to fill a stalled reader's TCP buffers on both ends and
    // then its bounded queue, several times over.
    const FRAMES: usize = 4_000;
    const PAYLOAD: usize = 16_000;
    let server = server().await;
    let leased = leased(&server).await;
    let (worker, _worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    let (_stalled, mut stalled_rx) = attach(&leased.ingress_url, &leased.device.access_token).await;
    let (_healthy, mut healthy_rx) = attach(&leased.ingress_url, &leased.device.access_token).await;

    let reader = tokio::spawn(async move {
        let mut seen = 0usize;
        while seen < FRAMES {
            let frame = timeout(WAIT, healthy_rx.recv())
                .await
                .expect("the healthy reader keeps receiving")
                .expect("still open")
                .expect("parses");
            assert_eq!(seq_of(&frame), seen as u64);
            seen += 1;
        }
        healthy_rx
    });

    let padding = "y".repeat(PAYLOAD);
    for seq in 0..FRAMES {
        worker
            .send(&SessionFrame::SessionMessage {
                message_id: None,
                message: json!({"seq": seq, "padding": padding}),
            })
            .await
            .expect("send");
    }
    // Every frame reached the reader that kept up.
    let _healthy_rx = reader.await.expect("healthy reader");

    // The one that did not was closed for it, having received only what
    // fit in its buffers before the queue overflowed.
    let (reason, delivered) = closed(&mut stalled_rx).await;
    assert_eq!(reason, CloseReason::ResourceLimit);
    assert!(reason.is_reconnectable());
    assert!(delivered < FRAMES, "delivered {delivered} of {FRAMES}");
}
