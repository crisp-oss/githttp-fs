// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Request guards: Bearer API keys, and the read-only gate on a replica.
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
//!
//! Two further guards live here, both inert unless `[replication]` is
//! configured: `require_replication_key` protects the replication server
//! (its own listener, its own `replication.secret`), and
//! `enforce_replica_read_only` is what makes a replica a replica — it turns
//! every mutating request on the content API into a `423`.

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
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

/// Validates the `Authorization: Bearer <secret>` header on the replication
/// server against `replication.secret`.
///
/// Still required even though that server listens on its own port: the port
/// keeps the surface off whatever proxies the content API, and the secret
/// keeps it shut to anything that reaches the port anyway. Neither is a
/// substitute for the other.
///
/// A separate credential from `server.api_key` on purpose. The replication
/// surface hands out whole repositories as packfiles and a live change
/// stream — a different grant with a different blast radius from the content
/// API — so it gets a key that can be rotated, scoped, and network-restricted
/// on its own. It also means a compromised content key does not let an
/// attacker attach to the notification stream and enumerate every tenant.
///
/// Rejects with the same opaque 401 as the content API when replication is
/// not configured at all: a node that does not replicate should not announce
/// that fact to an unauthenticated caller.
pub async fn require_replication_key(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let provided_key = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|header_value| header_value.to_str().ok())
        .and_then(|header_str| header_str.strip_prefix(BEARER_PREFIX));

    let authorised = match (&state.config.replication, provided_key) {
        (Some(replication), Some(key)) => {
            constant_time_eq(key.as_bytes(), replication.secret.as_bytes())
        }
        _ => false,
    };

    if authorised {
        Ok(next.run(request).await)
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing or invalid replication key" })),
        ))
    }
}

/// Guards a replica: refuses writes, and refuses everything while a cold
/// replica is still bootstrapping.
///
/// **Why 423 and not 405, and not 503.** A replica is not a server that
/// lacks these endpoints, so `405` is wrong; nor is it a server having a bad
/// moment, so `503` is wrong too — `503` promises that waiting helps, and
/// waiting never makes a replica accept a write. `423 Locked` says the true
/// thing: this resource is write-locked *on this node*, go to the master.
/// That is also why the refusal carries no `Retry-After`: there is no delay
/// after which the same request would succeed here, and inventing one would
/// send a failover-aware client back to the wrong node on a timer instead of
/// to the right one immediately. The bootstrap refusal below is the opposite
/// case — genuinely transient — and keeps both its `503` and its hint.
///
/// **Why classification is not by verb.** `POST` is not a reliable write
/// signal on this API: `POST /batch/files/read` is a read, while `PUT`,
/// `DELETE`, and every other `POST` (move, reorder, revert, rollback, hook
/// replay) mutate or enqueue. So reads are recognised by method *and*, for
/// `POST`, by the one route suffix that is a read. Matching on the suffix
/// rather than the whole path keeps this correct whether or not the router
/// has stripped the `/v1` prefix by the time the guard runs.
///
/// **Writes are classified before the bootstrap gate**, so a write gets the
/// same `423` whatever this node's catch-up state is: "still bootstrapping"
/// is never the useful half of the answer to a request this node would go on
/// refusing once current.
///
/// The bootstrap gate covers the case where serving would be actively
/// misleading: a replica holding no repositories at all would answer `404`
/// for content that exists, which is worse than refusing. A replica that
/// merely restarted with content on disk is never gated — stale beats
/// absent. The API-key ping is always allowed through so monitors can see
/// *why* a node is refusing; the unauthenticated `/v1/_health` routes say the
/// same thing without a credential, and never reach this guard at all.
// The `Err` variant is an `axum::Response`, which clippy flags as large. It
// is the shape axum's `from_fn` middleware requires, so it cannot be boxed.
#[allow(clippy::result_large_err)]
pub async fn enforce_replica_read_only(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, Response> {
    if !state.replica_status.is_replica() {
        return Ok(next.run(request).await);
    }

    let path = request.uri().path();
    let method = request.method();

    let is_read = match *method {
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS => true,
        axum::http::Method::POST => path.ends_with("/batch/files/read"),
        _ => false,
    };

    // A write is refused here rather than after the bootstrap gate, and
    // refused with no retry hint: this node does not take writes now and
    // will not take them once it has caught up, so the only useful answer is
    // "wrong node", not "come back later".
    if !is_read {
        tracing::debug!(method = %method, path = %path, "refusing write on a read-only replica");

        return Err((
            StatusCode::LOCKED,
            Json(json!({
                "error": "this node is a read-only replica; writes must go to the master"
            })),
        )
            .into_response());
    }

    // The API-key ping stays answerable even while a cold replica is refusing
    // everything else, because it exists to *explain* the refusal: gating it
    // would leave an operator staring at a 503 with no way to ask why. The
    // public health routes need no exception here — they are nested outside
    // this guard entirely (see `main::build_router`), so a bootstrapping
    // replica answers `/v1/_health/status` and `/v1/_health/replication`
    // without this middleware ever seeing them.
    let is_diagnostic = method == axum::http::Method::GET && matches!(path, "/" | "/v1");

    if !is_diagnostic && !state.replica_status.is_bootstrapped() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "5")],
            Json(json!({
                "error": "replica is bootstrapping and holds no content yet"
            })),
        )
            .into_response());
    }

    Ok(next.run(request).await)
}
