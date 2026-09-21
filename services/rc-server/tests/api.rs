//! End-to-end behaviour of the RFC-0008 surface.

mod support;

use std::time::Duration;

use rebon_rc_server::work_secret::WorkSecret;
use reqwest::StatusCode;
use serde_json::{json, Value};
use support::*;

#[tokio::test]
async fn health_is_anonymous_and_uncacheable() {
    let server = server().await;
    let response = client()
        .get(format!("{}/healthz", server.base))
        .send()
        .await
        .expect("healthz");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    let body: Value = response.json().await.expect("health json");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["service"], "rebon-rc");
}

#[tokio::test]
async fn bootstrap_mints_an_account_and_is_then_spent() {
    let server = server().await;
    let device = bootstrap(&server).await;
    assert!(device.account_id.starts_with("acc_"));
    assert!(device.device_id.starts_with("dev_"));

    // The same token a second time is just an unknown credential.
    let second = client()
        .post(format!("{}/v1/devices", server.base))
        .bearer_auth(&server.bootstrap_token)
        .json(&json!({"label": "second"}))
        .send()
        .await
        .expect("second bootstrap");
    assert_eq!(second.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_device_can_issue_list_and_revoke_devices() {
    let server = server().await;
    let first = bootstrap(&server).await;
    let second = issue_device(&server, &first.access_token, "phone").await;
    assert_eq!(second.account_id, first.account_id);

    let listed: Value = client()
        .get(format!("{}/v1/devices", server.base))
        .bearer_auth(&first.access_token)
        .send()
        .await
        .expect("list devices")
        .json()
        .await
        .expect("device json");
    let devices = listed["devices"].as_array().expect("devices array");
    assert_eq!(devices.len(), 2);
    assert!(devices.iter().any(|device| device["label"] == "phone"));
    assert!(
        devices.iter().all(|device| device["revoked_at"].is_null()),
        "no device is revoked yet"
    );

    let revoked = client()
        .delete(format!("{}/v1/devices/{}", server.base, second.device_id))
        .bearer_auth(&first.access_token)
        .send()
        .await
        .expect("revoke device");
    assert_eq!(revoked.status(), StatusCode::NO_CONTENT);

    // The revoked device's access token no longer authenticates.
    let rejected = client()
        .get(format!("{}/v1/devices", server.base))
        .bearer_auth(&second.access_token)
        .send()
        .await
        .expect("list as revoked device");
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);

    // Issuance and revocation are both audited.
    let actions: Vec<String> = server
        .state
        .store()
        .audit()
        .expect("audit")
        .into_iter()
        .map(|row| row.action)
        .collect();
    assert_eq!(
        actions,
        vec![
            "account.bootstrap",
            "device.issue",
            "device.issue",
            "device.revoke"
        ]
    );
}

#[tokio::test]
async fn a_refresh_token_buys_a_fresh_access_token() {
    let server = server().await;
    let device = bootstrap(&server).await;
    let response = client()
        .post(format!("{}/v1/devices/token", server.base))
        .bearer_auth(&device.refresh_token)
        .send()
        .await
        .expect("refresh");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("token json");
    let refreshed = body["access_token"].as_str().expect("access_token");
    assert_ne!(refreshed, device.access_token);

    // The new token works and the old one is replaced.
    let listed = client()
        .get(format!("{}/v1/devices", server.base))
        .bearer_auth(refreshed)
        .send()
        .await
        .expect("list with refreshed token");
    assert_eq!(listed.status(), StatusCode::OK);
    let stale = client()
        .get(format!("{}/v1/devices", server.base))
        .bearer_auth(&device.access_token)
        .send()
        .await
        .expect("list with stale token");
    assert_eq!(stale.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn registration_is_idempotent_and_rotates_the_secret() {
    let server = server().await;
    let device = bootstrap(&server).await;
    let config = bridge_config("client-env-1");
    let (environment_id, first_secret) = register(&server, &device.access_token, &config).await;

    let (again, second_secret) = register(&server, &device.access_token, &config).await;
    assert_eq!(again, environment_id, "same client id, same environment");
    assert_ne!(first_secret, second_secret, "the secret rotates every time");

    // The old secret is dead the moment the new one is issued.
    let stale = client()
        .get(format!(
            "{}/v1/environments/{environment_id}/work?timeoutMs=0",
            server.base
        ))
        .bearer_auth(&first_secret)
        .send()
        .await
        .expect("poll with the rotated-out secret");
    assert_eq!(stale.status(), StatusCode::UNAUTHORIZED);
    assert!(poll(&server, &environment_id, &second_secret, 0)
        .await
        .is_none());

    // A different client environment id is a different environment.
    let (other, _) = register(
        &server,
        &device.access_token,
        &bridge_config("client-env-2"),
    )
    .await;
    assert_ne!(other, environment_id);
}

#[tokio::test]
async fn listing_environments_shows_metadata_and_never_a_secret() {
    let server = server().await;
    let (device, environment_id, _) = bootstrapped_environment(&server).await;
    let response = client()
        .get(format!("{}/v1/environments", server.base))
        .bearer_auth(&device.access_token)
        .send()
        .await
        .expect("list environments");
    assert_eq!(response.status(), StatusCode::OK);
    let raw = response.text().await.expect("body text");
    assert!(
        !raw.contains("secret"),
        "environment listing must not leak secrets: {raw}"
    );
    let body: Value = serde_json::from_str(&raw).expect("environment json");
    let environments = body["environments"].as_array().expect("environments array");
    assert_eq!(environments.len(), 1);
    let environment = &environments[0];
    assert_eq!(environment["environment_id"], environment_id.as_str());
    assert_eq!(environment["machine_name"], "workshop");
    assert_eq!(environment["branch"], "main");
    assert_eq!(environment["spawn_mode"], "single-session");
    assert_eq!(environment["max_sessions"], 4);
    assert!(environment["last_seen_at"].is_string());
    assert!(environment["deregistered_at"].is_null());
}

#[tokio::test]
async fn a_long_poll_returns_queued_work() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let (work_id, session_id) = enqueue(
        &server,
        &device.access_token,
        &environment_id,
        "ship the thing",
    )
    .await;

    let work = poll(&server, &environment_id, &secret, 1_000)
        .await
        .expect("work is waiting");
    assert_eq!(work["id"], work_id.as_str());
    assert_eq!(work["type"], "work");
    assert_eq!(work["environment_id"], environment_id.as_str());
    assert_eq!(work["state"], "leased");
    assert_eq!(work["data"]["type"], "session");
    assert_eq!(work["data"]["id"], session_id.as_str());
    assert!(work["created_at"]
        .as_str()
        .expect("created_at")
        .ends_with('Z'));

    let secret_blob =
        WorkSecret::decode(work["secret"].as_str().expect("secret")).expect("decodes");
    assert_eq!(secret_blob.session_id.as_deref(), Some(session_id.as_str()));
    assert!(secret_blob
        .ingress_url
        .ends_with(&format!("/v1/sessions/{session_id}/stream")));
    assert_eq!(secret_blob.session_token.len(), 43);
}

#[tokio::test]
async fn an_empty_queue_times_out_with_204() {
    let server = server().await;
    let (_, environment_id, secret) = bootstrapped_environment(&server).await;
    let started = std::time::Instant::now();
    assert!(poll(&server, &environment_id, &secret, 300).await.is_none());
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "the poll must actually wait, not return immediately"
    );
}

#[tokio::test]
async fn a_waiting_poll_wakes_as_soon_as_work_is_enqueued() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;

    let base = server.base.clone();
    let polling_environment = environment_id.clone();
    let poller = tokio::spawn(async move {
        let started = std::time::Instant::now();
        let response = client()
            .get(format!(
                "{base}/v1/environments/{polling_environment}/work?timeoutMs=10000"
            ))
            .bearer_auth(secret)
            .send()
            .await
            .expect("poll");
        (response.status(), started.elapsed())
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    enqueue(&server, &device.access_token, &environment_id, "wake up").await;

    let (status, elapsed) = poller.await.expect("poller task");
    assert_eq!(status, StatusCode::OK);
    assert!(
        elapsed < Duration::from_secs(5),
        "the poll must be woken by the enqueue, not by its timeout ({elapsed:?})"
    );
}

#[tokio::test]
async fn only_one_of_two_pollers_gets_the_item() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;

    let mut pollers = Vec::new();
    for _ in 0..2 {
        let base = server.base.clone();
        let environment = environment_id.clone();
        let secret = secret.clone();
        pollers.push(tokio::spawn(async move {
            client()
                .get(format!(
                    "{base}/v1/environments/{environment}/work?timeoutMs=1500"
                ))
                .bearer_auth(secret)
                .send()
                .await
                .expect("poll")
                .status()
        }));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    enqueue(&server, &device.access_token, &environment_id, "one item").await;

    let mut statuses = Vec::new();
    for poller in pollers {
        statuses.push(poller.await.expect("poller task"));
    }
    statuses.sort_by_key(|status| status.as_u16());
    assert_eq!(
        statuses,
        vec![StatusCode::OK, StatusCode::NO_CONTENT],
        "exactly one poller may claim a single item"
    );
}

#[tokio::test]
async fn ack_heartbeat_and_stop_walk_an_item_through_its_lifecycle() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let (work_id, _) = enqueue(&server, &device.access_token, &environment_id, "run").await;
    let work = poll(&server, &environment_id, &secret, 1_000)
        .await
        .expect("work");
    let session_token = session_token_of(&work);

    let acked = client()
        .post(format!(
            "{}/v1/environments/{environment_id}/work/{work_id}/ack",
            server.base
        ))
        .bearer_auth(&secret)
        .json(&json!({"session_token": session_token}))
        .send()
        .await
        .expect("ack");
    assert_eq!(acked.status(), StatusCode::NO_CONTENT);

    let beat: Value = client()
        .post(format!(
            "{}/v1/environments/{environment_id}/work/{work_id}/heartbeat",
            server.base
        ))
        .bearer_auth(&secret)
        .json(&json!({"session_token": session_token}))
        .send()
        .await
        .expect("heartbeat")
        .json()
        .await
        .expect("heartbeat json");
    assert_eq!(beat["lease_extended"], true);
    // The row's own state, not a synthetic word: the item was acked above.
    assert_eq!(beat["state"], "acked");

    let stopped = client()
        .post(format!(
            "{}/v1/environments/{environment_id}/work/{work_id}/stop",
            server.base
        ))
        .bearer_auth(&secret)
        .json(&json!({"force": true}))
        .send()
        .await
        .expect("stop");
    assert_eq!(stopped.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        server.state.store().work_state(&work_id).expect("state"),
        Some("stopped".to_string())
    );

    // A heartbeat after the stop still answers 200: losing the lease is
    // reported in the body, because that is the signal a worker acts on
    // to stand down without a retry loop.
    let after: serde_json::Value = client()
        .post(format!(
            "{}/v1/environments/{environment_id}/work/{work_id}/heartbeat",
            server.base
        ))
        .bearer_auth(&secret)
        .json(&json!({"session_token": session_token}))
        .send()
        .await
        .expect("heartbeat after stop")
        .json()
        .await
        .expect("heartbeat json");
    assert_eq!(after["lease_extended"], false);
    assert_eq!(after["state"], "stopped");

    // Every other route keeps the worker's genuine token apart from an
    // unknown one: the lease is gone, which is a conflict, not a 401.
    let late_ack = client()
        .post(format!(
            "{}/v1/environments/{environment_id}/work/{work_id}/ack",
            server.base
        ))
        .bearer_auth(&secret)
        .json(&json!({"session_token": session_token}))
        .send()
        .await
        .expect("ack after stop");
    assert_eq!(late_ack.status(), StatusCode::CONFLICT);

    // An invented token on the same item is still a 401, so the two
    // failures stay distinguishable.
    let stranger = client()
        .post(format!(
            "{}/v1/environments/{environment_id}/work/{work_id}/ack",
            server.base
        ))
        .bearer_auth(&secret)
        .json(&json!({"session_token": rebon_rc_server::ids::generate_token()}))
        .send()
        .await
        .expect("ack with a stranger token");
    assert_eq!(stranger.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_controller_can_stop_work_the_bridge_is_holding() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let (work_id, _) = enqueue(&server, &device.access_token, &environment_id, "run").await;
    poll(&server, &environment_id, &secret, 1_000)
        .await
        .expect("work");

    let stopped = client()
        .post(format!(
            "{}/v1/environments/{environment_id}/work/{work_id}/stop",
            server.base
        ))
        .bearer_auth(&device.access_token)
        .json(&json!({"force": false}))
        .send()
        .await
        .expect("stop as controller");
    assert_eq!(stopped.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn an_expired_lease_returns_the_item_to_the_queue() {
    let server = server_with(Options {
        lease_ttl: Duration::from_secs(1),
        ..Options::default()
    })
    .await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let (work_id, _) = enqueue(&server, &device.access_token, &environment_id, "run").await;
    poll(&server, &environment_id, &secret, 1_000)
        .await
        .expect("work");
    assert_eq!(
        server.state.store().work_state(&work_id).expect("state"),
        Some("leased".to_string())
    );

    // Wait out the one-second lease, then run the sweep the 1 Hz task runs.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    server.state.cleanup();
    assert_eq!(
        server.state.store().work_state(&work_id).expect("state"),
        Some("ready".to_string())
    );

    // And it is handed out again, with a fresh session token.
    let again = poll(&server, &environment_id, &secret, 1_000)
        .await
        .expect("re-queued work");
    assert_eq!(again["id"], work_id.as_str());
}

#[tokio::test]
async fn the_reclaim_hint_takes_back_a_still_leased_item() {
    let server = server_with(Options {
        lease_ttl: Duration::from_secs(3_600),
        ..Options::default()
    })
    .await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let (work_id, _) = enqueue(&server, &device.access_token, &environment_id, "run").await;
    poll(&server, &environment_id, &secret, 1_000)
        .await
        .expect("work");

    // With an hour-long lease and no hint the item stays out of reach.
    assert!(poll(&server, &environment_id, &secret, 0).await.is_none());

    // The client says it considers anything leased for more than 0 ms
    // stale, and gets the item back.
    let reclaimed: Value = client()
        .get(format!(
            "{}/v1/environments/{environment_id}/work?timeoutMs=0&reclaimOlderThanMs=0",
            server.base
        ))
        .bearer_auth(&secret)
        .send()
        .await
        .expect("poll with reclaim hint")
        .json()
        .await
        .expect("work json");
    assert_eq!(reclaimed["id"], work_id.as_str());
}

#[tokio::test]
async fn a_session_event_is_persisted_verbatim() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let (_, session_id) = enqueue(&server, &device.access_token, &environment_id, "run").await;
    let work = poll(&server, &environment_id, &secret, 1_000)
        .await
        .expect("work");
    let session_token = session_token_of(&work);

    let event = json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": "req-77",
            "response": {"behavior": "deny", "message": "nope"}
        }
    });
    let posted = client()
        .post(format!("{}/v1/sessions/{session_id}/events", server.base))
        .bearer_auth(&session_token)
        .json(&event)
        .send()
        .await
        .expect("post event");
    assert_eq!(posted.status(), StatusCode::NO_CONTENT);

    let stored = server
        .state
        .store()
        .session_events(&session_id)
        .expect("events");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].kind, "control_response");
    let payload: Value = serde_json::from_str(&stored[0].payload_json).expect("payload json");
    assert_eq!(payload["response"]["request_id"], "req-77");
    assert_eq!(payload["response"]["response"]["behavior"], "deny");
}

