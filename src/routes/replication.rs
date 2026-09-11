// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! The replication surface: what a node serves to replicas following it.
//!
//! Three routes, mounted under `/_replication` (its own server) — its own top-level,
//! independently versioned prefix rather than a branch of `/v1`, for the
//! reasons laid out in `main::build_replication_router` — and guarded by
//! `replication.secret` rather than the content API's key:
//!
//! | Route | Answers |
//! |-------|---------|
//! | `GET /_replication/state` | every repository this node holds, with its HEAD sha |
//! | `GET /_replication/{collection_id}/{tenant_id}/pack` | a packfile of what the caller is missing |
//! | `GET /_replication/events` | a never-ending NDJSON stream of change notifications |
//!
//! They are served by masters *and* replicas alike, so replicas can chain
//! off one another for geographic tiering. Nothing here writes, which is why
//! the read-only guard — layered on `/v1` — never needs to consider them.

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderName, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    error::AppError,
    git::GitReplication,
    replication::{
        self, Issue, ReplicationEvent, ReplicationHealth, RepositoryEntry, RepositoryListing,
        EVENT_HEARTBEAT_SECS, HEAD_SHA_HEADER, INSTANCE_HEADER, NODE_ID_HEADER, PENDING_HEADER,
        PROTOCOL_HEADER, PROTOCOL_VERSION, REPOSITORIES_HEADER, SYNC_HEADER,
    },
    state::AppState,
    util::run_blocking,
    validate,
};

/// How many pack chunks may sit between the blocking pack builder and the
/// response body before the builder blocks. Small on purpose: the point of
/// streaming is that neither side accumulates the whole pack.
const PACK_CHANNEL_DEPTH: usize = 8;

/// Typed form of [`HEAD_SHA_HEADER`] for the response builder.
const HEAD_SHA_RESPONSE_HEADER: HeaderName = HeaderName::from_static(HEAD_SHA_HEADER);

/// The packfile response's protocol marker. Every other reply states its
/// version inside the JSON body; this one is raw binary, so the header is the
/// only place it can go.
const PROTOCOL_RESPONSE_HEADER: HeaderName = HeaderName::from_static(PROTOCOL_HEADER);

/// Depth of the per-connection notification buffer. A replica that cannot
/// keep up with this loses frames, which is harmless by design — see
/// `replication::ReplicationNotifier`.
const EVENT_CHANNEL_DEPTH: usize = 64;

#[derive(Deserialize)]
pub struct PackQuery {
    /// The commit the caller already holds. Everything reachable from it is
    /// excluded from the pack. Omitted means "send me everything".
    pub have: Option<String>,
}

/// GET /_replication/state
///
/// The whole-state snapshot a replica diffs against its own disk. Deletions
/// are inferable from it only when it carries `complete: true` — see
/// [`RepositoryListing`] — and it names the data-set identity this node
/// serves, which is what a fresh replica pins.
///
/// A **replica** that is still bootstrapping, or that has not paired with
/// its own master yet, answers `503` instead of a listing. Its listing would
/// be empty and complete, and a replica chained below it would read that as
/// "delete everything" — the exact mistake this route must not enable.
pub async fn replication_state(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    // The state poll is a replica's regular heartbeat, so it is where the
    // roster learns that this peer is alive and how far behind it says it is —
    // no extra request, and the numbers are as fresh as its last pass.
    let peer = peer_identity(&headers);

    if let Some(node_id) = &peer.node_id {
        state
            .replica_registry
            .note_request(node_id, peer.repositories, peer.pending, peer.sync);
    }

    let identity = state.identity.current();

    if state.replica_status.is_replica()
        && (!state.replica_status.is_bootstrapped() || identity.is_none())
    {
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "5")],
            Json(json!({
                "error": "this replica has not finished its first catch-up and cannot describe its state yet"
            })),
        )
            .into_response());
    }

    let index = state.repository_index.clone();

    let scan = run_blocking(move || Ok(index.snapshot())).await?;

    let repositories = scan
        .repositories
        .into_iter()
        .map(|repository| RepositoryEntry {
            collection_id: repository.collection_id,
            tenant_id: repository.tenant_id,
            head_sha: repository.head_sha,
        })
        .collect::<Vec<_>>();

    tracing::debug!(
        repositories = repositories.len(),
        complete = scan.complete,
        "serving replication state"
    );

    Ok((
        StatusCode::OK,
        Json(RepositoryListing {
            protocol: PROTOCOL_VERSION,
            identity,
            repositories,
            complete: scan.complete,
        }),
    )
        .into_response())
}

