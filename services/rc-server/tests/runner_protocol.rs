//! The protocol a session runner depends on, driven
//! end to end with the real `rebon-bridge` clients against the real
//! server: an environment that advertises projects, work items that say
//! what to run and where, frame identity, control-request verdicts and
//! the reported-state vocabulary.

mod support;

use std::time::Duration;

use rebon_bridge::api_client::{BridgeApiClient, BridgeApiError, PollOptions};
use rebon_bridge::config::{
    EnqueueWorkRequest, PermissionResponseBody, PermissionResponseEvent, SessionWork, WorkDataType,
    WorkItem,
};
use rebon_bridge::control_request::{
    plan_server_control_response, unsupported_control_error, ControlEffect, ControlVerdict,
    ServerControlRequestPlanInput,
};
use rebon_bridge::history::PageRequest;
use rebon_bridge::http_client::{DeviceCredentials, HttpBridgeApiClient, HttpClientConfig};
use rebon_bridge::projects::ProjectInfo;
use rebon_bridge::session_stream::{
    ControlResponseBody, DeliveredFrame, SessionFrame, SessionRunState, MAX_FRAME_ID_BYTES,
};
use rebon_bridge::stream_client::{
    CloseReason, RawMessage, SessionStream, SessionStreamOptions, SessionStreamRx, SessionStreamTx,
};
use rebon_bridge::work_secret::WorkSecret;
use serde_json::{json, Value};
use support::*;
use tokio::time::timeout;

// ─── Fixtures ─────────────────────────────────────────────────────────

fn settings(server: &Server) -> HttpClientConfig {
    let mut config = HttpClientConfig::new(server.base.clone());
    config.poll_wait = Duration::from_millis(1_000);
    config.poll_timeout_margin = Duration::from_secs(5);
    config.request_timeout = Duration::from_secs(5);
    config
}

/// A client acting as the machine (and, through its device token, as a
/// controller on the same account).
fn client_for(server: &Server, device: &Device) -> HttpBridgeApiClient {
    HttpBridgeApiClient::new(
        settings(server),
        DeviceCredentials::new(device.access_token.clone(), device.refresh_token.clone()),
    )
    .expect("client")
}

fn app() -> ProjectInfo {
    ProjectInfo::new("/home/me/src/app", "App")
        .with_remote("git@example.com:me/app.git")
        .with_branch("main")
}

fn docs() -> ProjectInfo {
    ProjectInfo::from_path(r"D:\work\docs")
}

/// One machine with two projects, registered through the real client.
async fn machine(server: &Server) -> (Device, HttpBridgeApiClient, String) {
    let device = bootstrap(server).await;
    let client = client_for(server, &device);
    let mut config = bridge_config("machine-1");
    config.projects = vec![app(), docs()];
    let registered = client
        .register_bridge_environment(&config)
        .await
        .expect("register");
    (device, client, registered.environment_id)
}

fn permanent(error: BridgeApiError) -> String {
    match error {
        BridgeApiError::Permanent(detail) => detail,
        other => panic!("expected a permanent failure, got {other:?}"),
    }
}

// ─── Projects ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_machine_registers_with_its_projects_and_the_list_shows_them() {
    let server = server().await;
    let (_, client, environment_id) = machine(&server).await;

    let listed = client.list_environments().await.expect("list");
    assert_eq!(listed.environments.len(), 1);
    let environment = &listed.environments[0];
    assert_eq!(environment.environment_id, environment_id);
    assert_eq!(environment.worker_type, "rebon");
    assert_eq!(environment.projects, vec![app(), docs()]);
    assert_eq!(environment.projects[1].label, "docs");
}

#[tokio::test]
async fn a_registration_without_projects_serves_its_dir() {
    let server = server().await;
    let device = bootstrap(&server).await;
    let client = client_for(&server, &device);
    // `bridge_config` sets `dir`, `branch` and `git_repo_url`, and no
    // projects — the shape every registration had before projects.
    client
        .register_bridge_environment(&bridge_config("legacy"))
        .await
        .expect("register");
    let listed = client.list_environments().await.expect("list");
    assert_eq!(
        listed.environments[0].projects,
        vec![ProjectInfo::new("/home/user/repo", "repo")
            .with_remote("git@github.com:user/repo.git")
            .with_branch("main")]
    );
}

