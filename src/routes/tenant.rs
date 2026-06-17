// githttp-fs
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
use serde_json::json;

use crate::{error::AppError, git, state::AppState, util::run_blocking, validate};

/// DELETE /:tenant_id
/// Permanently removes the tenant's entire repository from disk.
/// The in-memory lock entry is also cleaned up.
pub async fn delete_tenant(
    State(state): State<AppState>,
    Path(tenant_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    tracing::debug!(tenant_id = %tenant_id, "handling delete tenant request");

    let repo_path = state.config.server.repos_path.join(&tenant_id);

    // Acquire the write lock so any in-flight write finishes first.
    let lock = state.get_repo_lock(&tenant_id);
    let _lock_guard = lock.lock().await;

    let tenant_id_for_task = tenant_id.clone();

    run_blocking(move || git::GitTenant::delete_repo(&repo_path, &tenant_id_for_task)).await?;

    state.remove_repo_lock(&tenant_id);

    tracing::info!(tenant_id = %tenant_id, "tenant deleted");

    Ok((StatusCode::OK, Json(json!({ "deleted": tenant_id }))))
}
