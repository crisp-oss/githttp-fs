// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Integration tests for the commit routes: listing, detail, revert, and
//! point-in-time rollback.
//!
//! Revert and rollback are separate routes rather than one route with a
//! mode flag, and the difference between them — undo a change vs. restore a
//! point in time — is what most of this module asserts.

use axum::http::StatusCode;
use serde_json::json;

use crate::tests::harness::{author, TestServer};

const TENANT: &str = "/docs/acme";

#[tokio::test]
async fn a_commit_list_is_newest_first_and_carries_the_author() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "one").await;
    server.write_file(TENANT, "b.md", "two").await;

    let response = server.get(&format!("{}/commits", TENANT)).await;

    response.expect_status(StatusCode::OK);

    let body = response.json();

    assert_eq!(body["page"], 1);
    assert_eq!(body["has_more"], false);

    let commits = body["commits"].as_array().unwrap();

    assert_eq!(commits.len(), 3, "two writes plus the initial commit");
    assert_eq!(commits[0]["message"], "create: b.md");
    assert_eq!(commits[1]["message"], "create: a.md");
    assert_eq!(commits[2]["message"], "chore: initialize");

    assert_eq!(commits[0]["author"]["name"], "Test Author");
    assert_eq!(commits[0]["author"]["email"], "test@example.com");

    // Timestamps are RFC 3339 everywhere on this API.
    let committed_at = commits[0]["committed_at"].as_str().unwrap();

    chrono::DateTime::parse_from_rfc3339(committed_at)
        .unwrap_or_else(|err| panic!("committed_at is not RFC 3339 ({}): {}", err, committed_at));
}

#[tokio::test]
async fn an_update_says_update_and_a_first_write_says_create() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "one").await;
    server.write_file(TENANT, "a.md", "two").await;

    let commits = server.get(&format!("{}/commits", TENANT)).await.json();

    assert_eq!(commits["commits"][0]["message"], "update: a.md");
    assert_eq!(commits["commits"][1]["message"], "create: a.md");
}

#[tokio::test]
async fn a_caller_supplied_message_replaces_the_generated_one() {
    let server = TestServer::start().await;

    server
        .put(
            &format!("{}/files/a.md", TENANT),
            json!({ "author": author(), "content": "x", "message": "docs: add the intro" }),
        )
        .await
        .expect_status(StatusCode::OK);

    let commits = server.get(&format!("{}/commits", TENANT)).await.json();

    assert_eq!(commits["commits"][0]["message"], "docs: add the intro");
}

#[tokio::test]
async fn commit_listing_paginates() {
    let server = TestServer::start().await;

    for index in 0..5 {
        server
            .write_file(TENANT, &format!("file-{}.md", index), "x")
            .await;
    }

    let first = server
        .get(&format!("{}/commits?page=1&per_page=2", TENANT))
        .await
        .json();

    assert_eq!(first["commits"].as_array().unwrap().len(), 2);
    assert_eq!(first["has_more"], true);

    let last = server
        .get(&format!("{}/commits?page=3&per_page=2", TENANT))
        .await
        .json();

    // Five writes plus the initial commit: the third page holds the last two.
    assert_eq!(last["commits"].as_array().unwrap().len(), 2);
    assert_eq!(last["has_more"], false);
}

#[tokio::test]
async fn file_path_filters_the_listing_and_follows_renames_backward() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "old.md", "one").await;
    server.write_file(TENANT, "unrelated.md", "x").await;

    server
        .post(
            &format!("{}/files/old.md/move", TENANT),
            json!({ "author": author(), "destination": "new.md" }),
        )
        .await
        .expect_status(StatusCode::OK);

    server.write_file(TENANT, "new.md", "two").await;

    let body = server
        .get(&format!("{}/commits?file_path=new.md", TENANT))
        .await
        .json();

    let messages: Vec<_> = body["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|commit| commit["message"].as_str().unwrap().to_string())
        .collect();

    // Always pass the current path; the server resolves the prior name.
    assert_eq!(
        messages,
        vec!["update: new.md", "move: old.md -> new.md", "create: old.md"]
    );
}

