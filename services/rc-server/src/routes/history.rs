//! The controller's read side: a session's history and the account's
//! session list, both cursor-paged.
//!
//! A worker streams whether or not anyone is attached, and every frame
//! is already in `session_events`. These routes are how a controller
//! gets that back — including for a machine that is offline right now —
//! without the session stream's frame-size and queue limits. The
//! stream's attach-time replay only fills the live window; paging
//! further back is this module's job.
//!
//! Response bodies are [`rebon_bridge::history`] types, and the cursors
//! are [`crate::cursor`]'s: opaque, tagged, and scoped to the request
//! that produced them.
//!
//! Order of checks: the credential, then the resource (404 for anything
//! not on the caller's account), then the query (400). A stranger learns
//! nothing from a malformed query, and a cursor is only ever judged
//! against a resource the caller may read.

use std::sync::Arc;

use axum::{
    extract::{rejection::QueryRejection, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rebon_bridge::{
    history::{
        ReportedSessionState, SessionEventPage, SessionEventRecord, SessionPage, SessionSummary,
    },
    session_stream::SessionFrame,
};
use serde::Deserialize;
use serde_json::Value;
use tracing::warn;

use crate::{
    auth, cursor, db, error, ids, no_store,
    store::{SessionEvent, SessionListing},
    RcState, MAX_EVENT_PAGE_BYTES,
};

/// Longest `environment` filter accepted before it reaches a query.
const MAX_FILTER_CHARS: usize = 256;

/// Query string of `GET /v1/sessions/{session}/events`.
///
/// Strings rather than typed fields so an empty value (`?cursor=`, as
/// the RFC's route table spells it) means "absent" instead of failing
/// to parse.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsQuery {
    cursor: Option<String>,
    limit: Option<String>,
}

/// Query string of `GET /v1/sessions`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionsQuery {
    environment: Option<String>,
    cursor: Option<String>,
    limit: Option<String>,
}

fn present(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

/// Resolve `limit`: absent takes the default, anything above the
/// maximum is clamped to it, and zero or a non-number is refused.
fn page_limit(state: &RcState, limit: Option<String>) -> Result<usize, Response> {
    let config = state.config();
    let Some(raw) = present(limit) else {
        return Ok(config.page_limit_default);
    };
    // Digits only: `usize::from_str` would accept a leading `+`.
    if !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(error(StatusCode::BAD_REQUEST));
    }
    // All digits but too large for u64: that is a very large limit, not
    // a malformed one.
    let requested = raw.parse::<u64>().unwrap_or(u64::MAX);
    if requested == 0 {
        return Err(error(StatusCode::BAD_REQUEST));
    }
    Ok(usize::try_from(requested)
        .unwrap_or(usize::MAX)
        .min(config.page_limit_max))
}

fn event_record(event: SessionEvent) -> Result<SessionEventRecord, Response> {
    let payload: Value = serde_json::from_str(&event.payload_json).map_err(|failure| {
        // Everything stored went through a JSON parser on the way in, so
        // this is corruption, not a client error.
        warn!(event = event.event_id, error = %failure, "stored session event is not JSON");
        error(StatusCode::INTERNAL_SERVER_ERROR)
    })?;
    Ok(SessionEventRecord {
        event_id: u64::try_from(event.event_id).unwrap_or_default(),
        kind: event.kind,
        payload,
        created_at: ids::rfc3339(event.created_at_unix),
    })
}

