//! Environment registration, project updates, listing and
//! deregistration.
//!
//! The request body of `POST /v1/environments` is
//! [`rebon_bridge::config::BridgeConfig`] itself and the response is
//! [`rebon_bridge::config::RegisteredEnvironment`]; the project route
//! speaks [`rebon_bridge::projects::ProjectList`] and the listing is
//! [`rebon_bridge::projects::EnvironmentList`] — imported, never
//! re-declared, so a change to any of these shapes breaks this build
//! rather than drifting silently apart from the client.
//!
//! One environment is one machine. What it may be asked to run is the
//! project list it advertises: registration sets it (from `projects`, or
//! from `dir` when that is empty) and `PUT …/projects` replaces it.

use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::{rejection::JsonRejection, ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rebon_bridge::config::{BridgeConfig, RegisteredEnvironment};
use rebon_bridge::projects::{EnvironmentList, EnvironmentSummary, ProjectInfo, ProjectList};
use rebon_bridge::MAX_PROJECTS;
use std::collections::HashSet;
use tracing::info;

use crate::{auth, client_ip, db, error, ids, no_store, RcState};

/// Longest client-supplied string we accept in a registration field.
const MAX_FIELD: usize = 4_096;

fn too_long(value: &str) -> bool {
    value.chars().count() > MAX_FIELD
}

/// Whether a project list is one RC will store: at most
/// [`MAX_PROJECTS`] entries, every path non-empty and distinct, every
/// label non-empty, and nothing over [`MAX_FIELD`].
///
/// Paths are compared exactly — RC does not know the machine's
/// filesystem rules, so `/a` and `/a/` are two different projects.
fn sane_projects(projects: &[ProjectInfo]) -> bool {
    if projects.len() > MAX_PROJECTS {
        return false;
    }
    let mut seen = HashSet::with_capacity(projects.len());
    projects.iter().all(|project| {
        !project.path.is_empty()
            && !project.label.is_empty()
            && !too_long(&project.path)
            && !too_long(&project.label)
            && !project.remote.as_deref().is_some_and(too_long)
            && !project.branch.as_deref().is_some_and(too_long)
            && seen.insert(project.path.as_str())
    })
}

fn sane(config: &BridgeConfig) -> bool {
    let fields = [
        &config.bridge_id,
        &config.environment_id,
        &config.machine_name,
        &config.dir,
        &config.branch,
        &config.worker_type,
    ];
    if config.bridge_id.is_empty() || config.environment_id.is_empty() {
        return false;
    }
    if fields.iter().any(|field| field.chars().count() > MAX_FIELD) {
        return false;
    }
    if config
        .git_repo_url
        .as_ref()
        .is_some_and(|url| url.chars().count() > MAX_FIELD)
    {
        return false;
    }
    config.max_sessions >= 1 && sane_projects(&config.effective_projects())
}

/// `POST /v1/environments` — register or re-register an environment.
///
/// Idempotent on `BridgeConfig.environment_id` (and on
/// `reuse_environment_id` when the client supplies one): the same
/// bridge coming back gets the same backend environment id. The secret
/// is rotated on every call, so a re-register invalidates whatever the
/// previous process was holding — which is what makes a crashed
/// bridge's leaked secret short-lived.
///
/// Always 200: the operation is an upsert and the body is identical
/// whether a row was created or reused, so a split 200/201 would give
/// the client nothing to act on.
pub async fn register(
    State(state): State<Arc<RcState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    request: Result<Json<BridgeConfig>, JsonRejection>,
) -> Response {
    let ip = client_ip(state.config(), &headers, peer);
    if !state.allow_auth_request(ip) {
        return error(StatusCode::TOO_MANY_REQUESTS);
    }
    let device = match auth::controller(&state, &headers).await {
        Ok(device) => device,
        Err(rejection) => return rejection,
    };
    let Ok(Json(config)) = request else {
        return error(StatusCode::BAD_REQUEST);
    };
    if !sane(&config) {
        return error(StatusCode::BAD_REQUEST);
    }

    let environment_secret = ids::generate_token();
    let digest = state.digest(auth::DOMAIN_ENVIRONMENT, &environment_secret);
    let now_unix = ids::now_unix();
    let (account_id, device_id) = (device.account_id.clone(), device.device_id.clone());
    let registration = match db(&state, move |state| {
        state
            .store()
            .register_environment(&account_id, &device_id, &config, &digest, now_unix)
    })
    .await
    {
        Ok(registration) => registration,
        Err(rejection) => return rejection,
    };
    info!(
        environment = %registration.environment_id,
        device = %device.device_id,
        created = registration.created,
        "environment registered"
    );
    no_store(
        (
            StatusCode::OK,
            Json(RegisteredEnvironment {
                environment_id: registration.environment_id,
                environment_secret,
            }),
        )
            .into_response(),
    )
}

/// `GET /v1/environments` — the account's environments, each with the
/// projects it advertises.
///
/// Never includes a secret: a controller listing its machines has no
/// use for one, and handing them out would let any control surface
/// impersonate a bridge.
pub async fn list(State(state): State<Arc<RcState>>, headers: HeaderMap) -> Response {
    let device = match auth::controller(&state, &headers).await {
        Ok(device) => device,
        Err(rejection) => return rejection,
    };
    let account_id = device.account_id.clone();
    let rows = match db(&state, move |state| {
        state.store().list_environments(&account_id)
    })
    .await
    {
        Ok(rows) => rows,
        Err(rejection) => return rejection,
    };
    let environments = rows
        .into_iter()
        .map(|row| EnvironmentSummary {
            environment_id: row.environment_id,
            client_environment_id: row.client_environment_id,
            device_id: row.device_id,
            bridge_id: row.bridge_id,
            machine_name: row.machine_name,
            dir: row.dir,
            branch: row.branch,
            git_repo_url: row.git_repo_url,
            worker_type: row.worker_type,
            max_sessions: row.max_sessions,
            spawn_mode: row.spawn_mode,
            projects: row.projects,
            created_at: ids::rfc3339(row.created_at_unix),
            last_seen_at: ids::rfc3339(row.last_seen_at_unix),
            deregistered_at: row.deregistered_at_unix.map(ids::rfc3339),
        })
        .collect();
    no_store((StatusCode::OK, Json(EnvironmentList { environments })).into_response())
}

/// `PUT /v1/environments/{environment_id}/projects` — the environment
/// replaces the list of projects it serves.
///
/// Authenticated by the environment's own secret: only the machine knows
/// which checkouts it has. The body is the complete new list; the
/// response is the list as stored, so the caller sees exactly what work
/// will be checked against. A list that fails [`sane_projects`] is 400
/// and changes nothing. Work already queued is left alone — the runner
/// is the one that finally decides whether it can still serve it.
pub async fn update_projects(
    State(state): State<Arc<RcState>>,
    Path(environment_id): Path<String>,
    headers: HeaderMap,
    request: Result<Json<ProjectList>, JsonRejection>,
) -> Response {
    let environment = match auth::environment(&state, &headers, &environment_id).await {
        Ok(environment) => environment,
        Err(rejection) => return rejection,
    };
    let Ok(Json(list)) = request else {
        return error(StatusCode::BAD_REQUEST);
    };
    if !sane_projects(&list.projects) {
        return error(StatusCode::BAD_REQUEST);
    }
    let target = environment.environment_id.clone();
    let now_unix = ids::now_unix();
    let stored = match db(&state, move |state| {
        state
            .store()
            .replace_projects(&target, &list.projects, now_unix)?;
        state.store().environment_projects(&target)
    })
    .await
    {
        Ok(stored) => stored,
        Err(rejection) => return rejection,
    };
    info!(
        environment = %environment.environment_id,
        projects = stored.len(),
        "environment projects updated"
    );
    no_store((StatusCode::OK, Json(ProjectList { projects: stored })).into_response())
}

/// `DELETE /v1/environments/{environment_id}` — graceful deregistration.
///
/// Authenticated by the environment's own secret: a bridge shutting
/// down retires itself. The secret is replaced with an unguessable
/// value rather than cleared, and everything still queued for the
/// environment is stopped so a controller does not watch work sit in a
/// queue nobody will ever poll again.
pub async fn deregister(
    State(state): State<Arc<RcState>>,
    Path(environment_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let environment = match auth::environment(&state, &headers, &environment_id).await {
        Ok(environment) => environment,
        Err(rejection) => return rejection,
    };
    let target = environment.environment_id.clone();
    let now_unix = ids::now_unix();
    if let Err(rejection) = db(&state, move |state| {
        state.store().deregister_environment(&target, now_unix)
    })
    .await
    {
        return rejection;
    }
    // Wake any poller still parked on this environment so it observes
    // the closed queue instead of waiting out its timeout.
    state.notify_environment(&environment.environment_id);
    info!(environment = %environment.environment_id, "environment deregistered");
    no_store(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_list_is_checked_entry_by_entry() {
        assert!(sane_projects(&[]));
        assert!(sane_projects(&[
            ProjectInfo::from_path("/a"),
            ProjectInfo::from_path("/a/"),
        ]));

        let duplicate = [
            ProjectInfo::from_path("/a"),
            ProjectInfo::new("/a", "again"),
        ];
        assert!(!sane_projects(&duplicate));
        assert!(!sane_projects(&[ProjectInfo::new("", "empty")]));
        assert!(!sane_projects(&[ProjectInfo::new("/a", "")]));
        let long = "x".repeat(MAX_FIELD + 1);
        assert!(!sane_projects(&[ProjectInfo::new(long.clone(), "a")]));
        assert!(!sane_projects(&[ProjectInfo::new("/a", long.clone())]));
        assert!(!sane_projects(&[
            ProjectInfo::from_path("/a").with_remote(long.clone())
        ]));
        assert!(!sane_projects(&[
            ProjectInfo::from_path("/a").with_branch(long)
        ]));

        let many: Vec<ProjectInfo> = (0..=MAX_PROJECTS)
            .map(|index| ProjectInfo::from_path(format!("/p{index}")))
            .collect();
        assert!(!sane_projects(&many));
        assert!(sane_projects(&many[..MAX_PROJECTS]));
    }

    #[test]
    fn a_registration_is_checked_against_its_effective_projects() {
        let mut config = BridgeConfig::minimal("b", "e", "https://a", "wss://s");
        config.dir = "/srv/app".into();
        assert!(sane(&config));
        config.projects = vec![ProjectInfo::from_path("/x"), ProjectInfo::from_path("/x")];
        assert!(!sane(&config));
    }
}
