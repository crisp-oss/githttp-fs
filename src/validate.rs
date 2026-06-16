// Flavio
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

use std::path::{Component, Path};

use crate::error::AppError;

const MAX_TENANT_ID_LEN: usize = 64;

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

/// Strips a leading slash and rejects paths that try to escape the repo root
/// or access git internals. Returns the sanitised relative path.
pub fn file_path(raw: &str) -> Result<&str, AppError> {
    let path = raw.trim_start_matches('/');

    if path.is_empty() {
        return Err(AppError::InvalidPath {
            reason: "path must not be empty".to_string(),
        });
    }

    for component in Path::new(path).components() {
        match component {
            Component::ParentDir => {
                return Err(AppError::InvalidPath {
                    reason: "path must not contain '..' components".to_string(),
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

    Ok(path)
}
