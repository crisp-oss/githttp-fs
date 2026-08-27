// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Commit history routes: list, detail, revert, and rollback.
//!
//! These are the endpoints where git's history model surfaces in the API —
//! but only as opaque `sha` identifiers and `committed_at` timestamps; no
//! git terminology (refs, revspecs, branches) leaks through. The revert and
//! rollback endpoints are *writes*: they follow the same lock → blocking op
//! → hook → maintenance sequence as the file write handlers.

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
    error::AppError, git, hooks::HookJob, routes::AuthorRequest, state::AppState,
    util::run_blocking, validate,
};

#[derive(Deserialize)]
pub struct ListCommitsQuery {
    pub page: Option<usize>,
    pub per_page: Option<usize>,
    pub file_path: Option<String>,
    pub include_statistics: Option<bool>,
}

#[derive(Deserialize)]
pub struct RevertCommitRequest {
    pub author: AuthorRequest,
    pub message: Option<String>,
}

/// The rollback route takes exactly the same body as the revert route: which
/// files move is derived from `:sha` itself — the commit already records what
/// it touched — so there is nothing extra for the caller to pass.
pub type RollbackCommitRequest = RevertCommitRequest;

const DEFAULT_PER_PAGE: usize = 100;
const MAX_PER_PAGE: usize = 500;

/// GET /:collection_id/:tenant_id/commits?page=1&per_page=100
/// Returns a paginated list of commits without file content.
///
/// The optional `file_path` query parameter narrows the list to commits that
/// touched that file, following renames backward through history — callers
/// always pass the file's *current* path and the server resolves what it was
/// called before any moves.
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
    let include_statistics = query_params.include_statistics.unwrap_or(false);

    tracing::debug!(tenant_id = %tenant_id, page = page, per_page = per_page, file_path = ?file_path, include_statistics = include_statistics, "handling list commits request");

    let tenant_id_for_task = tenant_id.clone();

    let (commits, has_more) = run_blocking(move || {
        git::GitCommits::list_commits(
            &repo_path,
            &tenant_id_for_task,
            page,
            per_page,
            file_path.as_deref(),
            include_statistics,
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
    let sha = validate::commit_sha(&sha)?.to_string();

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
///
/// History is never rewritten: the reverted commit stays in the log and the
/// revert appears as a brand-new commit on top, so downstream mirrors and
/// audit trails remain append-only.
///
/// See [`rollback_file`] for the point-in-time counterpart, which restores a
/// single file to the state it had *at* a commit rather than undoing that
/// commit's changes.
pub async fn revert_commit(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, sha)): Path<(String, String, String)>,
    Json(body): Json<RevertCommitRequest>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();
    let sha = validate::commit_sha(&sha)?.to_string();

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

    let repo_path_for_maintenance = repo_path.clone();
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
        "revert complete, enqueuing hook delivery"
    );

    // Enqueued while the tenant write lock is still held, so per-tenant hook
    // order always matches commit order.
    state.hook_queue.enqueue(
        &lock_key,
        HookJob {
            collection_id,
            tenant_id,
            commit_sha: new_commit_sha.clone(),
            committed_at: Utc::now(),
            file_changes,
        },
    );

    state
        .maintenance
        .schedule(&lock_key, repo_path_for_maintenance, lock.clone());

    Ok((
        StatusCode::OK,
        Json(json!({
            "reverted_sha": sha,
            "commit_sha": new_commit_sha,
        })),
    ))
}

/// POST /:collection_id/:tenant_id/commits/:sha/rollback
/// Point-in-time rollback: restores every file `:sha` touched to the exact
/// state it had **at** that commit, whatever happened to those files since.
///
/// This is the time-machine sibling of [`revert_commit`]. Both work from the
/// same set of files — the ones that commit changed, so the request body
/// carries no paths — and differ in which side of the commit is restored: a
/// revert undoes what `:sha` did (it restores `parent(:sha)`), while a
/// rollback restores `:sha` itself, collapsing every later change to those
/// paths into one new commit. Deletions travel in both directions: a file
/// removed since comes back, and a file this commit deleted is deleted again.
/// Files the commit never touched are left alone.
///
/// History is never rewritten — the rollback is a new commit on top, so the
/// state it replaces stays reachable through its own commit. That is why the
/// method is `POST`, same as the revert route: the operation *appends* a
/// commit, it never removes one.
pub async fn rollback_commit(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, sha)): Path<(String, String, String)>,
    Json(body): Json<RollbackCommitRequest>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();
    let sha = validate::commit_sha(&sha)?.to_string();

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, sha = %sha, "handling rollback commit request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let lock_key = format!("{}/{}", collection_id, tenant_id);
    let lock = state.get_repo_lock(&lock_key);
    let _lock_guard = lock.lock().await;

    let RollbackCommitRequest { author, message } = body;

    let repo_path_for_maintenance = repo_path.clone();
    let tenant_id_for_task = tenant_id.clone();
    let sha_for_task = sha.clone();

    let (new_commit_sha, file_changes) = run_blocking(move || {
        git::GitCommits::rollback_commit(
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
        rolled_back_to_sha = %sha,
        new_sha = %new_commit_sha,
        file_change_count = file_changes.len(),
        "rollback complete"
    );

    // A rollback to the state the files already hold creates no commit, so
    // there is nothing to notify or to maintain.
    if !file_changes.is_empty() {
        // Enqueued while the tenant write lock is still held, so per-tenant
        // hook order always matches commit order.
        state.hook_queue.enqueue(
            &lock_key,
            HookJob {
                collection_id,
                tenant_id,
                commit_sha: new_commit_sha.clone(),
                committed_at: Utc::now(),
                file_changes,
            },
        );

        state
            .maintenance
            .schedule(&lock_key, repo_path_for_maintenance, lock.clone());
    }

    Ok((
        StatusCode::OK,
        Json(json!({
            "rolled_back_to_sha": sha,
            "commit_sha": new_commit_sha,
        })),
    ))
}
