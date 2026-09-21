//! Credential resolution.
//!
//! RFC-0008 §6 defines four credential classes. This implements
//! three of them plus the bootstrap token; the account session (a
//! browser cookie issued after an OIDC login) is not implemented, so
//! "controller" here means a device access token.
//!
//! | class | domain separator | scope |
//! |---|---|---|
//! | bootstrap | `rebon-rc-bootstrap-v1` | mints the first account, once |
//! | device refresh | `rebon-rc-device-refresh-v1` | one device, long-lived |
//! | device access | `rebon-rc-device-access-v1` | one device, short-lived |
//! | environment secret | `rebon-rc-environment-secret-v1` | one environment |
//! | session token | `rebon-rc-session-token-v1` | one work item's session |
//!
//! Every class is the same 32-byte token shape on the wire, so the
//! separator is what keeps a token minted for one purpose from ever
//! authenticating another: the stored digests simply do not match.
//!
//! ## Status codes
//!
//! The failure modes are kept distinct, and the distinction is part of
//! the contract tested in `tests/api.rs` and `tests/auth.rs`:
//!
//! - **401** — no credential, a malformed one, or one that resolves to
//!   nothing in the class the route accepts.
//! - **403** — a genuine credential used outside its scope: an
//!   environment secret against a different environment, a session
//!   token against a different session.
//! - **404** — a genuine, in-scope credential whose resource is dead:
//!   a deregistered environment, a revoked device, or a resource on
//!   another account (never confirmed to exist).
//! - **409** — a genuine session token whose work item is no longer
//!   leased: stopped, finished, archived or reclaimed. The worker is
//!   told its lease is gone ([`session`]) rather than that its
//!   credential is unknown.

use std::sync::Arc;

use axum::{
    http::{HeaderMap, StatusCode},
    response::Response,
};
use subtle::ConstantTimeEq;

use crate::{
    db, error, ids,
    store::{DeviceAuth, EnvironmentAuth, WorkByToken},
    RcState,
};

/// Domain separator for the one-time bootstrap token.
pub const DOMAIN_BOOTSTRAP: &[u8] = b"rebon-rc-bootstrap-v1";
/// Domain separator for long-lived device refresh tokens.
pub const DOMAIN_DEVICE_REFRESH: &[u8] = b"rebon-rc-device-refresh-v1";
/// Domain separator for short-lived device access tokens.
pub const DOMAIN_DEVICE_ACCESS: &[u8] = b"rebon-rc-device-access-v1";
/// Domain separator for per-environment secrets.
pub const DOMAIN_ENVIRONMENT: &[u8] = b"rebon-rc-environment-secret-v1";
/// Domain separator for per-work-item session tokens.
pub const DOMAIN_SESSION: &[u8] = b"rebon-rc-session-token-v1";

/// Pull a canonical bearer credential off the request, or fail with 401.
pub fn credential(headers: &HeaderMap) -> Result<String, Response> {
    ids::bearer(headers)
        .map(str::to_string)
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED))
}

/// Whether `token` is the configured bootstrap token.
///
/// Compared in constant time, and only meaningful while no account
/// exists — the caller checks that as part of the same transaction that
/// mints the account, so the token is consumed by first successful use.
pub fn is_bootstrap(state: &Arc<RcState>, token: &str) -> bool {
    let Some(expected) = state.bootstrap_digest() else {
        return false;
    };
    let presented = state.digest(DOMAIN_BOOTSTRAP, token);
    presented.ct_eq(expected.as_slice()).into()
}

/// Authenticate a controller: a device access token.
///
/// There is no account session yet, so this is the only controller
/// credential. When the web UI adds one, the cookie path joins here
/// and every route that names "device or account session" keeps working
/// unchanged.
pub async fn controller(state: &Arc<RcState>, headers: &HeaderMap) -> Result<DeviceAuth, Response> {
    let token = credential(headers)?;
    let digest = state.digest(DOMAIN_DEVICE_ACCESS, &token);
    let now_unix = ids::now_unix();
    let found = db(state, move |state| {
        state.store().device_by_access(&digest, now_unix)
    })
    .await?;
    match found {
        None => Err(error(StatusCode::UNAUTHORIZED)),
        Some(device) if device.revoked => Err(error(StatusCode::NOT_FOUND)),
        Some(device) => Ok(device),
    }
}