/// GET /_replication/:collection_id/:tenant_id/pack?have=<sha>
///
/// Streams a packfile holding every object reachable from this repository's
/// HEAD but not from `have`. The resulting HEAD is announced in the
/// `X-Replication-Head-Sha` response header, because the body is a binary
/// stream with nowhere to put it and the replica needs to know which commit
/// to fast-forward to once the import succeeds.
///
/// HEAD is resolved *before* the walk and pinned for it, so the pack matches
/// the announced sha exactly even if a commit lands mid-transfer. That
/// commit simply ships on the next sync.
pub async fn replication_pack(
    State(state): State<AppState>,
    Path((collection_id, tenant_id)): Path<(String, String)>,
    Query(query): Query<PackQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let peer_node_id = peer_identity(&headers).node_id;

    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    // `have` is caller-supplied and goes straight into a git lookup, so it
    // gets the same hexadecimal-only treatment as every other sha on this
    // API — no revspec can reach libgit2 through it.
    let have = match &query.have {
        Some(have) => Some(validate::commit_sha(have)?.to_string()),
        None => None,
    };

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    if !repo_path.exists() {
        return Err(AppError::TenantNotFound { tenant_id });
    }

    let head_probe_path = repo_path.clone();

    let head_sha = run_blocking(move || Ok(GitReplication::head_sha(&head_probe_path)))
        .await?
        .ok_or_else(|| AppError::TenantNotFound {
            tenant_id: tenant_id.clone(),
        })?;

    // Counted here rather than after the stream drains: the count is "packs
    // this peer was served", and a build that dies halfway still told us the
    // peer was here and asking. Tying it to completion would also mean writing
    // to the roster from inside the blocking task for no gain.
    if let Some(node_id) = &peer_node_id {
        state.replica_registry.note_pack_delivered(node_id);
    }

    tracing::debug!(
        collection_id = %collection_id,
        tenant_id = %tenant_id,
        head = %head_sha,
        have = ?have,
        peer = peer_node_id.as_deref().unwrap_or("(anonymous)"),
        "building replication pack"
    );

    let (sender, receiver) = mpsc::channel::<Result<Vec<u8>, std::io::Error>>(PACK_CHANNEL_DEPTH);

    let build_tenant_id = tenant_id.clone();
    let build_head_sha = head_sha.clone();

    // The pack builder is synchronous libgit2 work, so it runs on the
    // blocking pool and pushes chunks across as it produces them. A closed
    // channel (the client hung up) aborts the build rather than letting it
    // run to completion for nobody.
    tokio::task::spawn_blocking(move || {
        let outcome = GitReplication::build_pack(
            &repo_path,
            &build_tenant_id,
            &build_head_sha,
            have.as_deref(),
            |chunk| sender.blocking_send(Ok(chunk.to_vec())).is_ok(),
        );

        if let Err(err) = outcome {
            tracing::warn!(
                tenant_id = %build_tenant_id,
                err = %err,
                "replication pack build failed"
            );

            // The consumer sees a body error rather than a truncated pack
            // that would import as corrupt on the far side.
            let _ = sender.blocking_send(Err(std::io::Error::other(err.to_string())));
        }
    });

    let body = Body::from_stream(ReceiverStream::new(receiver));
    let protocol_value = PROTOCOL_VERSION.to_string();

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/x-git-packfile"),
            (HEAD_SHA_RESPONSE_HEADER, head_sha.as_str()),
            (PROTOCOL_RESPONSE_HEADER, protocol_value.as_str()),
        ],
        body,
    )
        .into_response())
}

