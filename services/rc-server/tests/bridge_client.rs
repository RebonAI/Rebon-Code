//! The HTTP client against the server, over a real socket.
//!
//! `rebon-bridge`'s own tests drive a stub that answers whatever they
//! script. That proves the client builds the requests it thinks it
//! builds; it cannot prove the server agrees. These tests close that
//! gap: a `HttpBridgeApiClient` talking HTTP to the same axum stack an
//! operator runs, through the whole lifecycle — register, poll, ack,
//! heartbeat, permission event, stop, reconnect, archive, deregister —
//! plus the credential paths (refresh-and-retry, a rejected token) and
//! the 204 that a long poll answers with when the queue stays empty.
//!
//! Enabling `rebon-bridge/http` as a dev-dependency here is also what
//! keeps that feature compiled: nothing in the main rebon workspace
//! turns it on.

mod support;

use std::time::{Duration, Instant};

use rebon_bridge::api_client::{BridgeApiClient, BridgeApiError, PollOptions};
use rebon_bridge::config::{PermissionResponseBody, PermissionResponseEvent, WorkDataType};
use rebon_bridge::http_client::{DeviceCredentials, HttpBridgeApiClient, HttpClientConfig};
use rebon_bridge::work_secret::WorkSecret;
use rebon_rc_server::ids;
use support::*;

/// Client config pointed at `server`, with the long poll shortened so a
/// test that expects a 204 does not sit for 25 seconds.
fn settings(server: &Server, poll_wait: Duration) -> HttpClientConfig {
    let mut config = HttpClientConfig::new(server.base.clone());
    config.poll_wait = poll_wait;
    config.poll_timeout_margin = Duration::from_secs(5);
    config.request_timeout = Duration::from_secs(5);
    config
}

fn bridge(server: &Server, device: &Device) -> HttpBridgeApiClient {
    HttpBridgeApiClient::new(
        settings(server, Duration::from_millis(300)),
        DeviceCredentials::new(device.access_token.clone(), device.refresh_token.clone()),
    )
    .expect("client")
}

fn permission_event() -> PermissionResponseEvent {
    PermissionResponseEvent::new(PermissionResponseBody::success(
        "req-e2e",
        serde_json::json!({"behavior": "allow"}),
    ))
}

