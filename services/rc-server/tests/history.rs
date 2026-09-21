//! The paged read routes end to end: the real server, read through the
//! real `rebon-bridge` HTTP client (`session_events`, `list_sessions`),
//! with the session stream's real client as the writer where it matters.
//!
//! Most tests seed history through `Store::record_session_event`, which
//! takes an explicit clock: the list's order is by a one-second activity
//! time, and a test that had to sleep for it would be slow and flaky.
//! The write-through test is the exception — there the whole point is
//! that the worker streamed with nobody attached.

mod support;

use std::path::Path;
use std::time::Duration;

use rebon_bridge::api_client::{BridgeApiClient, BridgeApiError};
use rebon_bridge::history::{PageRequest, SessionEventPage, SessionEventRecord, SessionPage};
use rebon_bridge::http_client::{DeviceCredentials, HttpBridgeApiClient, HttpClientConfig};
use rebon_bridge::session_stream::{SessionFrame, SessionRunState};
use rebon_bridge::stream_client::{SessionStream, SessionStreamOptions};
use rebon_bridge::work_secret::WorkSecret;
use rebon_rc_server::{auth, ids};
use reqwest::StatusCode;
use serde_json::{json, Value};
use support::*;

// ─── Fixtures ─────────────────────────────────────────────────────────

fn controller(server: &Server, access_token: &str) -> HttpBridgeApiClient {
    let mut config = HttpClientConfig::new(server.base.clone());
    config.request_timeout = Duration::from_secs(10);
    HttpBridgeApiClient::new(config, DeviceCredentials::access_only(access_token)).expect("client")
}

/// Persist `count` numbered message frames, all at `at`.
fn seed(server: &Server, session_id: &str, count: usize, at: i64) -> Vec<i64> {
    (0..count)
        .map(|seq| {
            let payload = json!({"type": "session_message", "message": {"seq": seq}});
            server
                .state
                .store()
                .record_session_event(session_id, "session_message", &payload.to_string(), at)
                .expect("record event")
        })
        .collect()
}

fn seq_of(record: &SessionEventRecord) -> u64 {
    match record.frame().expect("stored frame parses") {
        SessionFrame::SessionMessage { message, .. } => message["seq"].as_u64().expect("seq"),
        other => panic!("expected a session message, got {other:?}"),
    }
}

fn seqs(page: &SessionEventPage) -> Vec<u64> {
    page.events.iter().map(seq_of).collect()
}

/// Walk a session's history to its start, returning every page.
async fn walk(client: &HttpBridgeApiClient, session_id: &str, limit: u32) -> Vec<SessionEventPage> {
    let mut pages = Vec::new();
    let mut request = PageRequest::first().with_limit(limit);
    loop {
        let page = client
            .session_events(session_id, &request)
            .await
            .expect("page");
        let next = page.next_cursor.clone();
        pages.push(page);
        match next {
            Some(cursor) => request = PageRequest::after(cursor).with_limit(limit),
            None => return pages,
        }
        assert!(pages.len() < 1_000, "the walk does not terminate");
    }
}

async fn list_all(
    client: &HttpBridgeApiClient,
    environment_id: Option<&str>,
    limit: u32,
) -> Vec<SessionPage> {
    let mut pages = Vec::new();
    let mut request = PageRequest::first().with_limit(limit);
    loop {
        let page = client
            .list_sessions(environment_id, &request)
            .await
            .expect("page");
        let next = page.next_cursor.clone();
        pages.push(page);
        match next {
            Some(cursor) => request = PageRequest::after(cursor).with_limit(limit),
            None => return pages,
        }
        assert!(pages.len() < 1_000, "the walk does not terminate");
    }
}

fn session_ids(pages: &[SessionPage]) -> Vec<String> {
    pages
        .iter()
        .flat_map(|page| page.sessions.iter().map(|s| s.session_id.clone()))
        .collect()
}

/// A raw GET, for the requests the client would never build.
async fn raw_get(server: &Server, path_and_query: &str, token: Option<&str>) -> (u16, Value) {
    let mut request = client().get(format!("{}{path_and_query}", server.base));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.expect("request");
    let status = response.status().as_u16();
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store"),
        "{path_and_query} → {status}"
    );
    let body = response.json().await.unwrap_or(Value::Null);
    (status, body)
}

fn permanent_detail(error: &BridgeApiError) -> &str {
    match error {
        BridgeApiError::Permanent(detail) => detail,
        other => panic!("expected a permanent failure, got {other:?}"),
    }
}