/// `GET /v1/sessions/{session_id}/events` — one page of a session's
/// persisted history, for a controller on the session's account.
///
/// Pages walk backwards from the newest event; the events *within* a
/// page are oldest first. `next_cursor` is present exactly when older
/// events remain. A page ends early, before `limit`, once its payloads
/// reach [`MAX_EVENT_PAGE_BYTES`] — but never empty while events remain.
pub async fn events(
    State(state): State<Arc<RcState>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Response {
    if let Err(rejection) = auth::controller_for_session(&state, &headers, &session_id).await {
        return rejection;
    }
    let Ok(Query(query)) = query else {
        return error(StatusCode::BAD_REQUEST);
    };
    let limit = match page_limit(&state, query.limit) {
        Ok(limit) => limit,
        Err(rejection) => return rejection,
    };
    let before = match present(query.cursor) {
        None => i64::MAX,
        Some(presented) => {
            match cursor::decode_events(state.cursor_key(), &session_id, &presented) {
                Ok(before) => before,
                Err(_) => return error(StatusCode::BAD_REQUEST),
            }
        }
    };

    let target = session_id.clone();
    // One extra row answers "is there more?" without a second query.
    let mut rows = match db(&state, move |state| {
        state
            .store()
            .session_events_before(&target, before, limit.saturating_add(1))
    })
    .await
    {
        Ok(rows) => rows,
        Err(rejection) => return rejection,
    };

    let mut more = rows.len() > limit;
    rows.truncate(limit);
    let mut bytes = 0usize;
    let mut kept = 0usize;
    for row in &rows {
        bytes = bytes.saturating_add(row.payload_json.len());
        if kept > 0 && bytes > MAX_EVENT_PAGE_BYTES {
            more = true;
            break;
        }
        kept += 1;
    }
    rows.truncate(kept);

    let next_cursor = match (more, rows.last()) {
        (true, Some(oldest)) => Some(cursor::encode_events(
            state.cursor_key(),
            &session_id,
            oldest.event_id,
        )),
        _ => None,
    };
    let mut events = Vec::with_capacity(rows.len());
    // Rows come newest first; the page lists them oldest first.
    for row in rows.into_iter().rev() {
        match event_record(row) {
            Ok(record) => events.push(record),
            Err(rejection) => return rejection,
        }
    }
    no_store(
        (
            StatusCode::OK,
            Json(SessionEventPage {
                events,
                next_cursor,
            }),
        )
            .into_response(),
    )
}

/// The worker's last `session_state` frame, if it parses as one. Its
/// state is already in the `SessionRunState` vocabulary, an unknown word
/// included, so the list shows exactly what the worker said.
fn reported_state(reported: Option<(i64, String)>) -> Option<ReportedSessionState> {
    let (event_id, payload_json) = reported?;
    match serde_json::from_str::<SessionFrame>(&payload_json).ok()? {
        SessionFrame::SessionState { state, detail } => Some(ReportedSessionState {
            state,
            detail,
            event_id: u64::try_from(event_id).ok()?,
        }),
        _ => None,
    }
}

fn summary(row: SessionListing) -> SessionSummary {
    SessionSummary {
        session_id: row.session_id,
        environment_id: row.environment_id,
        state: row.state,
        reported_state: reported_state(row.reported_state),
        created_at: ids::rfc3339(row.created_at_unix),
        last_activity_at: ids::rfc3339(row.updated_at_unix),
        last_event_id: row
            .last_event
            .and_then(|(event_id, _)| u64::try_from(event_id).ok()),
        last_event_at: row.last_event.map(|(_, at)| ids::rfc3339(at)),
    }
}

/// `GET /v1/sessions` — the account's sessions, most recent activity
/// first, optionally only those on one environment.
///
/// "Activity" is the session row's `updated_at` — a persisted frame, a
/// queued prompt, a lease, an archive — at one-second resolution, with
/// the session id breaking ties. A session that becomes active while a
/// client is paging moves to the front: the walk neither repeats nor
/// crashes on it, and the client sees it on its next first-page read.
///
/// An `environment` that is not on the caller's account is 404, like
/// any other foreign resource. A deregistered environment of the
/// caller's own still lists its sessions.
pub async fn sessions(
    State(state): State<Arc<RcState>>,
    headers: HeaderMap,
    query: Result<Query<SessionsQuery>, QueryRejection>,
) -> Response {
    let device = match auth::controller(&state, &headers).await {
        Ok(device) => device,
        Err(rejection) => return rejection,
    };
    let Ok(Query(query)) = query else {
        return error(StatusCode::BAD_REQUEST);
    };
    let environment_id = present(query.environment);
    if let Some(environment_id) = &environment_id {
        if environment_id.len() > MAX_FILTER_CHARS {
            return error(StatusCode::BAD_REQUEST);
        }
        let wanted = environment_id.clone();
        let found = match db(&state, move |state| {
            state.store().environment_by_id(&wanted)
        })
        .await
        {
            Ok(found) => found,
            Err(rejection) => return rejection,
        };
        if !found.is_some_and(|environment| environment.account_id == device.account_id) {
            return error(StatusCode::NOT_FOUND);
        }
    }
    let limit = match page_limit(&state, query.limit) {
        Ok(limit) => limit,
        Err(rejection) => return rejection,
    };
    let after = match present(query.cursor) {
        None => None,
        Some(presented) => match cursor::decode_sessions(
            state.cursor_key(),
            &device.account_id,
            environment_id.as_deref(),
            &presented,
        ) {
            Ok(position) => Some(position),
            Err(_) => return error(StatusCode::BAD_REQUEST),
        },
    };

    let (account_id, filter) = (device.account_id.clone(), environment_id.clone());
    let mut rows = match db(&state, move |state| {
        state.store().list_sessions(
            &account_id,
            filter.as_deref(),
            after
                .as_ref()
                .map(|position| (position.updated_at_unix, position.session_id.as_str())),
            limit.saturating_add(1),
        )
    })
    .await
    {
        Ok(rows) => rows,
        Err(rejection) => return rejection,
    };

    let more = rows.len() > limit;
    rows.truncate(limit);
    let next_cursor = match (more, rows.last()) {
        (true, Some(last)) => Some(cursor::encode_sessions(
            state.cursor_key(),
            &device.account_id,
            environment_id.as_deref(),
            &cursor::SessionPosition {
                updated_at_unix: last.updated_at_unix,
                session_id: last.session_id.clone(),
            },
        )),
        _ => None,
    };
    let sessions = rows.into_iter().map(summary).collect();
    no_store(
        (
            StatusCode::OK,
            Json(SessionPage {
                sessions,
                next_cursor,
            }),
        )
            .into_response(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_session_state_frame_is_a_reported_state() {
        let state = reported_state(Some((
            4,
            r#"{"type":"session_state","state":"waiting","detail":"permission"}"#.into(),
        )))
        .expect("parsed");
        // A word this build does not know is shown as sent.
        assert_eq!(
            state.state,
            rebon_bridge::session_stream::SessionRunState::Other("waiting".into())
        );
        assert_eq!(state.detail.as_deref(), Some("permission"));
        assert_eq!(state.event_id, 4);

        assert!(reported_state(None).is_none());
        assert!(reported_state(Some((4, "not json".into()))).is_none());
        assert!(reported_state(Some((
            4,
            r#"{"type":"session_message","message":{}}"#.into()
        )))
        .is_none());
    }
}