#[tokio::test]
async fn a_machine_replaces_its_project_list() {
    let server = server().await;
    let (_, client, environment_id) = machine(&server).await;

    let replacement = vec![docs(), ProjectInfo::from_path("/srv/new")];
    let stored = client
        .update_projects(&environment_id, &replacement)
        .await
        .expect("update");
    assert_eq!(stored.projects, replacement);
    let listed = client.list_environments().await.expect("list");
    assert_eq!(listed.environments[0].projects, replacement);

    // An empty list is a valid state: the machine serves nothing.
    let emptied = client
        .update_projects(&environment_id, &[])
        .await
        .expect("empty update");
    assert!(emptied.projects.is_empty());

    // A list RC will not store changes nothing.
    let refused = client
        .update_projects(&environment_id, &[docs(), docs()])
        .await
        .expect_err("duplicate paths");
    assert!(permanent(refused).contains("400"));
    let refused = client
        .update_projects(&environment_id, &[ProjectInfo::new("", "nameless")])
        .await
        .expect_err("empty path");
    assert!(permanent(refused).contains("400"));
    let listed = client.list_environments().await.expect("list");
    assert!(listed.environments[0].projects.is_empty());

    // A re-registration is a full project update too.
    let mut config = bridge_config("machine-1");
    config.projects = vec![app()];
    client
        .register_bridge_environment(&config)
        .await
        .expect("re-register");
    let listed = client.list_environments().await.expect("list");
    assert_eq!(listed.environments[0].projects, vec![app()]);
}

#[tokio::test]
async fn only_the_environment_itself_may_replace_its_projects() {
    let server = server().await;
    let (device, client, environment_id) = machine(&server).await;
    let url = format!("{}/v1/environments/{environment_id}/projects", server.base);
    let body = serde_json::json!({"projects": []});

    // A device token is a controller, not the machine.
    let as_device = support::client()
        .put(&url)
        .bearer_auth(&device.access_token)
        .json(&body)
        .send()
        .await
        .expect("put");
    assert_eq!(as_device.status(), reqwest::StatusCode::UNAUTHORIZED);

    // Another environment's secret is the wrong environment.
    let (_, other_secret) =
        register(&server, &device.access_token, &bridge_config("machine-2")).await;
    let as_other = support::client()
        .put(&url)
        .bearer_auth(&other_secret)
        .json(&body)
        .send()
        .await
        .expect("put");
    assert_eq!(as_other.status(), reqwest::StatusCode::FORBIDDEN);

    // A malformed body is 400.
    let secret = client.environment_secret().expect("secret");
    let malformed = support::client()
        .put(&url)
        .bearer_auth(&secret)
        .json(&serde_json::json!({"projects": [{"path": "/a"}]}))
        .send()
        .await
        .expect("put");
    assert_eq!(malformed.status(), reqwest::StatusCode::BAD_REQUEST);

    // Nothing changed.
    let listed = client.list_environments().await.expect("list");
    let mine = listed
        .environments
        .iter()
        .find(|environment| environment.environment_id == environment_id)
        .expect("listed");
    assert_eq!(mine.projects, vec![app(), docs()]);
}

// ─── Work items ───────────────────────────────────────────────────────

/// Poll once through the real client and insist on an item.
async fn poll_item(client: &HttpBridgeApiClient, environment_id: &str) -> WorkItem {
    let secret = client.environment_secret().expect("secret");
    client
        .poll_for_work_item(environment_id, &secret, PollOptions::default())
        .await
        .expect("poll")
        .expect("an item is waiting")
}

