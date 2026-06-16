// Flavio
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

use crate::error::AppError;

/// Runs a blocking closure on Tokio's blocking thread pool, returning the
/// inner result. Centralises the JoinError → AppError mapping so handlers
/// stay focused on their logic.
pub async fn run_blocking<F, T>(blocking_fn: F) -> Result<T, AppError>
where
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(blocking_fn)
        .await
        .map_err(|join_err| AppError::TaskFailed(join_err.to_string()))?
}

/// Constant-time equality check for byte slices, used for comparing secrets
/// to avoid leaking length-prefix matches through timing side channels.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let mut difference: u8 = 0;

    for (left_byte, right_byte) in left.iter().zip(right.iter()) {
        difference |= left_byte ^ right_byte;
    }

    difference == 0
}