/// See `tests/session_stream.rs`: RC holds one account until OIDC, so a
/// second is written straight into the database and its device issued
/// through the ordinary store path.
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

fn on_disk() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().expect("tempdir");
    let database = directory.path().join("rc.sqlite3");
    (directory, database)
}

// ─── Event paging ─────────────────────────────────────────────────────

#[tokio::test]
async fn a_long_session_pages_back_to_its_start() {
    let server = server().await;
    let (device, environment_id, _) = bootstrapped_environment(&server).await;
    let (_, session_id) = enqueue(&server, &device.access_token, &environment_id, "long").await;
    seed(&server, &session_id, 23, ids::now_unix());
    let client = controller(&server, &device.access_token);

    // First page: the newest events, listed oldest first.
    let first = client
        .session_events(&session_id, &PageRequest::first().with_limit(10))
        .await
        .expect("first page");
    assert_eq!(seqs(&first), (13..23).collect::<Vec<_>>());
    assert!(first.next_cursor.is_some());
    let ids: Vec<u64> = first.events.iter().map(|event| event.event_id).collect();
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "{ids:?}");
    assert_eq!(first.events[0].kind, "session_message");
    assert!(first.events[0].created_at.ends_with('Z'));

    // Next pages, then the end: the last page is short and has no cursor.
    let pages = walk(&client, &session_id, 10).await;
    let per_page: Vec<Vec<u64>> = pages.iter().map(seqs).collect();
    assert_eq!(
        per_page,
        vec![
            (13..23).collect::<Vec<_>>(),
            (3..13).collect(),
            (0..3).collect()
        ]
    );
    assert!(pages[2].next_cursor.is_none());

    // Prepending pages in the order they arrive rebuilds the history.
    let mut rebuilt: Vec<u64> = Vec::new();
    for page in &pages {
        let mut older = seqs(page);
        older.extend(rebuilt);
        rebuilt = older;
    }
    assert_eq!(rebuilt, (0..23).collect::<Vec<_>>());

    // A history that divides evenly ends without an empty trailing page:
    // the page that reaches the start already says so.
    let even = walk(&client, &session_id, 23).await;
    assert_eq!(even.len(), 1);
    assert!(even[0].next_cursor.is_none());
    let exact = client
        .session_events(
            &session_id,
            &PageRequest::after(pages[0].next_cursor.clone().expect("cursor")).with_limit(13),
        )
        .await
        .expect("exact remainder");
    assert_eq!(exact.events.len(), 13);
    assert!(exact.next_cursor.is_none());
}

#[tokio::test]
async fn a_session_without_events_is_one_empty_page() {
    let server = server().await;
    let (device, environment_id, _) = bootstrapped_environment(&server).await;
    let (_, session_id) = enqueue(&server, &device.access_token, &environment_id, "quiet").await;
    let page = controller(&server, &device.access_token)
        .session_events(&session_id, &PageRequest::first())
        .await
        .expect("page");
    assert!(page.events.is_empty());
    assert!(page.next_cursor.is_none());
    // `next_cursor` is absent, not null.
    let (status, body) = raw_get(
        &server,
        &format!("/v1/sessions/{session_id}/events"),
        Some(&device.access_token),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({"events": []}));
}

#[tokio::test]
async fn a_gap_in_ids_does_not_break_paging() {
    let (_directory, database) = on_disk();
    let server = server_with(Options {
        database_path: Some(database.clone()),
        ..Options::default()
    })
    .await;
    let (device, environment_id, _) = bootstrapped_environment(&server).await;
    let (_, session_id) = enqueue(&server, &device.access_token, &environment_id, "gaps").await;
    let ids = seed(&server, &session_id, 12, ids::now_unix());
    let client = controller(&server, &device.access_token);

    let first = client
        .session_events(&session_id, &PageRequest::first().with_limit(4))
        .await
        .expect("first page");
    assert_eq!(seqs(&first), vec![8, 9, 10, 11]);
    let cursor = first.next_cursor.clone().expect("cursor");

    // Retention runs between two page reads: it removes the very row the
    // cursor was issued at, the rows just below it, and one further down.
    rusqlite::Connection::open(&database)
        .expect("open database")
        .execute(
            "DELETE FROM session_events WHERE event_id IN (?1, ?2, ?3, ?4)",
            [ids[8], ids[7], ids[6], ids[2]],
        )
        .expect("delete rows");

    let second = client
        .session_events(&session_id, &PageRequest::after(cursor).with_limit(4))
        .await
        .expect("second page");
    assert_eq!(seqs(&second), vec![1, 3, 4, 5]);
    let third = client
        .session_events(
            &session_id,
            &PageRequest::after(second.next_cursor.clone().expect("cursor")).with_limit(4),
        )
        .await
        .expect("third page");
    assert_eq!(seqs(&third), vec![0]);
    assert!(third.next_cursor.is_none());

    // A fresh walk over the gappy history sees exactly the survivors.
    let all: Vec<u64> = walk(&client, &session_id, 3)
        .await
        .iter()
        .rev()
        .flat_map(seqs)
        .collect();
    assert_eq!(all, vec![0, 1, 3, 4, 5, 9, 10, 11]);
}

