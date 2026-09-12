// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Integration tests for the `/files` routes: write, read, delete, move,
//! list, count, batch read, and existence.

use axum::http::StatusCode;
use serde_json::json;

use crate::tests::harness::{author, TestServer};

const TENANT: &str = "/docs/acme";

/// Leaf names of a listing level, in the order the server returned them.
fn names(nodes: &serde_json::Value) -> Vec<String> {
    nodes
        .as_array()
        .expect("listing level is not an array")
        .iter()
        .map(|node| node["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The children of a named directory node in a listing level.
fn children<'a>(nodes: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    nodes
        .as_array()
        .expect("listing level is not an array")
        .iter()
        .find(|node| node["name"] == name)
        .unwrap_or_else(|| panic!("no directory named {} in {}", name, nodes))
        .get("children")
        .unwrap_or_else(|| panic!("{} has no children", name))
}

// --- Write and read ------------------------------------------------------

#[tokio::test]
async fn a_first_write_initialises_the_repository_and_reads_back() {
    // Repositories are auto-initialised on first write — no provisioning
    // step exists in the API.
    let server = TestServer::start().await;

    assert!(!server.repo_path("docs", "acme").exists());

    let sha = server.write_file(TENANT, "docs/intro.md", "# Hello").await;

    assert!(!sha.is_empty());
    assert!(server.repo_path("docs", "acme").join(".git").is_dir());

    let response = server.get(&format!("{}/files/docs/intro.md", TENANT)).await;

    response.expect_status(StatusCode::OK);

    assert_eq!(response.json()["path"], "docs/intro.md");
    assert_eq!(response.json()["content"], "# Hello");
    // No order index anywhere, so the file is unlisted.
    assert_eq!(response.json()["position"], -1);
}

#[tokio::test]
async fn content_round_trips_byte_for_byte() {
    let server = TestServer::start().await;

    let content =
        "---\ntitle: Ünicode ✓\n---\r\n\r\nline with trailing spaces   \nno final newline";

    server.write_file(TENANT, "a.md", content).await;

    assert_eq!(
        server.read_file(TENANT, "a.md").await.as_deref(),
        Some(content)
    );
}

#[tokio::test]
async fn rewriting_identical_content_is_a_no_op_that_returns_head() {
    // Documented: clients that blindly re-write unchanged files cannot
    // pollute history with empty commits.
    let server = TestServer::start().await;

    let first = server.write_file(TENANT, "a.md", "same").await;
    let second = server.write_file(TENANT, "a.md", "same").await;

    assert_eq!(first, second, "an unchanged write must not create a commit");

    let commits = server.get(&format!("{}/commits", TENANT)).await;

    // Only the initialisation commit and the one write.
    assert_eq!(
        commits.json()["commits"].as_array().unwrap().len(),
        2,
        "an unchanged write created a commit"
    );
}

#[tokio::test]
async fn updating_content_creates_a_new_commit() {
    let server = TestServer::start().await;

    let first = server.write_file(TENANT, "a.md", "one").await;
    let second = server.write_file(TENANT, "a.md", "two").await;

    assert_ne!(first, second);
    assert_eq!(
        server.read_file(TENANT, "a.md").await.as_deref(),
        Some("two")
    );
}

#[tokio::test]
async fn a_write_requires_an_author() {
    let server = TestServer::start().await;

    let response = server
        .put(
            &format!("{}/files/a.md", TENANT),
            json!({ "content": "no author" }),
        )
        .await;

    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn reading_a_missing_file_or_tenant_is_not_found() {
    let server = TestServer::start().await;

    server
        .get(&format!("{}/files/missing.md", TENANT))
        .await
        .expect_status(StatusCode::NOT_FOUND);

    server
        .get("/docs/no-such-tenant/files/a.md")
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_folder_path_is_not_a_file_on_the_read_route() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "docs/intro.md", "x").await;

    server
        .get(&format!("{}/files/docs", TENANT))
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn path_traversal_is_refused_at_the_route() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "x").await;

    // Percent-encoded, because a URL parser resolves `..` segments on the
    // client side — these have to reach the server spelled as traversal.
    for path in [
        "%2E%2E%2Fescape.md",
        "docs%2F%2E%2E%2F%2E%2E%2Fescape.md",
        ".git/config",
        "docs%2F.git%2Fconfig",
    ] {
        let response = server
            .put(
                &format!("{}/files/{}", TENANT, path),
                json!({ "author": author(), "content": "x" }),
            )
            .await;

        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "traversal accepted: {}",
            path
        );
    }
}

