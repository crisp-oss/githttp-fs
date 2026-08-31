// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! File-order index routes: read, write, delete the presentation order of one
//! directory's entries.
//!
//! The order is its own resource rather than a flag on the file routes, and
//! that is the load-bearing decision of the whole feature:
//!
//! - **Validation cannot be bypassed.** If the index were an ordinary file, a
//!   client could `PUT` it without whatever "this is an index" flag guarded
//!   the format, and store anything. Here the server owns the path, so there
//!   is no unvalidated way in — and `git.rs` refuses the path on every
//!   `/files` route to keep it that way.
//! - **The stored format stays private.** Callers send and receive a JSON
//!   array of leaf names; that the server keeps it in a `.order.json` blob is
//!   an implementation detail, exactly as git itself is throughout this API.
//! - **Receivers get a real event.** An order change delivers as
//!   `order.updated` / `order.deleted` carrying a snapshot, instead of a
//!   `file.updated` on a magic path that the receiver would have to sniff,
//!   parse and diff.
//!
//! Writes follow the shared handler shape documented in `routes/mod.rs`
//! (validate → lock → blocking git op → hook enqueue → maintenance arm). The
//! `limits.allowed_extensions` whitelist does not apply: the server, not the
//! caller, decides this path.
//!
//! Both the root directory and any sub-directory are addressable, which is why
//! each verb has two handlers: axum's `{*path}` wildcard needs at least one
//! segment, so `/order` (the repository root) and `/order/{*path}` are
//! separate routes over the same three inner functions.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    error::AppError, git, hooks::HookJob, order, routes::AuthorRequest, state::AppState,
    util::run_blocking, validate,
};

#[derive(Deserialize)]
pub struct WriteOrderRequest {
    pub author: AuthorRequest,
    /// The directory's entries, in the order they should be presented. Leaf
    /// names only — a trailing slash on a directory is accepted and
    /// normalised, and every entry must exist in that directory.
    pub order: Vec<String>,
    pub message: Option<String>,
}

#[derive(Deserialize)]
pub struct DeleteOrderRequest {
    pub author: AuthorRequest,
    pub message: Option<String>,
}

/// GET /:collection_id/:tenant_id/order
/// The repository root's order.
pub async fn read_order_root(
    State(state): State<AppState>,
    Path((collection_id, tenant_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    read(state, collection_id, tenant_id, String::new()).await
}

/// GET /:collection_id/:tenant_id/order/*path
/// Returns the order stored for that directory, or `404` when it has none.
pub async fn read_order(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, directory)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, AppError> {
    read(state, collection_id, tenant_id, directory).await
}

/// PUT /:collection_id/:tenant_id/order
/// Replaces the repository root's order.
pub async fn write_order_root(
    State(state): State<AppState>,
    Path((collection_id, tenant_id)): Path<(String, String)>,
    Json(body): Json<WriteOrderRequest>,
) -> Result<impl IntoResponse, AppError> {
    write(state, collection_id, tenant_id, String::new(), body).await
}

/// PUT /:collection_id/:tenant_id/order/*path
/// Replaces the order stored for that directory, commits it, and fires one
/// `order.updated` hook carrying the resulting order.
///
/// Every entry must resolve inside that directory in the last committed state
/// (`400` otherwise) — the check runs under the tenant write lock, so it
/// cannot go stale before the commit it drives. Writing the order the index
/// already holds is a no-op: no commit, no hook, HEAD's sha in the response.
pub async fn write_order(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, directory)): Path<(String, String, String)>,
    Json(body): Json<WriteOrderRequest>,
) -> Result<impl IntoResponse, AppError> {
    write(state, collection_id, tenant_id, directory, body).await
}

/// DELETE /:collection_id/:tenant_id/order
/// Drops the repository root's order.
pub async fn delete_order_root(
    State(state): State<AppState>,
    Path((collection_id, tenant_id)): Path<(String, String)>,
    Json(body): Json<DeleteOrderRequest>,
) -> Result<impl IntoResponse, AppError> {
    delete(state, collection_id, tenant_id, String::new(), body).await
}

/// DELETE /:collection_id/:tenant_id/order/*path
/// Drops that directory's order, so it falls back to the ordinary listing
/// order, and fires one `order.deleted` hook. A directory with no order is a
/// `404`.
pub async fn delete_order(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, directory)): Path<(String, String, String)>,
    Json(body): Json<DeleteOrderRequest>,
) -> Result<impl IntoResponse, AppError> {
    delete(state, collection_id, tenant_id, directory, body).await
}

