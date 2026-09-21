//! Pins the wire contract RC shares with `rebon-bridge`.
//!
//! RFC-0008 §4 makes `rebon-bridge` the single definition of the
//! protocol and says a type change must break the server build. That
//! catches renames in Rust, but not a serde attribute change — dropping
//! `#[serde(rename = "type")]` or flipping a struct to camelCase would
//! still compile here while silently breaking every deployed bridge.
//!
//! So these tests assert the JSON *keys and values* on both sides: the
//! structs as `rebon-bridge` serializes them, and the same shapes as
//! they come off a live RC server. A drift in either crate fails here.

mod support;

use rebon_bridge::config::{
    HeartbeatOutcome, PermissionResponseBody, PermissionResponseEvent, RegisteredEnvironment,
    WorkData, WorkDataType, WorkItem, WorkResponse,
};
use rebon_bridge::history::{SessionEventPage, SessionPage};
use serde_json::{json, Value};
use support::*;

fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .expect("a JSON object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

#[test]
fn work_response_keeps_its_snake_case_shape() {
    let value = serde_json::to_value(WorkResponse {
        id: "wrk_1".into(),
        response_type: "work".into(),
        environment_id: "env_1".into(),
        state: "leased".into(),
        data: WorkData {
            data_type: WorkDataType::Session,
            id: "sess_1".into(),
        },
        secret: "opaque".into(),
        created_at: "2026-09-16T00:00:00.000Z".into(),
    })
    .expect("serialize");

    assert_eq!(
        keys(&value),
        vec![
            "created_at",
            "data",
            "environment_id",
            "id",
            "secret",
            "state",
            "type"
        ]
    );
    assert_eq!(value["type"], "work");
    assert_eq!(keys(&value["data"]), vec!["id", "type"]);
    assert_eq!(value["data"]["type"], "session");

    let healthcheck = serde_json::to_value(WorkData {
        data_type: WorkDataType::Healthcheck,
        id: "wrk_2".into(),
    })
    .expect("serialize");
    assert_eq!(healthcheck["type"], "healthcheck");
}

#[test]
fn registered_environment_keeps_its_snake_case_shape() {
    let value = serde_json::to_value(RegisteredEnvironment {
        environment_id: "env_1".into(),
        environment_secret: "shhh".into(),
    })
    .expect("serialize");
    assert_eq!(keys(&value), vec!["environment_id", "environment_secret"]);
}

#[test]
fn heartbeat_outcome_keeps_its_snake_case_shape() {
    let value = serde_json::to_value(HeartbeatOutcome {
        lease_extended: true,
        state: "running".into(),
    })
    .expect("serialize");
    assert_eq!(keys(&value), vec!["lease_extended", "state"]);
    assert_eq!(value["lease_extended"], true);
}

#[test]
fn permission_response_event_keeps_its_nested_shape() {
    let value = serde_json::to_value(PermissionResponseEvent::new(
        PermissionResponseBody::success("req-1", json!({"behavior": "allow"})),
    ))
    .expect("serialize");
    assert_eq!(keys(&value), vec!["response", "type"]);
    assert_eq!(value["type"], "control_response");
    assert_eq!(
        keys(&value["response"]),
        vec!["request_id", "response", "subtype"]
    );
    assert_eq!(value["response"]["subtype"], "success");
}

#[tokio::test]
async fn the_server_emits_exactly_those_shapes() {
    let server = server().await;
    let device = bootstrap(&server).await;

    // Registration response ≡ RegisteredEnvironment.
    let registration: Value = client()
        .post(format!("{}/v1/environments", server.base))
        .bearer_auth(&device.access_token)
        .json(&bridge_config("client-env-1"))
        .send()
        .await
        .expect("register")
        .json()
        .await
        .expect("json");
    assert_eq!(
        keys(&registration),
        vec!["environment_id", "environment_secret"]
    );
    let environment_id = registration["environment_id"].as_str().expect("id");
    let secret = registration["environment_secret"].as_str().expect("secret");
    // It parses back into the bridge's own type.
    let typed: RegisteredEnvironment =
        serde_json::from_value(registration.clone()).expect("RegisteredEnvironment");
    assert_eq!(typed.environment_id, environment_id);

    // Poll response ≡ WorkItem: the WorkResponse envelope plus `session`.
    let (work_id, session_id) =
        enqueue(&server, &device.access_token, environment_id, "hello").await;
    let work = poll(&server, environment_id, secret, 1_000)
        .await
        .expect("work");
    assert_eq!(
        keys(&work),
        vec![
            "created_at",
            "data",
            "environment_id",
            "id",
            "secret",
            "session",
            "state",
            "type"
        ]
    );
    assert_eq!(keys(&work["data"]), vec!["id", "type"]);
    assert_eq!(
        work["session"],
        json!({"project": "/home/user/repo", "prompt": "hello"})
    );
    let item: WorkItem = serde_json::from_value(work.clone()).expect("WorkItem");
    assert_eq!(item.session.expect("session").project, "/home/user/repo");
    let typed: WorkResponse = serde_json::from_value(work.clone()).expect("WorkResponse");
    assert_eq!(typed.id, work_id);
    assert_eq!(typed.response_type, "work");
    assert_eq!(typed.data.data_type, WorkDataType::Session);
    assert_eq!(typed.data.id, session_id);

    // Heartbeat response ≡ HeartbeatOutcome.
    let session_token = session_token_of(&work);
    let beat: Value = client()
        .post(format!(
            "{}/v1/environments/{environment_id}/work/{work_id}/heartbeat",
            server.base
        ))
        .bearer_auth(secret)
        .json(&json!({"session_token": session_token}))
        .send()
        .await
        .expect("heartbeat")
        .json()
        .await
        .expect("json");
    assert_eq!(keys(&beat), vec!["lease_extended", "state"]);
    let typed: HeartbeatOutcome = serde_json::from_value(beat).expect("HeartbeatOutcome");
    assert!(typed.lease_extended);

    // The request side too: a PermissionResponseEvent serialized by the
    // bridge's own type is accepted verbatim.
    let event = PermissionResponseEvent::new(PermissionResponseBody::success(
        "req-9",
        json!({"behavior": "allow"}),
    ));
    let accepted = client()
        .post(format!("{}/v1/sessions/{session_id}/events", server.base))
        .bearer_auth(&session_token)
        .json(&event)
        .send()
        .await
        .expect("post event");
    assert_eq!(accepted.status(), reqwest::StatusCode::NO_CONTENT);

    // And a BridgeConfig serialized by the bridge's own type registers.
    let stored = server
        .state
        .store()
        .list_environments(&device.account_id)
        .expect("list");
    assert_eq!(stored[0].client_environment_id, "client-env-1");
    assert_eq!(stored[0].spawn_mode, "single-session");
}

#[tokio::test]
async fn the_history_routes_emit_the_bridge_history_shapes() {
    let server = server().await;
    let (device, environment_id, _) = bootstrapped_environment(&server).await;
    let (_, session_id) = enqueue(&server, &device.access_token, &environment_id, "hi").await;
    let now = rebon_rc_server::ids::now_unix();
    for _ in 0..3 {
        server
            .state
            .store()
            .record_session_event(
                &session_id,
                "session_state",
                r#"{"type":"session_state","state":"idle"}"#,
                now,
            )
            .expect("record");
    }
    let get = |path: String| {
        let token = device.access_token.clone();
        async move {
            client()
                .get(path)
                .bearer_auth(token)
                .send()
                .await
                .expect("get")
                .json::<Value>()
                .await
                .expect("json")
        }
    };

    // Event page ≡ SessionEventPage.
    let page = get(format!(
        "{}/v1/sessions/{session_id}/events?limit=2",
        server.base
    ))
    .await;
    assert_eq!(keys(&page), vec!["events", "next_cursor"]);
    assert_eq!(
        keys(&page["events"][0]),
        vec!["created_at", "event_id", "kind", "payload"]
    );
    assert!(page["next_cursor"].is_string());
    let typed: SessionEventPage = serde_json::from_value(page).expect("SessionEventPage");
    assert_eq!(typed.events.len(), 2);

    // Session page ≡ SessionPage.
    let list = get(format!("{}/v1/sessions", server.base)).await;
    assert_eq!(keys(&list), vec!["sessions"]);
    assert_eq!(
        keys(&list["sessions"][0]),
        vec![
            "created_at",
            "environment_id",
            "last_activity_at",
            "last_event_at",
            "last_event_id",
            "reported_state",
            "session_id",
            "state"
        ]
    );
    assert_eq!(
        keys(&list["sessions"][0]["reported_state"]),
        vec!["event_id", "state"]
    );
    let typed: SessionPage = serde_json::from_value(list).expect("SessionPage");
    assert_eq!(typed.sessions[0].session_id, session_id);
}
