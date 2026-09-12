// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Integration tests for webhook delivery, against a stub receiver.
//!
//! Three documented promises are what this module exists to hold:
//!
//! - **One event per file, never batched** — a recursive operation fans out
//!   to one event per file rather than one summary event.
//! - **Order events come after every file event of the same commit**, so an
//!   order snapshot never names a file the receiver has not been told about.
//! - **Delivery is strictly sequential per repository**, because jobs are
//!   enqueued while the tenant write lock is still held.

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;

use crate::tests::harness::{author, HookReceiver, TestServer};

const TENANT: &str = "/docs/acme";

#[tokio::test]
async fn a_creation_delivers_one_payload_with_the_documented_shape() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    let sha = server.write_file(TENANT, "docs/intro.md", "# Hello").await;

    let events = receiver.wait_for_exactly(1).await;
    let payload = &events[0];

    assert_eq!(payload["event"], "file.created");
    // collection_id and tenant_id together are the repository's identity,
    // and together are what a receiver must key its rows on.
    assert_eq!(payload["collection_id"], "docs");
    assert_eq!(payload["tenant_id"], "acme");
    assert_eq!(payload["commit_sha"].as_str().unwrap(), sha);
    assert_eq!(payload["file"]["path"], "docs/intro.md");
    assert_eq!(payload["file"]["content"], "# Hello");

    // A live payload carries no `replayed` field at all — its absence is
    // what means "live".
    assert!(payload.get("replayed").is_none());

    chrono::DateTime::parse_from_rfc3339(payload["committed_at"].as_str().unwrap())
        .expect("committed_at is not RFC 3339");
}

#[tokio::test]
async fn an_update_a_delete_and_a_move_each_fire_their_own_kind() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "a.md", "one").await;
    server.write_file(TENANT, "a.md", "two").await;

    server
        .post(
            &format!("{}/files/a.md/move", TENANT),
            json!({ "author": author(), "destination": "b.md" }),
        )
        .await
        .expect_status(StatusCode::OK);

    server
        .delete(
            &format!("{}/files/b.md", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    let events = receiver.wait_for_exactly(4).await;

    assert_eq!(
        receiver.event_names(),
        vec![
            "file.created".to_string(),
            "file.updated".to_string(),
            "file.moved".to_string(),
            "file.deleted".to_string(),
        ]
    );

    // A rename is one event carrying both paths, so downstream entity
    // identity survives it.
    let moved = &events[2];

    assert_eq!(moved["event"], "file.moved");
    assert_eq!(moved["from"]["path"], "a.md");
    assert_eq!(moved["to"]["path"], "b.md");
    assert_eq!(moved["to"]["content"], "two");
}

#[tokio::test]
async fn an_unchanged_write_fires_nothing() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "a.md", "same").await;

    receiver.wait_for(1).await;

    server.write_file(TENANT, "a.md", "same").await;

    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    assert_eq!(
        receiver.events().len(),
        1,
        "an unchanged write delivered a hook"
    );
}