#[tokio::test]
async fn an_invalid_tenant_id_is_refused_before_anything_is_touched() {
    let server = TestServer::start().await;

    let response = server.get("/docs/bad..tenant/files/a.md").await;

    response.expect_status(StatusCode::BAD_REQUEST);

    assert!(response.error_message().contains("invalid tenant"));
}

#[tokio::test]
async fn the_extension_whitelist_applies_to_writes_and_move_destinations() {
    let server = TestServer::builder()
        .allowed_extensions(&["md", "mdx"])
        .start()
        .await;

    server.write_file(TENANT, "a.md", "x").await;

    server
        .put(
            &format!("{}/files/a.txt", TENANT),
            json!({ "author": author(), "content": "x" }),
        )
        .await
        .expect_status(StatusCode::BAD_REQUEST);

    // A move destination is checked; the source is not, so files written
    // before a whitelist was configured stay movable.
    server
        .post(
            &format!("{}/files/a.md/move", TENANT),
            json!({ "author": author(), "destination": "b.txt" }),
        )
        .await
        .expect_status(StatusCode::BAD_REQUEST);

    server
        .post(
            &format!("{}/files/a.md/move", TENANT),
            json!({ "author": author(), "destination": "b.mdx" }),
        )
        .await
        .expect_status(StatusCode::OK);
}

// --- Existence check -----------------------------------------------------

#[tokio::test]
async fn head_answers_existence_without_a_body() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "docs/intro.md", "x").await;

    let response = server
        .head(&format!("{}/files/docs/intro.md", TENANT))
        .await;

    response.expect_status(StatusCode::OK);

    assert!(response.text.is_empty(), "HEAD must carry no body");

    server
        .head(&format!("{}/files/docs/missing.md", TENANT))
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn check_prefix_path_widens_existence_to_folders() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "docs/guides/intro.md", "x").await;

    // Without the flag, a folder is simply "not a file".
    server
        .head(&format!("{}/files/docs/guides", TENANT))
        .await
        .expect_status(StatusCode::NOT_FOUND);

    server
        .head(&format!(
            "{}/files/docs/guides?check_prefix_path=true",
            TENANT
        ))
        .await
        .expect_status(StatusCode::OK);

    // With the parameter on, a trailing slash is tolerated.
    server
        .head(&format!(
            "{}/files/docs/guides/?check_prefix_path=true",
            TENANT
        ))
        .await
        .expect_status(StatusCode::OK);

    server
        .head(&format!(
            "{}/files/docs/nope?check_prefix_path=true",
            TENANT
        ))
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

// --- Delete and move -----------------------------------------------------

