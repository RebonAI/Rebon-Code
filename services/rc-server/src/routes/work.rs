//! The work queue: long poll, enqueue, ack, heartbeat, stop.
//!
//! These five routes are the whole bridge-facing surface. The poll is
//! authenticated by the environment secret; ack and heartbeat
//! additionally carry the per-item session token, which is the only
//! thing that distinguishes the worker currently holding a lease from
//! one whose lease lapsed while it was asleep.

use std::{sync::Arc, time::Duration};

use axum::{
    extract::{rejection::JsonRejection, rejection::QueryRejection, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rebon_bridge::config::{
    valid_rebon_session_id, EnqueueWorkRequest, EnqueuedWork, HeartbeatOutcome, SessionWork,
    WorkData, WorkDataType, WorkItem, WorkResponse,
};
use serde::Deserialize;
use tracing::info;

use crate::{
    auth, db, error, ids, lease, no_store,
    store::{work_state, Enqueued, NewWork, WorkRow},
    work_secret, RcState,
};

/// Longest prompt accepted on the enqueue route. Comfortably inside the
/// 256 KiB body limit while leaving room for the rest of the envelope.
const MAX_PROMPT: usize = 128 * 1024;

/// Longest project path accepted on the enqueue route — the same cap a
/// registration puts on the paths it advertises.
const MAX_PROJECT: usize = 4_096;

/// Query string of the long poll.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollQuery {
    /// How long the server may hold the request open, in milliseconds.
    #[serde(rename = "timeoutMs")]
    timeout_ms: Option<u64>,
    /// `PollOptions::reclaim_older_than_ms` — the client's hint that an
    /// item leased for longer than this is stale enough to take back.
    #[serde(rename = "reclaimOlderThanMs")]
    reclaim_older_than_ms: Option<u64>,
}

/// Body of the ack and heartbeat routes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTokenRequest {
    /// The session token handed out with the lease.
    session_token: String,
}

/// Body of the stop route.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopRequest {
    /// Whether the worker should be killed rather than asked to wind down.
    #[serde(default)]
    force: bool,
}

/// `GET /v1/environments/{environment_id}/work` — the long poll.
///
/// Hands out at most one `ready` item, leases it, and answers 200 with
/// a [`WorkItem`]: the [`WorkResponse`] envelope plus, for session work,
/// the `session` object saying which project to run in, the first prompt
/// and the Rebon session to continue. An empty queue parks on the environment's wakeup
/// handle until work arrives or the (capped) timeout runs out, and then
/// answers **204** — the client distinguishes "nothing to do" from
/// "something to do" by status code, never by an empty body.
pub async fn poll(
    State(state): State<Arc<RcState>>,
    Path(environment_id): Path<String>,
    query: Result<Query<PollQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let Ok(Query(query)) = query else {
        return error(StatusCode::BAD_REQUEST);
    };
    let environment = match auth::environment(&state, &headers, &environment_id).await {
        Ok(environment) => environment,
        Err(rejection) => return rejection,
    };

    let wait = match query.timeout_ms {
        Some(ms) => Duration::from_millis(ms).min(state.config().max_poll_wait),
        None => state.config().default_poll_wait,
    };
    let claimed = match lease::await_work(
        &state,
        &environment.environment_id,
        query.reclaim_older_than_ms,
        wait,
    )
    .await
    {
        Ok(claimed) => claimed,
        Err(rejection) => return rejection,
    };
    let Some((claim, session_token)) = claimed else {
        return no_store(StatusCode::NO_CONTENT.into_response());
    };

    // `WorkData.id` is the handle the worker correlates on: the session
    // for session work, the work item itself for a healthcheck that has
    // no session.
    let data_id = claim
        .session_id
        .clone()
        .unwrap_or_else(|| claim.work_id.clone());
    let secret = work_secret::mint(
        &state.config().session_ingress_url,
        session_token,
        claim.session_id.clone(),
    );
    info!(
        environment = %environment.environment_id,
        work = %claim.work_id,
        "work leased"
    );
    // Session work queued before projects existed has no project; it
    // goes out without a `session`, which a runner treats as work it
    // cannot place.
    let session = match (claim.work_type, claim.project_path) {
        (WorkDataType::Session, Some(project)) => Some(SessionWork {
            project,
            prompt: claim.prompt,
            resume_rebon_session_id: claim.resume_rebon_session_id,
        }),
        _ => None,
    };
    no_store(
        (
            StatusCode::OK,
            Json(WorkItem {
                session,
                response: WorkResponse {
                    id: claim.work_id,
                    response_type: "work".to_string(),
                    environment_id: environment.environment_id,
                    // The server's own vocabulary, not a separate wire one:
                    // the row really is leased the moment this is written.
                    state: work_state::LEASED.to_string(),
                    data: WorkData {
                        data_type: claim.work_type,
                        id: data_id,
                    },
                    secret: secret.encode(),
                    created_at: ids::rfc3339(claim.created_at_unix),
                },
            }),
        )
            .into_response(),
    )
}