#[tokio::test]
async fn the_subscription_list_filters_event_kinds() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder()
        .hooks(&receiver.url)
        .hook_events(&["file.deleted"])
        .start()
        .await;

    server.write_file(TENANT, "a.md", "x").await;

    server
        .delete(
            &format!("{}/files/a.md", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    let events = receiver.wait_for_exactly(1).await;

    assert_eq!(events[0]["event"], "file.deleted");
}

#[tokio::test]
async fn a_recursive_delete_fans_out_to_one_event_per_file() {
    // No batching or coalescing anywhere: N files means N events, so a
    // receiver applies them file by file with no special-casing.
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    for path in ["docs/a.md", "docs/b.md", "docs/deep/c.md"] {
        server.write_file(TENANT, path, "x").await;
    }

    receiver.wait_for(3).await;

    server
        .delete(
            &format!("{}/files/docs", TENANT),
            json!({ "author": author(), "allow_prefix_path_recurse": true }),
        )
        .await
        .expect_status(StatusCode::OK);

    let events = receiver.wait_for_exactly(6).await;

    let deletions: Vec<_> = events[3..]
        .iter()
        .map(|payload| payload["file"]["path"].as_str().unwrap())
        .collect();

    assert_eq!(deletions.len(), 3);

    for payload in &events[3..] {
        assert_eq!(payload["event"], "file.deleted");
    }

    // One commit, so every event carries the same sha.
    assert_eq!(events[3]["commit_sha"], events[5]["commit_sha"]);
}

#[tokio::test]
async fn a_recursive_move_emits_one_moved_event_per_file_not_a_delete_create_wave() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "docs/a.md", "A").await;
    server.write_file(TENANT, "docs/deep/b.md", "B").await;

    receiver.wait_for(2).await;

    server
        .post(
            &format!("{}/files/docs/move", TENANT),
            json!({
                "author": author(),
                "destination": "handbook",
                "allow_prefix_path_recurse": true
            }),
        )
        .await
        .expect_status(StatusCode::OK);

    let events = receiver.wait_for_exactly(4).await;

    for payload in &events[2..] {
        assert_eq!(payload["event"], "file.moved");

        let from = payload["from"]["path"].as_str().unwrap();
        let to = payload["to"]["path"].as_str().unwrap();

        // Every file keeps its own leaf name; only the ancestor prefix
        // changes.
        assert_eq!(
            from.rsplit('/').next(),
            to.rsplit('/').next(),
            "leaf name changed: {} -> {}",
            from,
            to
        );

        assert!(to.starts_with("handbook/"));
    }
}

#[tokio::test]
async fn a_revert_fans_out_over_its_whole_change_set() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "a.md", "one").await;

    let target = server.write_file(TENANT, "a.md", "two").await;

    receiver.wait_for(2).await;

    server
        .post(
            &format!("{}/commits/{}/revert", TENANT, target),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    let events = receiver.wait_for_exactly(3).await;

    assert_eq!(events[2]["event"], "file.updated");
    assert_eq!(events[2]["file"]["content"], "one");
}

// --- Order events --------------------------------------------------------

#[tokio::test]
async fn an_order_write_delivers_a_complete_snapshot() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "docs/a.md", "x").await;
    server.write_file(TENANT, "docs/b.md", "x").await;

    receiver.wait_for(2).await;

    server
        .put(
            &format!("{}/order/docs", TENANT),
            json!({ "author": author(), "order": ["b.md", "a.md"] }),
        )
        .await
        .expect_status(StatusCode::OK);

    let events = receiver.wait_for_exactly(3).await;
    let payload = &events[2];

    assert_eq!(payload["event"], "order.updated");
    assert_eq!(payload["directory"], "docs");
    // A complete resulting order, never a diff — applying it downstream is
    // a replace, so repeated delivery is harmless.
    assert_eq!(payload["order"], json!(["b.md", "a.md"]));

    // An order change is never delivered as a file event on the index's
    // own path — the kind is derived from the path, in one place.
    assert!(
        events
            .iter()
            .all(|event| event["file"]["path"] != "docs/.order.json"),
        "an index leaked as a file event"
    );
}

#[tokio::test]
async fn dropping_an_order_delivers_order_deleted_with_the_directory_only() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "docs/a.md", "x").await;

    server
        .put(
            &format!("{}/order/docs", TENANT),
            json!({ "author": author(), "order": ["a.md"] }),
        )
        .await
        .expect_status(StatusCode::OK);

    receiver.wait_for(2).await;

    server
        .delete(
            &format!("{}/order/docs", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    let events = receiver.wait_for_exactly(3).await;

    assert_eq!(events[2]["event"], "order.deleted");
    assert_eq!(events[2]["directory"], "docs");
    assert!(events[2].get("order").is_none());
}

#[tokio::test]
async fn the_repository_root_is_spelled_as_the_empty_string() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "a.md", "x").await;

    receiver.wait_for(1).await;

    server
        .put(
            &format!("{}/order", TENANT),
            json!({ "author": author(), "order": ["a.md"] }),
        )
        .await
        .expect_status(StatusCode::OK);

    let events = receiver.wait_for_exactly(2).await;

    assert_eq!(events[1]["directory"], "");
}

