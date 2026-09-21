//! Several controllers writing to one session at once:
//! prompts are stored and routed in the order they arrive, and the first
//! answer to a prompt is the only one that reaches the worker.
//!
//! The real server and the real `rebon-bridge` client over a real socket,
//! like `session_stream.rs`; the worker is played by a bare stream client.

mod support;

use std::time::Duration;

use rebon_bridge::config::PermissionResponseBody;
use rebon_bridge::session_stream::{
    error_code, AnsweredBy, ControlResponseBody, QuestionAnswer, SessionFrame,
};
use rebon_bridge::stream_client::{
    SessionStream, SessionStreamOptions, SessionStreamRx, SessionStreamTx,
};
use rebon_bridge::work_secret::WorkSecret;
use serde_json::{json, Value};
use support::*;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

/// One leased session, and two control surfaces on two devices.
struct Session {
    session_id: String,
    ingress_url: String,
    session_token: String,
    laptop: Device,
    phone: Device,
}

async fn session(server: &Server) -> Session {
    let (laptop, environment_id, secret) = bootstrapped_environment(server).await;
    let phone = issue_device(server, &laptop.access_token, "phone").await;
    let (_, session_id) = enqueue(server, &laptop.access_token, &environment_id, "hi").await;
    let work = poll(server, &environment_id, &secret, 2_000)
        .await
        .expect("the queued work is handed out");
    let secret = WorkSecret::decode(work["secret"].as_str().expect("secret")).expect("decodes");
    Session {
        session_id,
        ingress_url: secret.ingress_url,
        session_token: secret.session_token,
        laptop,
        phone,
    }
}

async fn attach(url: &str, token: &str) -> (SessionStreamTx, SessionStreamRx) {
    SessionStream::connect(&SessionStreamOptions::new(url, token))
        .await
        .expect("the stream accepts this credential")
        .split()
}

async fn next(rx: &mut SessionStreamRx) -> SessionFrame {
    timeout(WAIT, rx.recv())
        .await
        .expect("a frame arrives in time")
        .expect("the stream is still open")
        .expect("the frame parses")
}

async fn quiet(rx: &mut SessionStreamRx) {
    if let Ok(Some(frame)) = timeout(Duration::from_millis(300), rx.recv()).await {
        panic!("unexpected frame {frame:?}");
    }
}

fn stored(server: &Server, session_id: &str) -> Vec<Value> {
    server
        .state
        .store()
        .session_events(session_id)
        .expect("session events")
        .into_iter()
        .map(|event| serde_json::from_str(&event.payload_json).expect("stored json"))
        .collect()
}

fn allow(request_id: &str) -> SessionFrame {
    SessionFrame::PermissionResponse {
        response: PermissionResponseBody::success(request_id, json!({"behavior": "allow"})),
    }
}

fn deny(request_id: &str) -> SessionFrame {
    SessionFrame::PermissionResponse {
        response: PermissionResponseBody::success(request_id, json!({"behavior": "deny"})),
    }
}

fn request(request_id: &str) -> SessionFrame {
    SessionFrame::PermissionRequest {
        request_id: request_id.into(),
        request: json!({"tool": "Bash"}),
    }
}

/// The refusal a losing controller is sent, taken apart.
fn refusal(frame: SessionFrame) -> (String, AnsweredBy) {
    match frame {
        SessionFrame::StreamError {
            code,
            request_id,
            answered_by,
            message,
        } => {
            assert_eq!(code, error_code::ALREADY_ANSWERED, "{message}");
            (
                request_id.expect("the refusal names the prompt"),
                answered_by.expect("the refusal names the holder"),
            )
        }
        other => panic!("expected an already_answered refusal, got {other:?}"),
    }
}