/// Whether an enqueue body is well formed, before any lookup.
///
/// Session work needs a prompt, a resume target, or both; a healthcheck
/// takes none of the session fields. A prompt is non-empty and within
/// [`MAX_PROMPT`], a project path non-empty and within [`MAX_PROJECT`],
/// a session id an RC session id, and a resume target a plain name
/// ([`valid_rebon_session_id`]).
fn well_formed(request: &EnqueueWorkRequest) -> bool {
    match request.work_type {
        WorkDataType::Session => {
            (request.prompt.is_some() || request.resume_rebon_session_id.is_some())
                && request.prompt.as_deref().map_or(true, |prompt| {
                    !prompt.is_empty() && prompt.len() <= MAX_PROMPT
                })
                && request.project.as_deref().map_or(true, |project| {
                    !project.is_empty() && project.len() <= MAX_PROJECT
                })
                && request
                    .session_id
                    .as_deref()
                    .map_or(true, |id| ids::valid_id(id, "sess"))
                && request
                    .resume_rebon_session_id
                    .as_deref()
                    .map_or(true, valid_rebon_session_id)
        }
        WorkDataType::Healthcheck => {
            request.prompt.is_none()
                && request.session_id.is_none()
                && request.project.is_none()
                && request.resume_rebon_session_id.is_none()
        }
    }
}

/// `POST /v1/environments/{environment_id}/work` — a controller queues work.
///
/// The body is [`EnqueueWorkRequest`]. Besides a malformed body, two
/// things are refused, with the usual bare status:
///
/// | Refusal | Status |
/// |---|---|
/// | no advertised project could be chosen (see [`crate::store::Store::enqueue_work`]) | 400 |
/// | `session_id` names a session on another environment or account | 404 |
pub async fn enqueue(
    State(state): State<Arc<RcState>>,
    Path(environment_id): Path<String>,
    headers: HeaderMap,
    request: Result<Json<EnqueueWorkRequest>, JsonRejection>,
) -> Response {
    let (_, environment) =
        match auth::controller_for_environment(&state, &headers, &environment_id).await {
            Ok(pair) => pair,
            Err(rejection) => return rejection,
        };
    let Ok(Json(request)) = request else {
        return error(StatusCode::BAD_REQUEST);
    };
    if !well_formed(&request) {
        return error(StatusCode::BAD_REQUEST);
    }

    let now_unix = ids::now_unix();
    let target = environment.environment_id.clone();
    let account_id = environment.account_id.clone();
    let enqueued = db(&state, move |state| {
        state.store().enqueue_work(
            &target,
            &account_id,
            &NewWork {
                work_type: request.work_type,
                session_id: request.session_id.as_deref(),
                prompt: request.prompt.as_deref(),
                project: request.project.as_deref(),
                resume_rebon_session_id: request.resume_rebon_session_id.as_deref(),
            },
            now_unix,
        )
    })
    .await;
    let (work_id, session_id) = match enqueued {
        Ok(Enqueued::Queued {
            work_id,
            session_id,
        }) => (work_id, session_id),
        Ok(Enqueued::ProjectRefused) => return error(StatusCode::BAD_REQUEST),
        Ok(Enqueued::ForeignSession) => return error(StatusCode::NOT_FOUND),
        Err(rejection) => return rejection,
    };
    // Wake the bridge now rather than making it wait out its poll.
    state.notify_environment(&environment.environment_id);
    info!(environment = %environment.environment_id, work = %work_id, "work enqueued");
    no_store(
        (
            StatusCode::CREATED,
            Json(EnqueuedWork {
                work_id,
                session_id,
            }),
        )
            .into_response(),
    )
}

