// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Small cross-cutting helpers with no better home.

use std::path::Path;

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

/// [`run_blocking`] for a read of one tenant repository.
///
/// Reads never take the tenant write lock, so a tenant can be deleted while
/// a read of it is running. Deletion renames the repository away in one step
/// (`git::GitStaging`), which keeps a read that has not opened it yet from
/// seeing anything but "no tenant" — but libgit2 resolves refs and loose
/// objects by path on every lookup, so a read that already had it open fails
/// its next lookup with whatever git error that path produces. Such a read
/// answers `404`, the same as if it had arrived a moment later, rather than a
/// `500` for a tenant that simply stopped existing.
///
/// Only `Git` and `Io` failures are reinterpreted, and only when the
/// repository is gone once the read has returned: every other error means
/// what it says whatever happened to the tenant meanwhile.
pub async fn run_tenant_read<F, T>(
    repo_path: &Path,
    tenant_id: &str,
    blocking_fn: F,
) -> Result<T, AppError>
where
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
    T: Send + 'static,
{
    match run_blocking(blocking_fn).await {
        Err(AppError::Git(_) | AppError::Io(_)) if !repo_path.join(".git").exists() => {
            Err(AppError::TenantNotFound {
                tenant_id: tenant_id.to_string(),
            })
        }
        outcome => outcome,
    }
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
