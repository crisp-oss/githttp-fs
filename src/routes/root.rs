// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! The API root endpoint: an authenticated no-op.
//!
//! Clients (and deployment tooling) need a way to verify that their API key
//! is accepted before firing real traffic — without creating a tenant,
//! writing a file, or otherwise mutating anything. A `GET` on the API root
//! is that probe: it goes through the exact same Bearer-key middleware as
//! every other route, so a `200` proves the credential works end to end and
//! a `401` proves it does not. Nothing else is checked, which also makes it
//! a natural liveness probe for monitors that hold the key.
//!
//! The server root `/` itself is not part of the API: anything sent there
//! is redirected to `/v1` so that a caller who forgot the version prefix is
//! pointed at the right place instead of getting a bare 404.

use axum::{response::Redirect, Json};
use serde_json::json;

/// GET / (relative to the `/v1` nest, i.e. `GET /v1`)
///
/// Responds `200` with the JSON body `{ "pong": true }` — JSON like every
/// other endpoint, so clients never need a special parser for this one
/// route. Reaching this handler at all means the API-key middleware already
/// accepted the request — there is deliberately no logic here, so the
/// response can never leak anything about tenants, repositories, or server
/// internals.
pub async fn ping() -> Json<serde_json::Value> {
    Json(json!({ "pong": true }))
}

/// ANY /
///
/// Registered on the outer (unauthenticated) router: every request to the
/// bare server root, whatever its method, is answered with a `308 Permanent
/// Redirect` to `/v1`. 308 (rather than 301/302) so the method and body are
/// preserved across the redirect. No auth is required here — the response
/// carries nothing but the well-known API prefix, and the redirect target
/// enforces the API key itself.
pub async fn redirect_to_api_root() -> Redirect {
    Redirect::permanent("/v1")
}
