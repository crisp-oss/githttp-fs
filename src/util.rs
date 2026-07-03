// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Small cross-cutting helpers with no better home.

use crate::error::AppError;

/// Runs a blocking closure on Tokio's blocking thread pool, returning the
/// inner result. Centralises the JoinError → AppError mapping so handlers
/// stay focused on their logic.
///
/// Why this exists: libgit2 (and therefore everything in `git.rs`) is fully
/// synchronous — it does disk I/O, zlib compression, and SHA hashing on the
/// calling thread. Running that directly inside an async handler would park
/// a tokio worker thread and, under load, starve *every* request on the
/// server, not just the slow one. `spawn_blocking` moves the work onto
/// tokio's dedicated (much larger) blocking pool instead.
///
/// The double `?`-ish shape at the end unwraps two layers: the outer
/// `JoinError` (the task panicked or was cancelled — mapped to a 500) and
/// the inner `Result` produced by the closure itself.
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
///
/// A naive `left == right` short-circuits at the first mismatching byte, so
/// the comparison takes measurably longer the more leading bytes match —
/// enough signal for an attacker to brute-force a key byte by byte. This
/// version always walks both slices in full and folds every XOR into one
/// accumulator, so the running time depends only on the length.
///
/// The early length check *is* allowed to short-circuit: the length of the
/// configured API key is not considered secret.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    // XOR of two equal bytes is 0; OR-ing all XORs together means the
    // accumulator stays 0 only if every byte pair matched.
    let mut difference: u8 = 0;

    for (left_byte, right_byte) in left.iter().zip(right.iter()) {
        difference |= left_byte ^ right_byte;
    }

    difference == 0
}
