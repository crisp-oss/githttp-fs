// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Repository states the API deliberately cannot produce.
//!
//! Two documented behaviours can only be exercised from a repository built
//! by hand: the date-range listing filter needs commits spread across
//! *years*, and the invalid-UTF-8 rejection needs a blob the write route
//! would never accept (its `content` is a JSON string). Both are read paths,
//! so building their input with `git2` directly is honest — nothing here
//! bypasses a validation that the code under test is supposed to perform.

use std::path::Path;

use git2::{Repository, Signature, Time};

/// Commits `files` on top of HEAD with an explicit commit timestamp, and
/// returns the new sha. Paths are repo-root-relative; existing paths are
/// overwritten, everything else is carried over from HEAD's tree.
pub fn commit_files_at(
    repo_path: &Path,
    unix_time: i64,
    message: &str,
    files: &[(&str, &[u8])],
) -> String {
    let repo = Repository::open(repo_path).expect("cannot open repository");

    let parent = repo
        .head()
        .expect("no HEAD")
        .peel_to_commit()
        .expect("HEAD is not a commit");

    let mut builder = git2::build::TreeUpdateBuilder::new();

    for (path, content) in files {
        let oid = repo.blob(content).expect("cannot write blob");

        builder.upsert(path, oid, git2::FileMode::Blob);
    }

    let tree_oid = builder
        .create_updated(&repo, &parent.tree().expect("HEAD has no tree"))
        .expect("cannot build tree");

    let tree = repo.find_tree(tree_oid).expect("tree is gone");

    let signature = Signature::new("Fixture", "fixture@example.com", &Time::new(unix_time, 0))
        .expect("bad signature");

    let oid = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[&parent],
        )
        .expect("cannot commit");

    oid.to_string()
}

/// Unix timestamp of an RFC 3339 instant, for readable fixture dates.
pub fn at(rfc3339: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .expect("fixture date is not RFC 3339")
        .timestamp()
}