async fn read(
    state: AppState,
    collection_id: String,
    tenant_id: String,
    directory: String,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    // A directory, so the same rules as `prefix_path`: slashes trimmed, an
    // empty result meaning the repository root, `..`/`.`/`.git` rejected.
    let directory = validate::folder_path(&directory)?.to_string();

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, directory = %order::display_directory(&directory), "handling read order request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let directory_for_task = directory.clone();

    let stored_order = run_blocking(move || {
        git::GitOrder::read_order(&repo_path, &tenant_id, &directory_for_task)
    })
    .await?;

    Ok((
        StatusCode::OK,
        Json(json!({
            "directory": directory,
            "order": stored_order,
        })),
    ))
}

async fn write(
    state: AppState,
    collection_id: String,
    tenant_id: String,
    directory: String,
    body: WriteOrderRequest,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();
    let directory = validate::folder_path(&directory)?.to_string();

    // Everything judgeable from the values alone is settled before the
    // repository is opened; existence is checked in the git layer, under the
    // write lock.
    order::validate_order(&body.order)?;

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, directory = %order::display_directory(&directory), entry_count = body.order.len(), "handling write order request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let lock_key = format!("{}/{}", collection_id, tenant_id);
    let lock = state.get_repo_lock(&lock_key);
    let _lock_guard = lock.lock().await;

    let WriteOrderRequest {
        author,
        order: order_entries,
        message,
    } = body;

    let repo_path_for_maintenance = repo_path.clone();
    let tenant_id_for_task = tenant_id.clone();

    let (commit_sha, change) = run_blocking(move || {
        git::GitOrder::write_order(
            &repo_path,
            &tenant_id_for_task,
            &directory,
            &order_entries,
            message.as_deref(),
            &author.name,
            &author.email,
        )
    })
    .await?;

    // A `None` change means the index already held exactly this order: no
    // commit was created, so there is nothing to sync and no new objects for
    // maintenance to consolidate.
    let Some(change) = change else {
        tracing::debug!(tenant_id = %tenant_id, sha = %commit_sha, "order unchanged, no commit created");

        return Ok((StatusCode::OK, Json(json!({ "commit_sha": commit_sha }))));
    };

    tracing::debug!(tenant_id = %tenant_id, sha = %commit_sha, "order write committed, enqueuing hook delivery");

    // Enqueued while the tenant write lock is still held, so per-tenant hook
    // order always matches commit order. The change lands on the index's own
    // path, which is what `HookJob::new` classifies into an order event.
    state.hook_queue.enqueue(
        &lock_key,
        HookJob::new(
            collection_id,
            tenant_id,
            commit_sha.clone(),
            Utc::now(),
            vec![change],
        ),
    );

    state
        .maintenance
        .schedule(&lock_key, repo_path_for_maintenance, lock.clone());

    Ok((StatusCode::OK, Json(json!({ "commit_sha": commit_sha }))))
}

async fn delete(
    state: AppState,
    collection_id: String,
    tenant_id: String,
    directory: String,
    body: DeleteOrderRequest,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();
    let directory = validate::folder_path(&directory)?.to_string();

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, directory = %order::display_directory(&directory), "handling delete order request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let lock_key = format!("{}/{}", collection_id, tenant_id);
    let lock = state.get_repo_lock(&lock_key);
    let _lock_guard = lock.lock().await;

    let DeleteOrderRequest { author, message } = body;

    let repo_path_for_maintenance = repo_path.clone();
    let tenant_id_for_task = tenant_id.clone();

    let (commit_sha, change) = run_blocking(move || {
        git::GitOrder::delete_order(
            &repo_path,
            &tenant_id_for_task,
            &directory,
            message.as_deref(),
            &author.name,
            &author.email,
        )
    })
    .await?;

    tracing::debug!(tenant_id = %tenant_id, sha = %commit_sha, "order deletion committed, enqueuing hook delivery");

    // Enqueued while the tenant write lock is still held, so per-tenant hook
    // order always matches commit order.
    state.hook_queue.enqueue(
        &lock_key,
        HookJob::new(
            collection_id,
            tenant_id,
            commit_sha.clone(),
            Utc::now(),
            vec![change],
        ),
    );

    state
        .maintenance
        .schedule(&lock_key, repo_path_for_maintenance, lock.clone());

    Ok((StatusCode::OK, Json(json!({ "commit_sha": commit_sha }))))
}
