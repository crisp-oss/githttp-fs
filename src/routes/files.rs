// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! File CRUD routes: listing, read, existence check, write, delete, move.
//!
//! See `routes/mod.rs` for the shared handler shape (validate → lock →
//! blocking git op → hook enqueue → maintenance arm) that every write
//! handler here follows.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use std::collections::HashSet;

use crate::{
    error::AppError,
    git,
    hooks::HookJob,
    routes::AuthorRequest,
    seek::{SeekBody, SeekOptions},
    state::AppState,
    util::run_blocking,
    validate,
};

/// Query parameters for the listing endpoint. All optional: the bare
/// endpoint returns page 1 of the full recursive tree.
#[derive(Deserialize)]
pub struct ListFilesQuery {
    /// Folder to root the listing at (e.g. `/docs`); repo root if omitted.
    pub prefix_path: Option<String>,
    /// How many directory levels to descend from the listing root; full
    /// recursion if omitted.
    pub maximum_depth: Option<u32>,
    pub page: Option<usize>,
    pub per_page: Option<usize>,
}

// Pagination bounds shared with the commits endpoint: a generous default,
// and a hard cap so a caller cannot request unbounded response sizes.
const DEFAULT_PER_PAGE: usize = 100;
const MAX_PER_PAGE: usize = 500;

/// Body of the batch read endpoint: the paths to read, plus an optional
/// seek window applied to every file (see `seek.rs` for the field formats).
#[derive(Deserialize)]
pub struct BatchReadFilesRequest {
    pub files: Vec<String>,
    pub seek: Option<SeekBody>,
}

#[derive(Deserialize)]
pub struct WriteFileRequest {
    pub author: AuthorRequest,
    pub content: String,
    pub message: Option<String>,
}

#[derive(Deserialize)]
pub struct DeleteFileRequest {
    pub author: AuthorRequest,
    pub message: Option<String>,
}

#[derive(Deserialize)]
pub struct MoveFileRequest {
    pub author: AuthorRequest,
    pub destination: String,
    pub message: Option<String>,
}

/// GET /:collection_id/:tenant_id/files
/// Returns the repository contents as a recursive file tree.
/// Accepts an optional `prefix_path` query parameter (e.g. `?prefix_path=/docs`) to scope
/// the listing to a specific sub-directory. The path must be a folder and must
/// not escape the repository root (`..' components are rejected).
///
/// Pagination is *parent-based*: `page`/`per_page` window over the
/// root-level entries of the listing, each carrying its full subtree.
/// Combined with `maximum_depth` this lets clients bound response size on
/// arbitrarily large repositories.
pub async fn list_files(
    State(state): State<AppState>,
    Path((collection_id, tenant_id)): Path<(String, String)>,
    Query(query): Query<ListFilesQuery>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    // An empty sanitised prefix ("/" or "") means "repo root", which is the
    // same as no prefix at all — normalise it to None here so the git layer
    // only ever sees a meaningful prefix.
    let path_prefix: Option<String> = query
        .prefix_path
        .as_deref()
        .map(validate::folder_path)
        .transpose()?
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string());

    // maximum_depth=0 would mean "list nothing", which is more likely a
    // caller bug than an intent — reject it explicitly.
    let maximum_depth: Option<usize> = match query.maximum_depth {
        Some(0) => {
            return Err(AppError::InvalidOperation {
                reason: "maximum_depth must be at least 1".to_string(),
            })
        }
        Some(d) => Some(d as usize),
        None => None,
    };

    let page = query.page.unwrap_or(1).max(1);
    let per_page = query
        .per_page
        .unwrap_or(DEFAULT_PER_PAGE)
        .clamp(1, MAX_PER_PAGE);

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, path_prefix = ?path_prefix, maximum_depth = ?maximum_depth, page = page, per_page = per_page, "handling list files request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let tenant_id_for_task = tenant_id.clone();

    let (tree, has_more) = run_blocking(move || {
        git::GitFiles::list_files(
            &repo_path,
            &tenant_id_for_task,
            path_prefix.as_deref(),
            maximum_depth,
            page,
            per_page,
        )
    })
    .await?;

    tracing::debug!(tenant_id = %tenant_id, page = page, returned = tree.len(), has_more = has_more, "list files tree response ready");

    Ok(Json(json!({
        "page": page,
        "per_page": per_page,
        "has_more": has_more,
        "files": tree,
    })))
}

/// GET /:collection_id/:tenant_id/files/*path
/// Returns the file content and path as JSON.
///
/// Optional `seek_*` query parameters (`seek_from_line_starts_with`,
/// `seek_to_line_starts_with`, `seek_lines_maximum`) narrow `content` to a
/// line window — see `seek.rs` for the exact semantics and accepted
/// formats. The seek runs inside the git read so it can scan the blob as a
/// line stream instead of decoding it whole.
pub async fn read_file(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, file_path)): Path<(String, String, String)>,
    Query(seek): Query<SeekOptions>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();
    let file_path = validate::file_path(&file_path)?.to_string();

    let seek = seek.parse()?;

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, path = %file_path, seek = ?seek, "handling read file request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let file_path_for_task = file_path.clone();
    let tenant_id_for_task = tenant_id.clone();

    let content = run_blocking(move || {
        git::GitFiles::read_file(&repo_path, &tenant_id_for_task, &file_path_for_task, &seek)
    })
    .await?;

    Ok(Json(json!({
        "path": file_path,
        "content": content,
    })))
}

