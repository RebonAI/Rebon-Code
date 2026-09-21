//! The failure paths, which are as much of the contract as the happy ones.
//!
//! 401 = "I do not know this credential", 403 = "I know it, but not
//! here", 404 = "I know it, and the thing it points at is gone or was
//! never yours". These are exercised route by route because a server
//! that collapses them into one status is a server nobody can debug —
//! and one that collapses them the *other* way leaks which ids exist.

mod support;

use rebon_rc_server::ids;
use reqwest::StatusCode;
use serde_json::json;
use support::*;

#[tokio::test]
async fn no_route_but_health_is_anonymous() {
    let server = server().await;
    let routes = [
        format!("{}/v1/devices", server.base),
        format!("{}/v1/environments", server.base),
    ];
    for route in routes {
        let response = client()
            .get(&route)
            .send()
            .await
            .expect("anonymous request");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{route} must require a credential"
        );
    }
}

#[tokio::test]
async fn a_missing_or_malformed_credential_is_401() {
    let server = server().await;
    let (_, environment_id, _) = bootstrapped_environment(&server).await;
    let route = format!(
        "{}/v1/environments/{environment_id}/work?timeoutMs=0",
        server.base
    );

    let absent = client().get(&route).send().await.expect("no header");
    assert_eq!(absent.status(), StatusCode::UNAUTHORIZED);

    // Right shape, wrong value.
    let unknown = client()
        .get(&route)
        .bearer_auth(ids::generate_token())
        .send()
        .await
        .expect("unknown token");
    assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);

    // Not even the right shape.
    for malformed in ["", "short", "not base64url!!!", &"A".repeat(64)] {
        let response = client()
            .get(&route)
            .bearer_auth(malformed)
            .send()
            .await
            .expect("malformed token");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{malformed:?} must not authenticate"
        );
    }
}