#[tokio::test]
async fn archiving_a_session_finishes_its_outstanding_work() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let (work_id, session_id) =
        enqueue(&server, &device.access_token, &environment_id, "run").await;
    poll(&server, &environment_id, &secret, 1_000)
        .await
        .expect("work");

    let archived = client()
        .post(format!("{}/v1/sessions/{session_id}/archive", server.base))
        .bearer_auth(&secret)
        .send()
        .await
        .expect("archive");
    assert_eq!(archived.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        server.state.store().work_state(&work_id).expect("state"),
        Some("done".to_string())
    );
    let (_, _, state) = server
        .state
        .store()
        .session(&session_id)
        .expect("session")
        .expect("session exists");
    assert_eq!(state, "archived");
}

#[tokio::test]
async fn reconnect_supersedes_stale_work_and_requeues_the_session() {
    let server = server_with(Options {
        lease_ttl: Duration::from_secs(3_600),
        ..Options::default()
    })
    .await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let (stale_work_id, session_id) =
        enqueue(&server, &device.access_token, &environment_id, "run").await;
    let work = poll(&server, &environment_id, &secret, 1_000)
        .await
        .expect("work");
    let stale_token = session_token_of(&work);

    let response = client()
        .post(format!(
            "{}/v1/environments/{environment_id}/sessions/{session_id}/reconnect",
            server.base
        ))
        .bearer_auth(&device.access_token)
        .send()
        .await
        .expect("reconnect");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body: Value = response.json().await.expect("reconnect json");
    let fresh_work_id = body["work_id"].as_str().expect("work_id").to_string();
    assert_ne!(fresh_work_id, stale_work_id);

    // The old item is force-stopped and its token is dead.
    assert_eq!(
        server
            .state
            .store()
            .work_state(&stale_work_id)
            .expect("state"),
        Some("stopped".to_string())
    );
    let orphaned = client()
        .post(format!("{}/v1/sessions/{session_id}/events", server.base))
        .bearer_auth(&stale_token)
        .json(&json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": "r", "response": {}}
        }))
        .send()
        .await
        .expect("event with superseded token");
    // The token is genuine; its work item just is not leased any more.
    assert_eq!(orphaned.status(), StatusCode::CONFLICT);

    // The fresh item is waiting for whoever polls next.
    let requeued = poll(&server, &environment_id, &secret, 1_000)
        .await
        .expect("re-queued work");
    assert_eq!(requeued["id"], fresh_work_id.as_str());
    assert_eq!(requeued["data"]["id"], session_id.as_str());
}