/// POST /:collection_id/:tenant_id/batch/files/read
/// Reads several files in one request. The response array is index-aligned
/// with the request's `files` array: each slot is either the same
/// `{ path, content }` object the single read route returns, or `null`
/// when that path does not exist (or is a folder). An optional `seek`
/// object applies the same line window to every file.
///
/// The whole request is rejected upfront (400) when a path is invalid,
/// paths are duplicated, or more than `limits.batch_read_maximum_files`
/// paths are asked for — a safety cap against unbounded response sizes.
/// A file that exists but cannot be represented (invalid UTF-8) fails the
/// whole batch with a 422, so `null` strictly means "not found".
pub async fn batch_read_files(
    State(state): State<AppState>,
    Path((collection_id, tenant_id)): Path<(String, String)>,
    Json(body): Json<BatchReadFilesRequest>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    if body.files.is_empty() {
        return Err(AppError::InvalidOperation {
            reason: "files must contain at least one path".to_string(),
        });
    }

    let maximum_files = state.config.limits.batch_read_maximum_files;

    if body.files.len() > maximum_files {
        return Err(AppError::InvalidOperation {
            reason: format!(
                "files must not contain more than {} paths ({} requested)",
                maximum_files,
                body.files.len()
            ),
        });
    }

    // Sanitise every path with the same rules as the single read route,
    // *before* the uniqueness check so that two spellings of the same file
    // (e.g. `a.md` and `/a.md`) are caught as duplicates.
    let file_paths = body
        .files
        .iter()
        .map(|raw_path| validate::file_path(raw_path).map(|path| path.to_string()))
        .collect::<Result<Vec<String>, AppError>>()?;

    let mut seen_paths = HashSet::new();

    for file_path in &file_paths {
        if !seen_paths.insert(file_path.as_str()) {
            return Err(AppError::InvalidOperation {
                reason: format!("files must be unique: '{}' is requested twice", file_path),
            });
        }
    }

    let seek = body.seek.unwrap_or_default().parse()?;

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, count = file_paths.len(), seek = ?seek, "handling batch read files request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let file_paths_for_task = file_paths.clone();

    let contents = run_blocking(move || {
        git::GitFiles::batch_read_files(&repo_path, &tenant_id, &file_paths_for_task, &seek)
    })
    .await?;

    let files: Vec<_> = file_paths
        .iter()
        .zip(contents)
        .map(|(path, content)| content.map(|content| json!({ "path": path, "content": content })))
        .collect();

    Ok(Json(json!({ "files": files })))
}

/// HEAD /:collection_id/:tenant_id/files/*path
/// Returns 200 with no body when the file exists in HEAD, 404 otherwise.
/// Cheaper than GET as the blob content is never loaded.
pub async fn file_exists(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, file_path)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();
    let file_path = validate::file_path(&file_path)?.to_string();

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, path = %file_path, "handling file existence request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    run_blocking(move || git::GitFiles::file_exists(&repo_path, &tenant_id, &file_path)).await?;

    Ok(StatusCode::OK)
}

/// PUT /:collection_id/:tenant_id/files/*path
/// Creates or updates a file, commits the change, and fires a hook.
///
/// PUT is idempotent by design: the caller does not need to know whether the
/// file exists. The server decides created-vs-updated from HEAD's tree and
/// reflects that in both the auto-generated commit message and the hook
/// event kind. Idempotency extends to content: re-PUTting a file with the
/// exact content HEAD already holds creates no commit and fires no hook —
/// the response carries HEAD's sha instead. This is also the endpoint that
/// lazily initialises a tenant repository on first use — there is no
/// explicit "create tenant" call.
pub async fn write_file(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, file_path)): Path<(String, String, String)>,
    Json(body): Json<WriteFileRequest>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();
    let file_path = validate::file_path(&file_path)?.to_string();

    validate::file_extension(
        &file_path,
        state.config.limits.allowed_extensions.as_deref(),
    )?;

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, path = %file_path, "handling write file request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let lock_key = format!("{}/{}", collection_id, tenant_id);
    let lock = state.get_repo_lock(&lock_key);
    let _lock_guard = lock.lock().await;

    let WriteFileRequest {
        author,
        content,
        message,
    } = body;

    let repo_path_for_maintenance = repo_path.clone();

    let (commit_sha, file_change) = run_blocking(move || {
        git::GitFiles::write_file(
            &repo_path,
            &file_path,
            &content,
            message.as_deref(),
            &author.name,
            &author.email,
        )
    })
    .await?;

    // A `None` change means the content already matched HEAD: no commit was
    // created, so there is nothing for downstream systems to sync and no new
    // objects for maintenance to consolidate. The returned sha is HEAD's —
    // the commit whose tree already contains exactly this content.
    let Some(file_change) = file_change else {
        tracing::debug!(tenant_id = %tenant_id, sha = %commit_sha, "content unchanged, no commit created");

        return Ok((StatusCode::OK, Json(json!({ "commit_sha": commit_sha }))));
    };

    tracing::debug!(tenant_id = %tenant_id, sha = %commit_sha, "file write committed, enqueuing hook delivery");

    // Enqueued while the tenant write lock is still held, so per-tenant hook
    // order always matches commit order.
    state.hook_queue.enqueue(
        &lock_key,
        HookJob {
            tenant_id,
            commit_sha: commit_sha.clone(),
            committed_at: Utc::now(),
            file_changes: vec![file_change],
        },
    );

    state
        .maintenance
        .schedule(&lock_key, repo_path_for_maintenance, lock.clone());

    Ok((StatusCode::OK, Json(json!({ "commit_sha": commit_sha }))))
}