/// Authenticate a device by its long-lived refresh token.
pub async fn refresh(state: &Arc<RcState>, headers: &HeaderMap) -> Result<DeviceAuth, Response> {
    let token = credential(headers)?;
    let digest = state.digest(DOMAIN_DEVICE_REFRESH, &token);
    let found = db(state, move |state| state.store().device_by_refresh(&digest)).await?;
    match found {
        None => Err(error(StatusCode::UNAUTHORIZED)),
        Some(device) if device.revoked => Err(error(StatusCode::NOT_FOUND)),
        Some(device) => Ok(device),
    }
}

/// Authenticate an environment secret and confirm it is scoped to
/// `environment_id` from the path.
pub async fn environment(
    state: &Arc<RcState>,
    headers: &HeaderMap,
    environment_id: &str,
) -> Result<EnvironmentAuth, Response> {
    let token = credential(headers)?;
    let digest = state.digest(DOMAIN_ENVIRONMENT, &token);
    let now_unix = ids::now_unix();
    let found = db(state, move |state| {
        state.store().environment_by_secret(&digest, now_unix)
    })
    .await?;
    let Some(environment) = found else {
        return Err(error(StatusCode::UNAUTHORIZED));
    };
    if environment.environment_id != environment_id {
        return Err(error(StatusCode::FORBIDDEN));
    }
    if !environment.alive() {
        return Err(error(StatusCode::NOT_FOUND));
    }
    Ok(environment)
}

/// Authenticate a controller and confirm it owns `environment_id`.
///
/// A device from another account gets 404, not 403: an environment it
/// does not own is not an environment it gets told about.
pub async fn controller_for_environment(
    state: &Arc<RcState>,
    headers: &HeaderMap,
    environment_id: &str,
) -> Result<(DeviceAuth, EnvironmentAuth), Response> {
    let device = controller(state, headers).await?;
    let wanted = environment_id.to_string();
    let found = db(state, move |state| state.store().environment_by_id(&wanted)).await?;
    match found {
        Some(environment) if environment.account_id == device.account_id && environment.alive() => {
            Ok((device, environment))
        }
        _ => Err(error(StatusCode::NOT_FOUND)),
    }
}

/// Authenticate a controller and confirm the session is on its account.
///
/// Returns the session's environment id. Unlike
/// [`controller_for_environment`], a deregistered environment does not
/// hide its sessions: reading the history of a machine that is gone is
/// exactly what the history routes are for. A session on another
/// account is 404, the same answer as one that does not exist.
pub async fn controller_for_session(
    state: &Arc<RcState>,
    headers: &HeaderMap,
    session_id: &str,
) -> Result<(DeviceAuth, String), Response> {
    let device = controller(state, headers).await?;
    let wanted = session_id.to_string();
    let found = db(state, move |state| state.store().session(&wanted)).await?;
    match found {
        Some((environment_id, account_id, _)) if account_id == device.account_id => {
            Ok((device, environment_id))
        }
        _ => Err(error(StatusCode::NOT_FOUND)),
    }
}

/// A caller authenticated as one of the two classes that drive
/// environment- and session-scoped routes.
#[derive(Debug, Clone)]
pub enum Principal {
    /// A controller — a device access token.
    Device(DeviceAuth),
    /// A bridge, holding its environment secret.
    Environment(EnvironmentAuth),
}