#[tokio::test]
async fn order_events_are_delivered_after_every_file_event_of_the_same_commit() {
    // Sending the file changes first means an order snapshot never
    // references a file the receiver has not been told about yet.
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "docs/a.md", "x").await;
    server.write_file(TENANT, "docs/b.md", "x").await;

    server
        .put(
            &format!("{}/order/docs", TENANT),
            json!({ "author": author(), "order": ["a.md", "b.md"] }),
        )
        .await
        .expect_status(StatusCode::OK);

    receiver.wait_for(3).await;

    // Deleting a pinned file produces one file.deleted and then one
    // order.updated holding the index without it — in that order.
    server
        .delete(
            &format!("{}/files/docs/a.md", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    let events = receiver.wait_for_exactly(5).await;

    assert_eq!(events[3]["event"], "file.deleted");
    assert_eq!(events[4]["event"], "order.updated");
    assert_eq!(events[4]["order"], json!(["b.md"]));
}

#[tokio::test]
async fn a_receiver_not_subscribed_to_order_events_gets_none() {
    // This is what keeps the whole feature invisible to existing
    // deployments.
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder()
        .hooks(&receiver.url)
        .hook_events(&["file.created", "file.deleted"])
        .start()
        .await;

    server.write_file(TENANT, "docs/a.md", "x").await;

    server
        .put(
            &format!("{}/order/docs", TENANT),
            json!({ "author": author(), "order": ["a.md"] }),
        )
        .await
        .expect_status(StatusCode::OK);

    let events = receiver.wait_for_exactly(1).await;

    assert_eq!(events[0]["event"], "file.created");
}

// --- Ordering ------------------------------------------------------------

#[tokio::test]
async fn delivery_order_equals_commit_order() {
    // Jobs are enqueued while the tenant write lock is still held, so a
    // later commit can never overtake an earlier one at the receiver.
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    for index in 0..10 {
        server
            .write_file(TENANT, "a.md", &format!("version {}", index))
            .await;
    }

    let events = receiver.wait_for_exactly(10).await;

    let contents: Vec<_> = events
        .iter()
        .map(|payload| payload["file"]["content"].as_str().unwrap().to_string())
        .collect();

    let expected: Vec<_> = (0..10).map(|index| format!("version {}", index)).collect();

    assert_eq!(contents, expected);
}

// --- Replay --------------------------------------------------------------

#[tokio::test]
async fn the_delete_direction_replays_what_the_mirror_holds_and_git_does_not() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "kept.md", "x").await;

    receiver.wait_for(1).await;

    let response = server
        .post(
            &format!("{}/batch/replay/hook", TENANT),
            json!({ "direction": "delete", "files": ["kept.md", "orphan.md"] }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    // Everything *outside* the intersection — the mirror's orphans.
    assert_eq!(response.json()["files"], 1);
    assert_eq!(response.json()["orders"], 0);

    let events = receiver.wait_for_exactly(2).await;

    assert_eq!(events[1]["event"], "file.deleted");
    assert_eq!(events[1]["file"]["path"], "orphan.md");
    // Replayed payloads are marked, not renamed.
    assert_eq!(events[1]["replayed"], true);
}

#[tokio::test]
async fn the_create_direction_replays_what_both_sides_hold() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "a.md", "# A").await;
    server.write_file(TENANT, "b.md", "# B").await;

    receiver.wait_for(2).await;

    let response = server
        .post(
            &format!("{}/batch/replay/hook", TENANT),
            json!({ "direction": "create", "files": ["a.md", "never-existed.md"] }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    assert_eq!(response.json()["files"], 1);

    let events = receiver.wait_for_exactly(3).await;

    // `created` rather than `updated`: the case a replay is usually run for
    // is a row the receiver never got.
    assert_eq!(events[2]["event"], "file.created");
    assert_eq!(events[2]["file"]["path"], "a.md");
    assert_eq!(events[2]["file"]["content"], "# A");
    assert_eq!(events[2]["replayed"], true);
}

#[tokio::test]
async fn omitting_files_covers_the_whole_scope_on_create_and_nothing_on_delete() {
    // The asymmetry falls out of the set operation: git cannot be missing
    // what it just listed.
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "a.md", "x").await;
    server.write_file(TENANT, "docs/b.md", "x").await;

    receiver.wait_for(2).await;

    let deleting = server
        .post(
            &format!("{}/batch/replay/hook", TENANT),
            json!({ "direction": "delete" }),
        )
        .await;

    deleting.expect_status(StatusCode::OK);

    assert_eq!(deleting.json()["files"], 0);

    let creating = server
        .post(
            &format!("{}/batch/replay/hook", TENANT),
            json!({ "direction": "create" }),
        )
        .await;

    creating.expect_status(StatusCode::OK);

    assert_eq!(creating.json()["files"], 2);

    receiver.wait_for_exactly(4).await;
}

