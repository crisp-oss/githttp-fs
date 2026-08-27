// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Sanitisation of every user-controlled value that reaches the filesystem
//! or git layer.
//!
//! This module is the security boundary of the service. Collection ids,
//! tenant ids, file paths, and commit SHAs all arrive from URL segments and
//! request bodies, and all of them are eventually interpolated into on-disk
//! paths or git lookups. Each validator here follows the same philosophy:
//!
//! - **Allow-list, not deny-list.** Identifiers are restricted to a known
//!   safe character set rather than trying to enumerate dangerous ones.
//! - **Validate once, at the edge.** Route handlers call these functions
//!   first thing; everything below (`git.rs`, `hooks.rs`) can then trust its
//!   inputs and never re-checks them.
//! - **Return the sanitised value.** Validators return the (possibly
//!   trimmed) `&str` so callers cannot accidentally keep using the raw input.
//!
//! The threats being blocked: path traversal (`../../etc/passwd`), direct
//! access to git internals (`.git/config`), absolute paths escaping the
//! repo, identity confusion (`./foo.md` vs `foo.md`), and revspec injection
//! into commit lookups (`HEAD~1`, `:/pattern`).

use std::path::{Component, Path};

use crate::error::AppError;

// Identifier length caps keep on-disk directory names comfortably inside
// filesystem limits (255 bytes on most systems) with room to spare.
const MAX_TENANT_ID_LEN: usize = 64;
const MAX_COLLECTION_ID_LEN: usize = 64;

// Shortest unambiguous git SHA prefix, and enough room for a full SHA-256.
const MIN_COMMIT_SHA_LEN: usize = 4;
const MAX_COMMIT_SHA_LEN: usize = 64;

