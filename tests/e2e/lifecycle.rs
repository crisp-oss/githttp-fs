// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! One node's process lifecycle: startup, failure exits, boot repairs, and
//! what survives a restart.
//!
//! Everything here is unreachable from the in-crate suite for the same
//! reason: `main` reports these outcomes by *exiting*, or only exhibits them
//! across two process lifetimes. A config that cannot be read ends in
//! `std::process::exit(1)` after an `eprintln!`, which a caller inside the
//! process can neither catch nor observe — a supervisor can, and so can this.

use std::{fs, net::TcpListener, time::Duration};

use serde_json::Value;

use crate::node::{
    free_port, spawn_expecting_exit, spawn_with_config_path, wait_until, Node, NodeSpec,
};

/// A scratch directory plus a spec pointing into it, for the single-node
/// tests that do not want a whole deployment.
fn scratch(name: &str) -> (tempfile::TempDir, NodeSpec) {
    let dir = tempfile::tempdir().expect("cannot create scratch directory");
    let spec = NodeSpec::new(name, &dir.path().join(name));

    (dir, spec)
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn content_survives_a_restart_over_the_same_store() {
    let (_scratch, spec) = scratch("standalone");

    let mut node = Node::start(spec).await;

    node.write_file("docs/acme/files/intro.md", "# Hello")
        .await
        .expect_status(200);

    let before = node
        .get("/docs/acme/commits")
        .await
        .json()
        .get("commits")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default();

    node.restart().await;

    let read = node.read_file("docs/acme/files/intro.md").await;

    read.expect_status(200);

    assert_eq!(
        read.json().get("content").and_then(Value::as_str),
        Some("# Hello")
    );

    // History too, not just the last tree — the store is the whole object
    // database and a restart is not a fresh init.
    let after = node
        .get("/docs/acme/commits")
        .await
        .json()
        .get("commits")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default();

    assert_eq!(before, after);

    // And it takes writes again, appending to the same history.
    node.write_file("docs/acme/files/intro.md", "# Hello again")
        .await
        .expect_status(200);

    let grown = node
        .get("/docs/acme/commits")
        .await
        .json()
        .get("commits")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default();

    assert_eq!(grown, after + 1);
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn a_config_that_cannot_be_read_exits_non_zero_and_says_so() {
    let dir = tempfile::tempdir().expect("cannot create scratch directory");

    let (code, printed) = spawn_with_config_path(&dir.path().join("nowhere.toml"));

    assert_eq!(code, 1, "printed: {}", printed);
    assert!(
        printed.contains("Cannot read config file"),
        "printed: {}",
        printed
    );
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn a_config_that_does_not_parse_exits_non_zero() {
    let dir = tempfile::tempdir().expect("cannot create scratch directory");

    let (code, printed) = spawn_expecting_exit(dir.path(), "[server\nthis is not toml");

    assert_eq!(code, 1, "printed: {}", printed);
    assert!(
        printed.contains("Invalid config file"),
        "printed: {}",
        printed
    );
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn an_invalid_config_reports_every_problem_before_exiting() {
    let dir = tempfile::tempdir().expect("cannot create scratch directory");

    // Two independent problems, so the assertion is that validation reports
    // the set rather than stopping at the first — an operator fixing a
    // config one restart at a time is the failure mode this avoids.
    let config = format!(
        "[server]\n\
         host = \"127.0.0.1\"\n\
         port = 5355\n\
         api_key = \"\"\n\
         repos_path = {}\n\
         \n[limits]\n\
         batch_read_maximum_files = 0\n",
        serde_json::to_string(&dir.path().join("store").to_string_lossy()).unwrap()
    );

    let (code, printed) = spawn_expecting_exit(dir.path(), &config);

    assert_eq!(code, 1, "printed: {}", printed);
    assert!(
        printed.contains("server.api_key must not be empty"),
        "printed: {}",
        printed
    );
    assert!(
        printed.contains("limits.batch_read_maximum_files must be at least 1"),
        "printed: {}",
        printed
    );
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn a_taken_api_port_exits_rather_than_running_without_a_listener() {
    let dir = tempfile::tempdir().expect("cannot create scratch directory");

    // Held for the life of the test, so the node's bind cannot succeed.
    let squatter = TcpListener::bind("127.0.0.1:0").expect("cannot bind squatter");
    let taken = squatter.local_addr().expect("cannot read squatter").port();

    let config = format!(
        "[server]\n\
         host = \"127.0.0.1\"\n\
         port = {}\n\
         api_key = \"e2e\"\n\
         repos_path = {}\n",
        taken,
        serde_json::to_string(&dir.path().join("store").to_string_lossy()).unwrap()
    );

    let (code, printed) = spawn_expecting_exit(dir.path(), &config);

    assert_eq!(code, 1, "printed: {}", printed);
    assert!(
        printed.contains("failed to bind tcp listener"),
        "printed: {}",
        printed
    );
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn a_taken_replication_port_exits_too_rather_than_serving_content_alone() {
    let dir = tempfile::tempdir().expect("cannot create scratch directory");

    let squatter = TcpListener::bind("127.0.0.1:0").expect("cannot bind squatter");
    let taken = squatter.local_addr().expect("cannot read squatter").port();

    // The content listener binds first and succeeds; the peer listener is
    // what fails. A node serving reads but no longer replicating is a node
    // quietly going stale, so it must not survive this.
    let config = format!(
        "[server]\n\
         host = \"127.0.0.1\"\n\
         port = {}\n\
         api_key = \"e2e\"\n\
         repos_path = {}\n\
         \n[replication]\n\
         role = \"master\"\n\
         secret = \"e2e-replication-secret\"\n\
         node_id = \"e2e-master\"\n\
         port = {}\n",
        free_port(),
        serde_json::to_string(&dir.path().join("store").to_string_lossy()).unwrap(),
        taken
    );

    let (code, printed) = spawn_expecting_exit(dir.path(), &config);

    assert_eq!(code, 1, "printed: {}", printed);
    assert!(
        printed.contains("failed to bind replication tcp listener"),
        "printed: {}",
        printed
    );
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn a_stale_index_lock_is_removed_at_boot() {
    let (_scratch, spec) = scratch("standalone");

    let mut node = Node::start(spec).await;

    node.write_file("docs/acme/files/intro.md", "# Hello")
        .await
        .expect_status(200);

    node.stop();

    // Exactly what a process killed mid-operation leaves behind. At boot
    // nothing can be in flight, so every lock on disk is stale by
    // definition and removed regardless of its age.
    let lock = node.spec.store_path().join("docs/acme/.git/index.lock");

    fs::write(&lock, b"").expect("cannot plant a stale lock");

    assert!(lock.exists());

    node.restart().await;

    assert!(
        !lock.exists(),
        "a stale index lock survived a restart: {}",
        lock.display()
    );

    // And the node is not merely up but writable — the lock is gone in the
    // sense that matters.
    node.write_file("docs/acme/files/intro.md", "# Hello again")
        .await
        .expect_status(200);
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn a_node_with_no_working_tree_serves_content_it_never_wrote_to_disk() {
    let (_scratch, mut spec) = scratch("standalone");

    spec.checkout_files = false;

    let node = Node::start(spec).await;

    node.write_file("docs/acme/files/intro.md", "# Hello")
        .await
        .expect_status(200);

    let on_disk = node.spec.store_path().join("docs/acme/intro.md");

    assert!(
        !on_disk.exists(),
        "checkout_files = false still wrote {}",
        on_disk.display()
    );

    // Nothing this server answers reads the working tree: HEAD is
    // authoritative, so every route works with no files on disk at all.
    let read = node.read_file("docs/acme/files/intro.md").await;

    read.expect_status(200);

    assert_eq!(
        read.json().get("content").and_then(Value::as_str),
        Some("# Hello")
    );

    node.get("/docs/acme/files").await.expect_status(200);

    // Including the write paths that have to tolerate a file that was never
    // mirrored — the property promotion rests on.
    node.post(
        "/docs/acme/files/intro.md/move",
        serde_json::json!({
            "author": crate::node::author(),
            "destination": "renamed.md"
        }),
    )
    .await
    .expect_status(200);

    node.read_file("docs/acme/files/renamed.md")
        .await
        .expect_status(200);
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn autoheal_checks_out_a_store_that_ran_without_a_working_tree() {
    let (_scratch, mut spec) = scratch("standalone");

    spec.checkout_files = false;

    let mut node = Node::start(spec).await;

    node.write_file("docs/acme/files/intro.md", "# Hello")
        .await
        .expect_status(200);

    node.write_file("docs/acme/files/guides/deep.md", "# Deep")
        .await
        .expect_status(200);

    let shallow = node.spec.store_path().join("docs/acme/intro.md");
    let deep = node.spec.store_path().join("docs/acme/guides/deep.md");

    assert!(!shallow.exists());

    // Mirroring as work arrives is never retroactive, so turning the working
    // tree back on is not enough on its own — this is the two-step an
    // operator performs: enable the heal for one restart.
    node.spec.checkout_files = true;
    node.spec.checkout_files_autoheal = true;

    node.restart().await;

    // The pass is spawned rather than awaited before the listener binds, so
    // the node answers before the disk work is done.
    wait_until(
        "the startup heal to write the working tree",
        Duration::from_secs(20),
        || async { shallow.exists() && deep.exists() },
    )
    .await;

    assert_eq!(
        fs::read_to_string(&shallow).expect("cannot read healed file"),
        "# Hello"
    );
    assert_eq!(
        fs::read_to_string(&deep).expect("cannot read healed file"),
        "# Deep"
    );

    // The log says how much it actually wrote, which is what makes an
    // already-correct store distinguishable from a repaired one.
    assert!(
        node.logs().contains("healed_files"),
        "the heal pass reported nothing:\n{}",
        tail(&node.logs())
    );
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn autoheal_removes_files_head_no_longer_names() {
    let (_scratch, mut spec) = scratch("standalone");

    spec.checkout_files_autoheal = true;

    let mut node = Node::start(spec).await;

    node.write_file("docs/acme/files/intro.md", "# Hello")
        .await
        .expect_status(200);

    // A file HEAD never named, planted directly in the working tree. This is
    // the blast radius that makes the key opt-in: the pass removes it.
    let stray = node.spec.store_path().join("docs/acme/stray.md");

    fs::write(&stray, b"# Not in HEAD").expect("cannot plant a stray file");

    node.restart().await;

    wait_until(
        "the startup heal to remove a file HEAD does not name",
        Duration::from_secs(20),
        || async { !stray.exists() },
    )
    .await;

    // What HEAD does name is left exactly as it was.
    assert!(node.spec.store_path().join("docs/acme/intro.md").exists());
}

/// The last few lines of a log, for an assertion message that stays readable.
fn tail(logs: &str) -> String {
    let lines: Vec<&str> = logs.lines().collect();
    let start = lines.len().saturating_sub(25);

    lines[start..].join("\n")
}