#[tokio::test]
async fn deleting_a_file_removes_it() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "x").await;

    server
        .delete(
            &format!("{}/files/a.md", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert!(server.read_file(TENANT, "a.md").await.is_none());

    // Deleting it again is a 404 rather than a silent success.
    server
        .delete(
            &format!("{}/files/a.md", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn moving_a_file_relocates_its_content() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "docs/old.md", "# Hello").await;

    server
        .post(
            &format!("{}/files/docs/old.md/move", TENANT),
            json!({ "author": author(), "destination": "docs/new.md" }),
        )
        .await
        .expect_status(StatusCode::OK);

    assert!(server.read_file(TENANT, "docs/old.md").await.is_none());
    assert_eq!(
        server.read_file(TENANT, "docs/new.md").await.as_deref(),
        Some("# Hello")
    );
}

#[tokio::test]
async fn post_dispatches_on_the_url_suffix_never_on_the_body() {
    // Documented: the suffix — never the body's shape — decides which
    // operation runs, so a mistyped field cannot silently turn one into
    // the other.
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "x").await;

    let response = server
        .post(
            &format!("{}/files/a.md", TENANT),
            json!({ "author": author(), "destination": "b.md" }),
        )
        .await;

    response.expect_status(StatusCode::BAD_REQUEST);

    server
        .post(
            &format!("{}/files/a.md/unknown", TENANT),
            json!({ "author": author(), "destination": "b.md" }),
        )
        .await
        .expect_status(StatusCode::BAD_REQUEST);
}

// --- Listing -------------------------------------------------------------

/// Writes a small tree used by several listing tests.
async fn seed_tree(server: &TestServer) {
    for path in [
        "README.md",
        "docs/intro.md",
        "docs/guides/advanced.md",
        "docs/guides/basics.md",
        "notes/todo.md",
        ".hidden/secret.md",
        ".dotfile.md",
    ] {
        server.write_file(TENANT, path, "x").await;
    }
}

#[tokio::test]
async fn listing_sorts_directories_before_files_then_alphabetically() {
    let server = TestServer::start().await;

    seed_tree(&server).await;

    let response = server.get(&format!("{}/files", TENANT)).await;

    response.expect_status(StatusCode::OK);

    let body = response.json();

    assert_eq!(body["page"], 1);
    assert_eq!(body["per_page"], 100);
    assert_eq!(body["has_more"], false);

    // Hidden entries are excluded by default, so neither `.hidden/` nor
    // `.dotfile.md` shows.
    assert_eq!(names(&body["files"]), vec!["docs", "notes", "README.md"]);

    assert_eq!(
        names(children(&body["files"], "docs")),
        vec!["guides", "intro.md"]
    );
}

#[tokio::test]
async fn include_hidden_files_reveals_dot_entries_and_their_subtrees() {
    let server = TestServer::start().await;

    seed_tree(&server).await;

    let body = server
        .get(&format!("{}/files?include_hidden_files=true", TENANT))
        .await
        .json();

    assert_eq!(
        names(&body["files"]),
        vec![".hidden", "docs", "notes", ".dotfile.md", "README.md"]
    );

    assert_eq!(
        names(children(&body["files"], ".hidden")),
        vec!["secret.md"]
    );
}

#[tokio::test]
async fn prefix_path_scopes_the_listing_to_a_subdirectory() {
    let server = TestServer::start().await;

    seed_tree(&server).await;

    let body = server
        .get(&format!("{}/files?prefix_path=/docs", TENANT))
        .await
        .json();

    assert_eq!(names(&body["files"]), vec!["guides", "intro.md"]);

    // A non-existent folder is an empty tree, not an error.
    let empty = server
        .get(&format!("{}/files?prefix_path=/nope", TENANT))
        .await
        .json();

    assert_eq!(names(&empty["files"]), Vec::<String>::new());

    // A hidden folder named explicitly lists its contents — the hidden
    // filter applies to entry names, not to prefix_path resolution.
    let hidden = server
        .get(&format!("{}/files?prefix_path=/.hidden", TENANT))
        .await
        .json();

    assert_eq!(names(&hidden["files"]), vec!["secret.md"]);
}

#[tokio::test]
async fn prefix_path_rejects_traversal() {
    let server = TestServer::start().await;

    seed_tree(&server).await;

    for prefix in ["/../etc", "/docs/../..", "/.git"] {
        server
            .get(&format!("{}/files?prefix_path={}", TENANT, prefix))
            .await
            .expect_status(StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn maximum_depth_bounds_the_listing_and_leaves_childless_stubs() {
    let server = TestServer::start().await;

    seed_tree(&server).await;

    let body = server
        .get(&format!("{}/files?maximum_depth=1", TENANT))
        .await
        .json();

    assert_eq!(names(&body["files"]), vec!["docs", "notes", "README.md"]);

    // Directories deeper than the limit appear as stubs with no children.
    assert_eq!(
        children(&body["files"], "docs").as_array().unwrap().len(),
        0
    );

    server
        .get(&format!("{}/files?maximum_depth=0", TENANT))
        .await
        .expect_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pagination_windows_over_root_level_entries() {
    let server = TestServer::start().await;

    seed_tree(&server).await;

    let first = server
        .get(&format!("{}/files?page=1&per_page=2", TENANT))
        .await
        .json();

    assert_eq!(names(&first["files"]), vec!["docs", "notes"]);
    assert_eq!(first["has_more"], true);

    let second = server
        .get(&format!("{}/files?page=2&per_page=2", TENANT))
        .await
        .json();

    assert_eq!(names(&second["files"]), vec!["README.md"]);
    assert_eq!(second["has_more"], false);
}

#[tokio::test]
async fn file_name_starts_with_matches_leaf_names_case_insensitively() {
    let server = TestServer::start().await;

    seed_tree(&server).await;

    let body = server
        .get(&format!("{}/files?file_name_starts_with=Intro", TENANT))
        .await
        .json();

    // The match is nested, so its ancestor directory is present purely as
    // structure — and a directory holding no match is pruned entirely.
    assert_eq!(names(&body["files"]), vec!["docs"]);
    assert_eq!(names(children(&body["files"], "docs")), vec!["intro.md"]);
}

#[tokio::test]
async fn file_name_starts_with_accepts_an_array_of_prefixes() {
    let server = TestServer::start().await;

    seed_tree(&server).await;

    let body = server
        .get(&format!(
            "{}/files?file_name_starts_with=%5B%22intro%22%2C%20%22readme%22%5D",
            TENANT
        ))
        .await
        .json();

    assert_eq!(names(&body["files"]), vec!["docs", "README.md"]);
}

#[tokio::test]
async fn a_matched_directory_brings_its_whole_subtree() {
    let server = TestServer::start().await;

    seed_tree(&server).await;

    let body = server
        .get(&format!("{}/files?file_name_starts_with=guid", TENANT))
        .await
        .json();

    assert_eq!(names(&body["files"]), vec!["docs"]);

    // Every descendant shows, whether or not its own name matches.
    assert_eq!(
        names(children(children(&body["files"], "docs"), "guides")),
        vec!["advanced.md", "basics.md"]
    );
}

#[tokio::test]
async fn file_name_starts_with_rejects_empty_and_malformed_values() {
    let server = TestServer::start().await;

    seed_tree(&server).await;

    for value in ["", "%5B%5D", "%5B%22%22%5D", "%5B1%5D", "%5B"] {
        server
            .get(&format!("{}/files?file_name_starts_with={}", TENANT, value))
            .await
            .expect_status(StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn date_bounds_are_validated_strictly() {
    let server = TestServer::start().await;

    seed_tree(&server).await;

    for query in [
        "include_date_from=2026-06-16",
        "include_date_from=nonsense",
        "include_date_to=2026-13-01T00:00:00Z",
        // Equal bounds select nothing, so they are a 400 rather than an
        // empty listing.
        "include_date_from=2026-06-16T10:00:00Z&include_date_to=2026-06-16T10:00:00Z",
        // `from` must be strictly before `to`.
        "include_date_from=2026-06-17T10:00:00Z&include_date_to=2026-06-16T10:00:00Z",
        "include_date_type=whenever",
    ] {
        server
            .get(&format!("{}/files?{}", TENANT, query))
            .await
            .expect_status(StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn a_date_window_around_now_keeps_every_file() {
    let server = TestServer::start().await;

    seed_tree(&server).await;

    let body = server
        .get(&format!(
            "{}/files?include_date_from=2000-01-01T00:00:00Z&include_date_to=2999-01-01T00:00:00Z",
            TENANT
        ))
        .await
        .json();

    assert_eq!(names(&body["files"]), vec!["docs", "notes", "README.md"]);

    // A window entirely in the past keeps nothing, and directories left
    // holding no surviving file are pruned rather than shown as stubs.
    let empty = body_of_past_window(&server).await;

    assert_eq!(names(&empty["files"]), Vec::<String>::new());
}

async fn body_of_past_window(server: &TestServer) -> serde_json::Value {
    server
        .get(&format!(
            "{}/files?include_date_from=2000-01-01T00:00:00Z&include_date_to=2001-01-01T00:00:00Z",
            TENANT
        ))
        .await
        .json()
}

// --- Count ---------------------------------------------------------------

#[tokio::test]
async fn counting_walks_the_same_tree_as_the_listing() {
    let server = TestServer::start().await;

    seed_tree(&server).await;

    let body = server.get(&format!("{}/count/files", TENANT)).await.json();

    // Visible files: README.md, docs/intro.md, docs/guides/{advanced,basics}.md,
    // notes/todo.md. Visible directories: docs, docs/guides, notes.
    assert_eq!(body["files"], 5);
    assert_eq!(body["directories"], 3);

    let with_hidden = server
        .get(&format!("{}/count/files?include_hidden_files=true", TENANT))
        .await
        .json();

    assert_eq!(with_hidden["files"], 7);
    assert_eq!(with_hidden["directories"], 4);
}

#[tokio::test]
async fn counting_honours_prefix_path_and_maximum_depth() {
    let server = TestServer::start().await;

    seed_tree(&server).await;

    let scoped = server
        .get(&format!("{}/count/files?prefix_path=/docs", TENANT))
        .await
        .json();

    assert_eq!(scoped["files"], 3);
    assert_eq!(scoped["directories"], 1);

    // A directory sitting at the depth limit is counted — it exists at a
    // visible level — but its contents are not.
    let bounded = server
        .get(&format!(
            "{}/count/files?prefix_path=/docs&maximum_depth=1",
            TENANT
        ))
        .await
        .json();

    assert_eq!(bounded["files"], 1);
    assert_eq!(bounded["directories"], 1);

    server
        .get(&format!("{}/count/files?maximum_depth=0", TENANT))
        .await
        .expect_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn restrict_file_extensions_narrows_files_but_never_directories() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "docs/intro.md", "x").await;
    server.write_file(TENANT, "docs/page.mdx", "x").await;
    server.write_file(TENANT, "docs/notes.txt", "x").await;
    server.write_file(TENANT, "docs/LICENSE", "x").await;

    let body = server
        .get(&format!(
            "{}/count/files?restrict_file_extensions=%5B%22md%22%5D",
            TENANT
        ))
        .await
        .json();

    assert_eq!(body["files"], 1);
    assert_eq!(body["directories"], 1);

    // Leading dots are trimmed and matching is case-insensitive.
    let both = server
        .get(&format!(
            "{}/count/files?restrict_file_extensions=%5B%22.MD%22%2C%22mdx%22%5D",
            TENANT
        ))
        .await
        .json();

    assert_eq!(both["files"], 2);

    for value in ["%5B%5D", "%22md%22", "%5B%22%22%5D"] {
        server
            .get(&format!(
                "{}/count/files?restrict_file_extensions={}",
                TENANT, value
            ))
            .await
            .expect_status(StatusCode::BAD_REQUEST);
    }
}

// --- Seeked reads --------------------------------------------------------

#[tokio::test]
async fn a_seeked_read_narrows_content_to_the_window() {
    let server = TestServer::start().await;

    server
        .write_file(TENANT, "a.md", "---\ntitle: Hello\n---\n# Heading\nBody\n")
        .await;

    let body = server
        .get(&format!(
            "{}/files/a.md?seek_from_line_starts_with=%5B%22---%22%5D\
             &seek_to_line_starts_with=%24seek_from_line_starts_with",
            TENANT
        ))
        .await
        .json();

    assert_eq!(body["content"], "---\ntitle: Hello\n---\n");
    // The response shape is unchanged — only `content` narrows.
    assert_eq!(body["path"], "a.md");
}

#[tokio::test]
async fn a_seek_that_matches_nothing_is_an_empty_body_not_an_error() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "# Heading\n").await;

    let response = server
        .get(&format!(
            "{}/files/a.md?seek_from_line_starts_with=%5B%22@@@%22%5D",
            TENANT
        ))
        .await;

    response.expect_status(StatusCode::OK);

    assert_eq!(response.json()["content"], "");
}

#[tokio::test]
async fn malformed_seek_parameters_are_rejected() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "x\n").await;

    for query in [
        "seek_from_line_starts_with=---",
        "seek_from_line_starts_with=%5B%5D",
        "seek_lines_maximum=0",
        "seek_to_line_starts_with=%24seek_from_line_starts_with",
    ] {
        server
            .get(&format!("{}/files/a.md?{}", TENANT, query))
            .await
            .expect_status(StatusCode::BAD_REQUEST);
    }
}

// --- Batch read ----------------------------------------------------------

#[tokio::test]
async fn a_batch_read_is_index_aligned_with_its_request() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "# A").await;
    server.write_file(TENANT, "docs/b.md", "# B").await;

    let response = server
        .post(
            &format!("{}/batch/files/read", TENANT),
            json!({ "files": ["a.md", "missing.md", "docs/b.md"] }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    let files = response.json()["files"].clone();

    assert_eq!(files[0]["path"], "a.md");
    assert_eq!(files[0]["content"], "# A");
    // `null` strictly means "not found".
    assert!(files[1].is_null());
    assert_eq!(files[2]["path"], "docs/b.md");

    // No `position` field: a batch spans arbitrary directories, so ordering
    // information would cost one index read per distinct parent.
    assert!(files[0].get("position").is_none());
}

#[tokio::test]
async fn a_folder_reads_as_null_in_a_batch() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "docs/b.md", "# B").await;

    let body = server
        .post(
            &format!("{}/batch/files/read", TENANT),
            json!({ "files": ["docs"] }),
        )
        .await
        .json();

    assert!(body["files"][0].is_null());
}

#[tokio::test]
async fn an_entry_seek_replaces_the_request_level_one_entirely() {
    // Documented: no field-by-field merge.
    let server = TestServer::start().await;

    let content = "---\ntitle: T\n---\nbody\nmore\n";

    server.write_file(TENANT, "a.md", content).await;
    server.write_file(TENANT, "b.md", content).await;

    let body = server
        .post(
            &format!("{}/batch/files/read", TENANT),
            json!({
                "files": ["a.md", { "path": "b.md", "seek": { "lines_maximum": 2 } }],
                "seek": {
                    "from_line_starts_with": ["---"],
                    "to_line_starts_with": "$seek_from_line_starts_with"
                }
            }),
        )
        .await
        .json();

    assert_eq!(body["files"][0]["content"], "---\ntitle: T\n---\n");
    // The entry seek carries no from/to filters even though the shared one
    // sets them, so the window starts at line 0.
    assert_eq!(body["files"][1]["content"], "---\ntitle: T\n");
}

#[tokio::test]
async fn a_batch_is_validated_before_the_repository_is_touched() {
    let server = TestServer::builder()
        .batch_read_maximum_files(2)
        .start()
        .await;

    server.write_file(TENANT, "a.md", "# A").await;

    // Over the cap.
    server
        .post(
            &format!("{}/batch/files/read", TENANT),
            json!({ "files": ["a.md", "b.md", "c.md"] }),
        )
        .await
        .expect_status(StatusCode::BAD_REQUEST);

    // Empty list.
    server
        .post(
            &format!("{}/batch/files/read", TENANT),
            json!({ "files": [] }),
        )
        .await
        .expect_status(StatusCode::BAD_REQUEST);

    // Duplicate after sanitisation — `/a.md` and `a.md` are one path.
    let duplicate = server
        .post(
            &format!("{}/batch/files/read", TENANT),
            json!({ "files": ["a.md", "/a.md"] }),
        )
        .await;

    duplicate.expect_status(StatusCode::BAD_REQUEST);

    // Invalid path.
    server
        .post(
            &format!("{}/batch/files/read", TENANT),
            json!({ "files": ["../escape.md"] }),
        )
        .await
        .expect_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_batch_read_on_a_missing_tenant_is_not_found() {
    let server = TestServer::start().await;

    server
        .post(
            "/docs/no-such-tenant/batch/files/read",
            json!({ "files": ["a.md"] }),
        )
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

// --- Tenant deletion -----------------------------------------------------

#[tokio::test]
async fn deleting_a_tenant_removes_the_whole_repository() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "x").await;

    assert!(server.repo_path("docs", "acme").exists());

    server
        .request(reqwest::Method::DELETE, TENANT, None, true)
        .await
        .expect_status(StatusCode::OK);

    assert!(!server.repo_path("docs", "acme").exists());

    server
        .get(&format!("{}/files/a.md", TENANT))
        .await
        .expect_status(StatusCode::NOT_FOUND);

    // Deleting it again is a 404.
    server
        .request(reqwest::Method::DELETE, TENANT, None, true)
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn two_collections_holding_the_same_tenant_id_are_separate_repositories() {
    // `collection_id` and `tenant_id` together are the repository identity —
    // this is why a hook receiver must key on both.
    let server = TestServer::start().await;

    server.write_file("/docs/acme", "a.md", "docs copy").await;
    server.write_file("/blog/acme", "a.md", "blog copy").await;

    assert_eq!(
        server.read_file("/docs/acme", "a.md").await.as_deref(),
        Some("docs copy")
    );
    assert_eq!(
        server.read_file("/blog/acme", "a.md").await.as_deref(),
        Some("blog copy")
    );

    server
        .request(reqwest::Method::DELETE, "/docs/acme", None, true)
        .await
        .expect_status(StatusCode::OK);

    assert!(server.read_file("/blog/acme", "a.md").await.is_some());
}

// --- Date-range filtering ------------------------------------------------

/// Builds a repository whose files have distinct, widely separated git
/// dates — which no sequence of API calls can produce, since every write is
/// stamped `now`.
///
/// The resulting dates:
///
/// | file     | created    | updated    |
/// |----------|------------|------------|
/// | `a.md`   | 2020-01-01 | 2022-01-01 |
/// | `b.md`   | 2021-01-01 | 2021-01-01 |
async fn seed_dated_tree(server: &TestServer) {
    use crate::tests::fixture::{at, commit_files_at};

    // One ordinary write so the repository exists with its root commit.
    server.write_file(TENANT, "seed.md", "seed").await;

    let repo = server.repo_path("docs", "acme");

    commit_files_at(
        &repo,
        at("2020-01-01T00:00:00Z"),
        "add a",
        &[("a.md", b"one")],
    );
    commit_files_at(
        &repo,
        at("2021-01-01T00:00:00Z"),
        "add b",
        &[("b.md", b"one")],
    );
    commit_files_at(
        &repo,
        at("2022-01-01T00:00:00Z"),
        "edit a",
        &[("a.md", b"two")],
    );
}

#[tokio::test]
async fn the_date_window_is_half_open_on_the_updated_date_by_default() {
    let server = TestServer::start().await;

    seed_dated_tree(&server).await;

    // `updated` is the most recent commit that touched the file.
    let body = server
        .get(&format!(
            "{}/files?include_date_from=2021-06-01T00:00:00Z",
            TENANT
        ))
        .await
        .json();

    // a.md was last touched in 2022, and the seed file is stamped now —
    // both are after the bound. b.md's last touch (2021-01) is not.
    assert_eq!(names(&body["files"]), vec!["a.md", "seed.md"]);

    let body = server
        .get(&format!(
            "{}/files?include_date_to=2021-06-01T00:00:00Z",
            TENANT
        ))
        .await
        .json();

    // b.md (2021-01) is in; a.md (2022) is out; the seed file is stamped now
    // and so is out too.
    assert_eq!(names(&body["files"]), vec!["b.md"]);
}

#[tokio::test]
async fn include_date_type_created_selects_a_different_set_from_the_same_window() {
    let server = TestServer::start().await;

    seed_dated_tree(&server).await;

    // Same bound, different date: by `created`, a.md belongs to 2020 rather
    // than to 2022, so it falls inside a window that excludes it by
    // `updated`.
    let body = server
        .get(&format!(
            "{}/files?include_date_to=2021-06-01T00:00:00Z&include_date_type=created",
            TENANT
        ))
        .await
        .json();

    assert_eq!(names(&body["files"]), vec!["a.md", "b.md"]);
}

#[tokio::test]
async fn a_date_filter_prunes_directories_left_holding_nothing() {
    use crate::tests::fixture::{at, commit_files_at};

    let server = TestServer::start().await;

    server.write_file(TENANT, "seed.md", "seed").await;

    let repo = server.repo_path("docs", "acme");

    commit_files_at(
        &repo,
        at("2020-01-01T00:00:00Z"),
        "old",
        &[("old/a.md", b"x")],
    );
    commit_files_at(
        &repo,
        at("2022-01-01T00:00:00Z"),
        "new",
        &[("new/b.md", b"x")],
    );

    let body = server
        .get(&format!(
            "{}/files?include_date_from=2021-01-01T00:00:00Z&include_date_to=2023-01-01T00:00:00Z",
            TENANT
        ))
        .await
        .json();

    // Directories survive only as the structure leading to a surviving file.
    assert_eq!(names(&body["files"]), vec!["new"]);
    assert_eq!(names(children(&body["files"], "new")), vec!["b.md"]);
}

#[tokio::test]
async fn a_date_filter_intersects_with_a_name_search() {
    let server = TestServer::start().await;

    seed_dated_tree(&server).await;

    // A file must match both the name prefix and the date window.
    let body = server
        .get(&format!(
            "{}/files?file_name_starts_with=a&include_date_to=2021-06-01T00:00:00Z",
            TENANT
        ))
        .await
        .json();

    assert_eq!(names(&body["files"]), Vec::<String>::new());

    let body = server
        .get(&format!(
            "{}/files?file_name_starts_with=b&include_date_to=2021-06-01T00:00:00Z",
            TENANT
        ))
        .await
        .json();

    assert_eq!(names(&body["files"]), vec!["b.md"]);
}

// --- Search and depth composition ----------------------------------------

#[tokio::test]
async fn a_matched_directory_at_the_depth_limit_renders_as_a_childless_stub() {
    // `maximum_depth` bounds the descent uniformly: a directory sitting *at*
    // the limit shows because it matched, but nothing below it is walked.
    let server = TestServer::start().await;

    seed_tree(&server).await;

    let body = server
        .get(&format!(
            "{}/files?file_name_starts_with=docs&maximum_depth=1",
            TENANT
        ))
        .await
        .json();

    assert_eq!(names(&body["files"]), vec!["docs"]);
    assert_eq!(
        children(&body["files"], "docs").as_array().unwrap().len(),
        0
    );
}

#[tokio::test]
async fn a_match_below_the_depth_limit_is_never_found() {
    let server = TestServer::start().await;

    seed_tree(&server).await;

    // `advanced.md` lives at depth 3 and is invisible to a depth-2 listing.
    let body = server
        .get(&format!(
            "{}/files?file_name_starts_with=advanced&maximum_depth=2",
            TENANT
        ))
        .await
        .json();

    assert_eq!(names(&body["files"]), Vec::<String>::new());
}

// --- Content that cannot be represented ----------------------------------

#[tokio::test]
async fn a_file_that_is_not_utf8_fails_the_read_rather_than_reading_as_missing() {
    use crate::tests::fixture::commit_files_at;

    let server = TestServer::start().await;

    server.write_file(TENANT, "ok.md", "fine").await;

    // A blob the write route could never have accepted — its `content` is a
    // JSON string — but which a hand-made commit can hold.
    commit_files_at(
        &server.repo_path("docs", "acme"),
        1_780_000_000,
        "binary",
        &[("binary.md", &[0xff, 0xfe, 0x00])],
    );

    let single = server.get(&format!("{}/files/binary.md", TENANT)).await;

    single.expect_status(StatusCode::UNPROCESSABLE_ENTITY);

    assert!(
        single.error_message().contains("binary.md"),
        "{}",
        single.text
    );

    // In a batch it is a hard failure too, naming the path: `null` must
    // strictly mean "not found", never "unreadable".
    let batch = server
        .post(
            &format!("{}/batch/files/read", TENANT),
            json!({ "files": ["ok.md", "binary.md"] }),
        )
        .await;

    batch.expect_status(StatusCode::UNPROCESSABLE_ENTITY);

    assert!(
        batch.error_message().contains("binary.md"),
        "{}",
        batch.text
    );
}

// --- Seeking a packed object ---------------------------------------------

#[tokio::test]
async fn seeking_works_the_same_on_a_packed_object_as_on_a_loose_one() {
    // Loose objects are seeked through a streaming ODB read that stops
    // inflating early; libgit2 cannot stream a packed object, so those fall
    // back to a cursor over the blob. Both must return the same window.
    let server = TestServer::start().await;

    let content = "---\ntitle: T\n---\n# Heading\nbody\n";

    server.write_file(TENANT, "a.md", content).await;

    let query = format!(
        "{}/files/a.md?seek_from_line_starts_with=%5B%22---%22%5D\
         &seek_to_line_starts_with=%24seek_from_line_starts_with",
        TENANT
    );

    let loose = server.get(&query).await.json()["content"].clone();

    assert_eq!(loose, "---\ntitle: T\n---\n");

    // Consolidate every loose object into a packfile, then read again.
    crate::git::GitMaintenance::run(&server.repo_path("docs", "acme"), false, true)
        .expect("maintenance failed");

    assert_eq!(
        crate::git::GitMaintenance::pack_count(&server.repo_path("docs", "acme")),
        1
    );

    let packed = server.get(&query).await.json()["content"].clone();

    assert_eq!(packed, loose);

    // And a whole-file read is unaffected too.
    assert_eq!(
        server.read_file(TENANT, "a.md").await.as_deref(),
        Some(content)
    );
}
