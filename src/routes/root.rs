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

use axum::{extract::State, response::Redirect, Json};
use serde_json::json;

use crate::state::AppState;

/// GET / (relative to the `/v1` nest, i.e. `GET /v1`)
///
/// Responds `200` with the JSON body `{ "pong": true }` — JSON like every
/// other endpoint, so clients never need a special parser for this one
/// route. Reaching this handler at all means the API-key middleware already
/// accepted the request — the response carries nothing about tenants,
/// repositories, or server internals.
///
/// On a **replica** the body gains a `replica` object describing this node's
/// own replication state. That is the one thing a caller legitimately needs
/// from this route beyond "the key works": a client failing over between
/// nodes has to be able to tell whether the one it reached is current,
/// catching up, or cut off from its master. It is also the route the
/// read-only guard always lets through, so a node refusing every other
/// request can still explain itself.
///
/// A standalone server and a master answer exactly `{ "pong": true }`, as
/// they always have — the extra field appears only where it means something.
pub async fn ping(State(state): State<AppState>) -> Json<serde_json::Value> {
    if !state.replica_status.is_replica() {
        return Json(json!({ "pong": true }));
    }

    Json(json!({
        "pong": true,
        "replica": {
            // "ready" once this node holds content worth serving;
            // "bootstrapping" while a cold replica has none yet.
            "state": if state.replica_status.is_bootstrapped() {
                "ready"
            } else {
                "bootstrapping"
            },
            // Whether the live notification stream to the master is up.
            // False means this node is falling back on its poll interval —
            // still converging, just with more lag.
            "stream_connected": state.replica_status.stream_connected(),
            // When this node last compared its whole repository set against
            // the master, as an RFC 3339 date-time. Null before the first pass.
            "last_reconcile_at": state
                .replica_status
                .last_reconcile()
                .map(crate::replication::rfc3339::format),
            // Repositories known to be behind and awaiting a pull.
            "pending_repositories": state.replica_status.pending(),
            // One word on whether following is keeping up and will on its
            // own: "synced", "lagging", "stalled", or "halted". The full
            // story — what is halted and why — is on /v1/_health/replication.
            "sync": state.replica_status.sync_status().as_str(),
        }
    }))
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