#[tokio::test]
async fn a_replay_ends_with_one_order_snapshot_per_directory_holding_an_index() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "docs/a.md", "x").await;
    server.write_file(TENANT, "docs/b.md", "x").await;

    server
        .put(
            &format!("{}/order/docs", TENANT),
            json!({ "author": author(), "order": ["b.md", "a.md"] }),
        )
        .await
        .expect_status(StatusCode::OK);

    receiver.wait_for(3).await;

    let response = server
        .post(
            &format!("{}/batch/replay/hook", TENANT),
            json!({ "direction": "create" }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    assert_eq!(response.json()["files"], 2);
    assert_eq!(response.json()["orders"], 1);

    let events = receiver.wait_for_exactly(6).await;

    // File events first, then the order snapshots.
    assert_eq!(events[3]["event"], "file.created");
    assert_eq!(events[4]["event"], "file.created");
    assert_eq!(events[5]["event"], "order.updated");
    assert_eq!(events[5]["order"], json!(["b.md", "a.md"]));
    assert_eq!(events[5]["replayed"], true);
}

#[tokio::test]
async fn a_replay_commits_nothing() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "a.md", "x").await;

    let head = server.head_sha(TENANT).await;

    let response = server
        .post(
            &format!("{}/batch/replay/hook", TENANT),
            json!({ "direction": "create" }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    // `commit_sha` is the HEAD the snapshot was taken from — the honest
    // answer to "which state was this computed against".
    assert_eq!(response.json()["commit_sha"].as_str().unwrap(), head);
    assert_eq!(server.head_sha(TENANT).await, head);
}

#[tokio::test]
async fn a_replay_validates_its_inputs() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "docs/a.md", "x").await;

    for body in [
        // An explicitly empty list — omit the field instead.
        json!({ "direction": "create", "files": [] }),
        // Duplicates after sanitisation.
        json!({ "direction": "create", "files": ["docs/a.md", "/docs/a.md"] }),
        // Traversal.
        json!({ "direction": "create", "files": ["../escape.md"] }),
        // An out-of-scope entry: paths stay repo-root-relative, so the
        // prefix is a guard rail rather than a join.
        json!({ "direction": "create", "files": ["other/b.md"], "prefix_path": "/docs" }),
        // Over the throttle cap.
        json!({ "direction": "create", "delay_ms": 60001 }),
    ] {
        server
            .post(&format!("{}/batch/replay/hook", TENANT), body.clone())
            .await
            .expect_status(StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn a_missing_or_unknown_direction_is_rejected_by_the_body_schema() {
    // `direction` is required and admits exactly two values. Both failures
    // are caught while deserialising the body, so they answer 422 like every
    // other malformed body on this API (a missing `author`, say) rather than
    // the 400 the handler's own semantic checks produce.
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "docs/a.md", "x").await;

    for body in [
        json!({ "files": ["docs/a.md"] }),
        json!({ "direction": "update", "files": ["docs/a.md"] }),
        json!({ "direction": "", "files": ["docs/a.md"] }),
    ] {
        server
            .post(&format!("{}/batch/replay/hook", TENANT), body.clone())
            .await
            .expect_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
}

#[tokio::test]
async fn an_order_index_path_is_refused_in_a_replay_list() {
    // Events are classified by path, so a file.deleted on an index path
    // would arrive as an order.deleted and wipe a directory's stored order.
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "docs/a.md", "x").await;

    server
        .post(
            &format!("{}/batch/replay/hook", TENANT),
            json!({ "direction": "delete", "files": ["docs/.order.json"] }),
        )
        .await
        .expect_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_replay_with_no_receiver_configured_is_refused() {
    // The job would deliver nothing, and answering 200 with a file count
    // for a reconciliation that did nothing is worse than an error.
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "x").await;

    server
        .post(
            &format!("{}/batch/replay/hook", TENANT),
            json!({ "direction": "create" }),
        )
        .await
        .expect_status(StatusCode::BAD_REQUEST);
}

// --- Delivery under failure ----------------------------------------------

#[tokio::test]
async fn a_failed_delivery_is_retried_until_it_lands() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder()
        .hooks(&receiver.url)
        .hook_retries(5, 1)
        .start()
        .await;

    // The first two attempts are refused; the third succeeds.
    receiver.fail_next(2);

    server.write_file(TENANT, "a.md", "x").await;

    let deliveries = receiver.wait_for(3).await;

    // Every attempt carries the same payload — a retry is the same event
    // again, not a new one.
    assert_eq!(deliveries.len(), 3);
    assert_eq!(deliveries[0], deliveries[2]);
    assert_eq!(deliveries[2]["event"], "file.created");
}