/// Collection identifiers are used as the top-level on-disk directory name.
/// Same character-set rules as tenant identifiers.
pub fn collection_id(raw: &str) -> Result<&str, AppError> {
    let valid_length = !raw.is_empty() && raw.len() <= MAX_COLLECTION_ID_LEN;

    let valid_chars = raw
        .bytes()
        .all(|byte| matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_'));

    if valid_length && valid_chars {
        Ok(raw)
    } else {
        Err(AppError::InvalidTenant {
            tenant_id: raw.to_string(),
        })
    }
}

/// Tenant identifiers are used as on-disk directory names, so they must be
/// strictly limited to a safe character set. This prevents path traversal
/// (`..`) and operating-system metacharacters from reaching the filesystem.
pub fn tenant_id(raw: &str) -> Result<&str, AppError> {
    let valid_length = !raw.is_empty() && raw.len() <= MAX_TENANT_ID_LEN;

    let valid_chars = raw
        .bytes()
        .all(|byte| matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_'));

    if valid_length && valid_chars {
        Ok(raw)
    } else {
        Err(AppError::InvalidTenant {
            tenant_id: raw.to_string(),
        })
    }
}

/// Commit identifiers must be plain hexadecimal (a full or abbreviated SHA).
/// This guarantees the value can never be interpreted as a git revspec
/// (`HEAD~3`, `master@{1}`, `:/message-pattern`, ...), which would leak git
/// semantics through the API and allow expensive full-history searches.
pub fn commit_sha(raw: &str) -> Result<&str, AppError> {
    let valid_length = raw.len() >= MIN_COMMIT_SHA_LEN && raw.len() <= MAX_COMMIT_SHA_LEN;
    let valid_chars = raw.bytes().all(|byte| byte.is_ascii_hexdigit());

    if valid_length && valid_chars {
        Ok(raw)
    } else {
        Err(AppError::InvalidOperation {
            reason: format!("commit sha must be hexadecimal: {}", raw),
        })
    }
}

/// When an extension whitelist is configured, the final path component must
/// carry one of the allowed extensions (compared case-insensitively).
/// No whitelist configured means every extension is accepted.
pub fn file_extension(path: &str, allowed_extensions: Option<&[String]>) -> Result<(), AppError> {
    let Some(allowed) = allowed_extensions else {
        return Ok(());
    };

    let extension = Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("");

    let permitted = !extension.is_empty()
        && allowed.iter().any(|entry| {
            entry
                .trim_start_matches('.')
                .eq_ignore_ascii_case(extension)
        });

    if permitted {
        Ok(())
    } else {
        Err(AppError::InvalidPath {
            reason: format!("file extension is not allowed: {}", path),
        })
    }
}

/// Strips leading/trailing slashes and rejects folder paths that try to escape
/// the repo root or access git internals. Returns the sanitised relative path,
/// or an empty string if the caller passed `/` or an empty string (= repo root).
///
/// Used for the `prefix_path` listing parameter. Differs from [`file_path`]
/// in two ways: an empty result is *valid* (it means "the repo root"), and
/// trailing slashes are tolerated since callers naturally write folders as
/// `/docs/`.
pub fn folder_path(raw: &str) -> Result<&str, AppError> {
    let path = raw.trim_matches('/');

    if path.is_empty() {
        return Ok(path);
    }

    reject_unsafe_components(path)?;

    Ok(path)
}

/// Rejects any path component that would escape the repository root, reach
/// git internals, or split one file's identity across two spellings.
///
/// Component-wise inspection (rather than substring matching) is what makes
/// this robust: `Path::components()` splits exactly the way the OS will
/// interpret the path, so nothing can hide inside a segment.
fn reject_unsafe_components(path: &str) -> Result<(), AppError> {
    for component in Path::new(path).components() {
        match component {
            Component::ParentDir => {
                return Err(AppError::InvalidPath {
                    reason: "path must not contain '..' components".to_string(),
                });
            }
            // Only a leading `./` survives `components()` normalization, but it
            // must still be rejected: `./foo.md` and `foo.md` are the same file
            // on disk yet different identities in commits and hook payloads.
            Component::CurDir => {
                return Err(AppError::InvalidPath {
                    reason: "path must not contain '.' components".to_string(),
                });
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(AppError::InvalidPath {
                    reason: "path must be relative".to_string(),
                });
            }
            Component::Normal(name) if name == ".git" => {
                return Err(AppError::InvalidPath {
                    reason: "path must not reference .git".to_string(),
                });
            }
            _ => {}
        }
    }

    Ok(())
}

/// Strips a leading slash and rejects paths that try to escape the repo root
/// or access git internals. Returns the sanitised relative path.
///
/// This runs on every `*path` URL segment and on move destinations. The
/// sanitised path becomes the file's identity everywhere: on disk, in commit
/// trees, in hook payloads, and in `file_path` history filters — which is
/// why normalisation must be strict (two spellings of the same file would
/// split its history and confuse downstream receivers).
pub fn file_path(raw: &str) -> Result<&str, AppError> {
    let path = raw.trim_start_matches('/');

    if path.is_empty() {
        return Err(AppError::InvalidPath {
            reason: "path must not be empty".to_string(),
        });
    }

    reject_unsafe_components(path)?;

    Ok(path)
}

/// Same rules as [`file_path`], but trailing slashes are tolerated too, so the
/// path may name either a file or a folder.
///
/// Used on the `*path` segment (and move destination) of the routes that
/// accept a folder — the existence check with `check_prefix_path=true`, and
/// the delete/move routes with `allow_prefix_path_recurse: true` — where
/// callers naturally spell a folder as `docs/guides/`. The trailing slash is
/// stripped rather than kept: `docs/guides` is the folder's identity in commit
/// trees, and two spellings of the same folder must not diverge. An empty
/// result is still rejected — the repository root is not a deletable or
/// movable entry (that is what the tenant route is for).
pub fn file_or_folder_path(raw: &str) -> Result<&str, AppError> {
    let path = raw.trim_matches('/');

    if path.is_empty() {
        return Err(AppError::InvalidPath {
            reason: "path must not be empty".to_string(),
        });
    }

    reject_unsafe_components(path)?;

    Ok(path)
}