#[tokio::test]
async fn deregistering_retires_the_environment_and_its_queue() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let (work_id, _) = enqueue(&server, &device.access_token, &environment_id, "run").await;

    let response = client()
        .delete(format!("{}/v1/environments/{environment_id}", server.base))
        .bearer_auth(&secret)
        .send()
        .await
        .expect("deregister");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        server.state.store().work_state(&work_id).expect("state"),
        Some("stopped".to_string())
    );

    // The secret is retired.
    let after = client()
        .get(format!(
            "{}/v1/environments/{environment_id}/work?timeoutMs=0",
            server.base
        ))
        .bearer_auth(&secret)
        .send()
        .await
        .expect("poll after deregister");
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);

    // It still shows in the listing, marked dead, so a controller can
    // see the machine went away rather than watching it vanish.
    let body: Value = client()
        .get(format!("{}/v1/environments", server.base))
        .bearer_auth(&device.access_token)
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("json");
    assert!(body["environments"][0]["deregistered_at"].is_string());
}

#[tokio::test]
async fn a_body_over_the_limit_is_rejected() {
    let server = server_with(Options {
        max_body_bytes: 4_096,
        ..Options::default()
    })
    .await;
    let device = bootstrap(&server).await;
    let mut config = bridge_config("client-env-1");
    config.dir = "d".repeat(16_384);
    let response = client()
        .post(format!("{}/v1/environments", server.base))
        .bearer_auth(&device.access_token)
        .json(&config)
        .send()
        .await
        .expect("oversized registration");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: Value = response.json().await.expect("error json");
    assert_eq!(body["error"], "request rejected");
}