/// DELETE /:collection_id/:tenant_id/files/*path
/// Deletes a file, commits the removal, and fires a hook.
pub async fn delete_file(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, file_path)): Path<(String, String, String)>,
    Json(body): Json<DeleteFileRequest>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();
    let file_path = validate::file_path(&file_path)?.to_string();

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, path = %file_path, "handling delete file request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let lock_key = format!("{}/{}", collection_id, tenant_id);
    let lock = state.get_repo_lock(&lock_key);
    let _lock_guard = lock.lock().await;

    let DeleteFileRequest { author, message } = body;

    let repo_path_for_maintenance = repo_path.clone();
    let tenant_id_for_task = tenant_id.clone();

    let (commit_sha, file_change) = run_blocking(move || {
        git::GitFiles::delete_file(
            &repo_path,
            &tenant_id_for_task,
            &file_path,
            message.as_deref(),
            &author.name,
            &author.email,
        )
    })
    .await?;

    tracing::debug!(tenant_id = %tenant_id, sha = %commit_sha, "file deletion committed, enqueuing hook delivery");

    // Enqueued while the tenant write lock is still held, so per-tenant hook
    // order always matches commit order.
    state.hook_queue.enqueue(
        &lock_key,
        HookJob {
            tenant_id,
            commit_sha: commit_sha.clone(),
            committed_at: Utc::now(),
            file_changes: vec![file_change],
        },
    );

    state
        .maintenance
        .schedule(&lock_key, repo_path_for_maintenance, lock.clone());

    Ok((StatusCode::OK, Json(json!({ "commit_sha": commit_sha }))))
}

/// POST /:collection_id/:tenant_id/files/*path/move
/// Moves/renames a file to a new path in a single atomic commit, fires a
/// single hook with both the old and new paths so the receiver can
/// correlate the rename without losing attached metadata.
///
/// Axum cannot match a fixed suffix after a wildcard segment, so this handler
/// is registered on POST `/*path` and enforces the `/move` suffix itself.
pub async fn move_file(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, raw_path)): Path<(String, String, String)>,
    Json(body): Json<MoveFileRequest>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    // Enforce that the URL ends with /move — anything else on POST is not found.
    let from_path_raw = raw_path
        .strip_suffix("/move")
        .ok_or_else(|| AppError::InvalidPath {
            reason: "POST on a file path must end with /move".to_string(),
        })?;

    let from_path = validate::file_path(from_path_raw)?.to_string();
    let to_path = validate::file_path(&body.destination)?.to_string();

    // Only the destination is checked against the whitelist: files written
    // before the whitelist was configured must remain movable.
    validate::file_extension(&to_path, state.config.limits.allowed_extensions.as_deref())?;

    tracing::debug!(
        collection_id = %collection_id,
        tenant_id = %tenant_id,
        from_path = %from_path,
        to_path = %to_path,
        "handling move file request"
    );

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let lock_key = format!("{}/{}", collection_id, tenant_id);
    let lock = state.get_repo_lock(&lock_key);
    let _lock_guard = lock.lock().await;

    let MoveFileRequest {
        author,
        destination: _,
        message,
    } = body;

    let repo_path_for_maintenance = repo_path.clone();
    let tenant_id_for_task = tenant_id.clone();

    let (commit_sha, file_change) = run_blocking(move || {
        git::GitFiles::move_file(
            &repo_path,
            &tenant_id_for_task,
            &from_path,
            &to_path,
            message.as_deref(),
            &author.name,
            &author.email,
        )
    })
    .await?;

    tracing::debug!(tenant_id = %tenant_id, sha = %commit_sha, "file move committed, enqueuing hook delivery");

    // Enqueued while the tenant write lock is still held, so per-tenant hook
    // order always matches commit order.
    state.hook_queue.enqueue(
        &lock_key,
        HookJob {
            tenant_id,
            commit_sha: commit_sha.clone(),
            committed_at: Utc::now(),
            file_changes: vec![file_change],
        },
    );

    state
        .maintenance
        .schedule(&lock_key, repo_path_for_maintenance, lock.clone());

    Ok((StatusCode::OK, Json(json!({ "commit_sha": commit_sha }))))
}