#[tokio::test]
async fn the_page_size_is_defaulted_clamped_and_validated() {
    let server = server_with(Options {
        page_limit_default: 3,
        page_limit_max: 5,
        ..Options::default()
    })
    .await;
    let (device, environment_id, _) = bootstrapped_environment(&server).await;
    let (_, session_id) = enqueue(&server, &device.access_token, &environment_id, "sizes").await;
    let (_, other_session) = enqueue(&server, &device.access_token, &environment_id, "x").await;
    seed(&server, &session_id, 12, ids::now_unix());
    let client = controller(&server, &device.access_token);

    let defaulted = client
        .session_events(&session_id, &PageRequest::first())
        .await
        .expect("default");
    assert_eq!(defaulted.events.len(), 3);
    let clamped = client
        .session_events(&session_id, &PageRequest::first().with_limit(1_000))
        .await
        .expect("clamped");
    assert_eq!(clamped.events.len(), 5);
    assert!(clamped.next_cursor.is_some());
    let listed = client
        .list_sessions(None, &PageRequest::first().with_limit(u32::MAX))
        .await
        .expect("clamped list");
    assert_eq!(listed.sessions.len(), 2);

    let token = Some(device.access_token.as_str());
    let base = format!("/v1/sessions/{session_id}/events");
    // An empty value is "absent", as the RFC's `?cursor=&limit=` spells it.
    let (status, body) = raw_get(&server, &format!("{base}?cursor=&limit="), token).await;
    assert_eq!(status, 200);
    assert_eq!(body["events"].as_array().expect("events").len(), 3);
    // A limit too large for any integer is still just "a lot".
    let (status, body) = raw_get(
        &server,
        &format!("{base}?limit=99999999999999999999999"),
        token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["events"].as_array().expect("events").len(), 5);

    let other_cursor = client
        .session_events(&other_session, &PageRequest::first())
        .await
        .expect("empty")
        .next_cursor;
    assert!(other_cursor.is_none());
    let foreign_cursor = defaulted.next_cursor.clone().expect("cursor");
    let session_cursor = listed.next_cursor.clone();
    assert!(session_cursor.is_none());

    for query in [
        "?limit=0".to_string(),
        "?limit=-1".to_string(),
        "?limit=%2B3".to_string(),
        "?limit=three".to_string(),
        "?limit=1.5".to_string(),
        "?page=2".to_string(),
        "?cursor=42".to_string(),
        "?cursor=not%20a%20cursor".to_string(),
        format!("?cursor={}", "A".repeat(600)),
    ] {
        let (status, body) = raw_get(&server, &format!("{base}{query}"), token).await;
        assert_eq!(status, 400, "{query}");
        assert_eq!(body, json!({"error": "request rejected"}), "{query}");
    }
    // A genuine cursor from one session does not open another.
    let (status, _) = raw_get(
        &server,
        &format!("/v1/sessions/{other_session}/events?cursor={foreign_cursor}"),
        token,
    )
    .await;
    assert_eq!(status, 400);
    // ... and the client reports such a refusal as permanent.
    let error = client
        .session_events(&other_session, &PageRequest::after(foreign_cursor.clone()))
        .await
        .expect_err("refused");
    assert!(permanent_detail(&error).contains("400"), "{error:?}");
    // An event cursor is not a session-list cursor.
    let (status, _) = raw_get(
        &server,
        &format!("/v1/sessions?cursor={foreign_cursor}"),
        token,
    )
    .await;
    assert_eq!(status, 400);
    let (status, _) = raw_get(&server, "/v1/sessions?limit=0", token).await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn a_large_history_page_stops_at_the_byte_budget() {
    let server = server_with(Options {
        page_limit_max: 500,
        ..Options::default()
    })
    .await;
    let (device, environment_id, _) = bootstrapped_environment(&server).await;
    let (_, session_id) = enqueue(&server, &device.access_token, &environment_id, "big").await;
    // 40 frames of ~200 KiB: 8 MiB in all, twice the page budget.
    let filler = "x".repeat(200 * 1024);
    let now = ids::now_unix();
    for seq in 0..40 {
        let payload = json!({"type": "session_message", "message": {"seq": seq, "filler": filler}});
        server
            .state
            .store()
            .record_session_event(&session_id, "session_message", &payload.to_string(), now)
            .expect("record");
    }
    let client = controller(&server, &device.access_token);
    let pages = walk(&client, &session_id, 500).await;
    assert!(pages.len() >= 2, "{} pages", pages.len());
    for page in &pages {
        assert!(!page.events.is_empty());
        let bytes: usize = page
            .events
            .iter()
            .map(|event| event.payload.to_string().len())
            .sum();
        assert!(bytes <= rebon_rc_server::MAX_EVENT_PAGE_BYTES, "{bytes}");
    }
    let all: Vec<u64> = pages.iter().rev().flat_map(seqs).collect();
    assert_eq!(all, (0..40).collect::<Vec<_>>());
}

// ─── Scoping ──────────────────────────────────────────────────────────

#[tokio::test]
async fn another_account_is_told_nothing() {
    let (_directory, database) = on_disk();
    let server = server_with(Options {
        database_path: Some(database.clone()),
        ..Options::default()
    })
    .await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let (_, session_id) = enqueue(&server, &device.access_token, &environment_id, "mine").await;
    seed(&server, &session_id, 5, ids::now_unix());
    let owner_cursor = controller(&server, &device.access_token)
        .session_events(&session_id, &PageRequest::first().with_limit(2))
        .await
        .expect("owner page")
        .next_cursor
        .expect("cursor");
    let intruder = device_on_another_account(&server, &database);
    let unknown = ids::generate_id("sess");

    // Someone else's session and a nonexistent one: the same answer,
    // byte for byte, with or without a cursor.
    let foreign = raw_get(
        &server,
        &format!("/v1/sessions/{session_id}/events"),
        Some(&intruder),
    )
    .await;
    let missing = raw_get(
        &server,
        &format!("/v1/sessions/{unknown}/events"),
        Some(&intruder),
    )
    .await;
    assert_eq!(foreign.0, 404);
    assert_eq!(foreign, missing);
    let with_cursor = raw_get(
        &server,
        &format!("/v1/sessions/{session_id}/events?cursor={owner_cursor}&limit=0"),
        Some(&intruder),
    )
    .await;
    assert_eq!(
        with_cursor, foreign,
        "the scope check comes before the query"
    );
    let owner_missing = raw_get(
        &server,
        &format!("/v1/sessions/{unknown}/events"),
        Some(&device.access_token),
    )
    .await;
    assert_eq!(owner_missing, foreign);

    // Through the client: a permanent 404.
    let intruder_client = controller(&server, &intruder);
    let error = intruder_client
        .session_events(&session_id, &PageRequest::first())
        .await
        .expect_err("foreign");
    assert!(permanent_detail(&error).contains("404"), "{error:?}");

    // The list shows the intruder its own (empty) account, and naming
    // the owner's environment is a 404 like any foreign resource.
    let own = intruder_client
        .list_sessions(None, &PageRequest::first())
        .await
        .expect("own list");
    assert!(own.sessions.is_empty() && own.next_cursor.is_none());
    let foreign_filter = raw_get(
        &server,
        &format!("/v1/sessions?environment={environment_id}"),
        Some(&intruder),
    )
    .await;
    let missing_filter = raw_get(
        &server,
        &format!("/v1/sessions?environment={}", ids::generate_id("env")),
        Some(&intruder),
    )
    .await;
    assert_eq!(foreign_filter.0, 404);
    assert_eq!(foreign_filter, missing_filter);

    // Only a device access token reads: not the environment secret, not
    // the session token, not nothing.
    let work = poll(&server, &environment_id, &secret, 2_000)
        .await
        .expect("leased");
    let session_token = session_token_of(&work);
    for token in [None, Some(secret.as_str()), Some(session_token.as_str())] {
        let (status, _) =
            raw_get(&server, &format!("/v1/sessions/{session_id}/events"), token).await;
        assert_eq!(status, 401, "{token:?}");
        let (status, _) = raw_get(&server, "/v1/sessions", token).await;
        assert_eq!(status, 401, "{token:?}");
    }
}

// ─── Session list ─────────────────────────────────────────────────────

#[tokio::test]
async fn the_list_is_ordered_by_latest_activity_and_filters_by_environment() {
    let server = server().await;
    let device = bootstrap(&server).await;
    let (first_env, _) = register(&server, &device.access_token, &bridge_config("env-a")).await;
    let (second_env, _) = register(&server, &device.access_token, &bridge_config("env-b")).await;
    let token = device.access_token.as_str();
    let (_, old) = enqueue(&server, token, &first_env, "old").await;
    let (_, busy) = enqueue(&server, token, &first_env, "busy").await;
    let (_, elsewhere) = enqueue(&server, token, &second_env, "elsewhere").await;
    let (_, tied_a) = enqueue(&server, token, &first_env, "tie a").await;
    let (_, tied_b) = enqueue(&server, token, &first_env, "tie b").await;

    // Activity times far in the future, so enqueue's own clock cannot
    // interfere: busy > elsewhere > the tied pair > old.
    let base = ids::now_unix() + 1_000;
    let store = server.state.store();
    store
        .record_session_event(
            &old,
            "session_message",
            r#"{"type":"session_message","message":{}}"#,
            base,
        )
        .expect("record");
    let state_event = store
        .record_session_event(
            &busy,
            "session_state",
            r#"{"type":"session_state","state":"needs_input","detail":"permission"}"#,
            base + 30,
        )
        .expect("record");
    let busy_last = store
        .record_session_event(
            &busy,
            "session_message",
            r#"{"type":"session_message","message":{}}"#,
            base + 40,
        )
        .expect("record");
    store
        .record_session_event(
            &elsewhere,
            "session_message",
            r#"{"type":"session_message","message":{}}"#,
            base + 20,
        )
        .expect("record");
    for tied in [&tied_a, &tied_b] {
        store
            .record_session_event(
                tied,
                "session_message",
                r#"{"type":"session_message","message":{}}"#,
                base + 10,
            )
            .expect("record");
    }

    let client = controller(&server, token);
    let everything = client
        .list_sessions(None, &PageRequest::first())
        .await
        .expect("list");
    assert!(everything.next_cursor.is_none());
    let mut tied = [tied_a.clone(), tied_b.clone()];
    tied.sort();
    tied.reverse();
    let expected = vec![
        busy.clone(),
        elsewhere.clone(),
        tied[0].clone(),
        tied[1].clone(),
        old.clone(),
    ];
    assert_eq!(session_ids(std::slice::from_ref(&everything)), expected);

    // The row carries what a list needs without opening the session.
    let top = &everything.sessions[0];
    assert_eq!(top.environment_id, first_env);
    assert_eq!(top.state, "queued");
    assert_eq!(top.last_event_id, Some(busy_last as u64));
    assert_eq!(
        top.last_event_at.as_deref(),
        Some(ids::rfc3339(base + 40).as_str())
    );
    assert_eq!(top.last_activity_at, ids::rfc3339(base + 40));
    let reported = top.reported_state.as_ref().expect("reported state");
    assert_eq!(reported.state, SessionRunState::NeedsInput);
    assert_eq!(reported.detail.as_deref(), Some("permission"));
    assert_eq!(reported.event_id, state_event as u64);
    assert!(everything.sessions[1].reported_state.is_none());

    // One at a time, a page boundary lands between the two sessions that
    // share an activity second, and nothing is skipped or repeated.
    let single = list_all(&client, None, 1).await;
    assert_eq!(single.len(), 5);
    assert!(single[4].next_cursor.is_none());
    assert_eq!(session_ids(&single), expected);
    assert_eq!(session_ids(&list_all(&client, None, 2).await), expected);

    // The environment filter, paged, with its own cursors.
    let first_only = list_all(&client, Some(&first_env), 2).await;
    assert_eq!(
        session_ids(&first_only),
        vec![busy.clone(), tied[0].clone(), tied[1].clone(), old.clone()]
    );
    assert_eq!(
        session_ids(&list_all(&client, Some(&second_env), 2).await),
        vec![elsewhere.clone()]
    );
    // A cursor from the filtered walk does not continue an unfiltered one,
    // nor one filtered on a different environment.
    let filtered_cursor = first_only[0].next_cursor.clone().expect("cursor");
    for environment in [None, Some(second_env.as_str())] {
        let error = client
            .list_sessions(environment, &PageRequest::after(filtered_cursor.clone()))
            .await
            .expect_err("cursor from another filter");
        assert!(permanent_detail(&error).contains("400"), "{error:?}");
    }

    // New activity moves a session to the front.
    store
        .record_session_event(
            &old,
            "session_message",
            r#"{"type":"session_message","message":{}}"#,
            base + 50,
        )
        .expect("record");
    let refreshed = client
        .list_sessions(None, &PageRequest::first().with_limit(1))
        .await
        .expect("list");
    assert_eq!(refreshed.sessions[0].session_id, old);
}

// ─── Write-through ────────────────────────────────────────────────────

#[tokio::test]
async fn a_controller_reads_what_a_worker_streamed_alone_after_the_machine_is_gone() {
    let server = server().await;
    let device = bootstrap(&server).await;
    // The bridge side, as a bridge runs it: the real HTTP client.
    let bridge = controller(&server, &device.access_token);
    let registered = bridge
        .register_bridge_environment(&bridge_config("client-env-offline"))
        .await
        .expect("register");
    let environment_id = registered.environment_id.clone();
    let (_, session_id) =
        enqueue(&server, &device.access_token, &environment_id, "work alone").await;
    let work = bridge
        .poll_for_work(
            &environment_id,
            &registered.environment_secret,
            Default::default(),
        )
        .await
        .expect("poll")
        .expect("work");
    let secret = WorkSecret::decode(&work.secret).expect("work secret");

    // The worker streams with no controller socket ever attached.
    let worker = SessionStream::connect(&SessionStreamOptions::new(
        &secret.ingress_url,
        &secret.session_token,
    ))
    .await
    .expect("attach");
    for seq in 0..30 {
        worker
            .send(&SessionFrame::SessionMessage {
                message_id: None,
                message: json!({ "seq": seq }),
            })
            .await
            .expect("send");
    }
    worker
        .send(&SessionFrame::SessionState {
            state: "idle".into(),
            detail: None,
        })
        .await
        .expect("send");
    // `close` returns once the server has handled every earlier frame.
    worker.close().await.expect("close");

    // Then the machine goes away entirely.
    bridge
        .deregister_environment(&environment_id)
        .await
        .expect("deregister");

    // A controller that never attached reads all of it over REST.
    let reader = controller(&server, &device.access_token);
    let pages = walk(&reader, &session_id, 8).await;
    assert_eq!(pages.len(), 4);
    let events: Vec<SessionEventRecord> = pages
        .into_iter()
        .rev()
        .flat_map(|page| page.events)
        .collect();
    assert_eq!(events.len(), 31);
    let messages: Vec<u64> = events[..30].iter().map(seq_of).collect();
    assert_eq!(messages, (0..30).collect::<Vec<_>>());
    assert_eq!(events[30].kind, "session_state");
    assert_eq!(
        events[30].frame().expect("frame"),
        SessionFrame::SessionState {
            state: "idle".into(),
            detail: None
        }
    );

    // And the list still shows the session, under its dead environment.
    let listed = reader
        .list_sessions(Some(&environment_id), &PageRequest::first())
        .await
        .expect("list a deregistered environment");
    assert_eq!(listed.sessions.len(), 1);
    let summary = &listed.sessions[0];
    assert_eq!(summary.session_id, session_id);
    assert_eq!(summary.last_event_id, Some(events[30].event_id));
    assert_eq!(
        summary
            .reported_state
            .as_ref()
            .map(|state| state.state.as_str()),
        Some("idle")
    );
}

#[tokio::test]
async fn an_http_permission_event_appears_in_the_history() {
    let server = server().await;
    let (device, environment_id, secret) = bootstrapped_environment(&server).await;
    let (_, session_id) = enqueue(&server, &device.access_token, &environment_id, "ask").await;
    let work = poll(&server, &environment_id, &secret, 2_000)
        .await
        .expect("leased");
    let session_token = session_token_of(&work);
    let response = client()
        .post(format!("{}/v1/sessions/{session_id}/events", server.base))
        .bearer_auth(&session_token)
        .json(&json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": "req-1", "response": {"behavior": "allow"}}
        }))
        .send()
        .await
        .expect("post event");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let page = controller(&server, &device.access_token)
        .session_events(&session_id, &PageRequest::first())
        .await
        .expect("page");
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].kind, "control_response");
    assert!(matches!(
        page.events[0].frame().expect("frame"),
        SessionFrame::ControlResponse { .. }
    ));
}
