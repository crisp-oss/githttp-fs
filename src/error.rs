// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("file not found: {path}")]
    FileNotFound { path: String },

    #[error("commit not found: {sha}")]
    CommitNotFound { sha: String },

    #[error("tenant not found: {tenant_id}")]
    TenantNotFound { tenant_id: String },

    #[error("invalid tenant id: {tenant_id}")]
    InvalidTenant { tenant_id: String },

    #[error("invalid path: {reason}")]
    InvalidPath { reason: String },

    #[error("invalid operation: {reason}")]
    InvalidOperation { reason: String },

    #[error("file content is not valid UTF-8 at path: {path}")]
    InvalidUtf8 { path: String },

    #[error("git error: {0}")]
    Git(#[from] git2::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("background task failed: {0}")]
    TaskFailed(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            AppError::FileNotFound { .. }
            | AppError::CommitNotFound { .. }
            | AppError::TenantNotFound { .. } => StatusCode::NOT_FOUND,

            AppError::InvalidTenant { .. }
            | AppError::InvalidPath { .. }
            | AppError::InvalidOperation { .. } => StatusCode::BAD_REQUEST,

            AppError::InvalidUtf8 { .. } => StatusCode::UNPROCESSABLE_ENTITY,

            AppError::Git(_) | AppError::Io(_) | AppError::TaskFailed(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };

        if status.is_server_error() {
            tracing::error!("server error response: {}", self);
        }

        let body = Json(json!({ "error": self.to_string() }));

        (status, body).into_response()
    }
}