/// Whether a work item is still leased to a worker.
fn is_active(state: &str) -> bool {
    state == work_state::LEASED || state == work_state::ACKED
}

/// Resolve a work item within an environment and check the session
/// token the caller presented against the one issued with the lease.
///
/// A token that matches an item the caller no longer holds is a
/// **409**, not a 401: the credential is genuine and the worker needs to
/// know it lost the lease rather than that it should go looking for a
/// fresh credential. `allow_inactive` is for the heartbeat, which
/// reports that loss in its own body instead.
async fn work_with_token(
    state: &Arc<RcState>,
    headers: &HeaderMap,
    environment_id: &str,
    work_id: &str,
    session_token: &str,
    allow_inactive: bool,
) -> Result<WorkRow, Response> {
    let environment = auth::environment(state, headers, environment_id).await?;
    if !ids::valid_id(work_id, "wrk") || !ids::canonical_token(session_token) {
        return Err(error(StatusCode::NOT_FOUND));
    }
    let digest = state.digest(auth::DOMAIN_SESSION, session_token);
    let (target_environment, target_work) =
        (environment.environment_id.clone(), work_id.to_string());
    let row = db(state, move |state| {
        state
            .store()
            .work_row(&target_environment, &target_work, Some(&digest))
    })
    .await?;
    let Some(row) = row else {
        return Err(error(StatusCode::NOT_FOUND));
    };
    if !row.token_matches {
        // The item exists in this environment but this token never held
        // it: a token from a previous generation, or someone else's.
        return Err(error(StatusCode::UNAUTHORIZED));
    }
    if !allow_inactive && !is_active(&row.state) {
        // Genuine token, but the lease is gone — stopped, archived, or
        // reclaimed underneath the worker.
        return Err(error(StatusCode::CONFLICT));
    }
    Ok(row)
}

/// `POST /v1/environments/{environment_id}/work/{work_id}/ack` —
/// `BridgeApiClient::acknowledge_work`.
pub async fn acknowledge(
    State(state): State<Arc<RcState>>,
    Path((environment_id, work_id)): Path<(String, String)>,
    headers: HeaderMap,
    request: Result<Json<SessionTokenRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(request)) = request else {
        return error(StatusCode::BAD_REQUEST);
    };
    let row = match work_with_token(
        &state,
        &headers,
        &environment_id,
        &work_id,
        &request.session_token,
        false,
    )
    .await
    {
        Ok(row) => row,
        Err(rejection) => return rejection,
    };
    let lease_seconds = lease::lease_seconds(&state);
    let now_unix = ids::now_unix();
    let target = row.work_id.clone();
    let acknowledged = match db(&state, move |state| {
        state
            .store()
            .acknowledge_work(&target, lease_seconds, now_unix)
    })
    .await
    {
        Ok(acknowledged) => acknowledged,
        Err(rejection) => return rejection,
    };
    if !acknowledged {
        // Terminal item: the worker is acknowledging something that was
        // stopped or finished underneath it.
        return error(StatusCode::CONFLICT);
    }
    no_store(StatusCode::NO_CONTENT.into_response())
}

