// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Promotion: turning a replica into the master with a config swap.
//!
//! # What promotion is
//!
//! In a replicated deployment exactly one node — the master — accepts writes;
//! every replica is read-only and answers `423` to a write. So when the master
//! is lost (a crashed process, a dead host, a planned retirement) content can
//! still be *read* from every replica, but there is nowhere to write it until
//! one of them takes over the write role. That hand-over is promotion.
//!
//! It is deliberately manual, and there is no automatic failover: two nodes
//! accepting writes at once would fork two histories, and nothing in this
//! service merges a fork. An operator picks the replica (the one whose health
//! reports `sync: "synced"` with no issues), edits its config, and restarts it
//! — the full runbook is in `REPLICATION.md`. These tests rehearse it.
//!
//! # Why it is tested here and nowhere else
//!
//! "Promotion is a config swap" is the headline operational claim of the
//! replication design — set `role = "master"`, remove `master_url`, add
//! `[hooks]`, restart — and it is *structurally* untestable in process,
//! because the restart is half of it.
//!
//! Three properties of the rest of the codebase are what make the claim true,
//! and each is asserted below: a repository that was only ever pulled is
//! writable as-is (every write is built from HEAD's tree and the object
//! database, never from the working tree), a replica keeps full history
//! rather than a shallow copy (so the commit routes work on the promoted
//! node), and `.replication.json` already holds the data-set identity (so
//! peers re-point with a `master_url` change and never re-pair).

use serde_json::Value;

use crate::{
    deployment::Deployment,
    node::{wait_until, CONVERGE_TIMEOUT},
};

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn a_replica_promoted_by_config_swap_takes_writes_and_keeps_history() {
    let mut deployment = Deployment::start().await;

    // Two commits on the master, so the promoted node has history to keep
    // rather than a single root commit.
    deployment
        .master
        .write_file("docs/acme/files/intro.md", "# Hello")
        .await
        .expect_status(200);

    deployment
        .master
        .write_file("docs/acme/files/guide.md", "# Guide")
        .await
        .expect_status(200);

    deployment
        .wait_for_replica_content("docs/acme/files/guide.md", "# Guide")
        .await;

    deployment.receiver.wait_for(2).await;

    // The master is gone. Everything below is what an operator does next.
    deployment.master.stop();

    // The config swap, verbatim from the runbook: role, master_url, hooks.
    deployment.replica.spec.role = Some("master".to_string());
    deployment.replica.spec.master_url = None;
    deployment.replica.spec.hooks_url = Some(deployment.receiver.url.clone());

    deployment.replica.restart().await;

    let promoted = &deployment.replica;

    // It is a master now, and says so on the route a failover-aware client
    // reads without a credential.
    let status = promoted.health_status().await;

    assert_eq!(status.get("role").and_then(Value::as_str), Some("master"));
    assert_eq!(status.get("writable").and_then(Value::as_bool), Some(true));
    assert_eq!(
        status.get("status").and_then(Value::as_str),
        Some("healthy"),
        "a promoted node is not bootstrapping: {}",
        status
    );

    // Everything it pulled is still served.
    let read = promoted.read_file("docs/acme/files/intro.md").await;

    read.expect_status(200);

    assert_eq!(
        read.json().get("content").and_then(Value::as_str),
        Some("# Hello")
    );

    // A replica keeps *full* history, not a shallow copy, so the commit
    // routes work on the promoted node: initialize + two writes.
    let commits = promoted.get("/docs/acme/commits").await;

    commits.expect_status(200);

    let listed = commits
        .json()
        .get("commits")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default();

    assert_eq!(listed, 3, "promoted node lost history: {}", commits.text);

    // And the thing it refused five seconds ago now works — a repository
    // that was only ever pulled is writable as-is.
    promoted
        .write_file("docs/acme/files/after.md", "# After promotion")
        .await
        .expect_status(200);

    let written = promoted.read_file("docs/acme/files/after.md").await;

    written.expect_status(200);

    assert_eq!(
        written.json().get("content").and_then(Value::as_str),
        Some("# After promotion")
    );

    // `[hooks]` was added in the same swap, so the promoted node delivers
    // where the old master used to: three events now, not two.
    let payloads = deployment.receiver.wait_for(3).await;

    assert_eq!(
        payloads[2].get("event").and_then(Value::as_str),
        Some("file.created")
    );
    assert_eq!(
        payloads[2].pointer("/file/path").and_then(Value::as_str),
        Some("after.md")
    );
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn the_old_master_comes_back_as_a_replica_of_the_promoted_node() {
    let mut deployment = Deployment::start_without_hooks().await;

    deployment
        .master
        .write_file("docs/acme/files/intro.md", "# Hello")
        .await
        .expect_status(200);

    deployment
        .wait_for_replica_content("docs/acme/files/intro.md", "# Hello")
        .await;

    deployment.master.stop();

    deployment.replica.spec.role = Some("master".to_string());
    deployment.replica.spec.master_url = None;

    deployment.replica.restart().await;

    // A write the old master never saw, so following it is observable.
    deployment
        .replica
        .write_file("docs/acme/files/after.md", "# After promotion")
        .await
        .expect_status(200);

    // The old master returns as a replica. Its store already holds the
    // data-set identity — the same one, since both nodes have always been
    // the same set — so re-pointing is a `master_url` change and nothing
    // more: no re-pairing, no re-clone of what it already has.
    deployment.master.spec.role = Some("replica".to_string());
    deployment.master.spec.master_url = Some(deployment.replica.spec.replication_url());
    deployment.master.spec.hooks_url = None;

    deployment.master.restart().await;

    let returned = &deployment.master;

    wait_until(
        "the returned node to follow the promoted one",
        CONVERGE_TIMEOUT,
        || async {
            let reply = returned.read_file("docs/acme/files/after.md").await;

            reply.status.as_u16() == 200
                && reply.json().get("content").and_then(Value::as_str) == Some("# After promotion")
        },
    )
    .await;

    // It is following, not diverged: its history was strictly behind the
    // promoted node's, so the pack fast-forwards and nothing is locked.
    let health = returned.health_replication().await;

    assert_eq!(
        health.get("issues").and_then(Value::as_array).map(Vec::len),
        Some(0),
        "a returning node that was behind raises no issue: {}",
        health
    );
    assert_eq!(
        health
            .pointer("/replica/locked_repositories")
            .and_then(Value::as_u64),
        Some(0),
        "nothing should be locked: {}",
        health
    );

    // And it now refuses the writes it used to accept.
    returned
        .write_file("docs/acme/files/nope.md", "# No")
        .await
        .expect_status(423);
}
