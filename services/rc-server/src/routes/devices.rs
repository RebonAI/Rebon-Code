//! Device credentials: issue, refresh, list, revoke.
//!
//! A device credential is a long-lived refresh token plus a short-lived
//! access token. Everything else in RC is reached with the access
//! token; the refresh token only ever exchanges itself for a new one.
//! The very first device is minted from the bootstrap token because
//! there is no login yet — RFC-0008 §6's OIDC path would replace that
//! entry point without changing anything below it.

use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::{rejection::JsonRejection, ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rebon_bridge::devices::{IssueDeviceRequest, IssuedDevice, MAX_DEVICE_LABEL_CHARS};
use serde::Serialize;
use tracing::info;

use crate::{auth, client_ip, db, error, ids, ids::TokenDigest, no_store, RcState};

#[derive(Serialize)]
struct AccessToken {
    access_token: String,
    access_expires_at: String,
}

#[derive(Serialize)]
struct DeviceView {
    device_id: String,
    label: String,
    created_at: String,
    last_seen_at: Option<String>,
    revoked_at: Option<String>,
}

#[derive(Serialize)]
struct DeviceList {
    devices: Vec<DeviceView>,
}

/// Mint a credential pair and return (refresh, access, digests, expiry).
struct MintedPair {
    refresh_token: String,
    access_token: String,
    refresh_digest: TokenDigest,
    access_digest: TokenDigest,
    expires_at_unix: i64,
}

fn mint(state: &RcState, now_unix: i64) -> MintedPair {
    let refresh_token = ids::generate_token();
    let access_token = ids::generate_token();
    MintedPair {
        refresh_digest: state.digest(auth::DOMAIN_DEVICE_REFRESH, &refresh_token),
        access_digest: state.digest(auth::DOMAIN_DEVICE_ACCESS, &access_token),
        expires_at_unix: now_unix + state.config().access_token_ttl.as_secs().max(1) as i64,
        refresh_token,
        access_token,
    }
}

fn label_of(request: Option<IssueDeviceRequest>) -> Result<String, Response> {
    let label = request
        .and_then(|request| request.label)
        .unwrap_or_else(|| "device".to_string());
    if label.is_empty() || label.chars().count() > MAX_DEVICE_LABEL_CHARS {
        return Err(error(StatusCode::BAD_REQUEST));
    }
    Ok(label)
}

/// `POST /v1/devices` — issue a device credential.
///
/// Authenticated by an existing device access token, or, while no
/// account exists at all, by the one-time bootstrap token. The
/// bootstrap path mints the account and its first device in the same
/// transaction, which is what makes the token single-use: a second
/// attempt finds an account and is rejected as an ordinary bad
/// credential.
pub async fn issue_device(
    State(state): State<Arc<RcState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    request: Result<Json<IssueDeviceRequest>, JsonRejection>,
) -> Response {
    let ip = client_ip(state.config(), &headers, peer);
    if !state.allow_auth_request(ip) {
        return error(StatusCode::TOO_MANY_REQUESTS);
    }
    // An absent body is fine (the label is optional); a malformed one is not.
    let request = match request {
        Ok(Json(request)) => Some(request),
        Err(JsonRejection::MissingJsonContentType(_)) => None,
        Err(_) => return error(StatusCode::BAD_REQUEST),
    };
    let label = match label_of(request) {
        Ok(label) => label,
        Err(rejection) => return rejection,
    };

    let token = match auth::credential(&headers) {
        Ok(token) => token,
        Err(rejection) => return rejection,
    };
    let now_unix = ids::now_unix();
    let pair = mint(&state, now_unix);

    if auth::is_bootstrap(&state, &token) {
        let (refresh, access, expires, label_owned) = (
            pair.refresh_digest,
            pair.access_digest,
            pair.expires_at_unix,
            label.clone(),
        );
        let minted = match db(&state, move |state| {
            state
                .store()
                .bootstrap_account(&label_owned, &refresh, &access, expires, now_unix)
        })
        .await
        {
            Ok(minted) => minted,
            Err(rejection) => return rejection,
        };
        // `None` means an account already existed: the bootstrap token
        // has been spent, so it is no better than any other unknown
        // credential.
        let Some((account_id, device_id)) = minted else {
            return error(StatusCode::UNAUTHORIZED);
        };
        info!(account = %account_id, device = %device_id, "bootstrap account created");
        return issued(account_id, device_id, pair);
    }

    let device = match auth::controller(&state, &headers).await {
        Ok(device) => device,
        Err(rejection) => return rejection,
    };
    let (account_id, actor) = (device.account_id.clone(), device.device_id.clone());
    let (refresh, access, expires) = (
        pair.refresh_digest,
        pair.access_digest,
        pair.expires_at_unix,
    );
    let issued_id = match db(&state, move |state| {
        state.store().issue_device(
            &account_id,
            &actor,
            &label,
            &refresh,
            &access,
            expires,
            now_unix,
        )
    })
    .await
    {
        Ok(device_id) => device_id,
        Err(rejection) => return rejection,
    };
    info!(device = %issued_id, issued_by = %device.device_id, "device issued");
    issued(device.account_id, issued_id, pair)
}

fn issued(account_id: String, device_id: String, pair: MintedPair) -> Response {
    no_store(
        (
            StatusCode::CREATED,
            Json(IssuedDevice {
                account_id,
                device_id,
                refresh_token: pair.refresh_token,
                access_token: pair.access_token,
                access_expires_at: ids::rfc3339(pair.expires_at_unix),
            }),
        )
            .into_response(),
    )
}

/// `POST /v1/devices/token` — exchange a refresh token for an access token.
///
/// The refresh token itself is not rotated; a device that
/// needs new long-lived material is revoked and re-issued.
pub async fn refresh_token(
    State(state): State<Arc<RcState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = client_ip(state.config(), &headers, peer);
    if !state.allow_auth_request(ip) {
        return error(StatusCode::TOO_MANY_REQUESTS);
    }
    let device = match auth::refresh(&state, &headers).await {
        Ok(device) => device,
        Err(rejection) => return rejection,
    };
    let now_unix = ids::now_unix();
    let access_token = ids::generate_token();
    let digest = state.digest(auth::DOMAIN_DEVICE_ACCESS, &access_token);
    let expires_at_unix = now_unix + state.config().access_token_ttl.as_secs().max(1) as i64;
    let device_id = device.device_id.clone();
    if let Err(rejection) = db(&state, move |state| {
        state
            .store()
            .set_access_token(&device_id, &digest, expires_at_unix, now_unix)
    })
    .await
    {
        return rejection;
    }
    no_store(
        (
            StatusCode::OK,
            Json(AccessToken {
                access_token,
                access_expires_at: ids::rfc3339(expires_at_unix),
            }),
        )
            .into_response(),
    )
}

/// `GET /v1/devices` — list the account's devices, revoked ones included.
pub async fn list_devices(State(state): State<Arc<RcState>>, headers: HeaderMap) -> Response {
    let device = match auth::controller(&state, &headers).await {
        Ok(device) => device,
        Err(rejection) => return rejection,
    };
    let account_id = device.account_id.clone();
    let devices = match db(&state, move |state| state.store().list_devices(&account_id)).await {
        Ok(devices) => devices,
        Err(rejection) => return rejection,
    };
    let devices = devices
        .into_iter()
        .map(|row| DeviceView {
            device_id: row.device_id,
            label: row.label,
            created_at: ids::rfc3339(row.created_at_unix),
            last_seen_at: row.last_seen_at_unix.map(ids::rfc3339),
            revoked_at: row.revoked_at_unix.map(ids::rfc3339),
        })
        .collect();
    no_store((StatusCode::OK, Json(DeviceList { devices })).into_response())
}

/// `DELETE /v1/devices/{device_id}` — revoke a device.
///
/// Revocation cascades: the device's environment secrets stop
/// authenticating immediately, because every secret lookup joins the
/// owning device and checks that it is still live.
pub async fn revoke_device(
    State(state): State<Arc<RcState>>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let caller = match auth::controller(&state, &headers).await {
        Ok(device) => device,
        Err(rejection) => return rejection,
    };
    if !ids::valid_id(&device_id, "dev") {
        return error(StatusCode::NOT_FOUND);
    }
    let (account_id, actor, target) = (
        caller.account_id.clone(),
        caller.device_id.clone(),
        device_id.clone(),
    );
    let now_unix = ids::now_unix();
    let revoked = match db(&state, move |state| {
        state
            .store()
            .revoke_device(&account_id, &target, &actor, now_unix)
    })
    .await
    {
        Ok(revoked) => revoked,
        Err(rejection) => return rejection,
    };
    if !revoked {
        // Not a device of this account. We do not confirm whether it is
        // a device of some other one.
        return error(StatusCode::NOT_FOUND);
    }
    info!(device = %device_id, revoked_by = %caller.device_id, "device revoked");
    no_store(StatusCode::NO_CONTENT.into_response())
}