/// `POST /v1/environments/{environment_id}/work/{work_id}/heartbeat` —
/// `BridgeApiClient::heartbeat_work`.
///
/// A lost lease is not an error: the worker gets 200 with
/// `lease_extended: false` and the item's real state, which is exactly
/// the signal it needs to stand down without a retry loop.
pub async fn heartbeat(
    State(state): State<Arc<RcState>>,
    Path((environment_id, work_id)): Path<(String, String)>,
    headers: HeaderMap,
    request: Result<Json<SessionTokenRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(request)) = request else {
        return error(StatusCode::BAD_REQUEST);
    };
    let row = match work_with_token(
        &state,
        &headers,
        &environment_id,
        &work_id,
        &request.session_token,
        true,
    )
    .await
    {
        Ok(row) => row,
        Err(rejection) => return rejection,
    };
    let lease_seconds = lease::lease_seconds(&state);
    let now_unix = ids::now_unix();
    let target = row.work_id.clone();
    let outcome = match db(&state, move |state| {
        state
            .store()
            .heartbeat_work(&target, lease_seconds, now_unix)
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(rejection) => return rejection,
    };
    no_store(
        (
            StatusCode::OK,
            Json(HeartbeatOutcome {
                lease_extended: outcome.0,
                state: outcome.1,
            }),
        )
            .into_response(),
    )
}

/// `POST /v1/environments/{environment_id}/work/{work_id}/stop` —
/// `BridgeApiClient::stop_work`.
///
/// Reachable by the environment itself or by a controller, so a phone
/// can stop a run the machine is in the middle of. Idempotent: stopping
/// an already-terminal item still answers 204.
pub async fn stop(
    State(state): State<Arc<RcState>>,
    Path((environment_id, work_id)): Path<(String, String)>,
    headers: HeaderMap,
    request: Result<Json<StopRequest>, JsonRejection>,
) -> Response {
    let environment = match auth::environment_or_controller(&state, &headers, &environment_id).await
    {
        Ok(environment) => environment,
        Err(rejection) => return rejection,
    };
    let force = match request {
        Ok(Json(request)) => request.force,
        Err(JsonRejection::MissingJsonContentType(_)) => false,
        Err(_) => return error(StatusCode::BAD_REQUEST),
    };
    if !ids::valid_id(&work_id, "wrk") {
        return error(StatusCode::NOT_FOUND);
    }
    let (target_environment, target_work) = (environment.environment_id.clone(), work_id.clone());
    let row = match db(&state, move |state| {
        state
            .store()
            .work_row(&target_environment, &target_work, None)
    })
    .await
    {
        Ok(row) => row,
        Err(rejection) => return rejection,
    };
    if row.is_none() {
        return error(StatusCode::NOT_FOUND);
    }
    let now_unix = ids::now_unix();
    let target = work_id.clone();
    if let Err(rejection) = db(&state, move |state| {
        state.store().stop_work(&target, force, now_unix)
    })
    .await
    {
        return rejection;
    }
    info!(environment = %environment.environment_id, work = %work_id, force, "work stopped");
    no_store(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_enqueue_body_is_shape_checked() {
        let session = EnqueueWorkRequest::session("hi");
        assert!(well_formed(&session));
        assert!(well_formed(&session.clone().in_project("/srv/app")));
        assert!(well_formed(
            &session.clone().for_session("sess_AAAAAAAAAAAAAAAAAAAAAA")
        ));
        // A resume alone is enough; nothing at all is not.
        let mut resume_only = EnqueueWorkRequest::session("x").resuming("rebon-1");
        resume_only.prompt = None;
        assert!(well_formed(&resume_only));
        let mut nothing = resume_only.clone();
        nothing.resume_rebon_session_id = None;
        assert!(!well_formed(&nothing));

        assert!(!well_formed(&EnqueueWorkRequest::session("")));
        assert!(!well_formed(&EnqueueWorkRequest::session(
            "x".repeat(MAX_PROMPT + 1)
        )));
        assert!(!well_formed(&session.clone().in_project("")));
        assert!(!well_formed(
            &session.clone().in_project("x".repeat(MAX_PROJECT + 1))
        ));
        assert!(!well_formed(&session.clone().for_session("bogus")));
        assert!(!well_formed(&session.clone().resuming("../escape")));

        assert!(well_formed(&EnqueueWorkRequest::healthcheck()));
        for extra in [
            EnqueueWorkRequest::healthcheck().in_project("/srv/app"),
            EnqueueWorkRequest::healthcheck().resuming("rebon-1"),
            EnqueueWorkRequest::healthcheck().for_session("sess_AAAAAAAAAAAAAAAAAAAAAA"),
        ] {
            assert!(!well_formed(&extra), "{extra:?}");
        }
    }
}