#[tokio::test]
async fn a_generous_prompt_is_still_accepted() {
    let server = server().await;
    let (device, environment_id, _) = bootstrapped_environment(&server).await;
    // 64 KiB of prompt — far past relay's 1 KiB body limit, which is
    // exactly why RC does not inherit it.
    let prompt = "x".repeat(64 * 1024);
    let (work_id, _) = enqueue(&server, &device.access_token, &environment_id, &prompt).await;
    assert!(work_id.starts_with("wrk_"));
}

#[tokio::test]
async fn state_survives_a_restart() {
    let directory = tempfile::Builder::new()
        .prefix("rebon-rc-test-")
        .tempdir()
        .expect("temp dir");
    let database_path = directory.path().join("rc.sqlite3");
    let hmac_key = [42u8; 32];
    let bootstrap_token = rebon_rc_server::ids::generate_token();

    let (account_id, access_token, environment_id, work_id) = {
        let server = server_with(Options {
            database_path: Some(database_path.clone()),
            hmac_key,
            bootstrap_token: Some(bootstrap_token.clone()),
            ..Options::default()
        })
        .await;
        let (device, environment_id, _) = bootstrapped_environment(&server).await;
        let (work_id, _) = enqueue(
            &server,
            &device.access_token,
            &environment_id,
            "survive this",
        )
        .await;
        (
            device.account_id,
            device.access_token,
            environment_id,
            work_id,
        )
    };

    // Second process, same file and same HMAC key.
    let server = server_with(Options {
        database_path: Some(database_path),
        hmac_key,
        bootstrap_token: Some(bootstrap_token),
        ..Options::default()
    })
    .await;
    let body: Value = client()
        .get(format!("{}/v1/environments", server.base))
        .bearer_auth(&access_token)
        .send()
        .await
        .expect("list after restart")
        .json()
        .await
        .expect("json");
    let environments = body["environments"].as_array().expect("environments");
    assert_eq!(environments.len(), 1);
    assert_eq!(environments[0]["environment_id"], environment_id.as_str());
    assert!(!account_id.is_empty());
    // The queue survived too.
    assert_eq!(
        server.state.store().work_state(&work_id).expect("state"),
        Some("ready".to_string())
    );
}