#[tokio::test]
async fn backoff_grows_between_attempts() {
    // Attempt N waits `retry_backoff_ms * 2^(N-1)` before retrying.
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder()
        .hooks(&receiver.url)
        .hook_retries(4, 120)
        .start()
        .await;

    receiver.fail_next(2);

    server.write_file(TENANT, "a.md", "x").await;

    receiver.wait_for(3).await;

    let deliveries = receiver.deliveries();

    let first_gap = deliveries[1].at.duration_since(deliveries[0].at);
    let second_gap = deliveries[2].at.duration_since(deliveries[1].at);

    assert!(
        first_gap >= Duration::from_millis(100),
        "first retry did not wait: {:?}",
        first_gap
    );
    assert!(
        second_gap >= first_gap * 3 / 2,
        "backoff did not grow: {:?} then {:?}",
        first_gap,
        second_gap
    );
}

#[tokio::test]
async fn a_permanently_failing_delivery_gives_up_without_wedging_the_queue() {
    // The retry budget is finite: the event is lost (logged CRITICAL), and
    // the next commit's hooks still flow. A queue that blocked forever on
    // one dead payload would take the repository's whole future with it.
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder()
        .hooks(&receiver.url)
        .hook_retries(2, 1)
        .start()
        .await;

    receiver.fail_next(2);

    server.write_file(TENANT, "a.md", "one").await;

    receiver.wait_for(2).await;

    server.write_file(TENANT, "b.md", "two").await;

    let deliveries = receiver.wait_for(3).await;

    // Two refused attempts for the first file, then the second file's own
    // event — delivered, not stuck behind the abandoned one.
    assert_eq!(deliveries[2]["file"]["path"], "b.md");
}