#[tokio::test]
async fn a_work_item_carries_its_project_and_prompt_to_the_runner() {
    let server = server().await;
    let (_, client, environment_id) = machine(&server).await;

    let queued = client
        .enqueue_work(
            &environment_id,
            &EnqueueWorkRequest::session("fix the flaky test").in_project(docs().path),
        )
        .await
        .expect("enqueue");
    let session_id = queued.session_id.clone().expect("a session");

    let item = poll_item(&client, &environment_id).await;
    assert_eq!(item.response.id, queued.work_id);
    assert_eq!(item.response.data.data_type, WorkDataType::Session);
    assert_eq!(item.response.data.id, session_id);
    assert_eq!(
        item.session,
        Some(SessionWork {
            project: docs().path,
            prompt: Some("fix the flaky test".into()),
            resume_rebon_session_id: None,
        })
    );
    // The prompt is not credential material and is not in the secret.
    let secret = WorkSecret::decode(&item.response.secret).expect("secret decodes");
    assert_eq!(secret.session_id.as_deref(), Some(session_id.as_str()));
    let secret_json = serde_json::to_string(&secret).expect("serialize");
    assert!(!secret_json.contains("flaky"), "{secret_json}");

    // A healthcheck has no session object at all.
    client
        .enqueue_work(&environment_id, &EnqueueWorkRequest::healthcheck())
        .await
        .expect("enqueue a probe");
    let probe = poll_item(&client, &environment_id).await;
    assert_eq!(probe.response.data.data_type, WorkDataType::Healthcheck);
    assert_eq!(probe.session, None);
}

#[tokio::test]
async fn a_resume_target_round_trips_and_a_follow_up_keeps_the_project() {
    let server = server().await;
    let (_, client, environment_id) = machine(&server).await;

    let mut resume = EnqueueWorkRequest::session("unused")
        .in_project(app().path)
        .resuming("0192f3c4-5e6f-7a8b");
    // A resume needs no prompt.
    resume.prompt = None;
    let queued = client
        .enqueue_work(&environment_id, &resume)
        .await
        .expect("enqueue");
    let session_id = queued.session_id.expect("session");
    let item = poll_item(&client, &environment_id).await;
    assert_eq!(
        item.session,
        Some(SessionWork {
            project: app().path,
            prompt: None,
            resume_rebon_session_id: Some("0192f3c4-5e6f-7a8b".into()),
        })
    );

    // More work for the same session: the project is inherited.
    client
        .enqueue_work(
            &environment_id,
            &EnqueueWorkRequest::session("next").for_session(&session_id),
        )
        .await
        .expect("follow-up");
    let item = poll_item(&client, &environment_id).await;
    assert_eq!(item.response.data.id, session_id);
    let session = item.session.expect("session work");
    assert_eq!(session.project, app().path);
    assert_eq!(session.prompt.as_deref(), Some("next"));

    // But it cannot be moved to another project.
    let moved = client
        .enqueue_work(
            &environment_id,
            &EnqueueWorkRequest::session("next")
                .for_session(&session_id)
                .in_project(docs().path),
        )
        .await
        .expect_err("a session keeps its project");
    assert!(permanent(moved).contains("400"));

    // A reconnect re-queues it in the same place, without the prompt and
    // with the newest item's resume target (the follow-up had none).
    client
        .reconnect_session(&environment_id, &session_id)
        .await
        .expect("reconnect");
    let item = poll_item(&client, &environment_id).await;
    assert_eq!(
        item.session,
        Some(SessionWork {
            project: app().path,
            prompt: None,
            resume_rebon_session_id: None,
        })
    );
}

