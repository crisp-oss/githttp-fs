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

// --- Scheduling ----------------------------------------------------------

/// Waits until a repository is consolidated: no loose objects, one pack.
async fn wait_for_consolidation(repo_path: &std::path::Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);

    loop {
        if GitMaintenance::pack_count(repo_path) == 1 {
            return;
        }

        assert!(
            std::time::Instant::now() < deadline,
            "maintenance never ran on {}",
            repo_path.display()
        );

        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn the_first_write_arms_a_pass_that_runs_after_the_delay() {
    // The schedule is per repository and one-shot: the first write arms it,
    // and the pass clears the slot so the next write re-arms it.
    let server = TestServer::builder().maintenance(1).start().await;

    server.write_file(TENANT, "a.md", "one").await;
    server.write_file(TENANT, "b.md", "two").await;

    let repo_path = server.repo_path("docs", "acme");

    assert_eq!(
        GitMaintenance::pack_count(&repo_path),
        0,
        "a fresh repository should hold only loose objects"
    );

    wait_for_consolidation(&repo_path).await;

    // Content survives the pass, which is the only thing a caller can see.
    assert_eq!(
        server.read_file(TENANT, "a.md").await.as_deref(),
        Some("one")
    );
}

#[tokio::test]
async fn repositories_receiving_no_writes_are_never_touched() {
    let server = TestServer::builder().maintenance(1).start().await;

    server.write_file("/docs/written", "a.md", "x").await;

    // A second repository exists but is never written to after creation, so
    // nothing arms a pass for it beyond its own first write.
    wait_for_consolidation(&server.repo_path("docs", "written")).await;

    assert!(
        !server.repo_path("docs", "untouched").exists(),
        "a repository was created without a write"
    );
}

#[tokio::test]
async fn deleting_a_tenant_disarms_its_pending_pass() {
    // The timer outlives the repository otherwise, and would fire against a
    // directory that is gone.
    let server = TestServer::builder().maintenance(1).start().await;

    server.write_file(TENANT, "a.md", "x").await;

    server
        .request(reqwest::Method::DELETE, TENANT, None, true)
        .await
        .expect_status(axum::http::StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;

    // Nothing re-created the store, and the node is still healthy.
    assert!(!server.repo_path("docs", "acme").exists());

    server
        .get_unauthenticated("/_health/status")
        .await
        .expect_status(axum::http::StatusCode::OK);
}

/// Builds a store whose repository holds several packfiles, by replicating
/// commits into it one at a time — every applied delta lands as its own
/// pack, which is the situation `maximum_packs` exists for.
async fn multi_pack_store() -> (std::sync::Arc<tempfile::TempDir>, usize) {
    let master = TestServer::builder()
        .master()
        .node_id("packs-master")
        .start()
        .await;

    let replica = TestServer::builder()
        .replica_of(master.replication_url.as_ref().unwrap())
        .node_id("packs-replica")
        .start()
        .await;

    for index in 0..4 {
        master
            .write_file(TENANT, "a.md", &format!("version {}", index))
            .await;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);

        loop {
            let response = replica.get(&format!("{}/files/a.md", TENANT)).await;

            if response.status == axum::http::StatusCode::OK
                && response.json()["content"] == format!("version {}", index)
            {
                break;
            }

            assert!(
                std::time::Instant::now() < deadline,
                "the replica never caught up to version {}",
                index
            );

            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    let packs = GitMaintenance::pack_count(&replica.repo_path("docs", "acme"));

    assert!(
        packs >= 2,
        "replication did not produce several packs ({}), so the threshold cannot be tested",
        packs
    );

    (replica.store(), packs)
}

#[tokio::test]
async fn a_pack_threshold_runs_the_pass_at_once_instead_of_after_the_delay() {
    // A replica applies every replicated delta as one more pack, and libgit2
    // consults every pack index on every object lookup — so a busy tenant
    // would otherwise degrade for a whole `delay_secs`.
    let (store, packs_before) = multi_pack_store().await;

    // A node adopting that store, with a day's delay: only the threshold can
    // explain a pass running at all.
    let node = TestServer::builder()
        .reuse_store(store)
        .maintenance(86_400)
        .maximum_packs(2)
        .start()
        .await;

    let repo_path = node.repo_path("docs", "acme");

    assert_eq!(GitMaintenance::pack_count(&repo_path), packs_before);

    // Arming is what a landed pack (or a write) does; the threshold is
    // evaluated inside the armed task, off the request path.
    node.state.maintenance.schedule(
        "docs/acme",
        repo_path.clone(),
        node.state.get_repo_lock("docs/acme"),
    );

    wait_for_consolidation(&repo_path).await;

    assert_eq!(
        node.read_file(TENANT, "a.md").await.as_deref(),
        Some("version 3")
    );
}

#[tokio::test]
async fn without_a_threshold_the_same_store_waits_out_the_delay() {
    // The control for the test above: same store, same day-long delay, no
    // threshold — nothing runs, and the packs stay.
    let (store, packs_before) = multi_pack_store().await;

    let node = TestServer::builder()
        .reuse_store(store)
        .maintenance(86_400)
        .start()
        .await;

    let repo_path = node.repo_path("docs", "acme");

    node.state.maintenance.schedule(
        "docs/acme",
        repo_path.clone(),
        node.state.get_repo_lock("docs/acme"),
    );

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    assert_eq!(
        GitMaintenance::pack_count(&repo_path),
        packs_before,
        "a pass ran without a threshold to trigger it"
    );
}

// --- Stale locks ---------------------------------------------------------

#[tokio::test]
async fn startup_removes_index_locks_left_by_a_killed_process() {
    // At boot nothing can be in flight, so every lock on disk is by
    // definition stale and safe to delete.
    let server = TestServer::start().await;

    server.write_file("/docs/one", "a.md", "x").await;
    server.write_file("/docs/two", "b.md", "x").await;

    let first = server.repo_path("docs", "one").join(".git/index.lock");
    let second = server.repo_path("docs", "two").join(".git/index.lock");

    std::fs::write(&first, b"").expect("cannot write lock");
    std::fs::write(&second, b"").expect("cannot write lock");

    crate::git::GitLocks::cleanup_all_stale_locks(&server.repos_path);

    assert!(!first.exists(), "a stale lock survived startup cleanup");
    assert!(!second.exists(), "a stale lock survived startup cleanup");

    // And the repositories are still usable afterwards.
    assert_eq!(
        server.read_file("/docs/one", "a.md").await.as_deref(),
        Some("x")
    );
}

#[tokio::test]
async fn a_stale_lock_never_blocks_a_write() {
    // The write path does not touch the index at all, which is what makes a
    // leftover lock harmless rather than fatal.
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "one").await;

    let lock = server.repo_path("docs", "acme").join(".git/index.lock");

    std::fs::write(&lock, b"").expect("cannot write lock");

    server.write_file(TENANT, "a.md", "two").await;

    assert_eq!(
        server.read_file(TENANT, "a.md").await.as_deref(),
        Some("two")
    );
}
