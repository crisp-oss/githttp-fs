// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Tests for the background maintenance pass (`GitMaintenance::run`).
//!
//! The *when* lives in `maintenance.rs` and is a timer; the *what* is tested
//! here directly, because the promise that matters is about data rather than
//! scheduling: **maintenance can never destroy data by default**, and commit
//! history is safe in both modes.

use git2::Repository;

use crate::{git::GitMaintenance, tests::harness::TestServer};

const TENANT: &str = "/docs/acme";

/// Writes an object nothing references, the way a write that failed
/// mid-operation leaves one behind.
fn write_orphan_blob(repo_path: &std::path::Path) -> git2::Oid {
    let repo = Repository::open(repo_path).expect("cannot open repository");

    repo.blob(b"orphaned by a failed write")
        .expect("cannot write blob")
}

fn blob_exists(repo_path: &std::path::Path, oid: git2::Oid) -> bool {
    let repo = Repository::open(repo_path).expect("cannot open repository");

    // Bound rather than returned directly: the borrow the lookup holds on
    // `repo` must end before `repo` itself does.
    let found = repo.find_blob(oid).is_ok();

    found
}

#[tokio::test]
async fn a_pass_consolidates_loose_objects_into_one_pack() {
    let server = TestServer::start().await;

    for index in 0..5 {
        server
            .write_file(TENANT, &format!("file-{}.md", index), "content")
            .await;
    }

    let repo_path = server.repo_path("docs", "acme");

    let report = GitMaintenance::run(&repo_path, false, true).expect("maintenance failed");

    assert!(report.packed_objects > 0, "nothing was packed");
    assert!(
        report.loose_objects_removed > 0,
        "loose objects were left behind"
    );
    assert_eq!(GitMaintenance::pack_count(&repo_path), 1);
}

#[tokio::test]
async fn a_second_pass_on_a_consolidated_repository_is_skipped() {
    // No loose objects and at most one pack: there is nothing to do, and
    // repacking anyway would rewrite the whole store on every timer.
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "x").await;

    let repo_path = server.repo_path("docs", "acme");

    GitMaintenance::run(&repo_path, false, true).expect("first pass failed");

    let second = GitMaintenance::run(&repo_path, false, true).expect("second pass failed");

    assert_eq!(second.packed_objects, 0);
    assert_eq!(second.loose_objects_removed, 0);
    assert_eq!(second.old_packs_removed, 0);
}

#[tokio::test]
async fn history_and_content_survive_a_pass() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "one").await;
    server.write_file(TENANT, "a.md", "two").await;

    server
        .delete(
            &format!("{}/files/a.md", TENANT),
            serde_json::json!({ "author": crate::tests::harness::author() }),
        )
        .await
        .expect_status(axum::http::StatusCode::OK);

    server.write_file(TENANT, "b.md", "kept").await;

    let commits_before = server.get(&format!("{}/commits", TENANT)).await.json();

    GitMaintenance::run(&server.repo_path("docs", "acme"), true, true).expect("maintenance failed");

    // History is append-only, so every past file version — including
    // versions of since-deleted files — stays reachable through its commit,
    // in both modes.
    let commits_after = server.get(&format!("{}/commits", TENANT)).await.json();

    assert_eq!(commits_before["commits"], commits_after["commits"]);

    let deleted_version = commits_after["commits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|commit| commit["message"] == "update: a.md")
        .expect("the update commit is gone")["sha"]
        .as_str()
        .unwrap()
        .to_string();

    let detail = server
        .get(&format!("{}/commits/{}", TENANT, deleted_version))
        .await;

    detail.expect_status(axum::http::StatusCode::OK);

    assert_eq!(detail.json()["files"][0]["content"], "two");

    assert_eq!(
        server.read_file(TENANT, "b.md").await.as_deref(),
        Some("kept")
    );
}

#[tokio::test]
async fn the_default_pass_carries_unreachable_objects_over() {
    // By default every object is carried into the new pack, so maintenance
    // can never destroy data under any circumstance.
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "x").await;

    let repo_path = server.repo_path("docs", "acme");
    let orphan = write_orphan_blob(&repo_path);

    assert!(blob_exists(&repo_path, orphan));

    GitMaintenance::run(&repo_path, false, true).expect("maintenance failed");

    assert!(
        blob_exists(&repo_path, orphan),
        "a non-destructive pass dropped an unreachable object"
    );
}

#[tokio::test]
async fn destructive_prune_drops_unreachable_objects() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "x").await;

    let repo_path = server.repo_path("docs", "acme");
    let orphan = write_orphan_blob(&repo_path);

    GitMaintenance::run(&repo_path, true, true).expect("maintenance failed");

    assert!(
        !blob_exists(&repo_path, orphan),
        "a destructive pass kept an unreachable object"
    );

    // What is reachable from a ref is untouched.
    assert_eq!(server.read_file(TENANT, "a.md").await.as_deref(), Some("x"));
}

#[tokio::test]
async fn a_pass_on_a_deleted_tenant_is_a_no_op() {
    // The tenant may have been deleted while the timer was armed.
    let server = TestServer::start().await;

    let report = GitMaintenance::run(&server.repo_path("docs", "never-existed"), false, true)
        .expect("maintenance on a missing repository failed");

    assert_eq!(report.packed_objects, 0);
}
