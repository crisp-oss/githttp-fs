// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

use axum::{
    extract::{Path, Query, State},
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
pub struct ListCommitsQuery {
    pub page: Option<usize>,
    pub per_page: Option<usize>,
    pub file_path: Option<String>,
}

#[derive(Deserialize)]
pub struct RevertCommitRequest {
    pub author: AuthorRequest,
    pub message: Option<String>,
}

const DEFAULT_PER_PAGE: usize = 100;
const MAX_PER_PAGE: usize = 500;

/// GET /:collection_id/:tenant_id/commits?page=1&per_page=100
/// Returns a paginated list of commits without file content.
pub async fn list_commits(
    State(state): State<AppState>,
    Path((collection_id, tenant_id)): Path<(String, String)>,
    Query(query_params): Query<ListCommitsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();
    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let page = query_params.page.unwrap_or(1).max(1);
    let per_page = query_params
        .per_page
        .unwrap_or(DEFAULT_PER_PAGE)
        .clamp(1, MAX_PER_PAGE);
    let file_path: Option<String> = query_params
        .file_path
        .as_deref()
        .map(validate::file_path)
        .transpose()?
        .map(|p| p.to_string());

    tracing::debug!(tenant_id = %tenant_id, page = page, per_page = per_page, file_path = ?file_path, "handling list commits request");

    let tenant_id_for_task = tenant_id.clone();

    let (commits, has_more) = run_blocking(move || {
        git::GitCommits::list_commits(
            &repo_path,
            &tenant_id_for_task,
            page,
            per_page,
            file_path.as_deref(),
        )
    })
    .await?;

    tracing::debug!(tenant_id = %tenant_id, page = page, returned = commits.len(), has_more = has_more, "list commits response ready");

    Ok(Json(json!({
        "page": page,
        "per_page": per_page,
        "has_more": has_more,
        "commits": commits,
    })))
}

/// GET /:collection_id/:tenant_id/commits/:sha
/// Returns full commit detail: metadata, per-file diffs, and file content
/// at the point of the commit.
pub async fn get_commit(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, sha)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, sha = %sha, "handling get commit request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let tenant_id_for_task = tenant_id.clone();

    let commit_detail =
        run_blocking(move || git::GitCommits::get_commit(&repo_path, &tenant_id_for_task, &sha))
            .await?;

    tracing::debug!(
        tenant_id = %tenant_id,
        sha = %commit_detail.sha,
        file_count = commit_detail.files.len(),
        "get commit response ready"
    );

    Ok(Json(commit_detail))
}

/// POST /:collection_id/:tenant_id/commits/:sha/revert
/// Reverts all changes from the specified commit by creating a new inverse
/// commit. Fires individual hooks for each file that changes as a result.
pub async fn revert_commit(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, sha)): Path<(String, String, String)>,
    Json(body): Json<RevertCommitRequest>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, sha = %sha, "handling revert commit request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let lock_key = format!("{}/{}", collection_id, tenant_id);
    let lock = state.get_repo_lock(&lock_key);
    let _lock_guard = lock.lock().await;

    let RevertCommitRequest { author, message } = body;

    let tenant_id_for_task = tenant_id.clone();
    let sha_for_task = sha.clone();

    let (new_commit_sha, file_changes) = run_blocking(move || {
        git::GitCommits::revert_commit(
            &repo_path,
            &tenant_id_for_task,
            &sha_for_task,
            message.as_deref(),
            &author.name,
            &author.email,
        )
    })
    .await?;

    tracing::debug!(
        tenant_id = %tenant_id,
        reverted_sha = %sha,
        new_sha = %new_commit_sha,
        file_change_count = file_changes.len(),
        "revert complete, spawning hook delivery"
    );

    HookDelivery::spawn(
        state.http_client.clone(),
        state.config.clone(),
        tenant_id,
        new_commit_sha.clone(),
        Utc::now(),
        file_changes,
    );

    Ok((
        StatusCode::OK,
        Json(json!({
            "reverted_sha": sha,
            "commit_sha": new_commit_sha,
        })),
    ))
}
