//! Session-scoped routes: permission events, archive, reconnect.
//!
//! The durable side of a session: the events a bridge posts back, the
//! archive transition, and the controller's escape hatch when a bridge
//! disappears mid-session. The live side is [`crate::routes::stream`].

use std::sync::Arc;

use axum::{
    extract::{rejection::JsonRejection, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rebon_bridge::config::PermissionResponseEvent;
use serde::Serialize;
use tracing::info;

use crate::{
    auth, db, error, hub::Outbound, ids, no_store, routes::stream::delivered_text, RcState,
};

#[derive(Serialize)]
struct ReconnectedWork {
    work_id: String,
    session_id: String,
}

/// `POST /v1/sessions/{session_id}/events` —
/// `BridgeApiClient::send_permission_response_event`.
///
/// Authenticated by the session token, which is scoped to exactly this
/// session and dies with the work item that issued it. The event is
/// stored verbatim: RC is the plaintext side of RFC-0008 §3, and a
/// controller reading the session later needs the decision as it was
/// sent, not a lossy projection of it.
///
/// It is also fanned out to any controller attached to the session
/// stream. A `PermissionResponseEvent` serializes as exactly the
/// `control_response` frame the socket carries — that is why
/// `rebon-bridge` gives them one body type — so a decision posted here
/// and a decision sent over the socket are indistinguishable both live
/// and on replay. Without the fan-out, one of the two surfaces would
/// show a decision the other only revealed after a reconnect.
pub async fn events(
    State(state): State<Arc<RcState>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    request: Result<Json<PermissionResponseEvent>, JsonRejection>,
) -> Response {
    let work = match auth::session(&state, &headers, &session_id).await {
        Ok(work) => work,
        Err(rejection) => return rejection,
    };
    let Ok(Json(event)) = request else {
        return error(StatusCode::BAD_REQUEST);
    };
    let Ok(payload_json) = serde_json::to_string(&event) else {
        return error(StatusCode::BAD_REQUEST);
    };
    let now_unix = ids::now_unix();
    let (target, kind) = (session_id.clone(), event.event_type.clone());
    let fan_out = payload_json.clone();
    let event_id = match db(&state, move |state| {
        state
            .store()
            .record_session_event(&target, &kind, &payload_json, now_unix)
    })
    .await
    {
        Ok(event_id) => event_id,
        Err(rejection) => return rejection,
    };
    if let Some(hub) = state.attached_session(&session_id) {
        hub.fan_out_to_controllers(
            Outbound::persisted(event_id, delivered_text(event_id, &fan_out).into()),
            None,
        );
    }
    info!(
        session = %session_id,
        work = %work.work_id,
        request_id = %event.response.request_id,
        "session event recorded"
    );
    no_store(StatusCode::NO_CONTENT.into_response())
}

/// `POST /v1/sessions/{session_id}/archive` —
/// `BridgeApiClient::archive_session`.
///
/// Reachable by the session's environment or by a controller on the
/// owning account. Outstanding work for the session is marked done, so
/// archiving does not leave an item queued for a session nobody will
/// serve.
pub async fn archive(
    State(state): State<Arc<RcState>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let environment_id = match auth::session_scope(&state, &headers, &session_id).await {
        Ok(environment_id) => environment_id,
        Err(rejection) => return rejection,
    };
    let now_unix = ids::now_unix();
    let target = session_id.clone();
    if let Err(rejection) = db(&state, move |state| {
        state.store().archive_session(&target, now_unix)
    })
    .await
    {
        return rejection;
    }
    info!(session = %session_id, environment = %environment_id, "session archived");
    no_store(StatusCode::NO_CONTENT.into_response())
}

/// `POST /v1/environments/{environment_id}/sessions/{session_id}/reconnect`
/// — `BridgeApiClient::reconnect_session`.
///
/// Force-supersedes every outstanding item for the session and queues a
/// fresh one. This is what `--session-id` needs when the bridge that
/// owned a session is gone: without it the controller would have to
/// wait out the old lease before the session could be picked up again.
pub async fn reconnect(
    State(state): State<Arc<RcState>>,
    Path((environment_id, session_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, environment) =
        match auth::controller_for_environment(&state, &headers, &environment_id).await {
            Ok(pair) => pair,
            Err(rejection) => return rejection,
        };
    let lookup = session_id.clone();
    let session = match db(&state, move |state| state.store().session(&lookup)).await {
        Ok(session) => session,
        Err(rejection) => return rejection,
    };
    // The session must exist and live on the environment named in the
    // path; a session id from a sibling environment is not a scope
    // error, it is simply not found here.
    match &session {
        Some((session_environment, _, _)) if *session_environment == environment.environment_id => {
        }
        _ => return error(StatusCode::NOT_FOUND),
    }

    let now_unix = ids::now_unix();
    let (target_environment, target_session) =
        (environment.environment_id.clone(), session_id.clone());
    let work_id = match db(&state, move |state| {
        state
            .store()
            .reconnect_session(&target_environment, &target_session, now_unix)
    })
    .await
    {
        Ok(work_id) => work_id,
        Err(rejection) => return rejection,
    };
    state.notify_environment(&environment.environment_id);
    info!(
        environment = %environment.environment_id,
        session = %session_id,
        work = %work_id,
        "session re-queued"
    );
    no_store(
        (
            StatusCode::ACCEPTED,
            Json(ReconnectedWork {
                work_id,
                session_id,
            }),
        )
            .into_response(),
    )
}
