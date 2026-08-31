// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! The per-directory file-order index: its reserved path, its stored format,
//! and the structural validation of a caller-supplied order.
//!
//! Git has no ordering of its own — tree entries are name-sorted by
//! definition and carry no metadata slot — so a presentation order has to be
//! stored as data. It is stored **per directory**: one `.order.json` holding
//! the leaf names of that directory's entries, in the order the caller wants
//! them presented. Ordering is a sibling-level concern, and scoping the
//! storage the same way is what keeps two costs bounded:
//!
//! - **A reorder touches one small file**, so its commit and its hook payload
//!   are proportional to one directory rather than to the repository.
//! - **A folder move needs no index rewriting at all**: entries are leaf
//!   names, so every index inside a relocated subtree is still correct once it
//!   travels with the subtree.
//!
//! Two rules make a stored index robust, and both live here in spirit rather
//! than in code:
//!
//! - **Sparse.** An index need not list every sibling. Listed entries come
//!   first, in index order; unlisted ones follow in the ordinary listing order
//!   (directories first, then alphabetical).
//! - **Stale-tolerant on read.** An entry naming something that is no longer
//!   there is silently ignored. Writes are validated strictly against HEAD, so
//!   staleness should not arise — but a revert or a rollback can restore an
//!   older index from history, and that must never turn a read into an error.
//!
//! The index is not addressable through the `/files` routes. It is a separate
//! resource, owned by `routes/order.rs`, and letting a client `PUT` it as an
//! ordinary file would bypass exactly the validation this module exists to
//! perform. `git.rs` enforces that invisibility — see [`is_order_file`].

use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// Reserved leaf name of the order index. Dot-prefixed by Unix convention for
/// metadata; the prefix is a courtesy to humans browsing a repository rather
/// than a mechanism, since the index is filtered out of every `/files` route
/// regardless of `include_hidden_files`.
pub const ORDER_FILE_NAME: &str = ".order.json";

/// The stored document, deserialising side. An object rather than a bare
/// array so per-entry metadata (a title, a hidden flag, a group) can be added
/// later without a format version bump.
#[derive(Deserialize)]
struct OrderDocument {
    order: Vec<String>,
}

/// The stored document, serialising side — borrows the order rather than
/// cloning it on every write.
#[derive(Serialize)]
struct OrderDocumentRef<'a> {
    order: &'a [String],
}

/// Repo-root-relative path of the index that orders `directory`
/// (`""` being the repository root).
pub fn order_file_path(directory: &str) -> String {
    if directory.is_empty() {
        ORDER_FILE_NAME.to_string()
    } else {
        format!("{}/{}", directory, ORDER_FILE_NAME)
    }
}

/// Whether `path` names an order index. Compared on the leaf name alone, so
/// it holds at any depth — which is what makes the invisibility rule in
/// `git.rs` a single check per tree entry.
pub fn is_order_file(path: &str) -> bool {
    split_parent(path).1 == ORDER_FILE_NAME
}

/// The directory an order index orders, or `None` when `path` is not an
/// index. The repository root's index yields `""`.
///
/// This is the funnel the hook layer classifies on: an event's kind is
/// derived from the *path* a commit touched, never from the route that
/// produced it, so a revert, a rollback and a recursive folder move all
/// classify exactly like an explicit order write.
pub fn directory_of_order_file(path: &str) -> Option<&str> {
    let (parent, leaf) = split_parent(path);

    if leaf == ORDER_FILE_NAME {
        Some(parent)
    } else {
        None
    }
}

/// Splits a repo-root-relative path into its parent directory and its leaf
/// name. A path with no slash sits at the repository root, so its parent is
/// the empty string.
pub fn split_parent(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((parent, leaf)) => (parent, leaf),
        None => ("", path),
    }
}

/// The entry an index line refers to, with the directory-marking trailing
/// slash (if any) stripped. Two spellings of one name are the same entry, so
/// every comparison — duplicate detection, rank lookup, upkeep matching —
/// goes through this.
pub fn entry_name(entry: &str) -> &str {
    entry.strip_suffix('/').unwrap_or(entry)
}

/// Canonical index spelling of a directory entry: the trailing slash marks it
/// as a folder, so a stored index is readable on its own without resolving
/// every name against a tree.
pub fn directory_entry(name: &str) -> String {
    format!("{}/", name)
}

/// How a directory is named in messages and errors — the repository root has
/// no path of its own, so it shows as `/`.
pub fn display_directory(directory: &str) -> &str {
    if directory.is_empty() {
        "/"
    } else {
        directory
    }
}

/// Serialises an order list into the stored document. Pretty-printed with a
/// trailing newline so the file reads as a sensible git diff when a human
/// inspects the repository.
pub fn serialize(order: &[String]) -> String {
    let mut document = serde_json::to_string_pretty(&OrderDocumentRef { order })
        .unwrap_or_else(|_err| "{\n  \"order\": []\n}".to_string());

    document.push('\n');

    document
}

/// Parses a stored document, or `None` when it cannot be read as one.
///
/// A malformed index is treated as no index at all rather than as an error:
/// only this server's order route writes the file, so the only way to get a
/// broken one is a hand-edited commit — and that must not be able to turn
/// every listing of that directory into a `500`.
pub fn parse(content: &str) -> Option<Vec<String>> {
    serde_json::from_str::<OrderDocument>(content)
        .ok()
        .map(|document| document.order)
}

/// Structural validation of a caller-supplied order, run before the
/// repository is opened. This covers everything that can be judged from the
/// values alone; whether each entry actually *exists* is checked in `git.rs`,
/// against HEAD's tree, under the tenant write lock.
///
/// An empty order is rejected rather than accepted as "no ordering": it would
/// mean exactly what deleting the index means, and one operation with two
/// spellings is a worse API than two operations with one each.
pub fn validate_order(order: &[String]) -> Result<(), AppError> {
    if order.is_empty() {
        return Err(AppError::InvalidOperation {
            reason: "order must contain at least one entry; delete the index instead of writing an empty order"
                .to_string(),
        });
    }

    let mut seen: Vec<&str> = Vec::with_capacity(order.len());

    for entry in order {
        let name = entry_name(entry);

        if name.is_empty() {
            return Err(AppError::InvalidOperation {
                reason: "order entries must not be empty".to_string(),
            });
        }

        // Entries are leaf names, not paths: that is what confines an index to
        // its own directory and lets a relocated subtree keep its indexes
        // verbatim. A nested path would break both.
        if name.contains('/') {
            return Err(AppError::InvalidOperation {
                reason: format!("order entries must be leaf names, not paths: {}", entry),
            });
        }

        if name == "." || name == ".." {
            return Err(AppError::InvalidOperation {
                reason: format!("order entry must not be a relative reference: {}", entry),
            });
        }

        if name == ".git" {
            return Err(AppError::InvalidOperation {
                reason: "order entry must not reference .git".to_string(),
            });
        }

        // The index cannot order itself, and it is invisible to every listing
        // anyway — naming it can only be a caller bug.
        if name == ORDER_FILE_NAME {
            return Err(AppError::InvalidOperation {
                reason: format!("order entry must not reference the index itself: {}", entry),
            });
        }

        if seen.contains(&name) {
            return Err(AppError::InvalidOperation {
                reason: format!("order entries must be unique: '{}' is listed twice", name),
            });
        }

        seen.push(name);
    }

    Ok(())
}
