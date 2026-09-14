// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Integration tests for the prefix-path (folder) operations: recursive
//! delete and recursive move.
//!
//! The flag that enables them is opt-in per request and only ever *permits*
//! — a path resolving to a file runs the ordinary single-file operation
//! unchanged, and a folder path without the flag stays a `404`. Both of
//! those are asserted here, because "the flag never forces" is the property
//! that keeps recursion from being entered by accident.

use axum::http::StatusCode;
use serde_json::json;

use crate::tests::harness::{author, TestServer};

const TENANT: &str = "/docs/acme";

async fn seed(server: &TestServer) {
    for path in [
        "docs/guides/intro.md",
        "docs/guides/deep/advanced.md",
        "docs/guides/deep/extra.md",
        "docs/keep.md",
        "other.md",
    ] {
        server
            .write_file(TENANT, path, &format!("# {}", path))
            .await;
    }
}

#[tokio::test]
async fn a_folder_delete_needs_the_flag() {
    let server = TestServer::start().await;

    seed(&server).await;

    // Without it, a folder is simply "not a file".
    server
        .delete(
            &format!("{}/files/docs/guides", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::NOT_FOUND);

    server
        .delete(
            &format!("{}/files/docs/guides", TENANT),
            json!({ "author": author(), "allow_prefix_path_recurse": false }),
        )
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_recursive_delete_removes_the_whole_subtree_in_one_commit() {
    let server = TestServer::start().await;

    seed(&server).await;

    let before = server.head_sha(TENANT).await;

    let response = server
        .delete(
            &format!("{}/files/docs/guides", TENANT),
            json!({ "author": author(), "allow_prefix_path_recurse": true }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    let sha = response.json()["commit_sha"].as_str().unwrap().to_string();

    assert_ne!(sha, before);
    assert_eq!(sha, server.head_sha(TENANT).await, "more than one commit");

    for path in [
        "docs/guides/intro.md",
        "docs/guides/deep/advanced.md",
        "docs/guides/deep/extra.md",
    ] {
        assert!(
            server.read_file(TENANT, path).await.is_none(),
            "still present: {}",
            path
        );
    }

    // Everything outside the folder survives.
    assert!(server.read_file(TENANT, "docs/keep.md").await.is_some());
    assert!(server.read_file(TENANT, "other.md").await.is_some());
}

#[tokio::test]
async fn a_recursive_delete_auto_message_marks_the_folder_with_a_slash() {
    // The trailing slash is what distinguishes a folder-wide deletion from
    // a single-file one in history.
    let server = TestServer::start().await;

    seed(&server).await;

    server
        .delete(
            &format!("{}/files/docs/guides", TENANT),
            json!({ "author": author(), "allow_prefix_path_recurse": true }),
        )
        .await
        .expect_status(StatusCode::OK);

    let commits = server.get(&format!("{}/commits?per_page=1", TENANT)).await;

    assert_eq!(
        commits.json()["commits"][0]["message"],
        "delete: docs/guides/"
    );
}

#[tokio::test]
async fn the_flag_permits_but_does_not_force_folder_semantics() {
    let server = TestServer::start().await;

    seed(&server).await;

    // A file path with the flag on runs the ordinary single-file delete.
    server
        .delete(
            &format!("{}/files/docs/keep.md", TENANT),
            json!({ "author": author(), "allow_prefix_path_recurse": true }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert!(server.read_file(TENANT, "docs/keep.md").await.is_none());
    assert!(server
        .read_file(TENANT, "docs/guides/intro.md")
        .await
        .is_some());

    // A path resolving to nothing still answers 404.
    server
        .delete(
            &format!("{}/files/docs/nothing-here", TENANT),
            json!({ "author": author(), "allow_prefix_path_recurse": true }),
        )
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_trailing_slash_is_tolerated_once_folders_are_in_scope() {
    let server = TestServer::start().await;

    seed(&server).await;

    server
        .delete(
            &format!("{}/files/docs/guides/", TENANT),
            json!({ "author": author(), "allow_prefix_path_recurse": true }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert!(server
        .read_file(TENANT, "docs/guides/intro.md")
        .await
        .is_none());
}

#[tokio::test]
async fn the_repository_root_is_not_addressable_by_either_operation() {
    // Deleting everything remains the tenant route's job.
    let server = TestServer::start().await;

    seed(&server).await;

    server
        .delete(
            &format!("{}/files//", TENANT),
            json!({ "author": author(), "allow_prefix_path_recurse": true }),
        )
        .await
        .expect_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_recursive_move_relocates_the_subtree_keeping_every_leaf_name() {
    let server = TestServer::start().await;

    seed(&server).await;

    let response = server
        .post(
            &format!("{}/files/docs/guides/move", TENANT),
            json!({
                "author": author(),
                "destination": "handbook",
                "allow_prefix_path_recurse": true
            }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    assert_eq!(
        server
            .read_file(TENANT, "handbook/intro.md")
            .await
            .as_deref(),
        Some("# docs/guides/intro.md")
    );
    assert_eq!(
        server
            .read_file(TENANT, "handbook/deep/advanced.md")
            .await
            .as_deref(),
        Some("# docs/guides/deep/advanced.md")
    );

    assert!(server
        .read_file(TENANT, "docs/guides/intro.md")
        .await
        .is_none());
    assert!(server.read_file(TENANT, "docs/keep.md").await.is_some());

    // One commit for the whole folder.
    let commits = server.get(&format!("{}/commits?per_page=1", TENANT)).await;

    assert_eq!(
        commits.json()["commits"][0]["message"],
        "move: docs/guides/ → handbook/"
    );
}

#[tokio::test]
async fn a_recursive_move_needs_a_destination_that_does_not_exist() {
    let server = TestServer::start().await;

    seed(&server).await;

    // Occupied by a file.
    server
        .post(
            &format!("{}/files/docs/guides/move", TENANT),
            json!({
                "author": author(),
                "destination": "other.md",
                "allow_prefix_path_recurse": true
            }),
        )
        .await
        .expect_status(StatusCode::BAD_REQUEST);

    // Occupied by a folder.
    server
        .post(
            &format!("{}/files/docs/guides/move", TENANT),
            json!({
                "author": author(),
                "destination": "docs/guides/deep",
                "allow_prefix_path_recurse": true
            }),
        )
        .await
        .expect_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_folder_cannot_be_moved_inside_itself() {
    let server = TestServer::start().await;

    seed(&server).await;

    let response = server
        .post(
            &format!("{}/files/docs/guides/move", TENANT),
            json!({
                "author": author(),
                "destination": "docs/guides/nested",
                "allow_prefix_path_recurse": true
            }),
        )
        .await;

    response.expect_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_folder_destination_skips_the_extension_whitelist() {
    // It carries no extension of its own, and every file inside keeps its
    // leaf name — extensions are preserved by construction.
    let server = TestServer::builder()
        .allowed_extensions(&["md"])
        .start()
        .await;

    server.write_file(TENANT, "docs/guides/intro.md", "x").await;

    server
        .post(
            &format!("{}/files/docs/guides/move", TENANT),
            json!({
                "author": author(),
                "destination": "handbook",
                "allow_prefix_path_recurse": true
            }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert!(server
        .read_file(TENANT, "handbook/intro.md")
        .await
        .is_some());

    // It still applies when the source turns out to be a file, even with
    // the flag on.
    server
        .post(
            &format!("{}/files/handbook/intro.md/move", TENANT),
            json!({
                "author": author(),
                "destination": "handbook/intro.txt",
                "allow_prefix_path_recurse": true
            }),
        )
        .await
        .expect_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_recursive_folder_move_carries_its_indexes_untouched() {
    // Entries are leaf names, so an index inside a relocated subtree is
    // still correct once it travels with it — no rewriting at all.
    let server = TestServer::start().await;

    seed(&server).await;

    server
        .put(
            &format!("{}/order/docs/guides/deep", TENANT),
            json!({ "author": author(), "order": ["extra.md", "advanced.md"] }),
        )
        .await
        .expect_status(StatusCode::OK);

    server
        .post(
            &format!("{}/files/docs/guides/move", TENANT),
            json!({
                "author": author(),
                "destination": "handbook",
                "allow_prefix_path_recurse": true
            }),
        )
        .await
        .expect_status(StatusCode::OK);

    let moved = server.get(&format!("{}/order/handbook/deep", TENANT)).await;

    moved.expect_status(StatusCode::OK);

    assert_eq!(moved.json()["order"], json!(["extra.md", "advanced.md"]));

    server
        .get(&format!("{}/order/docs/guides/deep", TENANT))
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_recursive_folder_delete_takes_the_indexes_inside_it() {
    let server = TestServer::start().await;

    seed(&server).await;

    server
        .put(
            &format!("{}/order/docs/guides/deep", TENANT),
            json!({ "author": author(), "order": ["extra.md"] }),
        )
        .await
        .expect_status(StatusCode::OK);

    server
        .delete(
            &format!("{}/files/docs/guides", TENANT),
            json!({ "author": author(), "allow_prefix_path_recurse": true }),
        )
        .await
        .expect_status(StatusCode::OK);

    server
        .get(&format!("{}/order/docs/guides/deep", TENANT))
        .await
        .expect_status(StatusCode::NOT_FOUND);
}