#[tokio::test]
async fn work_for_a_project_the_machine_did_not_advertise_is_refused() {
    let server = server().await;
    let (_, client, environment_id) = machine(&server).await;

    for request in [
        // Not advertised.
        EnqueueWorkRequest::session("hi").in_project("/etc"),
        // Advertised, but spelt differently: paths are compared exactly.
        EnqueueWorkRequest::session("hi").in_project(format!("{}/", app().path)),
        // Two projects and none named.
        EnqueueWorkRequest::session("hi"),
        // A resume target that is not a plain name.
        EnqueueWorkRequest::session("hi")
            .in_project(app().path)
            .resuming("../../.ssh"),
    ] {
        let refused = client
            .enqueue_work(&environment_id, &request)
            .await
            .expect_err("refused");
        assert!(permanent(refused).contains("400"), "{request:?}");
    }

    // Once a project is withdrawn, new work for it is refused too.
    client
        .update_projects(&environment_id, &[docs()])
        .await
        .expect("update");
    let refused = client
        .enqueue_work(
            &environment_id,
            &EnqueueWorkRequest::session("hi").in_project(app().path),
        )
        .await
        .expect_err("withdrawn");
    assert!(permanent(refused).contains("400"));
    // With exactly one project left, naming none picks it.
    client
        .enqueue_work(&environment_id, &EnqueueWorkRequest::session("hi"))
        .await
        .expect("the only project");
    let item = poll_item(&client, &environment_id).await;
    assert_eq!(item.session.expect("session").project, docs().path);

    // Nothing refused was queued.
    let secret = client.environment_secret().expect("secret");
    assert!(client
        .poll_for_work_item(&environment_id, &secret, PollOptions::default())
        .await
        .expect("poll")
        .is_none());
}

#[tokio::test]
async fn a_session_on_another_environment_cannot_be_queued_for() {
    let server = server().await;
    let (device, client, environment_id) = machine(&server).await;
    let queued = client
        .enqueue_work(
            &environment_id,
            &EnqueueWorkRequest::session("hi").in_project(app().path),
        )
        .await
        .expect("enqueue");
    let session_id = queued.session_id.expect("session");

    // A second machine on the same account names that session.
    let (other_environment, _) =
        register(&server, &device.access_token, &bridge_config("machine-2")).await;
    let refused = client
        .enqueue_work(
            &other_environment,
            &EnqueueWorkRequest::session("hi").for_session(&session_id),
        )
        .await
        .expect_err("foreign session");
    assert!(permanent(refused).contains("404"));
}

// ─── Frame identity ───────────────────────────────────────────────────

/// Upper bound on any single wait on a socket.
const WAIT: Duration = Duration::from_secs(10);

/// A leased session on `machine`, as the runner sees it.
struct Leased {
    device: Device,
    client: HttpBridgeApiClient,
    session_id: String,
    session_token: String,
    ingress_url: String,
}

async fn leased(server: &Server) -> Leased {
    let (device, client, environment_id) = machine(server).await;
    client
        .enqueue_work(
            &environment_id,
            &EnqueueWorkRequest::session("hello").in_project(app().path),
        )
        .await
        .expect("enqueue");
    let item = poll_item(&client, &environment_id).await;
    let secret = WorkSecret::decode(&item.response.secret).expect("secret");
    Leased {
        device,
        client,
        session_id: secret.session_id.expect("session"),
        session_token: secret.session_token,
        ingress_url: secret.ingress_url,
    }
}

async fn attach(url: &str, token: &str) -> (SessionStreamTx, SessionStreamRx) {
    SessionStream::connect(&SessionStreamOptions::new(url, token))
        .await
        .expect("attached")
        .split()
}

async fn next_delivered(rx: &mut SessionStreamRx) -> DeliveredFrame {
    timeout(WAIT, rx.recv_delivered())
        .await
        .expect("a frame in time")
        .expect("the stream is open")
        .expect("the frame parses")
}

async fn close_reason(rx: &mut SessionStreamRx) -> CloseReason {
    timeout(WAIT, async { while rx.recv().await.is_some() {} })
        .await
        .expect("closed in time");
    rx.close_reason().cloned().expect("a close reason")
}

/// Every event id of the session's history, oldest first.
async fn history_ids(leased: &Leased) -> Vec<(u64, Value)> {
    let page = leased
        .client
        .session_events(&leased.session_id, &PageRequest::first())
        .await
        .expect("history");
    assert!(page.next_cursor.is_none());
    page.events
        .into_iter()
        .map(|event| (event.event_id, event.payload))
        .collect()
}

