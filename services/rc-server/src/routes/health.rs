//! `GET /healthz` — the one anonymous route.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use crate::no_store;

#[derive(Serialize)]
struct Health {
    status: &'static str,
    service: &'static str,
    /// Which RFC-0008 phase this build implements. Bumped as later
    /// phases land, so an operator can tell at a glance whether the
    /// session stream exists yet.
    phase: u8,
}

/// Liveness probe. Deliberately reveals nothing about accounts,
/// environments or queue depth.
pub async fn health() -> Response {
    no_store(
        (
            StatusCode::OK,
            Json(Health {
                status: "ok",
                service: "rebon-rc",
                phase: 1,
            }),
        )
            .into_response(),
    )
}