#[tokio::test]
async fn the_client_drives_a_work_item_through_its_whole_life() {
    let server = server().await;
    let device = bootstrap(&server).await;
    let client = bridge(&server, &device);

    // Register. The response is what fills in the environment secret the
    // credential-free methods further down depend on.
    let registered = client
        .register_bridge_environment(&bridge_config("client-env-e2e"))
        .await
        .expect("register");
    let environment_id = registered.environment_id.clone();
    let secret = registered.environment_secret.clone();
    assert!(ids::valid_id(&environment_id, "env"), "{environment_id}");
    assert_eq!(
        client.environment_secret().as_deref(),
        Some(secret.as_str())
    );

    // An empty queue answers 204, which is `Ok(None)` and not an error.
    assert!(client
        .poll_for_work(&environment_id, &secret, PollOptions::default())
        .await
        .expect("poll an empty queue")
        .is_none());

    // A controller queues work over the route the RFC gives it.
    let (work_id, session_id) = enqueue(
        &server,
        &device.access_token,
        &environment_id,
        "explain the lease state machine",
    )
    .await;

    // The bridge picks it up.
    let work = client
        .poll_for_work(&environment_id, &secret, PollOptions::default())
        .await
        .expect("poll")
        .expect("work is waiting");
    assert_eq!(work.id, work_id);
    assert_eq!(work.response_type, "work");
    assert_eq!(work.environment_id, environment_id);
    assert_eq!(work.state, "leased");
    assert_eq!(work.data.data_type, WorkDataType::Session);
    assert_eq!(work.data.id, session_id);

    // The opaque secret decodes with the shared codec, and carries the
    // session token every session-scoped call below needs.
    let decoded = WorkSecret::decode(&work.secret).expect("work secret decodes");
    assert_eq!(decoded.session_id.as_deref(), Some(session_id.as_str()));
    assert!(
        decoded
            .ingress_url
            .ends_with(&format!("/v1/sessions/{session_id}/stream")),
        "{}",
        decoded.ingress_url
    );
    let session_token = decoded.session_token;

    // Ack, then heartbeat: environment secret on the wire, session token
    // in the body.
    client
        .acknowledge_work(&environment_id, &work_id, &session_token)
        .await
        .expect("ack");
    let outcome = client
        .heartbeat_work(&environment_id, &work_id, &session_token)
        .await
        .expect("heartbeat");
    assert!(outcome.lease_extended);
    // The row's own state — the item was acked a moment ago.
    assert_eq!(outcome.state, "acked");

    // A permission decision goes back under the session token.
    client
        .send_permission_response_event(&session_id, &permission_event(), &session_token)
        .await
        .expect("permission response event");
    let events = server
        .state
        .store()
        .session_events(&session_id)
        .expect("session events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "control_response");
    assert!(
        events[0].payload_json.contains("req-e2e"),
        "{}",
        events[0].payload_json
    );

    // Stop is idempotent and takes `{"force":…}`.
    client
        .stop_work(&environment_id, &work_id, true)
        .await
        .expect("stop");

    // A controller re-queues the session; the bridge sees a *new* item
    // for the same session.
    client
        .reconnect_session(&environment_id, &session_id)
        .await
        .expect("reconnect");
    let requeued = client
        .poll_for_work(&environment_id, &secret, PollOptions::default())
        .await
        .expect("poll")
        .expect("re-queued work");
    assert_ne!(requeued.id, work_id);
    assert_eq!(requeued.data.id, session_id);

    // Archive and deregister carry no credential in the trait, and must
    // use the stored environment secret rather than the device token.
    client.archive_session(&session_id).await.expect("archive");
    client
        .deregister_environment(&environment_id)
        .await
        .expect("deregister");

    // The secret is retired with the environment, so the same client can
    // no longer poll with it.
    let error = client
        .poll_for_work(&environment_id, &secret, PollOptions::default())
        .await
        .expect_err("a retired secret does not poll");
    assert!(
        matches!(error, BridgeApiError::Unauthorized(_)),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_healthcheck_item_arrives_with_a_session_less_secret() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let client = bridge(&server, &device);
    client.set_environment_secret(secret.clone());

    let queued = support::client()
        .post(format!(
            "{}/v1/environments/{environment_id}/work",
            server.base
        ))
        .bearer_auth(&device.access_token)
        .json(&serde_json::json!({"type": "healthcheck"}))
        .send()
        .await
        .expect("enqueue healthcheck");
    assert_eq!(queued.status(), reqwest::StatusCode::CREATED);

    let work = client
        .poll_for_work(&environment_id, &secret, PollOptions::default())
        .await
        .expect("poll")
        .expect("healthcheck work");
    assert_eq!(work.data.data_type, WorkDataType::Healthcheck);
    // `WorkData.id` is the work item itself when there is no session.
    assert_eq!(work.data.id, work.id);

    let decoded = WorkSecret::decode(&work.secret).expect("work secret decodes");
    assert!(decoded.session_id.is_none());
    assert_eq!(decoded.ingress_url, server.base.replace("http://", "ws://"));
}

#[tokio::test]
async fn the_long_poll_parks_until_a_controller_queues_work() {
    // Two seconds of wait against a five-second HTTP timeout: if the
    // client strangled its own poll this would come back as a timeout
    // instead of as work.
    let server = server().await;
    let device = bootstrap(&server).await;
    let client = HttpBridgeApiClient::new(
        settings(&server, Duration::from_secs(2)),
        DeviceCredentials::new(device.access_token.clone(), device.refresh_token.clone()),
    )
    .expect("client");
    let registered = client
        .register_bridge_environment(&bridge_config("client-env-longpoll"))
        .await
        .expect("register");

    let queueing = {
        let base = server.base.clone();
        let access_token = device.access_token.clone();
        let environment_id = registered.environment_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let response = reqwest::Client::new()
                .post(format!("{base}/v1/environments/{environment_id}/work"))
                .bearer_auth(access_token)
                .json(&serde_json::json!({"type": "session", "prompt": "late arrival"}))
                .send()
                .await
                .expect("enqueue");
            assert_eq!(response.status(), reqwest::StatusCode::CREATED);
        })
    };

    let started = Instant::now();
    let work = client
        .poll_for_work(
            &registered.environment_id,
            &registered.environment_secret,
            PollOptions::default(),
        )
        .await
        .expect("poll")
        .expect("the parked poll is woken by the enqueue");
    let elapsed = started.elapsed();
    queueing.await.expect("queueing task");

    assert_eq!(work.data.data_type, WorkDataType::Session);
    assert!(
        elapsed < Duration::from_millis(1_800),
        "the poll waited out its timeout instead of being woken: {elapsed:?}"
    );
}

#[tokio::test]
async fn a_reclaim_hint_takes_back_an_item_another_worker_is_sitting_on() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let client = bridge(&server, &device);
    let (work_id, _) = enqueue(&server, &device.access_token, &environment_id, "hello").await;

    // First poll leases it; a second poll without the hint respects that
    // live lease and times out.
    let first = client
        .poll_for_work(&environment_id, &secret, PollOptions::default())
        .await
        .expect("poll")
        .expect("work");
    assert_eq!(first.id, work_id);
    assert!(client
        .poll_for_work(&environment_id, &secret, PollOptions::default())
        .await
        .expect("poll")
        .is_none());

    // With the hint, an item leased for longer than the client is willing
    // to wait comes back.
    let reclaimed = client
        .poll_for_work(
            &environment_id,
            &secret,
            PollOptions {
                reclaim_older_than_ms: Some(0),
            },
        )
        .await
        .expect("poll")
        .expect("the stale lease is reclaimed");
    assert_eq!(reclaimed.id, work_id);
}