#[tokio::test]
async fn a_controller_sees_the_event_ids_history_pages_by() {
    let server = server().await;
    let leased = leased(&server).await;
    let (worker, mut worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    let (_live, mut live) = attach(&leased.ingress_url, &leased.device.access_token).await;

    let sent = [
        SessionFrame::message(json!({"n": 1})),
        SessionFrame::message_with_id("m-2", json!({"n": 2})),
        SessionFrame::SessionState {
            state: "idle".into(),
            detail: None,
        },
    ];
    let mut live_ids = Vec::new();
    for frame in &sent {
        worker.send(frame).await.expect("send");
        let delivered = next_delivered(&mut live).await;
        assert_eq!(&delivered.frame, frame);
        live_ids.push(delivered.event_id.expect("a persisted frame has an id"));
    }

    // A decision posted over HTTP is stamped the same way.
    let event = PermissionResponseEvent::new(PermissionResponseBody::success(
        "req-1",
        json!({"behavior": "allow"}),
    ));
    leased
        .client
        .send_permission_response_event(&leased.session_id, &event, &leased.session_token)
        .await
        .expect("post event");
    let delivered = next_delivered(&mut live).await;
    live_ids.push(delivered.event_id.expect("id"));

    // History has exactly those ids, and its payloads are the frames as
    // sent — no `event_id` inside.
    let history = history_ids(&leased).await;
    assert_eq!(
        history.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        live_ids
    );
    for ((_, payload), frame) in history.iter().zip(&sent) {
        assert!(payload.get("event_id").is_none(), "{payload}");
        assert_eq!(payload, &serde_json::to_value(frame).expect("json"));
    }

    // A controller that attaches now is replayed the same ids, so it can
    // splice a history page and the live socket together.
    let (_late, mut late) = attach(&leased.ingress_url, &leased.device.access_token).await;
    for expected in &live_ids {
        assert_eq!(next_delivered(&mut late).await.event_id, Some(*expected));
    }

    // A frame RC did not persist has no id.
    worker.close().await.expect("close");
    close_reason(&mut worker_rx).await;
    let (controller, mut controller_rx) =
        attach(&leased.ingress_url, &leased.device.access_token).await;
    for _ in &live_ids {
        next_delivered(&mut controller_rx).await;
    }
    // The worker's detach can trail its close frame by a moment, and a
    // prompt that still reached it is not echoed to its sender, so ask
    // until the refusal comes.
    let mut refused = None;
    for _ in 0..50 {
        controller
            .send(&SessionFrame::prompt("anyone?"))
            .await
            .expect("send");
        if let Ok(Some(Ok(delivered))) =
            timeout(Duration::from_millis(200), controller_rx.recv_delivered()).await
        {
            if let SessionFrame::StreamError { .. } = delivered.frame {
                refused = Some(delivered);
                break;
            }
        }
    }
    let refused = refused.expect("a stream_error once the worker is gone");
    assert_eq!(refused.event_id, None);
}

#[tokio::test]
async fn a_worker_resend_is_stored_once_and_the_socket_stays_open() {
    let server = server().await;
    let leased = leased(&server).await;
    let (worker, _worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    let (_controller, mut controller_rx) =
        attach(&leased.ingress_url, &leased.device.access_token).await;

    let message = SessionFrame::message_with_id("m-1", json!({"text": "once"}));
    let request = SessionFrame::PermissionRequest {
        request_id: "perm-1".into(),
        request: json!({"tool": "Bash"}),
    };
    worker.send(&message).await.expect("send");
    worker.send(&request).await.expect("send");
    let first = next_delivered(&mut controller_rx).await;
    assert_eq!(first.frame, message);
    assert_eq!(next_delivered(&mut controller_rx).await.frame, request);

    // Resent on the same socket…
    worker.send(&message).await.expect("resend");
    worker.send(&request).await.expect("resend");
    // …and again after a reconnect, which supersedes the first socket.
    let (again, _again_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    again.send(&message).await.expect("resend");
    again.send(&request).await.expect("resend");
    // A frame without an id is never deduplicated, and marks the end.
    let marker = SessionFrame::message(json!({"text": "after"}));
    again.send(&marker).await.expect("send");
    again.send(&marker).await.expect("send");

    // The controller sees neither resend: the next two frames are the
    // markers.
    assert_eq!(next_delivered(&mut controller_rx).await.frame, marker);
    assert_eq!(next_delivered(&mut controller_rx).await.frame, marker);

    let history = history_ids(&leased).await;
    assert_eq!(history.len(), 4, "{history:?}");
    assert_eq!(history[0].0, first.event_id.expect("id"));

    // The worker is still attached: a later frame still goes through.
    again
        .send(&SessionFrame::message(json!({"text": "still here"})))
        .await
        .expect("send");
    assert_eq!(
        next_delivered(&mut controller_rx).await.frame,
        SessionFrame::message(json!({"text": "still here"}))
    );
}

#[tokio::test]
async fn a_peer_may_not_supply_an_event_id_or_a_bad_message_id() {
    let server = server().await;
    let leased = leased(&server).await;

    for text in [
        r#"{"event_id":1,"type":"session_message","message":{}}"#.to_string(),
        r#"{"event_id":null,"type":"session_message","message":{}}"#.to_string(),
        r#"{"type":"session_message","message_id":"","message":{}}"#.to_string(),
        format!(
            r#"{{"type":"session_message","message_id":"{}","message":{{}}}}"#,
            "x".repeat(MAX_FRAME_ID_BYTES + 1)
        ),
    ] {
        let (worker, mut worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
        worker
            .send_raw(RawMessage::Text(text.clone()))
            .await
            .expect("send");
        assert_eq!(
            close_reason(&mut worker_rx).await,
            CloseReason::ProtocolError,
            "{text}"
        );
    }

    // A controller is held to the same rule.
    let (_worker, _worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    let (controller, mut controller_rx) =
        attach(&leased.ingress_url, &leased.device.access_token).await;
    controller
        .send_raw(RawMessage::Text(
            r#"{"event_id":7,"type":"prompt","text":"hi"}"#.into(),
        ))
        .await
        .expect("send");
    assert_eq!(
        close_reason(&mut controller_rx).await,
        CloseReason::ProtocolError
    );
    assert!(history_ids(&leased).await.is_empty());

    // The longest id that is allowed is fine.
    let (worker, _) = attach(&leased.ingress_url, &leased.session_token).await;
    let (_observer, mut observer) = attach(&leased.ingress_url, &leased.device.access_token).await;
    let longest = SessionFrame::message_with_id("y".repeat(MAX_FRAME_ID_BYTES), json!(1));
    worker.send(&longest).await.expect("send");
    assert_eq!(next_delivered(&mut observer).await.frame, longest);
}

// ─── Binding the local session ────────────────────────────────────────

#[tokio::test]
async fn the_bound_local_session_is_what_later_work_resumes() {
    let server = server().await;
    let (device, client, environment_id) = machine(&server).await;
    let queued = client
        .enqueue_work(
            &environment_id,
            &EnqueueWorkRequest::session("hello").in_project(app().path),
        )
        .await
        .expect("enqueue");
    let session_id = queued.session_id.expect("session");
    let item = poll_item(&client, &environment_id).await;
    assert_eq!(
        item.session
            .as_ref()
            .expect("session")
            .resume_rebon_session_id,
        None,
        "a new session has nothing to resume yet"
    );
    let secret = WorkSecret::decode(&item.response.secret).expect("secret");
    let (worker, _worker_rx) = attach(&secret.ingress_url, &secret.session_token).await;
    let (controller, mut controller_rx) = attach(&secret.ingress_url, &device.access_token).await;

    // The runner says which local session it opened, and says it again
    // after a reconnect; the controller sees it once.
    let bound = SessionFrame::bound("0192f3c4-local");
    worker.send(&bound).await.expect("bind");
    worker.send(&bound).await.expect("rebind");
    assert_eq!(next_delivered(&mut controller_rx).await.frame, bound);
    let marker = SessionFrame::message(json!({"text": "after"}));
    worker.send(&marker).await.expect("marker");
    assert_eq!(next_delivered(&mut controller_rx).await.frame, marker);

    // A controller may not bind a session.
    controller.send(&bound).await.expect("send");
    assert_eq!(
        close_reason(&mut controller_rx).await,
        CloseReason::ProtocolError
    );

    // A prompt queued for the session continues the bound local session…
    client
        .enqueue_work(
            &environment_id,
            &EnqueueWorkRequest::session("next").for_session(&session_id),
        )
        .await
        .expect("follow-up");
    let item = poll_item(&client, &environment_id).await;
    assert_eq!(
        item.session,
        Some(SessionWork {
            project: app().path,
            prompt: Some("next".into()),
            resume_rebon_session_id: Some("0192f3c4-local".into()),
        })
    );

    // …and so does a reconnect.
    client
        .reconnect_session(&environment_id, &session_id)
        .await
        .expect("reconnect");
    let item = poll_item(&client, &environment_id).await;
    assert_eq!(
        item.session,
        Some(SessionWork {
            project: app().path,
            prompt: None,
            resume_rebon_session_id: Some("0192f3c4-local".into()),
        })
    );
}

#[tokio::test]
async fn a_binding_that_is_not_a_plain_session_id_is_a_protocol_error() {
    let server = server().await;
    let leased = leased(&server).await;
    for text in [
        r#"{"type":"session_bound","rebon_session_id":"../../etc"}"#,
        r#"{"type":"session_bound","rebon_session_id":""}"#,
        r#"{"type":"session_bound"}"#,
    ] {
        let (worker, mut worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
        worker
            .send_raw(RawMessage::Text(text.into()))
            .await
            .expect("send");
        assert_eq!(
            close_reason(&mut worker_rx).await,
            CloseReason::ProtocolError,
            "{text}"
        );
    }
    assert!(history_ids(&leased).await.is_empty());
}

// ─── Control requests ─────────────────────────────────────────────────

#[tokio::test]
async fn control_request_verdicts_reach_the_controller_as_given() {
    let server = server().await;
    let leased = leased(&server).await;
    let (worker, mut worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    let (controller, mut controller_rx) =
        attach(&leased.ingress_url, &leased.device.access_token).await;

    // What a Rebon runner decides for each request.
    let cases: [(&str, Value, Option<ControlVerdict>); 4] = [
        (
            "set_model",
            json!({"model": "gpt-6"}),
            Some(ControlVerdict::Applied(ControlEffect::NextTurn)),
        ),
        // Rebon has no thinking-token setting: no verdict at all.
        (
            "set_max_thinking_tokens",
            json!({"max_thinking_tokens": 2048}),
            None,
        ),
        (
            "set_permission_mode",
            json!({"mode": "bypassPermissions"}),
            Some(ControlVerdict::Rejected(
                "bypass is disabled on this machine".into(),
            )),
        ),
        (
            "interrupt",
            Value::Null,
            Some(ControlVerdict::Applied(ControlEffect::Now)),
        ),
    ];
    for (index, (subtype, params, verdict)) in cases.into_iter().enumerate() {
        let request_id = format!("ctl-{index}");
        controller
            .send(&SessionFrame::ControlRequest {
                request_id: request_id.clone(),
                subtype: subtype.into(),
                params,
            })
            .await
            .expect("send");
        let received = next_delivered(&mut worker_rx).await.frame;
        let parsed = received.control_subtype().expect("control request");
        assert!(parsed.needs_verdict());
        let plan = plan_server_control_response(ServerControlRequestPlanInput {
            request_id: &request_id,
            subtype: parsed,
            outbound_only: false,
            verdict: verdict.clone(),
            pid: 1,
        });
        worker
            .send(&SessionFrame::ControlResponse {
                response: ControlResponseBody::from(plan),
            })
            .await
            .expect("respond");

        let SessionFrame::ControlResponse { response } =
            next_delivered(&mut controller_rx).await.frame
        else {
            panic!("expected a control response");
        };
        assert_eq!(response.request_id, request_id);
        match verdict {
            Some(ControlVerdict::Applied(effect)) => {
                assert_eq!(response.subtype, "success");
                assert_eq!(response.applied(), Some(effect));
            }
            Some(ControlVerdict::Rejected(error)) => {
                assert_eq!(response.subtype, "error");
                assert_eq!(response.error, Some(error));
                assert_eq!(response.applied(), None);
            }
            None => {
                assert_eq!(response.subtype, "error");
                assert_eq!(response.error, Some(unsupported_control_error(subtype)));
            }
        }
    }

    // The history holds the answers as the worker sent them.
    let history = history_ids(&leased).await;
    let answers: Vec<&Value> = history
        .iter()
        .map(|(_, payload)| payload)
        .filter(|payload| payload["type"] == "control_response")
        .collect();
    assert_eq!(answers.len(), 4);
    assert_eq!(
        answers[0]["response"]["response"],
        json!({"applies": "next_turn"})
    );
    assert_eq!(
        answers[3]["response"]["response"],
        json!({"applies": "now"})
    );
}

// ─── Reported state ───────────────────────────────────────────────────

#[tokio::test]
async fn the_session_list_shows_the_state_vocabulary() {
    let server = server().await;
    let leased = leased(&server).await;
    let (worker, _worker_rx) = attach(&leased.ingress_url, &leased.session_token).await;
    let (_controller, mut controller_rx) =
        attach(&leased.ingress_url, &leased.device.access_token).await;

    let reported = |state: SessionRunState, detail: Option<&str>| SessionFrame::SessionState {
        state,
        detail: detail.map(str::to_string),
    };
    let steps = [
        (SessionRunState::Starting, None),
        (SessionRunState::Running, None),
        (
            SessionRunState::NeedsInput,
            Some("Bash wants to run cargo test"),
        ),
        (SessionRunState::Idle, None),
        // A word from a newer worker is shown as sent.
        (SessionRunState::Other("compacting".into()), None),
        (SessionRunState::Failed, Some("model quota exhausted")),
    ];
    for (state, detail) in steps {
        let frame = reported(state.clone(), detail);
        worker.send(&frame).await.expect("send");
        let delivered = next_delivered(&mut controller_rx).await;
        assert_eq!(delivered.frame, frame);

        let page = leased
            .client
            .list_sessions(None, &PageRequest::first())
            .await
            .expect("list");
        let summary = page
            .sessions
            .iter()
            .find(|summary| summary.session_id == leased.session_id)
            .expect("listed");
        let shown = summary.reported_state.as_ref().expect("reported");
        assert_eq!(shown.state, state);
        assert_eq!(shown.detail.as_deref(), detail);
        assert_eq!(Some(shown.event_id), delivered.event_id);
        // RC's own lifecycle is a separate field.
        assert_eq!(summary.state, "running");
    }

    let page = leased
        .client
        .list_sessions(None, &PageRequest::first())
        .await
        .expect("list");
    let last = page.sessions[0].reported_state.as_ref().expect("reported");
    assert!(last.state.is_terminal());

    // An empty state word is malformed, not a new word.
    worker
        .send_raw(RawMessage::Text(
            r#"{"type":"session_state","state":""}"#.into(),
        ))
        .await
        .expect("send");
    let (_, mut observer) = attach(&leased.ingress_url, &leased.device.access_token).await;
    // The observer is replayed six state frames and nothing more.
    for _ in 0..6 {
        next_delivered(&mut observer).await;
    }
    assert!(
        timeout(Duration::from_millis(300), observer.recv())
            .await
            .is_err(),
        "the malformed frame was not stored"
    );
}