#[tokio::test]
async fn statistics_are_opt_in_on_the_listing_and_unconditional_on_the_detail() {
    let server = TestServer::start().await;

    let sha = server.write_file(TENANT, "a.md", "one\ntwo\n").await;

    let plain = server.get(&format!("{}/commits", TENANT)).await.json();

    assert!(
        plain["commits"][0].get("statistics").is_none(),
        "statistics leaked into a plain listing"
    );

    let with_statistics = server
        .get(&format!("{}/commits?include_statistics=true", TENANT))
        .await
        .json();

    assert_eq!(with_statistics["commits"][0]["statistics"]["insertions"], 2);
    assert_eq!(
        with_statistics["commits"][0]["statistics"]["files_changed"],
        1
    );

    let detail = server
        .get(&format!("{}/commits/{}", TENANT, sha))
        .await
        .json();

    assert_eq!(detail["statistics"]["insertions"], 2);
}

#[tokio::test]
async fn commit_detail_reports_per_file_changes_with_content_and_diff() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "one\n").await;

    let sha = server.write_file(TENANT, "a.md", "two\n").await;

    let response = server.get(&format!("{}/commits/{}", TENANT, sha)).await;

    response.expect_status(StatusCode::OK);

    let body = response.json();

    assert_eq!(body["sha"].as_str().unwrap(), sha);

    let file = &body["files"][0];

    assert_eq!(file["path"], "a.md");
    assert_eq!(file["change"], "updated");
    assert_eq!(file["content"], "two\n");
    assert!(file["diff"].as_str().unwrap().contains("+two"));
}

