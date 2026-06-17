// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::Response,
    Json,
};
use serde_json::json;

use crate::{state::AppState, util::constant_time_eq};

const BEARER_PREFIX: &str = "Bearer ";

/// Validates the `Authorization: Bearer <key>` header on every request.
pub async fn require_api_key(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let provided_key = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|header_value| header_value.to_str().ok())
        .and_then(|header_str| header_str.strip_prefix(BEARER_PREFIX));

    let authorised = provided_key
        .map(|key| constant_time_eq(key.as_bytes(), state.config.server.api_key.as_bytes()))
        .unwrap_or(false);

    if authorised {
        Ok(next.run(request).await)
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing or invalid API key" })),
        ))
    }
}
