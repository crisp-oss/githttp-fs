// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! A full replicated deployment, exercised the way an operator would.
//!
//! Every test here brings up two real processes on scratch disks and talks to
//! them over the network. The in-crate suite asserts the same protocol
//! in-process; what these add is that it survives being two programs — that
//! the peer listener really is on its own port, that a replica really does
//! follow a `master_url` read out of a config file, and that killing one
//! process leaves the other answering.

use serde_json::{json, Value};

use crate::{
    deployment::Deployment,
    node::{author, wait_until, CONVERGE_TIMEOUT},
};

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn a_write_to_the_master_is_served_by_the_replica() {
    let deployment = Deployment::start_without_hooks().await;

    deployment
        .master
        .write_file("docs/acme/files/intro.md", "# Hello")
        .await
        .expect_status(200);

    deployment
        .wait_for_replica_content("docs/acme/files/intro.md", "# Hello")
        .await;

    // And an update travels too, rather than only the initial clone.
    deployment
        .master
        .write_file("docs/acme/files/intro.md", "# Hello again")
        .await
        .expect_status(200);

    deployment
        .wait_for_replica_content("docs/acme/files/intro.md", "# Hello again")
        .await;

    // The listing route agrees with the read route on the replica — the
    // whole tree landed, not just the one blob a read happened to resolve.
    let listing = deployment.replica.get("/docs/acme/files").await;

    listing.expect_status(200);

    assert_eq!(
        listing
            .json()
            .pointer("/files/0/name")
            .and_then(Value::as_str),
        Some("intro.md")
    );
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn a_delete_on_the_master_reaches_the_replica() {
    let deployment = Deployment::start_without_hooks().await;

    deployment
        .master
        .write_file("docs/acme/files/gone.md", "# Temporary")
        .await
        .expect_status(200);

    deployment
        .wait_for_replica_content("docs/acme/files/gone.md", "# Temporary")
        .await;

    deployment
        .master
        .delete("/docs/acme/files/gone.md", json!({ "author": author() }))
        .await
        .expect_status(200);

    // A replica applies history, so a deletion is a new commit to follow
    // rather than a file to remove — but the read route must stop serving it.
    deployment
        .wait_for_replica_missing("docs/acme/files/gone.md")
        .await;
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn the_replica_refuses_writes_and_says_which_node_takes_them() {
    let deployment = Deployment::start_without_hooks().await;

    // "Writes are classified before the bootstrap gate", so the answer is
    // the same whatever the replica's catch-up state is.
    let refused = deployment
        .replica
        .write_file("docs/acme/files/nope.md", "# No")
        .await;

    refused.expect_status(423);

    // A failover-aware client should not have to learn this from a 423: the
    // public status route says it outright, on both nodes.
    let master_status = deployment.master.health_status().await;
    let replica_status = deployment.replica.health_status().await;

    assert_eq!(
        master_status.get("role").and_then(Value::as_str),
        Some("master")
    );
    assert_eq!(
        master_status.get("writable").and_then(Value::as_bool),
        Some(true)
    );

    assert_eq!(
        replica_status.get("role").and_then(Value::as_str),
        Some("replica")
    );
    assert_eq!(
        replica_status.get("writable").and_then(Value::as_bool),
        Some(false)
    );

    // The authenticated ping is the one route the read-only guard always
    // lets through, so a node refusing everything can still explain itself.
    let ping = deployment.replica.get("").await;

    ping.expect_status(200);

    assert_eq!(ping.json().get("pong").and_then(Value::as_bool), Some(true));
    assert!(
        ping.json().get("replica").is_some(),
        "a replica's ping carries its follower state: {}",
        ping.text
    );
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn the_master_roster_lists_the_replica_and_both_agree_on_the_data_set() {
    let deployment = Deployment::start_without_hooks().await;

    deployment.wait_for_roster().await;
    deployment.wait_for_replica_synced().await;

    let master_health = deployment.master.health_replication().await;
    let replica_health = deployment.replica.health_replication().await;

    assert_eq!(
        master_health.get("status").and_then(Value::as_str),
        Some("healthy"),
        "master health: {}",
        master_health
    );

    // One probe against the master covers the set: the replica's own verdict
    // rides along on the roster row.
    let row = master_health
        .get("replicas")
        .and_then(Value::as_array)
        .and_then(|replicas| replicas.first().cloned())
        .expect("master roster holds the replica");

    assert_eq!(
        row.get("node_id").and_then(Value::as_str),
        Some("e2e-replica")
    );
    assert_eq!(
        row.get("stream_connected").and_then(Value::as_bool),
        Some(true),
        "roster row: {}",
        row
    );

    // `.replication.json` holds the data-set identity on every node of a set
    // — the property that lets a replica be promoted without re-pairing.
    let master_identity = master_health.pointer("/node/identity").cloned();
    let replica_identity = replica_health.pointer("/node/identity").cloned();

    assert!(
        master_identity.as_ref().is_some_and(|id| !id.is_null()),
        "master reports no identity: {}",
        master_health
    );
    assert_eq!(master_identity, replica_identity);

    // The replica follows the master's *replication* port, never its content
    // port — the separation that keeps publishing one from publishing both.
    assert_eq!(
        replica_health
            .pointer("/master/url")
            .and_then(Value::as_str),
        Some(deployment.master.spec.replication_url().as_str())
    );
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn the_replica_keeps_serving_reads_after_the_master_dies() {
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

    // Stale beats absent: a replica that holds content serves it whatever
    // its master is doing.
    let read = deployment
        .replica
        .read_file("docs/acme/files/intro.md")
        .await;

    read.expect_status(200);

    assert_eq!(
        read.json().get("content").and_then(Value::as_str),
        Some("# Hello")
    );

    // And it says the master is gone rather than pretending otherwise.
    wait_until(
        "replica to report its master unreachable",
        CONVERGE_TIMEOUT,
        || async {
            deployment
                .replica
                .health_replication()
                .await
                .pointer("/master/reachable")
                .and_then(Value::as_bool)
                == Some(false)
        },
    )
    .await;

    // A master that is down is a condition that heals itself, so the node is
    // degraded rather than halted — nothing here is waiting on a human.
    let health = deployment.replica.health_replication().await;

    assert_ne!(
        health.get("status").and_then(Value::as_str),
        Some("halted"),
        "a down master must not halt a replica: {}",
        health
    );
    assert_eq!(
        health.get("issues").and_then(Value::as_array).map(Vec::len),
        Some(0),
        "a down master raises no issue: {}",
        health
    );
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn hooks_are_delivered_by_the_master_and_by_nothing_else() {
    let deployment = Deployment::start().await;

    deployment
        .master
        .write_file("docs/acme/files/intro.md", "# Hello")
        .await
        .expect_status(200);

    let payloads = deployment.receiver.wait_for(1).await;

    assert_eq!(
        payloads[0].get("event").and_then(Value::as_str),
        Some("file.created")
    );
    assert_eq!(
        payloads[0].get("collection_id").and_then(Value::as_str),
        Some("docs")
    );
    assert_eq!(
        payloads[0].get("tenant_id").and_then(Value::as_str),
        Some("acme")
    );
    assert_eq!(
        payloads[0].pointer("/file/content").and_then(Value::as_str),
        Some("# Hello")
    );

    // A live payload carries no `replayed` flag — its absence is what means
    // "live", so nothing an existing receiver parses changes.
    assert!(payloads[0].get("replayed").is_none());

    // Hook delivery belongs to the node that accepted the commit. Once the
    // replica has applied the same commit, it must still not have delivered
    // anything: exactly one event for one write, across the deployment.
    deployment
        .wait_for_replica_content("docs/acme/files/intro.md", "# Hello")
        .await;

    deployment.receiver.settle().await;

    assert_eq!(
        deployment.receiver.count(),
        1,
        "a replica must not deliver hooks: {:?}",
        deployment.receiver.events()
    );
}

#[tokio::test]
#[ignore = "e2e: spawns real processes; run with --ignored"]
async fn the_health_routes_answer_without_a_credential_on_every_node() {
    let deployment = Deployment::start_without_hooks().await;

    for node in [&deployment.master, &deployment.replica] {
        node.get_unauthenticated("/_health/status")
            .await
            .expect_status(200);

        node.get_unauthenticated("/_health/replication")
            .await
            .expect_status(200);

        // The exemption stops there: everything else still wants the key.
        node.get_unauthenticated("/docs/acme/files")
            .await
            .expect_status(401);
    }

    // `_health` reserves two exact paths, not a collection — a tenant route
    // that happens to sit under it still routes to the tenant handlers, and
    // therefore still wants a credential.
    deployment
        .replica
        .get_unauthenticated("/_health/acme/files")
        .await
        .expect_status(401);
}