#[tokio::test]
async fn a_moved_file_carries_its_previous_path_in_the_detail() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "old.md", "# Hello").await;

    let response = server
        .post(
            &format!("{}/files/old.md/move", TENANT),
            json!({ "author": author(), "destination": "new.md" }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    let sha = response.json()["commit_sha"].as_str().unwrap().to_string();

    let detail = server
        .get(&format!("{}/commits/{}", TENANT, sha))
        .await
        .json();

    let file = &detail["files"][0];

    assert_eq!(file["change"], "moved");
    assert_eq!(file["path"], "new.md");
    assert_eq!(file["from_path"], "old.md");
}

#[tokio::test]
async fn a_deleted_file_has_empty_content_in_the_detail() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "x").await;

    let response = server
        .delete(
            &format!("{}/files/a.md", TENANT),
            json!({ "author": author() }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    let sha = response.json()["commit_sha"].as_str().unwrap().to_string();

    let detail = server
        .get(&format!("{}/commits/{}", TENANT, sha))
        .await
        .json();

    assert_eq!(detail["files"][0]["change"], "deleted");
    assert_eq!(detail["files"][0]["content"], "");
}

#[tokio::test]
async fn a_sha_parameter_accepts_only_hexadecimal() {
    // Revspecs must never reach a lookup: no git semantics leak through the
    // API, and a history-search denial of service is impossible.
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "x").await;

    for sha in ["HEAD", "HEAD~1", "master@%7B1%7D", "abc"] {
        server
            .get(&format!("{}/commits/{}", TENANT, sha))
            .await
            .expect_status(StatusCode::BAD_REQUEST);
    }

    // Well-formed but absent is a 404, not a 400.
    server
        .get(&format!("{}/commits/{}", TENANT, "0".repeat(40)))
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_abbreviated_sha_resolves() {
    let server = TestServer::start().await;

    let sha = server.write_file(TENANT, "a.md", "x").await;

    let response = server
        .get(&format!("{}/commits/{}", TENANT, &sha[..7]))
        .await;

    response.expect_status(StatusCode::OK);

    assert_eq!(response.json()["sha"].as_str().unwrap(), sha);
}

// --- Revert --------------------------------------------------------------

#[tokio::test]
async fn a_revert_undoes_a_commit_with_a_new_commit() {
    // Reverts never rewrite history.
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "one").await;

    let target = server.write_file(TENANT, "a.md", "two").await;

    let response = server
        .post(
            &format!("{}/commits/{}/revert", TENANT, target),
            json!({ "author": author() }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    let body = response.json();

    assert_eq!(body["reverted_sha"].as_str().unwrap(), target);
    assert_ne!(body["commit_sha"].as_str().unwrap(), target);

    assert_eq!(
        server.read_file(TENANT, "a.md").await.as_deref(),
        Some("one")
    );

    // The reverted commit is still in history.
    server
        .get(&format!("{}/commits/{}", TENANT, target))
        .await
        .expect_status(StatusCode::OK);
}

#[tokio::test]
async fn reverting_a_creation_deletes_the_file_again() {
    let server = TestServer::start().await;

    let target = server.write_file(TENANT, "a.md", "one").await;

    server
        .post(
            &format!("{}/commits/{}/revert", TENANT, target),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert!(server.read_file(TENANT, "a.md").await.is_none());
}

// --- Rollback ------------------------------------------------------------

#[tokio::test]
async fn a_rollback_restores_the_state_a_commit_had_discarding_later_changes() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "one").await;

    let target = server.write_file(TENANT, "a.md", "two").await;

    server.write_file(TENANT, "a.md", "three").await;
    server.write_file(TENANT, "a.md", "four").await;

    let response = server
        .post(
            &format!("{}/commits/{}/rollback", TENANT, target),
            json!({ "author": author() }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    assert_eq!(
        response.json()["rolled_back_to_sha"].as_str().unwrap(),
        target
    );

    // Restored to the state it had *at* that commit, however many commits
    // changed it since — which is what separates this from a revert.
    assert_eq!(
        server.read_file(TENANT, "a.md").await.as_deref(),
        Some("two")
    );
}

#[tokio::test]
async fn a_rollback_leaves_files_the_commit_never_touched_alone() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "one").await;

    let target = server.write_file(TENANT, "a.md", "two").await;

    server.write_file(TENANT, "untouched.md", "keep me").await;

    server
        .post(
            &format!("{}/commits/{}/rollback", TENANT, target),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert_eq!(
        server.read_file(TENANT, "untouched.md").await.as_deref(),
        Some("keep me")
    );
}

#[tokio::test]
async fn a_rollback_brings_back_a_since_deleted_file() {
    // At :sha it exists, at HEAD it is absent → re-created.
    let server = TestServer::start().await;

    let target = server.write_file(TENANT, "a.md", "one").await;

    server
        .delete(
            &format!("{}/files/a.md", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert!(server.read_file(TENANT, "a.md").await.is_none());

    server
        .post(
            &format!("{}/commits/{}/rollback", TENANT, target),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert_eq!(
        server.read_file(TENANT, "a.md").await.as_deref(),
        Some("one")
    );
}

#[tokio::test]
async fn a_rollback_deletes_a_file_the_target_commit_had_deleted() {
    // At :sha it is absent, at HEAD it exists → deleted again.
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "one").await;

    let response = server
        .delete(
            &format!("{}/files/a.md", TENANT),
            json!({ "author": author() }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    let deletion = response.json()["commit_sha"].as_str().unwrap().to_string();

    server.write_file(TENANT, "a.md", "back again").await;

    server
        .post(
            &format!("{}/commits/{}/rollback", TENANT, deletion),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert!(server.read_file(TENANT, "a.md").await.is_none());
}

#[tokio::test]
async fn a_rollback_that_changes_nothing_creates_no_commit() {
    let server = TestServer::start().await;

    let target = server.write_file(TENANT, "a.md", "one").await;

    let response = server
        .post(
            &format!("{}/commits/{}/rollback", TENANT, target),
            json!({ "author": author() }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    // The repository already holds that state, so the whole request is a
    // no-op and the response carries current HEAD.
    assert_eq!(response.json()["commit_sha"].as_str().unwrap(), target);
}

#[tokio::test]
async fn rolling_back_to_the_initial_commit_is_legal_where_reverting_it_is_not() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "one").await;

    let commits = server.get(&format!("{}/commits", TENANT)).await.json();
    let initial = commits["commits"].as_array().unwrap().last().unwrap()["sha"]
        .as_str()
        .unwrap()
        .to_string();

    // With no parent, reverting has nothing to undo against.
    server
        .post(
            &format!("{}/commits/{}/revert", TENANT, initial),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::BAD_REQUEST);

    // The rollback needs no parent: the change set is simply its whole tree.
    server
        .post(
            &format!("{}/commits/{}/rollback", TENANT, initial),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);
}

#[tokio::test]
async fn a_rollback_restores_a_rename_as_a_rename() {
    // One `file.moved`, not a delete plus a create — which is what preserves
    // downstream entity identity through a rollback.
    //
    // The scenario has to put HEAD back on the *pre*-rename path for the
    // rollback to have a rename to redo: renaming a file and then renaming
    // it back, and rolling back the first of the two.
    let receiver = crate::tests::harness::HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "old.md", "# Hello").await;

    let rename = server
        .post(
            &format!("{}/files/old.md/move", TENANT),
            json!({ "author": author(), "destination": "new.md" }),
        )
        .await;

    rename.expect_status(StatusCode::OK);

    let rename_sha = rename.json()["commit_sha"].as_str().unwrap().to_string();

    server
        .post(
            &format!("{}/files/new.md/move", TENANT),
            json!({ "author": author(), "destination": "old.md" }),
        )
        .await
        .expect_status(StatusCode::OK);

    receiver.wait_for(3).await;

    // HEAD holds the pre-rename path and nothing sits at the post-rename
    // one, so the rollback settles it as a single move.
    server
        .post(
            &format!("{}/commits/{}/rollback", TENANT, rename_sha),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert_eq!(
        server.read_file(TENANT, "new.md").await.as_deref(),
        Some("# Hello")
    );
    assert!(server.read_file(TENANT, "old.md").await.is_none());

    let events = receiver.wait_for_exactly(4).await;

    assert_eq!(events[3]["event"], "file.moved");
    assert_eq!(events[3]["from"]["path"], "old.md");
    assert_eq!(events[3]["to"]["path"], "new.md");
}

#[tokio::test]
async fn a_rollback_leaves_a_path_the_commit_never_touched_even_when_it_is_the_other_half_of_a_rename(
) {
    // Rolling back the commit that *created* a file restores that file and
    // nothing else: the later rename's destination was never in this
    // commit's change set, so it stays exactly where it is.
    let server = TestServer::start().await;

    let create_sha = server.write_file(TENANT, "old.md", "# Hello").await;

    server
        .post(
            &format!("{}/files/old.md/move", TENANT),
            json!({ "author": author(), "destination": "new.md" }),
        )
        .await
        .expect_status(StatusCode::OK);

    server
        .post(
            &format!("{}/commits/{}/rollback", TENANT, create_sha),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert_eq!(
        server.read_file(TENANT, "old.md").await.as_deref(),
        Some("# Hello")
    );
    assert_eq!(
        server.read_file(TENANT, "new.md").await.as_deref(),
        Some("# Hello"),
        "a path outside the commit's change set was touched"
    );
}

#[tokio::test]
async fn a_revert_of_a_rename_is_also_a_rename() {
    let receiver = crate::tests::harness::HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "old.md", "# Hello").await;

    let rename = server
        .post(
            &format!("{}/files/old.md/move", TENANT),
            json!({ "author": author(), "destination": "new.md" }),
        )
        .await;

    rename.expect_status(StatusCode::OK);

    let rename_sha = rename.json()["commit_sha"].as_str().unwrap().to_string();

    receiver.wait_for(2).await;

    server
        .post(
            &format!("{}/commits/{}/revert", TENANT, rename_sha),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert_eq!(
        server.read_file(TENANT, "old.md").await.as_deref(),
        Some("# Hello")
    );

    let events = receiver.wait_for(3).await;

    assert_eq!(events[2]["event"], "file.moved");
    assert_eq!(events[2]["from"]["path"], "new.md");
    assert_eq!(events[2]["to"]["path"], "old.md");
}