fn is_refusal(frame: &SessionFrame) -> bool {
    matches!(frame, SessionFrame::StreamError { .. })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn of_two_answers_racing_for_one_prompt_only_the_first_reaches_the_worker() {
    let server = server().await;
    let session = session(&server).await;
    let (worker, mut worker_rx) = attach(&session.ingress_url, &session.session_token).await;
    let (laptop, mut laptop_rx) = attach(&session.ingress_url, &session.laptop.access_token).await;
    let (phone, mut phone_rx) = attach(&session.ingress_url, &session.phone.access_token).await;

    worker.send(&request("perm-1")).await.expect("send");
    assert_eq!(next(&mut laptop_rx).await, request("perm-1"));
    assert_eq!(next(&mut phone_rx).await, request("perm-1"));

    // Both at once. Whichever the server took first is the one the
    // worker gets; the other is refused.
    let (laptop_answer, phone_answer) = (allow("perm-1"), deny("perm-1"));
    let (sent_allow, sent_deny) =
        tokio::join!(laptop.send(&laptop_answer), phone.send(&phone_answer));
    sent_allow.expect("send");
    sent_deny.expect("send");
    let routed = next(&mut worker_rx).await;
    quiet(&mut worker_rx).await;

    let laptop_won = routed == allow("perm-1");
    if !laptop_won {
        assert_eq!(routed, deny("perm-1"));
    }
    let (winner, winner_rx, loser_rx) = if laptop_won {
        (&session.laptop, &mut laptop_rx, &mut phone_rx)
    } else {
        (&session.phone, &mut phone_rx, &mut laptop_rx)
    };
    // The loser hears the winning answer mirrored and its own refusal,
    // in either order; the winner hears nothing back.
    let loser_frames = [next(loser_rx).await, next(loser_rx).await];
    quiet(loser_rx).await;
    quiet(winner_rx).await;
    assert!(loser_frames.contains(&routed), "{loser_frames:?}");
    let refused = loser_frames
        .into_iter()
        .find(is_refusal)
        .expect("the loser is refused");
    let (request_id, holder) = refusal(refused);
    assert_eq!(request_id, "perm-1");
    assert_eq!(holder.device_id, winner.device_id);
    assert_eq!(
        holder.label.as_deref(),
        Some(if laptop_won { "first" } else { "phone" })
    );
    assert!(holder
        .connection_id
        .as_deref()
        .is_some_and(|tag| tag.starts_with("c-") && tag.len() == 18));

    // Only the winner is in the history, under the event id the refusal
    // names.
    let history = stored(&server, &session.session_id);
    let kinds: Vec<&str> = history
        .iter()
        .map(|event| event["type"].as_str().expect("type"))
        .collect();
    assert_eq!(kinds, vec!["permission_request", "permission_response"]);
    let events = server
        .state
        .store()
        .session_events(&session.session_id)
        .expect("events");
    assert_eq!(holder.event_id, Some(events[1].event_id as u64));
    // No credential anywhere in the refusal.
    let text = serde_json::to_string(&holder).expect("serializes");
    for secret in [
        &session.laptop.access_token,
        &session.laptop.refresh_token,
        &session.phone.access_token,
        &session.phone.refresh_token,
        &session.session_token,
    ] {
        assert!(!text.contains(secret.as_str()));
    }
}

#[tokio::test]
async fn the_first_answer_is_mirrored_and_later_ones_are_refused_from_any_other_surface() {
    let server = server().await;
    let session = session(&server).await;
    let (worker, mut worker_rx) = attach(&session.ingress_url, &session.session_token).await;
    let (laptop, mut laptop_rx) = attach(&session.ingress_url, &session.laptop.access_token).await;
    let (phone, mut phone_rx) = attach(&session.ingress_url, &session.phone.access_token).await;
    // A second surface on the laptop's own device is still another
    // controller.
    let (tab, mut tab_rx) = attach(&session.ingress_url, &session.laptop.access_token).await;

    let question = SessionFrame::PermissionRequest {
        request_id: "perm-q".into(),
        request: json!({"_meta": {"rebonRc": {"kind": "question", "answerWith": "question_response"}}}),
    };
    worker.send(&question).await.expect("send");
    for rx in [&mut laptop_rx, &mut phone_rx, &mut tab_rx] {
        assert_eq!(next(rx).await, question);
    }

    let answers = SessionFrame::QuestionResponse {
        request_id: "perm-q".into(),
        answers: vec![QuestionAnswer::options([0])],
    };
    phone.send(&answers).await.expect("send");
    assert_eq!(next(&mut worker_rx).await, answers);
    // The others close their prompt on the mirror.
    assert_eq!(next(&mut laptop_rx).await, answers);
    assert_eq!(next(&mut tab_rx).await, answers);

    // A late answer of either kind, from either surface, is refused.
    for (sender, rx, late) in [
        (&laptop, &mut laptop_rx, allow("perm-q")),
        (&tab, &mut tab_rx, answers.clone()),
    ] {
        sender.send(&late).await.expect("send");
        let (request_id, holder) = refusal(next(rx).await);
        assert_eq!(request_id, "perm-q");
        assert_eq!(holder.device_id, session.phone.device_id);
        assert_eq!(holder.label.as_deref(), Some("phone"));
    }
    quiet(&mut worker_rx).await;
    // Nobody else hears about a refusal.
    quiet(&mut phone_rx).await;

    // The holder may send again: that goes through.
    phone.send(&answers).await.expect("send");
    assert_eq!(next(&mut worker_rx).await, answers);

    // Other frames are not answers and are never held up.
    laptop
        .send(&SessionFrame::prompt("next"))
        .await
        .expect("send");
    assert_eq!(next(&mut worker_rx).await, SessionFrame::prompt("next"));
    // Nor is another prompt's answer.
    laptop.send(&allow("perm-other")).await.expect("send");
    assert_eq!(next(&mut worker_rx).await, allow("perm-other"));
}

#[tokio::test]
async fn a_runner_refusal_reopens_the_prompt() {
    let server = server().await;
    let session = session(&server).await;
    let (worker, mut worker_rx) = attach(&session.ingress_url, &session.session_token).await;
    let (laptop, mut laptop_rx) = attach(&session.ingress_url, &session.laptop.access_token).await;
    let (phone, mut phone_rx) = attach(&session.ingress_url, &session.phone.access_token).await;

    worker.send(&request("perm-1")).await.expect("send");
    next(&mut laptop_rx).await;
    next(&mut phone_rx).await;

    // The laptop asks for a standing rule, which the runner refuses.
    laptop.send(&allow("perm-1")).await.expect("send");
    next(&mut worker_rx).await;
    next(&mut phone_rx).await;
    phone.send(&deny("perm-1")).await.expect("send");
    refusal(next(&mut phone_rx).await);

    let refused = SessionFrame::ControlResponse {
        response: ControlResponseBody::error("perm-1", "only this call can be allowed remotely"),
    };
    worker.send(&refused).await.expect("send");
    // Every controller sees the refusal, as before.
    assert_eq!(next(&mut laptop_rx).await, refused);
    assert_eq!(next(&mut phone_rx).await, refused);

    // Now the phone's answer is the first one that counts.
    phone.send(&deny("perm-1")).await.expect("send");
    assert_eq!(next(&mut worker_rx).await, deny("perm-1"));
    assert_eq!(next(&mut laptop_rx).await, deny("perm-1"));
    laptop.send(&allow("perm-1")).await.expect("send");
    let (_, holder) = refusal(next(&mut laptop_rx).await);
    assert_eq!(holder.device_id, session.phone.device_id);

    // A success response, or an error for something nobody answered,
    // reopens nothing.
    worker
        .send(&SessionFrame::ControlResponse {
            response: ControlResponseBody::success("perm-1", None),
        })
        .await
        .expect("send");
    next(&mut laptop_rx).await;
    laptop.send(&allow("perm-1")).await.expect("send");
    refusal(next(&mut laptop_rx).await);
}

#[tokio::test]
async fn an_answer_held_for_a_worker_socket_that_is_gone_does_not_block_the_next() {
    let server = server().await;
    let session = session(&server).await;
    let (worker, mut worker_rx) = attach(&session.ingress_url, &session.session_token).await;
    let (laptop, mut laptop_rx) = attach(&session.ingress_url, &session.laptop.access_token).await;
    let (phone, mut phone_rx) = attach(&session.ingress_url, &session.phone.access_token).await;

    worker.send(&request("perm-1")).await.expect("send");
    next(&mut laptop_rx).await;
    next(&mut phone_rx).await;
    laptop.send(&allow("perm-1")).await.expect("send");
    next(&mut worker_rx).await;
    next(&mut phone_rx).await;

    // The runner reconnects: a worker gets no replay, so whether the
    // laptop's answer landed is the runner's to say, not the server's.
    drop(worker);
    let (_worker, mut worker_rx) = attach(&session.ingress_url, &session.session_token).await;
    phone.send(&deny("perm-1")).await.expect("send");
    assert_eq!(next(&mut worker_rx).await, deny("perm-1"));
    assert_eq!(next(&mut laptop_rx).await, deny("perm-1"));

    // And the phone now holds it on the new socket.
    laptop.send(&allow("perm-1")).await.expect("send");
    let (_, holder) = refusal(next(&mut laptop_rx).await);
    assert_eq!(holder.device_id, session.phone.device_id);
    quiet(&mut worker_rx).await;
}

#[tokio::test]
async fn a_claim_survives_a_restart_as_advisory_only() {
    let directory = tempfile::tempdir().expect("tempdir");
    let database = directory.path().join("rc.sqlite3");
    let options = || Options {
        database_path: Some(database.clone()),
        ..Options::default()
    };
    let first = server_with(options()).await;
    let session = session(&first).await;
    {
        let (worker, mut worker_rx) = attach(&session.ingress_url, &session.session_token).await;
        let (laptop, mut laptop_rx) =
            attach(&session.ingress_url, &session.laptop.access_token).await;
        worker.send(&request("perm-1")).await.expect("send");
        next(&mut laptop_rx).await;
        laptop.send(&allow("perm-1")).await.expect("send");
        next(&mut worker_rx).await;
    }
    drop(first);

    // Same database, new process. Every socket is new, so the claim can
    // no longer say whether its answer arrived: the next answer is
    // routed, and the runner judges it.
    let second = server_with(options()).await;
    let url = session.ingress_url.replace(
        session.ingress_url.split("/v1/").next().expect("origin"),
        &second.base.replace("http://", "ws://"),
    );
    let (_worker, mut worker_rx) = attach(&url, &session.session_token).await;
    let (phone, mut phone_rx) = attach(&url, &session.phone.access_token).await;
    // The replay carries the prompt and the answer that holds it.
    assert_eq!(next(&mut phone_rx).await, request("perm-1"));
    assert_eq!(next(&mut phone_rx).await, allow("perm-1"));
    phone.send(&deny("perm-1")).await.expect("send");
    assert_eq!(next(&mut worker_rx).await, deny("perm-1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prompts_from_many_controllers_reach_the_worker_in_the_order_they_were_stored() {
    const PER_CONTROLLER: usize = 40;
    let server = server().await;
    let session = session(&server).await;
    let (_worker, mut worker_rx) = attach(&session.ingress_url, &session.session_token).await;
    let (laptop, _laptop_rx) = attach(&session.ingress_url, &session.laptop.access_token).await;
    let (phone, _phone_rx) = attach(&session.ingress_url, &session.phone.access_token).await;
    let (tab, _tab_rx) = attach(&session.ingress_url, &session.laptop.access_token).await;

    let mut writers = Vec::new();
    for (name, sender) in [("laptop", laptop), ("phone", phone), ("tab", tab)] {
        writers.push(tokio::spawn(async move {
            for seq in 0..PER_CONTROLLER {
                sender
                    .send(&SessionFrame::prompt(format!("{name}-{seq}")))
                    .await
                    .expect("send");
                if seq % 7 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            sender
        }));
    }
    let mut routed = Vec::new();
    while routed.len() < PER_CONTROLLER * 3 {
        match next(&mut worker_rx).await {
            SessionFrame::Prompt { text, .. } => routed.push(text),
            other => panic!("expected a prompt, got {other:?}"),
        }
    }
    for writer in writers {
        writer.await.expect("writer");
    }
    quiet(&mut worker_rx).await;

    // The worker's order is the history's order, exactly: nothing is
    // stored in one order and routed in another.
    let history: Vec<String> = stored(&server, &session.session_id)
        .into_iter()
        .map(|event| event["text"].as_str().expect("text").to_string())
        .collect();
    assert_eq!(routed, history);
    // And each controller's prompts keep the order it sent them in.
    for name in ["laptop", "phone", "tab"] {
        let own: Vec<&String> = routed
            .iter()
            .filter(|text| text.starts_with(&format!("{name}-")))
            .collect();
        let expected: Vec<String> = (0..PER_CONTROLLER)
            .map(|seq| format!("{name}-{seq}"))
            .collect();
        assert_eq!(own, expected.iter().collect::<Vec<_>>(), "{name}");
    }
}

#[tokio::test]
async fn a_prompt_sent_while_the_session_is_busy_is_routed_not_refused() {
    // The server does not second-guess a running turn: the session host
    // queues the prompt behind it. What the server owes is to deliver it.
    let server = server().await;
    let session = session(&server).await;
    let (worker, mut worker_rx) = attach(&session.ingress_url, &session.session_token).await;
    let (laptop, mut laptop_rx) = attach(&session.ingress_url, &session.laptop.access_token).await;
    let running = SessionFrame::SessionState {
        state: "running".into(),
        detail: None,
    };
    worker.send(&running).await.expect("send");
    assert_eq!(next(&mut laptop_rx).await, running);
    for text in ["first", "second"] {
        laptop
            .send(&SessionFrame::prompt(text))
            .await
            .expect("send");
    }
    assert_eq!(next(&mut worker_rx).await, SessionFrame::prompt("first"));
    assert_eq!(next(&mut worker_rx).await, SessionFrame::prompt("second"));
    quiet(&mut laptop_rx).await;
}
