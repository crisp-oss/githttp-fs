// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! The public health surface: `/v1/_health/*`.
//!
//! Two routes, and they are the **only** unauthenticated routes on the
//! content API:
//!
//! | Route | Answers |
//! |-------|---------|
//! | `GET /v1/_health/status` | what this process is: name, version, role, whether it takes writes |
//! | `GET /v1/_health/replication` | the replication picture, identical to what peers read from `/_replication/health` |
//!
//! **Why no API key.** These answer questions a caller has to be able to ask
//! *before* it holds a credential, or when the credential is exactly what is
//! in doubt: a load balancer deciding whether to route to this node, a
//! deployment probe waiting for a rollout to come up, a client discovering
//! which node of a set accepts writes, an operator's dashboard covering a
//! whole deployment. Requiring the content key for that turns a health probe
//! into a secret-distribution problem, and puts the product's key into every
//! monitor. `GET /v1` remains the *authenticated* probe, and it is the one to
//! use to verify a key; these two never look at one.
//!
//! **What that costs, and why it is bounded.** Nothing here touches tenant
//! content: no repository is opened, no path is resolved, no tenant is named
//! in a request or a response. `status` reads process-level facts only, so it
//! cannot be used to make this node do work. `replication` is the operator's
//! view of the node set, which does name peers and count repositories — see
//! [`health_replication`] for what that means for an operator who considers
//! their topology sensitive.
//!
//! They live under a `_health` prefix, which makes `_health` the one
//! collection id this API reserves — and it reserves it narrowly. Only these
//! two exact paths are static routes, so `/v1/_health/{tenant_id}/files` and
//! every other deeper tenant route still resolve normally; the sole collision
//! is a collection named `_health` holding a tenant named `status` or
//! `replication`, whose two-segment tenant route (`DELETE`) these shadow.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde_json::json;

use crate::{error::AppError, state::AppState};

/// GET /v1/_health/status
///
/// What this process is, in the smallest honest terms: which build is
/// running, which role it plays in a replicated set, whether it accepts
/// writes, and how long it has been up.
///
/// Deliberately **free of I/O**. Every field is read from the config or from
/// an atomic already in memory — no repository is opened, no directory is
/// walked, nothing is scheduled on the blocking pool. That is what makes it
/// safe to leave unauthenticated and safe to poll at whatever interval a
/// monitor likes: an anonymous caller cannot make this node do work by
/// asking.
///
/// It always answers `200`, including on a replica that is still
/// bootstrapping. The status code is about the process being alive enough to
/// answer; *what* it can serve is the body's job to say, via `status` and
/// `writable`. Collapsing the two into a status code would force a caller
/// that just wants the version to interpret a `503`, and would hide the one
/// state a bootstrapping node most needs to be able to report.
pub async fn health_status(State(state): State<AppState>) -> impl IntoResponse {
    let replica_status = &state.replica_status;

    // Role comes from the config rather than from the replica status object,
    // so a standalone node is distinguishable from a master — `is_replica` is
    // false on both. Same three values `/v1/_health/replication` reports.
    let role = match &state.config.replication {
        None => "standalone",
        Some(replication) if replication.is_replica() => "replica",
        Some(_) => "master",
    };

    // A replica that holds no content yet is refusing reads, and this is the
    // route that says so. Everything else is "healthy": this handler answers
    // from memory, so there is no other failure it could witness.
    let status = if replica_status.is_replica() && !replica_status.is_bootstrapped() {
        "bootstrapping"
    } else {
        "healthy"
    };

    let now = chrono::Utc::now().timestamp();

    Json(json!({
        "status": status,
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "role": role,
        // Whether a write sent here would be accepted at all. A replica
        // answers `423` to every write, so a failover-aware client can read
        // this once instead of discovering it from a rejection.
        "writable": !replica_status.is_replica(),
        "started_at": crate::replication::rfc3339::format(state.started_at),
        "uptime_secs": (now - state.started_at).max(0),
    }))
}

/// GET /v1/_health/replication
///
/// The replication picture as this node sees it: its own role and identity,
/// the master's reachability, and every replica following it. Byte-identical
/// to what peers read from `/_replication/health` with the replication key —
/// one struct and one builder behind two doors, differing only in who may
/// open them.
///
/// It answers on **every** node, whatever its role, including a standalone
/// one (as `role: "standalone"`). One probe then works against a whole
/// deployment without the prober knowing which node is which — a `404` here
/// would force exactly that knowledge into the monitoring config.
///
/// Unauthenticated, like its sibling, because the audience is monitoring
/// rather than the application. The trade is worth stating plainly: this body
/// names peer node ids, the master's URL (credentials stripped), the data-set
/// identity, and a repository count, so it describes a deployment's topology
/// to anyone who can reach the port. None of it is a credential — pulling
/// from a peer needs `replication.secret`, which appears nowhere here — but
/// an operator who treats internal hostnames as sensitive should keep this
/// port off the public internet, which is the same decision they already make
/// for the replication listener.
///
/// Unlike [`health_status`] it does touch the filesystem, through the
/// repository index (only on its first use, which is a directory scan), so it
/// goes through the blocking pool.
pub async fn health_replication(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    Ok((
        StatusCode::OK,
        Json(super::replication::health_for(&state).await?),
    ))
}
