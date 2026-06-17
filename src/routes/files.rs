// Flavio
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use crate::{
    error::AppError, git, hooks::HookDelivery, routes::AuthorRequest, state::AppState,
    util::run_blocking, validate,
};

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

/// GET /:tenant_id/files
/// Returns the repository contents as a recursive file tree.
pub async fn list_files(
    State(state): State<AppState>,
    Path(tenant_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    tracing::debug!(tenant_id = %tenant_id, "handling list files request");

    let repo_path = state.config.server.repos_path.join(&tenant_id);

    let tenant_id_for_task = tenant_id.clone();

    let tree =
        run_blocking(move || git::GitFiles::list_files(&repo_path, &tenant_id_for_task)).await?;

    tracing::debug!(tenant_id = %tenant_id, "list files tree response ready");

    Ok(Json(json!(tree)))
}

/// GET /:tenant_id/files/*path
/// Returns the file content and path as JSON.
pub async fn read_file(
    State(state): State<AppState>,
    Path((tenant_id, file_path)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();
    let file_path = validate::file_path(&file_path)?.to_string();

    tracing::debug!(tenant_id = %tenant_id, path = %file_path, "handling read file request");

    let repo_path = state.config.server.repos_path.join(&tenant_id);

    let file_path_for_task = file_path.clone();
    let tenant_id_for_task = tenant_id.clone();

    let content = run_blocking(move || {
        git::GitFiles::read_file(&repo_path, &tenant_id_for_task, &file_path_for_task)
    })
    .await?;

    Ok(Json(json!({
        "path": file_path,
        "content": content,
    })))
}

/// PUT /:tenant_id/files/*path
/// Creates or updates a file, commits the change, and fires a hook.
pub async fn write_file(
    State(state): State<AppState>,
    Path((tenant_id, file_path)): Path<(String, String)>,
    Json(body): Json<WriteFileRequest>,
) -> Result<impl IntoResponse, AppError> {
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();
    let file_path = validate::file_path(&file_path)?.to_string();

    tracing::debug!(tenant_id = %tenant_id, path = %file_path, "handling write file request");

    let repo_path = state.config.server.repos_path.join(&tenant_id);

    let lock = state.get_repo_lock(&tenant_id);
    let _lock_guard = lock.lock().await;

    let WriteFileRequest {
        author,
        content,
        message,
    } = body;

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

    tracing::debug!(tenant_id = %tenant_id, sha = %commit_sha, "file write committed, spawning hook delivery");

    HookDelivery::spawn(
        state.http_client.clone(),
        state.config.clone(),
        tenant_id,
        commit_sha.clone(),
        Utc::now(),
        vec![file_change],
    );

    Ok((StatusCode::OK, Json(json!({ "commit_sha": commit_sha }))))
}

/// DELETE /:tenant_id/files/*path
/// Deletes a file, commits the removal, and fires a hook.
pub async fn delete_file(
    State(state): State<AppState>,
    Path((tenant_id, file_path)): Path<(String, String)>,
    Json(body): Json<DeleteFileRequest>,
) -> Result<impl IntoResponse, AppError> {
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();
    let file_path = validate::file_path(&file_path)?.to_string();

    tracing::debug!(tenant_id = %tenant_id, path = %file_path, "handling delete file request");

    let repo_path = state.config.server.repos_path.join(&tenant_id);

    let lock = state.get_repo_lock(&tenant_id);
    let _lock_guard = lock.lock().await;

    let DeleteFileRequest { author, message } = body;

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

    tracing::debug!(tenant_id = %tenant_id, sha = %commit_sha, "file deletion committed, spawning hook delivery");

    HookDelivery::spawn(
        state.http_client.clone(),
        state.config.clone(),
        tenant_id,
        commit_sha.clone(),
        Utc::now(),
        vec![file_change],
    );

    Ok((StatusCode::OK, Json(json!({ "commit_sha": commit_sha }))))
}

/// POST /:tenant_id/files/*path/move
/// Moves/renames a file to a new path in a single atomic commit, fires a
/// single hook with both the old and new paths so the receiver can
/// correlate the rename without losing attached metadata.
///
/// Axum cannot match a fixed suffix after a wildcard segment, so this handler
/// is registered on POST `/*path` and enforces the `/move` suffix itself.
pub async fn move_file(
    State(state): State<AppState>,
    Path((tenant_id, raw_path)): Path<(String, String)>,
    Json(body): Json<MoveFileRequest>,
) -> Result<impl IntoResponse, AppError> {
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    // Enforce that the URL ends with /move — anything else on POST is not found.
    let from_path_raw = raw_path
        .strip_suffix("/move")
        .ok_or_else(|| AppError::InvalidPath {
            reason: "POST on a file path must end with /move".to_string(),
        })?;

    let from_path = validate::file_path(from_path_raw)?.to_string();
    let to_path = validate::file_path(&body.destination)?.to_string();

    tracing::debug!(
        tenant_id = %tenant_id,
        from_path = %from_path,
        to_path = %to_path,
        "handling move file request"
    );

    let repo_path = state.config.server.repos_path.join(&tenant_id);

    let lock = state.get_repo_lock(&tenant_id);
    let _lock_guard = lock.lock().await;

    let MoveFileRequest {
        author,
        destination: _,
        message,
    } = body;

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

    tracing::debug!(tenant_id = %tenant_id, sha = %commit_sha, "file move committed, spawning hook delivery");

    HookDelivery::spawn(
        state.http_client.clone(),
        state.config.clone(),
        tenant_id,
        commit_sha.clone(),
        Utc::now(),
        vec![file_change],
    );

    Ok((StatusCode::OK, Json(json!({ "commit_sha": commit_sha }))))
}