/// Identify the caller without yet deciding what they may touch.
///
/// Resolving the credential *before* looking up the resource is what
/// keeps an unknown-resource 404 from telling an unauthenticated
/// stranger which ids exist.
pub async fn device_or_environment(
    state: &Arc<RcState>,
    headers: &HeaderMap,
) -> Result<Principal, Response> {
    let token = credential(headers)?;
    let access_digest = state.digest(DOMAIN_DEVICE_ACCESS, &token);
    let secret_digest = state.digest(DOMAIN_ENVIRONMENT, &token);
    let now_unix = ids::now_unix();
    let resolved = db(state, move |state| {
        if let Some(device) = state.store().device_by_access(&access_digest, now_unix)? {
            return Ok(Some(Principal::Device(device)));
        }
        Ok(state
            .store()
            .environment_by_secret(&secret_digest, now_unix)?
            .map(Principal::Environment))
    })
    .await?;
    match resolved {
        None => Err(error(StatusCode::UNAUTHORIZED)),
        Some(Principal::Device(device)) if device.revoked => Err(error(StatusCode::NOT_FOUND)),
        Some(principal) => Ok(principal),
    }
}

/// Authenticate either side of a route the RFC marks "environment
/// secret or controller", returning the environment both agree on.
pub async fn environment_or_controller(
    state: &Arc<RcState>,
    headers: &HeaderMap,
    environment_id: &str,
) -> Result<EnvironmentAuth, Response> {
    match device_or_environment(state, headers).await? {
        Principal::Environment(environment) => {
            if environment.environment_id != environment_id {
                return Err(error(StatusCode::FORBIDDEN));
            }
            if !environment.alive() {
                return Err(error(StatusCode::NOT_FOUND));
            }
            Ok(environment)
        }
        Principal::Device(device) => {
            let wanted = environment_id.to_string();
            let found = db(state, move |state| state.store().environment_by_id(&wanted)).await?;
            match found {
                Some(environment)
                    if environment.account_id == device.account_id && environment.alive() =>
                {
                    Ok(environment)
                }
                _ => Err(error(StatusCode::NOT_FOUND)),
            }
        }
    }
}

/// Authorize a caller against a session, for the routes the RFC marks
/// "environment secret or controller" but addresses by session id.
///
/// Returns the session's environment id.
pub async fn session_scope(
    state: &Arc<RcState>,
    headers: &HeaderMap,
    session_id: &str,
) -> Result<String, Response> {
    let principal = device_or_environment(state, headers).await?;
    let wanted = session_id.to_string();
    let found = db(state, move |state| state.store().session(&wanted)).await?;
    let Some((environment_id, account_id, _)) = found else {
        return Err(error(StatusCode::NOT_FOUND));
    };
    match principal {
        Principal::Environment(environment) => {
            if environment.environment_id != environment_id {
                return Err(error(StatusCode::FORBIDDEN));
            }
            if !environment.alive() {
                return Err(error(StatusCode::NOT_FOUND));
            }
        }
        Principal::Device(device) => {
            if device.account_id != account_id {
                return Err(error(StatusCode::NOT_FOUND));
            }
        }
    }
    Ok(environment_id)
}

/// Authenticate a session token and confirm it is scoped to
/// `session_id` from the path.
///
/// The token is minted per work item, so it dies when the item is
/// reclaimed, stopped or finished — exactly the lifetime RFC-0008 §6
/// asks for ("expires with the session").
pub async fn session(
    state: &Arc<RcState>,
    headers: &HeaderMap,
    session_id: &str,
) -> Result<WorkByToken, Response> {
    let token = credential(headers)?;
    let digest = state.digest(DOMAIN_SESSION, &token);
    let found = db(state, move |state| {
        state.store().work_by_session_token(&digest)
    })
    .await?;
    let Some(work) = found else {
        return Err(error(StatusCode::UNAUTHORIZED));
    };
    if work.session_id.as_deref() != Some(session_id) {
        return Err(error(StatusCode::FORBIDDEN));
    }
    use crate::store::work_state;
    if work.state != work_state::LEASED && work.state != work_state::ACKED {
        // Genuine token whose work item is no longer leased: tell the
        // worker its lease is gone rather than that its credential is
        // unknown.
        return Err(error(StatusCode::CONFLICT));
    }
    Ok(work)
}