/// GET /_replication/events
///
/// A response that never completes: one NDJSON frame per repository change,
/// plus a heartbeat while nothing is happening.
///
/// The connection is dialled by the replica, which is what keeps replicas
/// free of inbound network exposure and lets them join without the master
/// knowing they exist. Frames carry identities and shas, never content —
/// a replica reacts by asking what the state is, so a lost frame costs
/// nothing beyond latency.
pub async fn replication_events(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let peer = peer_identity(&headers);
    let peer_node_id = peer.node_id.clone();

    // Presence is the lifetime of this connection, which is why it is marked
    // open here and closed when the task below ends: an open socket is a fact,
    // where a timeout would only ever be a guess.
    //
    // A node id already connected from a *different process* is refused
    // with `409`: two replicas sharing a name would share a roster row and
    // hide each other. The same process reconnecting (same instance token)
    // replaces its old connection. A peer that sends an id but no instance
    // token is an older build; it is let through without the check rather
    // than locked out of a feature it predates.
    if let Some(node_id) = &peer_node_id {
        state
            .replica_registry
            .note_request(node_id, peer.repositories, peer.pending, peer.sync);

        let instance = peer.instance.as_deref().unwrap_or("");

        if let Err(collision) = state.replica_registry.stream_opened(node_id, instance) {
            tracing::error!(
                node_id = %collision.node_id,
                "refusing notification stream: another process is already connected under this node id; \
                 every replica needs a unique replication.node_id"
            );

            state.issues.raise(
                &replication::Issues::collision_key(&collision.node_id),
                Issue::NodeIdCollision {
                    node_id: collision.node_id.clone(),
                    since: chrono::Utc::now().timestamp(),
                },
            );

            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "node id '{}' is already connected from another process",
                        collision.node_id
                    )
                })),
            )
                .into_response();
        }

        if state
            .issues
            .clear(&replication::Issues::collision_key(node_id))
        {
            tracing::info!(node_id = %node_id, "node id collision cleared");
        }
    }

    let (sender, receiver) = mpsc::channel::<Result<Vec<u8>, std::io::Error>>(EVENT_CHANNEL_DEPTH);

    let mut notifications = state.replication.subscribe();

    tracing::info!(
        connected_replicas = state.replication.connected_replicas(),
        peer = peer_node_id.as_deref().unwrap_or("(anonymous)"),
        "replication notification stream opened"
    );

    let registry = state.replica_registry.clone();
    let stream_peer = peer_node_id.clone();
    let stream_instance = peer.instance.clone().unwrap_or_default();

    let own_node_id = state
        .config
        .replication
        .as_ref()
        .map(|replication| replication.node_id().to_string())
        .unwrap_or_default();

    let own_identity = state.identity.current();

    tokio::spawn(async move {
        let heartbeat = tokio::time::Duration::from_secs(EVENT_HEARTBEAT_SECS);

        // The session opens by stating who this node is and which protocol it
        // speaks, so the version is negotiated once per connection rather than
        // repeated on every frame — and the replica learns the master's
        // identity here instead of waiting for its next health fetch.
        if let Ok(mut hello) = serde_json::to_vec(&ReplicationEvent::Hello {
            protocol: PROTOCOL_VERSION,
            node_id: own_node_id,
            identity: own_identity,
        }) {
            hello.push(b'\n');

            if sender.send(Ok(hello)).await.is_err() {
                return;
            }
        }

        loop {
            let frame = tokio::select! {
                received = notifications.recv() => match received {
                    Ok(event) => event,

                    // Lagged: this connection could not keep up and frames
                    // were dropped. Closing is the honest response — the
                    // replica reconnects and, because every reconnect
                    // triggers a full reconcile, recovers strictly more
                    // than the frames it lost.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            skipped = skipped,
                            "replica fell behind the notification stream, closing it to force a reconcile"
                        );

                        break;
                    }

                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },

                _ = tokio::time::sleep(heartbeat) => ReplicationEvent::Heartbeat,
            };

            let Ok(mut line) = serde_json::to_vec(&frame) else {
                continue;
            };

            line.push(b'\n');

            // A send failure means the replica disconnected; the task ends
            // and its broadcast subscription drops with it.
            if sender.send(Ok(line)).await.is_err() {
                break;
            }
        }

        if let Some(node_id) = &stream_peer {
            registry.stream_closed(node_id, &stream_instance);
        }

        tracing::info!(
            peer = stream_peer.as_deref().unwrap_or("(anonymous)"),
            "replication notification stream closed"
        );
    });

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(ReceiverStream::new(receiver)),
    )
        .into_response()
}