#[tokio::test]
async fn a_credential_of_the_wrong_class_does_not_cross_over() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;

    // An environment secret is not a controller credential.
    let as_controller = client()
        .get(format!("{}/v1/environments", server.base))
        .bearer_auth(&secret)
        .send()
        .await
        .expect("list as environment");
    assert_eq!(as_controller.status(), StatusCode::UNAUTHORIZED);

    // A device access token is not an environment secret: the poll
    // route accepts only the latter.
    let as_bridge = client()
        .get(format!(
            "{}/v1/environments/{environment_id}/work?timeoutMs=0",
            server.base
        ))
        .bearer_auth(&device.access_token)
        .send()
        .await
        .expect("poll as controller");
    assert_eq!(as_bridge.status(), StatusCode::UNAUTHORIZED);

    // A refresh token is not an access token.
    let as_access = client()
        .get(format!("{}/v1/devices", server.base))
        .bearer_auth(&device.refresh_token)
        .send()
        .await
        .expect("list with refresh token");
    assert_eq!(as_access.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_environment_secret_used_on_another_environment_is_403() {
    let server = server().await;
    let device = bootstrap(&server).await;
    let (first_id, first_secret) = register(
        &server,
        &device.access_token,
        &bridge_config("client-env-1"),
    )
    .await;
    let (second_id, _) = register(
        &server,
        &device.access_token,
        &bridge_config("client-env-2"),
    )
    .await;
    assert_ne!(first_id, second_id);

    let crossed = client()
        .get(format!(
            "{}/v1/environments/{second_id}/work?timeoutMs=0",
            server.base
        ))
        .bearer_auth(&first_secret)
        .send()
        .await
        .expect("poll another environment");
    assert_eq!(
        crossed.status(),
        StatusCode::FORBIDDEN,
        "a genuine secret outside its scope is 403, not 401"
    );

    let deregistered = client()
        .delete(format!("{}/v1/environments/{second_id}", server.base))
        .bearer_auth(&first_secret)
        .send()
        .await
        .expect("deregister another environment");
    assert_eq!(deregistered.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_session_token_used_on_another_session_is_403() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let (_, first_session) = enqueue(&server, &device.access_token, &environment_id, "one").await;
    let (_, second_session) = enqueue(&server, &device.access_token, &environment_id, "two").await;
    let work = poll(&server, &environment_id, &secret, 1_000)
        .await
        .expect("work");
    let token = session_token_of(&work);
    assert_eq!(work["data"]["id"], first_session.as_str());

    let crossed = client()
        .post(format!(
            "{}/v1/sessions/{second_session}/events",
            server.base
        ))
        .bearer_auth(&token)
        .json(&json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": "r", "response": {}}
        }))
        .send()
        .await
        .expect("event on another session");
    assert_eq!(crossed.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_deregistered_environment_is_404_to_its_own_device() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    client()
        .delete(format!("{}/v1/environments/{environment_id}", server.base))
        .bearer_auth(&secret)
        .send()
        .await
        .expect("deregister");

    // The controller still holds a valid credential; the environment is
    // simply no longer there to queue work on.
    let response = client()
        .post(format!(
            "{}/v1/environments/{environment_id}/work",
            server.base
        ))
        .bearer_auth(&device.access_token)
        .json(&json!({"type": "session", "prompt": "hello"}))
        .send()
        .await
        .expect("enqueue on a dead environment");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn revoking_a_device_kills_its_environment_secret() {
    let server = server().await;
    let first = bootstrap(&server).await;
    let second = issue_device(&server, &first.access_token, "laptop").await;
    let (environment_id, secret) = register(
        &server,
        &second.access_token,
        &bridge_config("client-env-2"),
    )
    .await;
    // The secret works while its device is live.
    assert!(poll(&server, &environment_id, &secret, 0).await.is_none());

    client()
        .delete(format!("{}/v1/devices/{}", server.base, second.device_id))
        .bearer_auth(&first.access_token)
        .send()
        .await
        .expect("revoke");

    // The secret is still genuine, so this is not 401 — the environment
    // behind it is disabled, which is 404.
    let response = client()
        .get(format!(
            "{}/v1/environments/{environment_id}/work?timeoutMs=0",
            server.base
        ))
        .bearer_auth(&secret)
        .send()
        .await
        .expect("poll with a cascaded-dead secret");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_revoked_devices_refresh_token_is_404() {
    let server = server().await;
    let first = bootstrap(&server).await;
    let second = issue_device(&server, &first.access_token, "laptop").await;
    client()
        .delete(format!("{}/v1/devices/{}", server.base, second.device_id))
        .bearer_auth(&first.access_token)
        .send()
        .await
        .expect("revoke");

    // Revocation clears the access token outright — that credential is
    // simply unknown afterwards.
    let access = client()
        .get(format!("{}/v1/devices", server.base))
        .bearer_auth(&second.access_token)
        .send()
        .await
        .expect("list with revoked access token");
    assert_eq!(access.status(), StatusCode::UNAUTHORIZED);

    // The refresh token still resolves, to a device that is switched off.
    let refresh = client()
        .post(format!("{}/v1/devices/token", server.base))
        .bearer_auth(&second.refresh_token)
        .send()
        .await
        .expect("refresh as revoked device");
    assert_eq!(refresh.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_credential_from_another_server_is_never_honoured() {
    let server = server().await;
    let (_, environment_id, _) = bootstrapped_environment(&server).await;

    // There is exactly one account per server (there is no login
    // yet), so cross-*account* isolation is covered at the store layer.
    // What is reachable over HTTP is the neighbouring case: a perfectly
    // valid credential minted somewhere else.
    let stranger = server_with(Options::default()).await;
    let outsider = bootstrap(&stranger).await;

    let response = client()
        .post(format!(
            "{}/v1/environments/{environment_id}/work",
            server.base
        ))
        .bearer_auth(&outsider.access_token)
        .json(&json!({"type": "session", "prompt": "hello"}))
        .send()
        .await
        .expect("enqueue as an outsider");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_ids_are_404_and_never_a_wildcard() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;

    let unknown_device = client()
        .delete(format!(
            "{}/v1/devices/{}",
            server.base,
            ids::generate_id("dev")
        ))
        .bearer_auth(&device.access_token)
        .send()
        .await
        .expect("revoke unknown device");
    assert_eq!(unknown_device.status(), StatusCode::NOT_FOUND);

    let malformed_device = client()
        .delete(format!("{}/v1/devices/not-an-id", server.base))
        .bearer_auth(&device.access_token)
        .send()
        .await
        .expect("revoke malformed device id");
    assert_eq!(malformed_device.status(), StatusCode::NOT_FOUND);

    let unknown_work = client()
        .post(format!(
            "{}/v1/environments/{environment_id}/work/{}/stop",
            server.base,
            ids::generate_id("wrk")
        ))
        .bearer_auth(&secret)
        .json(&json!({"force": false}))
        .send()
        .await
        .expect("stop unknown work");
    assert_eq!(unknown_work.status(), StatusCode::NOT_FOUND);

    let unknown_session = client()
        .post(format!(
            "{}/v1/environments/{environment_id}/sessions/{}/reconnect",
            server.base,
            ids::generate_id("sess")
        ))
        .bearer_auth(&device.access_token)
        .send()
        .await
        .expect("reconnect unknown session");
    assert_eq!(unknown_session.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_work_item_from_another_environment_is_not_reachable() {
    let server = server().await;
    let device = bootstrap(&server).await;
    let (first_id, first_secret) = register(
        &server,
        &device.access_token,
        &bridge_config("client-env-1"),
    )
    .await;
    let (second_id, _) = register(
        &server,
        &device.access_token,
        &bridge_config("client-env-2"),
    )
    .await;
    let (work_id, _) = enqueue(&server, &device.access_token, &second_id, "elsewhere").await;

    let response = client()
        .post(format!(
            "{}/v1/environments/{first_id}/work/{work_id}/stop",
            server.base
        ))
        .bearer_auth(&first_secret)
        .json(&json!({"force": false}))
        .send()
        .await
        .expect("stop a sibling environment's work");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ack_with_the_wrong_session_token_is_401() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let (work_id, _) = enqueue(&server, &device.access_token, &environment_id, "run").await;
    poll(&server, &environment_id, &secret, 1_000)
        .await
        .expect("work");

    let response = client()
        .post(format!(
            "{}/v1/environments/{environment_id}/work/{work_id}/ack",
            server.base
        ))
        .bearer_auth(&secret)
        .json(&json!({"session_token": ids::generate_token()}))
        .send()
        .await
        .expect("ack with a foreign token");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn malformed_bodies_are_rejected_before_anything_is_written() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let route = format!("{}/v1/environments/{environment_id}/work", server.base);

    let cases = [
        json!({"type": "session"}),                         // no prompt
        json!({"type": "session", "prompt": ""}),           // empty prompt
        json!({"type": "healthcheck", "prompt": "no"}),     // prompt on a probe
        json!({"type": "nonsense", "prompt": "hi"}),        // unknown type
        json!({"prompt": "hi"}),                            // no type
        json!({"type": "session", "prompt": "hi", "x": 1}), // unknown field
        json!({"type": "session", "prompt": "hi", "session_id": "bogus"}),
    ];
    for case in cases {
        let response = client()
            .post(&route)
            .bearer_auth(&device.access_token)
            .json(&case)
            .send()
            .await
            .expect("enqueue");
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{case} must be rejected"
        );
    }
    assert!(
        poll(&server, &environment_id, &secret, 0).await.is_none(),
        "a rejected body must not leave a work item behind"
    );
}

#[tokio::test]
async fn the_error_body_is_identical_whatever_went_wrong() {
    let server = server().await;
    let (_, environment_id, _) = bootstrapped_environment(&server).await;
    let mut bodies = Vec::new();
    for (route, token) in [
        (format!("{}/v1/devices", server.base), ids::generate_token()),
        (
            format!(
                "{}/v1/environments/{environment_id}/work?timeoutMs=0",
                server.base
            ),
            ids::generate_token(),
        ),
        (
            format!("{}/v1/nonexistent", server.base),
            ids::generate_token(),
        ),
    ] {
        let response = client()
            .get(&route)
            .bearer_auth(token)
            .send()
            .await
            .expect("request");
        assert!(response.status().is_client_error());
        assert_eq!(
            response
                .headers()
                .get("cache-control")
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        bodies.push(response.text().await.expect("body"));
    }
    assert!(
        bodies.windows(2).all(|pair| pair[0] == pair[1]),
        "error bodies must not distinguish failures: {bodies:?}"
    );
}