#[tokio::test]
async fn a_wedged_receiver_holds_up_only_its_own_repository() {
    // The concurrency half of the ordering promise: different queue keys are
    // different tokio tasks, so a slow or down receiver for one repository
    // never delays another.
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    receiver.stall_tenant("stuck");

    server.write_file("/docs/stuck", "a.md", "x").await;

    // Its delivery is now held open, occupying that repository's queue.
    receiver.wait_for(1).await;

    server.write_file("/docs/stuck", "b.md", "x").await;
    server.write_file("/docs/moving", "c.md", "x").await;

    let deadline = std::time::Instant::now() + Duration::from_secs(10);

    loop {
        let delivered: Vec<_> = receiver
            .events()
            .into_iter()
            .filter(|payload| payload["tenant_id"] == "moving")
            .collect();

        if !delivered.is_empty() {
            break;
        }

        assert!(
            deadline > std::time::Instant::now(),
            "a healthy repository was held up by a wedged one: {:?}",
            receiver.event_names()
        );

        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Meanwhile the stalled repository's *second* event is still waiting
    // behind its first — ordering within a repository is never sacrificed to
    // make progress.
    assert_eq!(
        receiver
            .events()
            .iter()
            .filter(|payload| payload["tenant_id"] == "stuck")
            .count(),
        1
    );

    receiver.resume();

    // And it drains in order once the receiver recovers.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);

    loop {
        let stuck: Vec<_> = receiver
            .events()
            .into_iter()
            .filter(|payload| payload["tenant_id"] == "stuck")
            .collect();

        if stuck.len() == 2 {
            assert_eq!(stuck[0]["file"]["path"], "a.md");
            assert_eq!(stuck[1]["file"]["path"], "b.md");

            break;
        }

        assert!(
            deadline > std::time::Instant::now(),
            "the stalled repository never drained"
        );

        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn the_configured_auth_header_travels_on_every_delivery() {
    // How a receiver authenticates this server.
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder()
        .hooks(&receiver.url)
        .hook_auth("Authorization", "Bearer hook-secret")
        .start()
        .await;

    server.write_file(TENANT, "a.md", "x").await;
    server.write_file(TENANT, "b.md", "x").await;

    receiver.wait_for(2).await;

    for delivery in receiver.deliveries() {
        assert_eq!(delivery.auth_header.as_deref(), Some("Bearer hook-secret"));
    }
}

#[tokio::test]
async fn no_auth_header_is_sent_when_none_is_configured() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "a.md", "x").await;

    receiver.wait_for(1).await;

    assert_eq!(receiver.deliveries()[0].auth_header, None);
}

#[tokio::test]
async fn delay_ms_throttles_between_replayed_deliveries_and_not_after_the_last() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    for path in ["a.md", "b.md", "c.md"] {
        server.write_file(TENANT, path, "x").await;
    }

    receiver.wait_for(3).await;

    let started = std::time::Instant::now();

    let response = server
        .post(
            &format!("{}/batch/replay/hook", TENANT),
            json!({ "direction": "create", "delay_ms": 200 }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    // The response returns as soon as the job is enqueued: a 200 means
    // "scheduled", not "delivered".
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "the request waited for delivery: {:?}",
        started.elapsed()
    );

    receiver.wait_for(6).await;

    let replayed: Vec<_> = receiver
        .deliveries()
        .into_iter()
        .filter(|delivery| delivery.payload["replayed"] == true)
        .collect();

    assert_eq!(replayed.len(), 3);

    for pair in replayed.windows(2) {
        let gap = pair[1].at.duration_since(pair[0].at);

        assert!(
            gap >= Duration::from_millis(150),
            "deliveries were not throttled: {:?}",
            gap
        );
    }
}

#[tokio::test]
async fn a_replay_scopes_its_snapshot_to_the_prefix_path() {
    let receiver = HookReceiver::start().await;
    let server = TestServer::builder().hooks(&receiver.url).start().await;

    server.write_file(TENANT, "docs/a.md", "x").await;
    server.write_file(TENANT, "other/b.md", "x").await;

    receiver.wait_for(2).await;

    let response = server
        .post(
            &format!("{}/batch/replay/hook", TENANT),
            json!({ "direction": "create", "prefix_path": "/docs" }),
        )
        .await;

    response.expect_status(StatusCode::OK);

    assert_eq!(response.json()["files"], 1);

    let events = receiver.wait_for_exactly(3).await;

    // Paths stay repo-root-relative: the prefix scopes which files are in
    // the snapshot, it is not joined onto them.
    assert_eq!(events[2]["file"]["path"], "docs/a.md");
}