/// What a peer said about itself on a request's headers. Everything is
/// optional — see [`peer_identity`].
struct PeerHeaders {
    node_id: Option<String>,
    instance: Option<String>,
    repositories: Option<usize>,
    pending: Option<usize>,
    sync: Option<String>,
}

/// Reads the identity a peer volunteered, and the numbers only it can know,
/// off the request headers.
///
/// Everything here is optional. A peer that sends no id — or one that fails
/// `validate::node_id`, which bounds what is stored in the roster and echoed
/// into logs — stays out of the roster entirely rather than being rejected:
/// identity is telemetry, and the Bearer key is what actually gates the
/// surface, so a missing or malformed header must never fail a request that
/// is otherwise perfectly valid. The one exception is a node id that
/// *collides*, which the events route refuses — see there.
fn peer_identity(headers: &HeaderMap) -> PeerHeaders {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };

    let number = |name: &str| header(name).and_then(|value| value.parse::<usize>().ok());

    let node_id = header(NODE_ID_HEADER).and_then(|node_id| match validate::node_id(&node_id) {
        Ok(_) => Some(node_id),
        Err(err) => {
            tracing::debug!(err = %err, "ignoring invalid peer node id");

            None
        }
    });

    // The instance token and sync word are bounded the same way the node id
    // is, since both land in memory and in log lines.
    let bounded = |name: &str| header(name).filter(|value| validate::node_id(value).is_ok());

    PeerHeaders {
        node_id,
        instance: bounded(INSTANCE_HEADER),
        repositories: number(REPOSITORIES_HEADER),
        pending: number(PENDING_HEADER),
        sync: bounded(SYNC_HEADER),
    }
}

/// GET /_replication/health
///
/// The replication picture as this node sees it, for **peers** — which is what
/// lets a replica learn the master's roster, since a replica holds the
/// replication key and not necessarily the content one. Byte-identical to what
/// `GET /v1/_health/replication` serves the operator; the two differ only in
/// who may open them — that one is public, this one takes the replication
/// secret.
pub async fn replication_health(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let peer = peer_identity(&headers);

    if let Some(node_id) = &peer.node_id {
        state
            .replica_registry
            .note_request(node_id, peer.repositories, peer.pending, peer.sync);
    }

    Ok((StatusCode::OK, Json(health_for(&state).await?)))
}

/// Counts this node's repositories, then builds the health picture.
///
/// Shared with the operator-facing door, `GET /v1/_health/replication`
/// ([`super::health::health_replication`]): one builder, two routes, so the
/// two audiences can never be shown different pictures of the same node.
///
/// The count comes from the in-memory index. Its first use runs a directory
/// scan, so it still goes through the blocking pool like every other
/// filesystem walk in this codebase rather than the request thread.
pub async fn health_for(state: &AppState) -> Result<ReplicationHealth, AppError> {
    let index = state.repository_index.clone();

    let repositories = run_blocking(move || Ok(index.snapshot().repositories.len())).await?;

    Ok(replication::build_health(state, repositories))
}
