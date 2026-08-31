// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! The single application error type and its mapping to HTTP responses.
//!
//! Every fallible function in the codebase returns `Result<_, AppError>`,
//! and `AppError` implements axum's `IntoResponse`. That combination lets
//! handlers be written as plain `?`-propagating functions: any error that
//! bubbles up is automatically converted into a JSON error body with the
//! right status code, in one place, with consistent shape
//! (`{ "error": "<message>" }`).
//!
//! Design notes:
//!
//! - Variants are grouped by *HTTP semantics*, not by origin: "thing does
//!   not exist" variants map to 404, "caller sent something invalid" map to
//!   400, and infrastructure failures (git, io, task join) map to 500.
//! - `InvalidUtf8` gets its own 422: the request was well-formed, but the
//!   stored blob cannot be represented in a JSON string, which is an
//!   entity-level problem rather than a syntax one.
//! - The `#[from]` conversions on `Git` and `Io` are what make `?` work
//!   directly on `git2` and `std::fs` calls throughout `git.rs`.
//! - Only 5xx responses are logged. 4xx responses are the *caller's*
//!   mistake and would just generate noise at error level; the message is
//!   already returned to them in the body.

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

    /// No order index is stored for that directory. Named after the resource
    /// the caller asked for (the directory's order), not after the file it is
    /// stored in — the storage path is not part of the API surface.
    #[error("order index not found for directory: {directory}")]
    OrderNotFound { directory: String },

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
        // The single source of truth for error → status-code mapping. Adding
        // a new variant forces a decision here (the match is exhaustive).
        let status = match &self {
            AppError::FileNotFound { .. }
            | AppError::CommitNotFound { .. }
            | AppError::OrderNotFound { .. }
            | AppError::TenantNotFound { .. } => StatusCode::NOT_FOUND,

            AppError::InvalidTenant { .. }
            | AppError::InvalidPath { .. }
            | AppError::InvalidOperation { .. } => StatusCode::BAD_REQUEST,

            AppError::InvalidUtf8 { .. } => StatusCode::UNPROCESSABLE_ENTITY,

            AppError::Git(_) | AppError::Io(_) | AppError::TaskFailed(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };

        // 5xx means *we* failed — log it so operators see it. 4xx means the
        // caller failed — the message in the response body is enough.
        if status.is_server_error() {
            tracing::error!("server error response: {}", self);
        }

        let body = Json(json!({ "error": self.to_string() }));

        (status, body).into_response()
    }
}