#[tokio::test]
async fn a_stale_access_token_is_refreshed_once_and_the_call_retried() {
    let server = server().await;
    let device = bootstrap(&server).await;
    // Well-formed, but the server has never issued it: the first attempt
    // is a genuine 401 from the real auth path.
    let stale = ids::generate_token();
    let client = HttpBridgeApiClient::new(
        settings(&server, Duration::from_millis(300)),
        DeviceCredentials::new(stale.clone(), device.refresh_token.clone()),
    )
    .expect("client");

    let registered = client
        .register_bridge_environment(&bridge_config("client-env-refresh"))
        .await
        .expect("registration succeeds on the retry");
    assert!(ids::valid_id(&registered.environment_id, "env"));
    assert_ne!(client.access_token(), stale);

    // The refreshed token keeps working for the other device-scoped route.
    let (_, session_id) = enqueue(
        &server,
        &client.access_token(),
        &registered.environment_id,
        "hello",
    )
    .await;
    client
        .reconnect_session(&registered.environment_id, &session_id)
        .await
        .expect("reconnect with the refreshed token");
}

#[tokio::test]
async fn a_credential_the_server_never_issued_stays_unauthorized() {
    let server = server().await;
    let _device = bootstrap(&server).await;
    let client = HttpBridgeApiClient::new(
        settings(&server, Duration::from_millis(300)),
        DeviceCredentials::new(ids::generate_token(), ids::generate_token()),
    )
    .expect("client");

    let error = client
        .register_bridge_environment(&bridge_config("client-env-nope"))
        .await
        .expect_err("rejected");
    assert!(
        matches!(error, BridgeApiError::Unauthorized(_)),
        "{error:?}"
    );
}

#[tokio::test]
async fn the_server_side_failure_modes_land_in_the_right_buckets() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let client = bridge(&server, &device);
    client.set_environment_secret(secret.clone());

    // 404 — a work id that was never minted.
    let unknown = ids::generate_id("wrk");
    let error = client
        .acknowledge_work(&environment_id, &unknown, &ids::generate_token())
        .await
        .expect_err("unknown work");
    assert!(matches!(error, BridgeApiError::Permanent(_)), "{error:?}");

    // 401 — a session token from nowhere.
    let error = client
        .send_permission_response_event(
            "sess_whatever",
            &permission_event(),
            &ids::generate_token(),
        )
        .await
        .expect_err("unknown session token");
    assert!(
        matches!(error, BridgeApiError::Unauthorized(_)),
        "{error:?}"
    );

    // 403 — a genuine environment secret aimed at a different environment.
    let (other_environment, _) = register(
        &server,
        &device.access_token,
        &bridge_config("client-env-other"),
    )
    .await;
    let error = client
        .poll_for_work(&other_environment, &secret, PollOptions::default())
        .await
        .expect_err("cross-environment poll");
    assert!(matches!(error, BridgeApiError::Permanent(_)), "{error:?}");

    // Acking an item that has already been stopped. The README calls this
    // the 409 case, but stopping nulls the item's session-token digest in
    // the same statement, so the caller is no longer the worker holding
    // it and the server answers 401 first. Either way the client must not
    // treat it as retryable.
    let (work_id, _) = enqueue(&server, &device.access_token, &environment_id, "hello").await;
    let work = client
        .poll_for_work(&environment_id, &secret, PollOptions::default())
        .await
        .expect("poll")
        .expect("work");
    let session_token = WorkSecret::decode(&work.secret)
        .expect("work secret")
        .session_token;
    client
        .stop_work(&environment_id, &work_id, false)
        .await
        .expect("stop");
    let error = client
        .acknowledge_work(&environment_id, &work_id, &session_token)
        .await
        .expect_err("acking a stopped item");
    assert!(
        matches!(
            error,
            BridgeApiError::Unauthorized(_) | BridgeApiError::Permanent(_)
        ),
        "{error:?}"
    );
    assert!(!error.is_transient());
}

#[tokio::test]
async fn the_credential_free_methods_refuse_to_run_before_registration() {
    let server = server().await;
    let device = bootstrap(&server).await;
    let client = bridge(&server, &device);

    for error in [
        client.archive_session("sess_1").await.expect_err("archive"),
        client
            .deregister_environment("env_1")
            .await
            .expect_err("deregister"),
    ] {
        assert!(matches!(error, BridgeApiError::Permanent(_)), "{error:?}");
    }
}
