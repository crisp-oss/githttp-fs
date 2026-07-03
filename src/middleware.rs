// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Authentication middleware: a single static Bearer API key.
//!
//! githttp-fs is designed to sit *behind* a trusted application server (the
//! CMS backend that owns user accounts and permissions), not to face end
//! users directly. One shared secret between those two machines is therefore
//! the right amount of auth — per-user tokens, scopes, and rate limiting are
//! the upstream application's job.
//!
//! The key comparison uses a constant-time equality check so that an
//! attacker probing the endpoint cannot use response-time differences to
//! discover the key one prefix byte at a time (a classic timing side
//! channel that `==` on byte slices would expose, since it bails at the
//! first mismatching byte).

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::Response,
    Json,
};
use serde_json::json;

use crate::{state::AppState, util::constant_time_eq};

const BEARER_PREFIX: &str = "Bearer ";

/// Validates the `Authorization: Bearer <key>` header on every request.
///
/// Layered onto the whole `/v1` router in `main::build_router`, so handlers
/// never have to think about auth. Rejections return the same generic 401
/// body whether the header is missing, malformed, or simply wrong — no
/// information about *why* auth failed is leaked to the caller.
pub async fn require_api_key(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    // Each `and_then` narrows: header present → valid UTF-8 → has the
    // "Bearer " prefix. Any failure collapses to `None`, i.e. unauthorised.
    let provided_key = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|header_value| header_value.to_str().ok())
        .and_then(|header_str| header_str.strip_prefix(BEARER_PREFIX));

    let authorised = provided_key
        .map(|key| constant_time_eq(key.as_bytes(), state.config.server.api_key.as_bytes()))
        .unwrap_or(false);

    if authorised {
        Ok(next.run(request).await)
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing or invalid API key" })),
        ))
    }
}
