// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Integration tests for the file order index: the `/order` routes, the
//! `/files/*path/reorder` route, ordered listings, and the implicit upkeep
//! that rides along with file operations.

use axum::http::StatusCode;
use serde_json::{json, Value};

use crate::tests::harness::{author, TestServer};

const TENANT: &str = "/docs/acme";

fn names(nodes: &Value) -> Vec<String> {
    nodes
        .as_array()
        .expect("listing level is not an array")
        .iter()
        .map(|node| node["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Writes an order index for a directory and asserts it committed.
async fn put_order(server: &TestServer, directory: &str, order: &[&str]) -> String {
    let response = server
        .put(
            &format!("{}/order{}", TENANT, directory),
            json!({ "author": author(), "order": order }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    response.json()["commit_sha"].as_str().unwrap().to_string()
}

/// Seeds a directory with three files and one sub-directory.
async fn seed(server: &TestServer) {
    server.write_file(TENANT, "docs/intro.md", "x").await;
    server.write_file(TENANT, "docs/advanced.mdx", "x").await;
    server.write_file(TENANT, "docs/zebra.md", "x").await;
    server
        .write_file(TENANT, "docs/getting-started/first.md", "x")
        .await;
}

// --- The /order routes ---------------------------------------------------

#[tokio::test]
async fn an_order_round_trips_in_the_canonical_spelling() {
    let server = TestServer::start().await;

    seed(&server).await;

    // The caller may spell a directory either way; the server stores
    // directories with a trailing slash and files without.
    put_order(
        &server,
        "/docs",
        &["intro.md", "getting-started", "advanced.mdx"],
    )
    .await;

    let response = server.get(&format!("{}/order/docs", TENANT)).await;

    response.expect_status(StatusCode::OK);

    let body = response.json();

    assert_eq!(body["directory"], "docs");
    assert_eq!(
        body["order"],
        json!(["intro.md", "getting-started/", "advanced.mdx"])
    );
}

#[tokio::test]
async fn the_repository_root_has_its_own_addressable_order() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "README.md", "x").await;
    server.write_file(TENANT, "docs/intro.md", "x").await;

    let response = server
        .put(
            &format!("{}/order", TENANT),
            json!({ "author": author(), "order": ["README.md", "docs"] }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    let body = server.get(&format!("{}/order", TENANT)).await.json();

    assert_eq!(body["directory"], "");
    assert_eq!(body["order"], json!(["README.md", "docs/"]));
}

#[tokio::test]
async fn a_directory_with_no_order_is_not_found_rather_than_empty() {
    // "Unordered" and "ordered as nothing" must not be confusable.
    let server = TestServer::start().await;

    seed(&server).await;

    server
        .get(&format!("{}/order/docs", TENANT))
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn writing_the_order_a_directory_already_holds_is_a_no_op() {
    let server = TestServer::start().await;

    seed(&server).await;

    let first = put_order(&server, "/docs", &["intro.md", "advanced.mdx"]).await;
    let second = put_order(&server, "/docs", &["intro.md", "advanced.mdx"]).await;

    assert_eq!(first, second, "an unchanged order write must not commit");
}

#[tokio::test]
async fn an_order_entry_must_exist_in_that_directory() {
    let server = TestServer::start().await;

    seed(&server).await;

    let response = server
        .put(
            &format!("{}/order/docs", TENANT),
            json!({ "author": author(), "order": ["intro.md", "ghost.md"] }),
        )
        .await;

    response.expect_status(StatusCode::BAD_REQUEST);

    // A directory that does not exist at all is a 404.
    server
        .put(
            &format!("{}/order/nope", TENANT),
            json!({ "author": author(), "order": ["a.md"] }),
        )
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_order_may_be_sparse() {
    // Entries must exist, but not every existing sibling need be listed.
    let server = TestServer::start().await;

    seed(&server).await;

    put_order(&server, "/docs", &["zebra.md"]).await;

    let body = server.get(&format!("{}/order/docs", TENANT)).await.json();

    assert_eq!(body["order"], json!(["zebra.md"]));
}

#[tokio::test]
async fn an_empty_order_is_rejected_and_delete_is_how_you_unset_one() {
    let server = TestServer::start().await;

    seed(&server).await;

    server
        .put(
            &format!("{}/order/docs", TENANT),
            json!({ "author": author(), "order": [] }),
        )
        .await
        .expect_status(StatusCode::BAD_REQUEST);

    put_order(&server, "/docs", &["intro.md"]).await;

    server
        .delete(
            &format!("{}/order/docs", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    server
        .get(&format!("{}/order/docs", TENANT))
        .await
        .expect_status(StatusCode::NOT_FOUND);

    // Deleting an order that is not there is a 404, not a silent success.
    server
        .delete(
            &format!("{}/order/docs", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn hidden_entries_need_an_opt_in_on_a_whole_order_write() {
    let server = TestServer::start().await;

    seed(&server).await;
    server.write_file(TENANT, "docs/.secret.md", "x").await;

    let rejected = server
        .put(
            &format!("{}/order/docs", TENANT),
            json!({ "author": author(), "order": ["intro.md", ".secret.md"] }),
        )
        .await;

    rejected.expect_status(StatusCode::BAD_REQUEST);

    assert!(rejected.error_message().contains("allow_hidden_files"));

    server
        .put(
            &format!("{}/order/docs", TENANT),
            json!({
                "author": author(),
                "order": ["intro.md", ".secret.md"],
                "allow_hidden_files": true
            }),
        )
        .await
        .expect_status(StatusCode::OK);
}

#[tokio::test]
async fn the_index_is_invisible_to_every_files_route() {
    // Documented: the index is a separate resource, never a file — which is
    // what makes its format impossible to bypass.
    let server = TestServer::start().await;

    seed(&server).await;
    put_order(&server, "/docs", &["intro.md"]).await;

    // Listing, with hidden entries included.
    let body = server
        .get(&format!(
            "{}/files?prefix_path=/docs&include_hidden_files=true",
            TENANT
        ))
        .await
        .json();

    assert!(
        !names(&body["files"]).contains(&".order.json".to_string()),
        "the index leaked into a listing: {:?}",
        names(&body["files"])
    );

    // Read, HEAD, and batch read.
    server
        .get(&format!("{}/files/docs/.order.json", TENANT))
        .await
        .expect_status(StatusCode::NOT_FOUND);

    server
        .head(&format!("{}/files/docs/.order.json", TENANT))
        .await
        .expect_status(StatusCode::NOT_FOUND);

    let batch = server
        .post(
            &format!("{}/batch/files/read", TENANT),
            json!({ "files": ["docs/.order.json"] }),
        )
        .await
        .json();

    assert!(batch["files"][0].is_null());

    // Writes refuse the path outright, pointing at the order routes.
    let write = server
        .put(
            &format!("{}/files/docs/.order.json", TENANT),
            json!({ "author": author(), "content": "{}" }),
        )
        .await;

    write.expect_status(StatusCode::BAD_REQUEST);

    assert!(write.error_message().contains("/order"), "{}", write.text);

    // A delete or a move source sees it as simply not a file.
    server
        .delete(
            &format!("{}/files/docs/.order.json", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_count_route_never_counts_an_index() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "docs/intro.md", "x").await;

    let before = server
        .get(&format!("{}/count/files?include_hidden_files=true", TENANT))
        .await
        .json();

    put_order(&server, "/docs", &["intro.md"]).await;

    let after = server
        .get(&format!("{}/count/files?include_hidden_files=true", TENANT))
        .await
        .json();

    assert_eq!(before["files"], after["files"]);
}

// --- Ordered listings ----------------------------------------------------

#[tokio::test]
async fn apply_order_index_orders_a_level_and_interleaves_kinds() {
    let server = TestServer::start().await;

    seed(&server).await;

    // Default order is directories first, then alphabetical.
    let plain = server
        .get(&format!("{}/files?prefix_path=/docs", TENANT))
        .await
        .json();

    assert_eq!(
        names(&plain["files"]),
        vec!["getting-started", "advanced.mdx", "intro.md", "zebra.md"]
    );

    put_order(
        &server,
        "/docs",
        &["intro.md", "getting-started", "advanced.mdx"],
    )
    .await;

    let ordered = server
        .get(&format!(
            "{}/files?prefix_path=/docs&apply_order_index=true",
            TENANT
        ))
        .await
        .json();

    // Listed entries first, in index order, files and directories
    // interleaved freely; unlisted ones follow in the ordinary order.
    assert_eq!(
        names(&ordered["files"]),
        vec!["intro.md", "getting-started", "advanced.mdx", "zebra.md"]
    );
}

#[tokio::test]
async fn implicit_order_default_index_places_the_unlisted_entries() {
    let server = TestServer::start().await;

    seed(&server).await;

    put_order(&server, "/docs", &["intro.md", "advanced.mdx"]).await;

    // 0 (or any negative value) lifts every unordered entry above the whole
    // index — an unlisted entry sorts before a listed one on an equal index.
    for value in ["0", "-1"] {
        let body = server
            .get(&format!(
                "{}/files?prefix_path=/docs&apply_order_index=true&implicit_order_default_index={}",
                TENANT, value
            ))
            .await
            .json();

        assert_eq!(
            names(&body["files"]),
            vec!["getting-started", "zebra.md", "intro.md", "advanced.mdx"],
            "implicit_order_default_index={}",
            value
        );
    }

    // 2 slots them between the index's second and third entries — here,
    // after both listed entries.
    let between = server
        .get(&format!(
            "{}/files?prefix_path=/docs&apply_order_index=true&implicit_order_default_index=2",
            TENANT
        ))
        .await
        .json();

    assert_eq!(
        names(&between["files"]),
        vec!["intro.md", "advanced.mdx", "getting-started", "zebra.md"]
    );
}

#[tokio::test]
async fn the_ordering_parameters_are_inert_on_their_own() {
    // `implicit_order_default_index` is only read when apply_order_index is
    // on, exactly as include_date_type is inert without a date bound.
    let server = TestServer::start().await;

    seed(&server).await;
    put_order(&server, "/docs", &["zebra.md"]).await;

    let body = server
        .get(&format!(
            "{}/files?prefix_path=/docs&implicit_order_default_index=0",
            TENANT
        ))
        .await
        .json();

    assert_eq!(
        names(&body["files"]),
        vec!["getting-started", "advanced.mdx", "intro.md", "zebra.md"]
    );
}

#[tokio::test]
async fn a_stale_index_entry_ranks_nothing_and_never_fails_a_listing() {
    let server = TestServer::start().await;

    seed(&server).await;
    put_order(&server, "/docs", &["zebra.md", "intro.md"]).await;

    // Deleting a file drops it from the index, so reach staleness the only
    // other way: order a file, then check a listing still renders after the
    // file is gone by way of a folder move out of the directory.
    server
        .delete(
            &format!("{}/files/docs/zebra.md", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    let body = server
        .get(&format!(
            "{}/files?prefix_path=/docs&apply_order_index=true",
            TENANT
        ))
        .await
        .json();

    assert_eq!(
        names(&body["files"]),
        vec!["intro.md", "getting-started", "advanced.mdx"]
    );
}

#[tokio::test]
async fn a_read_reports_the_files_position_in_its_parents_index() {
    let server = TestServer::start().await;

    seed(&server).await;
    put_order(&server, "/docs", &["intro.md", "advanced.mdx"]).await;

    let intro = server
        .get(&format!("{}/files/docs/intro.md", TENANT))
        .await
        .json();

    assert_eq!(intro["position"], 0);

    let advanced = server
        .get(&format!("{}/files/docs/advanced.mdx", TENANT))
        .await
        .json();

    assert_eq!(advanced["position"], 1);

    // -1 for an unlisted file, which is also the answer when the directory
    // has no index at all — from the caller's point of view the same state.
    let zebra = server
        .get(&format!("{}/files/docs/zebra.md", TENANT))
        .await
        .json();

    assert_eq!(zebra["position"], -1);
}

// --- The reorder route ---------------------------------------------------

/// A directory's stored order, or `None` when it has none.
async fn stored_order(server: &TestServer, directory: &str) -> Option<Vec<String>> {
    let response = server.get(&format!("{}/order{}", TENANT, directory)).await;

    if response.status == StatusCode::NOT_FOUND {
        return None;
    }

    response.expect_status(StatusCode::OK);

    Some(
        response.json()["order"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry.as_str().unwrap().to_string())
            .collect(),
    )
}

async fn reorder(
    server: &TestServer,
    path: &str,
    body: Value,
) -> crate::tests::harness::TestResponse {
    server
        .post(&format!("{}/files/{}/reorder", TENANT, path), body)
        .await
}

#[tokio::test]
async fn a_first_reorder_materialises_the_index_over_the_whole_directory() {
    // Documented: the price is that the first reorder in a directory pins
    // all of its entries, which is what makes the second one land where the
    // caller expects.
    let server = TestServer::start().await;

    seed(&server).await;

    reorder(
        &server,
        "docs/zebra.md",
        json!({ "author": author(), "position": 0 }),
    )
    .await
    .expect_status(StatusCode::OK);

    assert_eq!(
        stored_order(&server, "/docs").await.unwrap(),
        vec!["zebra.md", "getting-started/", "advanced.mdx", "intro.md"]
    );
}

#[tokio::test]
async fn a_position_counts_against_the_whole_directory_as_rendered() {
    let server = TestServer::start().await;

    seed(&server).await;

    // The rendered order is [getting-started, advanced.mdx, intro.md,
    // zebra.md]; position 2 must put the entry where the caller saw row 2.
    reorder(
        &server,
        "docs/zebra.md",
        json!({ "author": author(), "position": 2 }),
    )
    .await
    .expect_status(StatusCode::OK);

    assert_eq!(
        stored_order(&server, "/docs").await.unwrap(),
        vec!["getting-started/", "advanced.mdx", "zebra.md", "intro.md"]
    );
}

#[tokio::test]
async fn implicit_order_default_index_shapes_what_a_reorder_materialises() {
    let server = TestServer::start().await;

    seed(&server).await;
    put_order(&server, "/docs", &["intro.md"]).await;

    // With the unlisted siblings lifted on top, the sequence the caller saw
    // is [getting-started, advanced.mdx, zebra.md, intro.md].
    reorder(
        &server,
        "docs/zebra.md",
        json!({ "author": author(), "position": 0, "implicit_order_default_index": 0 }),
    )
    .await
    .expect_status(StatusCode::OK);

    assert_eq!(
        stored_order(&server, "/docs").await.unwrap(),
        vec!["zebra.md", "getting-started/", "advanced.mdx", "intro.md"]
    );
}

#[tokio::test]
async fn a_position_past_the_end_is_clamped_to_the_tail() {
    // A caller cannot be expected to count the directory.
    let server = TestServer::start().await;

    seed(&server).await;

    reorder(
        &server,
        "docs/intro.md",
        json!({ "author": author(), "position": 999 }),
    )
    .await
    .expect_status(StatusCode::OK);

    let order = stored_order(&server, "/docs").await.unwrap();

    assert_eq!(order.last().unwrap(), "intro.md");
}

#[tokio::test]
async fn position_minus_one_unpins_an_entry_without_materialising_anything() {
    let server = TestServer::start().await;

    seed(&server).await;
    put_order(&server, "/docs", &["intro.md", "advanced.mdx"]).await;

    reorder(
        &server,
        "docs/intro.md",
        json!({ "author": author(), "position": -1 }),
    )
    .await
    .expect_status(StatusCode::OK);

    // Only the named entry left; nothing else was pinned in passing.
    assert_eq!(
        stored_order(&server, "/docs").await.unwrap(),
        vec!["advanced.mdx"]
    );

    // The file itself is untouched.
    assert!(server.read_file(TENANT, "docs/intro.md").await.is_some());
}

#[tokio::test]
async fn unpinning_the_last_entry_removes_the_index() {
    // An empty index and no index are the same state.
    let server = TestServer::start().await;

    seed(&server).await;
    put_order(&server, "/docs", &["intro.md"]).await;

    reorder(
        &server,
        "docs/intro.md",
        json!({ "author": author(), "position": -1 }),
    )
    .await
    .expect_status(StatusCode::OK);

    assert!(stored_order(&server, "/docs").await.is_none());
}

#[tokio::test]
async fn an_already_unlisted_entry_sent_minus_one_is_a_no_op() {
    let server = TestServer::start().await;

    seed(&server).await;

    let before = server.head_sha(TENANT).await;

    // Including when the directory has no index at all.
    let response = reorder(
        &server,
        "docs/intro.md",
        json!({ "author": author(), "position": -1 }),
    )
    .await;

    response.expect_status(StatusCode::OK);

    assert_eq!(response.json()["commit_sha"], before);
    assert!(stored_order(&server, "/docs").await.is_none());
}

#[tokio::test]
async fn asking_for_the_state_the_index_already_holds_is_a_no_op() {
    let server = TestServer::start().await;

    seed(&server).await;

    // Materialise the whole directory first, so the second request's result
    // is byte-for-byte what is stored.
    reorder(
        &server,
        "docs/intro.md",
        json!({ "author": author(), "position": 0 }),
    )
    .await
    .expect_status(StatusCode::OK);

    let settled = server.head_sha(TENANT).await;

    let repeat = reorder(
        &server,
        "docs/intro.md",
        json!({ "author": author(), "position": 0 }),
    )
    .await;

    repeat.expect_status(StatusCode::OK);

    assert_eq!(repeat.json()["commit_sha"], settled);
}

#[tokio::test]
async fn a_position_must_be_a_number_and_never_below_minus_one() {
    let server = TestServer::start().await;

    seed(&server).await;

    for position in [json!(-2), json!("0"), json!(1.5)] {
        reorder(
            &server,
            "docs/intro.md",
            json!({ "author": author(), "position": position }),
        )
        .await
        .expect_status(StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn reordering_something_that_does_not_exist_is_not_found() {
    let server = TestServer::start().await;

    seed(&server).await;

    for position in [json!(0), json!(-1)] {
        reorder(
            &server,
            "docs/ghost.md",
            json!({ "author": author(), "position": position }),
        )
        .await
        .expect_status(StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn hidden_siblings_stay_out_of_a_materialised_index_by_default() {
    let server = TestServer::start().await;

    seed(&server).await;
    server.write_file(TENANT, "docs/.secret.md", "x").await;

    reorder(
        &server,
        "docs/intro.md",
        json!({ "author": author(), "position": 0 }),
    )
    .await
    .expect_status(StatusCode::OK);

    let order = stored_order(&server, "/docs").await.unwrap();

    assert!(
        !order.iter().any(|entry| entry == ".secret.md"),
        "a hidden sibling was pinned without being asked for: {:?}",
        order
    );

    // With the opt-in, hidden siblings are folded in like any other.
    server
        .delete(
            &format!("{}/order/docs", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    reorder(
        &server,
        "docs/intro.md",
        json!({
            "author": author(),
            "position": 0,
            "implicit_allow_hidden_files": true
        }),
    )
    .await
    .expect_status(StatusCode::OK);

    assert!(stored_order(&server, "/docs")
        .await
        .unwrap()
        .iter()
        .any(|entry| entry == ".secret.md"));
}

#[tokio::test]
async fn pinning_a_hidden_entry_requires_the_flag_but_unpinning_one_does_not() {
    let server = TestServer::start().await;

    seed(&server).await;
    server.write_file(TENANT, "docs/.secret.md", "x").await;

    let refused = reorder(
        &server,
        "docs/.secret.md",
        json!({ "author": author(), "position": 0 }),
    )
    .await;

    refused.expect_status(StatusCode::BAD_REQUEST);

    assert!(
        refused
            .error_message()
            .contains("implicit_allow_hidden_files"),
        "{}",
        refused.text
    );

    reorder(
        &server,
        "docs/.secret.md",
        json!({
            "author": author(),
            "position": 0,
            "implicit_allow_hidden_files": true
        }),
    )
    .await
    .expect_status(StatusCode::OK);

    assert_eq!(
        stored_order(&server, "/docs").await.unwrap()[0],
        ".secret.md"
    );

    // The guardrail keeps dot-files out; it does not trap them in, so -1
    // needs no flag.
    reorder(
        &server,
        "docs/.secret.md",
        json!({ "author": author(), "position": -1 }),
    )
    .await
    .expect_status(StatusCode::OK);

    assert!(!stored_order(&server, "/docs")
        .await
        .unwrap()
        .iter()
        .any(|entry| entry == ".secret.md"));
}

#[tokio::test]
async fn a_hidden_entry_the_index_already_names_survives_an_unrelated_reorder() {
    // It was pinned deliberately, and this request said nothing about it.
    let server = TestServer::start().await;

    seed(&server).await;
    server.write_file(TENANT, "docs/.secret.md", "x").await;

    server
        .put(
            &format!("{}/order/docs", TENANT),
            json!({
                "author": author(),
                "order": [".secret.md", "intro.md"],
                "allow_hidden_files": true
            }),
        )
        .await
        .expect_status(StatusCode::OK);

    reorder(
        &server,
        "docs/advanced.mdx",
        json!({ "author": author(), "position": 0 }),
    )
    .await
    .expect_status(StatusCode::OK);

    assert!(stored_order(&server, "/docs")
        .await
        .unwrap()
        .iter()
        .any(|entry| entry == ".secret.md"));
}

#[tokio::test]
async fn a_folder_is_positionable_only_with_allow_prefix_path() {
    let server = TestServer::start().await;

    seed(&server).await;

    // Without the flag, a folder path is simply "not a file".
    reorder(
        &server,
        "docs/getting-started",
        json!({ "author": author(), "position": 0 }),
    )
    .await
    .expect_status(StatusCode::NOT_FOUND);

    reorder(
        &server,
        "docs/getting-started",
        json!({ "author": author(), "position": 0, "allow_prefix_path": true }),
    )
    .await
    .expect_status(StatusCode::OK);

    // Stored in the canonical trailing-slash spelling.
    assert_eq!(
        stored_order(&server, "/docs").await.unwrap()[0],
        "getting-started/"
    );

    // The folder's contents are untouched.
    assert!(server
        .read_file(TENANT, "docs/getting-started/first.md")
        .await
        .is_some());
}

#[tokio::test]
async fn allow_prefix_path_only_permits_and_never_forces() {
    // A file path behaves identically with the flag on.
    let server = TestServer::start().await;

    seed(&server).await;

    reorder(
        &server,
        "docs/zebra.md",
        json!({ "author": author(), "position": 0, "allow_prefix_path": true }),
    )
    .await
    .expect_status(StatusCode::OK);

    assert_eq!(stored_order(&server, "/docs").await.unwrap()[0], "zebra.md");
}

// --- Implicit upkeep -----------------------------------------------------

#[tokio::test]
async fn deleting_a_file_drops_it_from_its_parents_index() {
    let server = TestServer::start().await;

    seed(&server).await;
    put_order(&server, "/docs", &["intro.md", "advanced.mdx", "zebra.md"]).await;

    server
        .delete(
            &format!("{}/files/docs/advanced.mdx", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert_eq!(
        stored_order(&server, "/docs").await.unwrap(),
        vec!["intro.md", "zebra.md"]
    );
}

#[tokio::test]
async fn a_rename_inside_one_directory_keeps_its_position() {
    // Demoting a file to the tail for changing its name would silently
    // reorder content the caller only renamed.
    let server = TestServer::start().await;

    seed(&server).await;
    put_order(&server, "/docs", &["intro.md", "advanced.mdx", "zebra.md"]).await;

    server
        .post(
            &format!("{}/files/docs/advanced.mdx/move", TENANT),
            json!({ "author": author(), "destination": "docs/renamed.mdx" }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert_eq!(
        stored_order(&server, "/docs").await.unwrap(),
        vec!["intro.md", "renamed.mdx", "zebra.md"]
    );
}

#[tokio::test]
async fn a_cross_directory_move_appends_only_where_an_index_already_exists() {
    let server = TestServer::start().await;

    seed(&server).await;
    put_order(&server, "/docs", &["intro.md", "zebra.md"]).await;

    // The destination directory has no index, so none is created — pinning
    // one file while its siblings stay implicitly ordered would surprise.
    server
        .post(
            &format!("{}/files/docs/zebra.md/move", TENANT),
            json!({ "author": author(), "destination": "docs/getting-started/zebra.md" }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert_eq!(
        stored_order(&server, "/docs").await.unwrap(),
        vec!["intro.md"]
    );
    assert!(stored_order(&server, "/docs/getting-started")
        .await
        .is_none());

    // With an index in place at the destination, the entry is appended.
    put_order(&server, "/docs/getting-started", &["first.md"]).await;

    server
        .post(
            &format!("{}/files/docs/intro.md/move", TENANT),
            json!({ "author": author(), "destination": "docs/getting-started/intro.md" }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert_eq!(
        stored_order(&server, "/docs/getting-started")
            .await
            .unwrap(),
        vec!["first.md", "intro.md"]
    );

    // The source index is left empty by that move, so it is removed.
    assert!(stored_order(&server, "/docs").await.is_none());
}

#[tokio::test]
async fn creating_a_file_changes_no_index() {
    // A new file is unlisted, which means "at the tail" — indexes stay
    // sparse by default.
    let server = TestServer::start().await;

    seed(&server).await;
    put_order(&server, "/docs", &["intro.md"]).await;

    server.write_file(TENANT, "docs/fresh.md", "x").await;

    assert_eq!(
        stored_order(&server, "/docs").await.unwrap(),
        vec!["intro.md"]
    );
}
