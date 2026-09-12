// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Integration tests for the working tree: the `server.checkout_files`
//! switch, and the `server.checkout_files_autoheal` startup pass.
//!
//! The property under test throughout is the one promotion rests on:
//! **nothing reads the working tree**. Existence checks come from HEAD's
//! tree, moved content from HEAD's blob, and every mirror removal tolerates
//! a file that was never there — so a node running with no working tree at
//! all must answer every route identically.

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;

use crate::tests::harness::{author, TestServer};

const TENANT: &str = "/docs/acme";

/// Waits for a path to appear on disk, since the heal pass is spawned rather
/// than awaited. Returns whether it arrived.
async fn wait_for_path(path: &std::path::Path) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);

    while std::time::Instant::now() < deadline {
        if path.exists() {
            return true;
        }

        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    false
}

#[tokio::test]
async fn files_are_mirrored_to_disk_by_default() {
    // A courtesy copy of HEAD, so a human can `ls` a tenant repository.
    let server = TestServer::start().await;

    server.write_file(TENANT, "docs/intro.md", "# Hello").await;

    let on_disk = server.repo_path("docs", "acme").join("docs/intro.md");

    assert!(wait_for_path(&on_disk).await, "file was not mirrored");
    assert_eq!(
        std::fs::read_to_string(&on_disk).unwrap(),
        "# Hello",
        "mirrored content differs from what was committed"
    );
}

#[tokio::test]
async fn the_mirror_follows_deletes_and_moves() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "docs/intro.md", "x").await;

    let repo = server.repo_path("docs", "acme");

    assert!(wait_for_path(&repo.join("docs/intro.md")).await);

    server
        .post(
            &format!("{}/files/docs/intro.md/move", TENANT),
            json!({ "author": author(), "destination": "handbook/intro.md" }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert!(wait_for_path(&repo.join("handbook/intro.md")).await);
    assert!(!repo.join("docs/intro.md").exists());

    server
        .delete(
            &format!("{}/files/handbook/intro.md", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert!(!repo.join("handbook/intro.md").exists());
}

#[tokio::test]
async fn with_checkout_files_off_nothing_is_written_to_disk() {
    let server = TestServer::builder().checkout_files(false).start().await;

    server.write_file(TENANT, "docs/intro.md", "# Hello").await;

    let repo = server.repo_path("docs", "acme");

    // The object store is there; the checked-out copy is not.
    assert!(repo.join(".git").is_dir());

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        !repo.join("docs/intro.md").exists(),
        "a file was mirrored with checkout_files off"
    );
}

#[tokio::test]
async fn every_route_answers_identically_with_no_working_tree() {
    // HEAD is authoritative: nothing this server answers reads the files on
    // disk, which is exactly what makes the switch safe.
    let server = TestServer::builder().checkout_files(false).start().await;

    server.write_file(TENANT, "docs/intro.md", "# Hello").await;
    server.write_file(TENANT, "docs/other.md", "# Other").await;

    assert_eq!(
        server.read_file(TENANT, "docs/intro.md").await.as_deref(),
        Some("# Hello")
    );

    server
        .head(&format!("{}/files/docs/intro.md", TENANT))
        .await
        .expect_status(StatusCode::OK);

    let listing = server.get(&format!("{}/files", TENANT)).await;

    listing.expect_status(StatusCode::OK);

    assert_eq!(listing.json()["files"][0]["name"], "docs");

    // A move reads its content from HEAD's blob, not from disk.
    server
        .post(
            &format!("{}/files/docs/intro.md/move", TENANT),
            json!({ "author": author(), "destination": "moved.md" }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert_eq!(
        server.read_file(TENANT, "moved.md").await.as_deref(),
        Some("# Hello")
    );

    // A delete tolerates a file that was never mirrored.
    server
        .delete(
            &format!("{}/files/docs/other.md", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    // And so does a recursive one.
    server.write_file(TENANT, "docs/a.md", "x").await;

    server
        .delete(
            &format!("{}/files/docs", TENANT),
            json!({ "author": author(), "allow_prefix_path_recurse": true }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert!(server.read_file(TENANT, "docs/a.md").await.is_none());
}

#[tokio::test]
async fn autoheal_is_off_by_default_so_a_restart_changes_nothing() {
    // The default has to be what an existing deployment already did: this
    // pass never ran before, walks the whole store, and removes files.
    let store = {
        let writer = TestServer::builder().checkout_files(false).start().await;

        writer.write_file(TENANT, "docs/intro.md", "# Hello").await;

        writer.store()
    };

    // Restart with files back on, but without asking for the repair.
    let server = TestServer::builder()
        .reuse_store(store)
        .checkout_files(true)
        .start()
        .await;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        !server
            .repo_path("docs", "acme")
            .join("docs/intro.md")
            .exists(),
        "a store was healed without being asked"
    );

    // The API is unaffected either way.
    assert_eq!(
        server.read_file(TENANT, "docs/intro.md").await.as_deref(),
        Some("# Hello")
    );
}

#[tokio::test]
async fn autoheal_writes_the_files_a_disabled_node_never_mirrored() {
    // The documented two-step: enable it for one restart, then turn it off.
    let store = {
        let writer = TestServer::builder().checkout_files(false).start().await;

        writer.write_file(TENANT, "docs/intro.md", "# Hello").await;
        writer.write_file(TENANT, "README.md", "# Readme").await;

        writer.store()
    };

    let server = TestServer::builder()
        .reuse_store(store)
        .checkout_files(true)
        .checkout_files_autoheal(true)
        .start()
        .await;

    let repo = server.repo_path("docs", "acme");

    assert!(
        wait_for_path(&repo.join("docs/intro.md")).await,
        "the heal pass did not write the missing file"
    );
    assert!(wait_for_path(&repo.join("README.md")).await);

    assert_eq!(
        std::fs::read_to_string(repo.join("docs/intro.md")).unwrap(),
        "# Hello"
    );
}

#[tokio::test]
async fn autoheal_removes_a_file_head_no_longer_names() {
    // This is the blast radius that makes the key opt-in: the pass removes,
    // not just writes.
    let store = {
        let writer = TestServer::start().await;

        writer.write_file(TENANT, "docs/intro.md", "x").await;

        let stray = writer.repo_path("docs", "acme").join("docs/stray.md");

        assert!(wait_for_path(&writer.repo_path("docs", "acme").join("docs/intro.md")).await);

        std::fs::write(&stray, "not in HEAD").expect("cannot write stray file");

        writer.store()
    };

    let server = TestServer::builder()
        .reuse_store(store)
        .checkout_files_autoheal(true)
        .start()
        .await;

    let stray = server.repo_path("docs", "acme").join("docs/stray.md");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);

    while stray.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    assert!(
        !stray.exists(),
        "the heal pass left a file HEAD does not name"
    );

    // What HEAD does name is still there.
    assert!(server
        .repo_path("docs", "acme")
        .join("docs/intro.md")
        .exists());
}

#[tokio::test]
async fn autoheal_is_inert_when_no_files_are_kept_on_disk() {
    let store = {
        let writer = TestServer::builder().checkout_files(false).start().await;

        writer.write_file(TENANT, "docs/intro.md", "x").await;

        writer.store()
    };

    let server = TestServer::builder()
        .reuse_store(store)
        .checkout_files(false)
        .checkout_files_autoheal(true)
        .start()
        .await;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        !server
            .repo_path("docs", "acme")
            .join("docs/intro.md")
            .exists(),
        "autoheal wrote files on a node keeping none"
    );
}
