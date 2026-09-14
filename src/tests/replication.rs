// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Integration tests for replication: the peer surface, and a real
//! master/replica pair converging over it.
//!
//! Both listeners are bound here, as `main` binds them — the content API and
//! the peer surface on separate ports, with separate credentials. That
//! separation is itself under test: the replication secret must not open the
//! content API, and the API key must not open the peer surface.

use std::time::{Duration, Instant};

use axum::http::StatusCode;
use serde_json::json;

use crate::{
    replication::{NODE_ID_HEADER, PROTOCOL_VERSION, URL_PREFIX},
    tests::harness::{author, TestServer, REPLICATION_SECRET},
};

const TENANT: &str = "/docs/acme";
const CONVERGE_TIMEOUT: Duration = Duration::from_secs(20);

/// Reads a peer route on `server`, authenticating as a peer.
async fn peer_get(server: &TestServer, path: &str, secret: Option<&str>) -> reqwest::Response {
    let url = format!(
        "{}{}{}",
        server
            .replication_url
            .as_ref()
            .expect("this node serves no replication surface"),
        URL_PREFIX,
        path
    );

    let mut request = reqwest::Client::new().get(url);

    if let Some(secret) = secret {
        request = request.bearer_auth(secret);
    }

    request.send().await.expect("peer request failed")
}

/// Reads a file from a node that may not be serving content yet. A cold
/// replica answers `503` to every content read until its first catch-up
/// lands, which for a poll helper is "not yet" rather than a failure.
async fn read_when_ready(server: &TestServer, tenant: &str, path: &str) -> Option<String> {
    let response = server.get(&format!("{}/files/{}", tenant, path)).await;

    match response.status {
        StatusCode::OK => Some(
            response.json()["content"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        ),
        StatusCode::NOT_FOUND | StatusCode::SERVICE_UNAVAILABLE => None,
        other => panic!("unexpected read status {}: {}", other, response.text),
    }
}

/// Waits until `path` reads back as `expected` on the replica.
async fn wait_for_content(replica: &TestServer, path: &str, expected: &str) {
    let deadline = Instant::now() + CONVERGE_TIMEOUT;

    loop {
        if read_when_ready(replica, TENANT, path).await.as_deref() == Some(expected) {
            return;
        }

        if Instant::now() >= deadline {
            panic!(
                "replica never converged on {} = {:?} (holds {:?})",
                path,
                expected,
                read_when_ready(replica, TENANT, path).await
            );
        }

        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Waits until a node's own peer listing holds `count` repositories — the
/// set the deletion guard measures against.
async fn wait_for_repository_count(server: &TestServer, count: usize) {
    let deadline = Instant::now() + CONVERGE_TIMEOUT;

    loop {
        let response = peer_get(server, "/state", Some(REPLICATION_SECRET)).await;

        if response.status() == StatusCode::OK {
            let body: serde_json::Value = response.json().await.expect("state is not JSON");

            if body["repositories"].as_array().unwrap().len() == count {
                return;
            }
        }

        assert!(
            Instant::now() < deadline,
            "node never reached {} repositories",
            count
        );

        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Starts a master and a replica following it.
async fn pair() -> (TestServer, TestServer) {
    let master = TestServer::builder()
        .master()
        .node_id("test-master")
        .start()
        .await;

    let replica = TestServer::builder()
        .replica_of(master.replication_url.as_ref().unwrap())
        .node_id("test-replica")
        .start()
        .await;

    (master, replica)
}

// --- The peer surface ----------------------------------------------------

#[tokio::test]
async fn the_peer_surface_has_its_own_credential() {
    let master = TestServer::builder().master().start().await;

    // No credential at all.
    assert_eq!(
        peer_get(&master, "/state", None).await.status(),
        StatusCode::UNAUTHORIZED
    );

    // The *content* API key does not open the peer surface: no caller of the
    // product's API ever holds the replication secret, and vice versa.
    assert_eq!(
        peer_get(&master, "/state", Some(&master.api_key))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    assert_eq!(
        peer_get(&master, "/state", Some(REPLICATION_SECRET))
            .await
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_replication_secret_does_not_open_the_content_api() {
    // The mirror image, and the reason the two are separate listeners: a
    // proxy pointed at one cannot reach the other, and neither credential
    // crosses over.
    let master = TestServer::builder().master().start().await;

    let response = reqwest::Client::new()
        .get(&master.base_url)
        .bearer_auth(REPLICATION_SECRET)
        .send()
        .await
        .expect("request failed");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn state_lists_every_repository_with_its_head() {
    let master = TestServer::builder().master().start().await;

    let sha = master.write_file(TENANT, "a.md", "x").await;

    master.write_file("/blog/other", "b.md", "x").await;

    let body: serde_json::Value = peer_get(&master, "/state", Some(REPLICATION_SECRET))
        .await
        .json()
        .await
        .expect("state is not JSON");

    assert_eq!(body["protocol"], PROTOCOL_VERSION);
    // A master generated its data-set identity at startup.
    assert_eq!(
        body["identity"].as_str().map(str::len),
        Some(64),
        "a master must serve a data-set identity"
    );
    // Deletions may only be inferred from a complete listing.
    assert_eq!(body["complete"], true);

    let repositories = body["repositories"].as_array().unwrap();

    assert_eq!(repositories.len(), 2);

    let acme = repositories
        .iter()
        .find(|entry| entry["tenant_id"] == "acme")
        .expect("the acme repository is missing");

    assert_eq!(acme["collection_id"], "docs");
    assert_eq!(acme["head_sha"].as_str().unwrap(), sha);
}

#[tokio::test]
async fn a_pack_carries_the_head_it_announces() {
    let master = TestServer::builder().master().start().await;

    let sha = master.write_file(TENANT, "a.md", "x").await;

    let response = peer_get(&master, "/docs/acme/pack", Some(REPLICATION_SECRET)).await;

    assert_eq!(response.status(), StatusCode::OK);

    // The body is a binary stream with nowhere to put the sha, so it travels
    // in a header.
    assert_eq!(
        response
            .headers()
            .get("x-replication-head-sha")
            .and_then(|value| value.to_str().ok()),
        Some(sha.as_str())
    );

    let pack = response.bytes().await.expect("cannot read pack body");

    assert!(!pack.is_empty(), "an empty pack for a non-empty repository");
    assert_eq!(&pack[..4], b"PACK", "the body is not a packfile");
}

#[tokio::test]
async fn the_have_parameter_takes_only_hexadecimal() {
    // It goes straight into a git lookup, so it gets the same treatment as
    // every other sha on this API — no revspec can reach libgit2 through it.
    let master = TestServer::builder().master().start().await;

    master.write_file(TENANT, "a.md", "x").await;

    for have in ["HEAD", "HEAD~1", "nope"] {
        let response = peer_get(
            &master,
            &format!("/docs/acme/pack?have={}", have),
            Some(REPLICATION_SECRET),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "accepted have={}",
            have
        );
    }
}

#[tokio::test]
async fn the_peer_health_body_is_what_the_public_route_serves() {
    // One struct, one builder, two doors — the peer reads it with the
    // replication secret, an operator reads it with no credential at all.
    let master = TestServer::builder().master().start().await;

    let peer: serde_json::Value = peer_get(&master, "/health", Some(REPLICATION_SECRET))
        .await
        .json()
        .await
        .expect("peer health is not JSON");

    let public = master
        .get_unauthenticated("/_health/replication")
        .await
        .json();

    assert_eq!(peer, public);
    assert_eq!(peer["node"]["role"], "master");
}

// --- A converging pair ---------------------------------------------------

#[tokio::test]
async fn a_replica_converges_on_its_master() {
    let (master, replica) = pair().await;

    master.write_file(TENANT, "docs/intro.md", "# Hello").await;

    wait_for_content(&replica, "docs/intro.md", "# Hello").await;

    // Updates follow too, not just the initial clone.
    master
        .write_file(TENANT, "docs/intro.md", "# Updated")
        .await;

    wait_for_content(&replica, "docs/intro.md", "# Updated").await;

    // And deletions.
    master
        .delete(
            &format!("{}/files/docs/intro.md", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::OK);

    let deadline = Instant::now() + CONVERGE_TIMEOUT;

    while read_when_ready(&replica, TENANT, "docs/intro.md")
        .await
        .is_some()
    {
        assert!(
            Instant::now() < deadline,
            "a deletion never reached the replica"
        );

        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn a_replica_serves_exact_git_backed_reads_once_caught_up() {
    let (master, replica) = pair().await;

    master.write_file(TENANT, "docs/a.md", "# A").await;
    master.write_file(TENANT, "docs/b.md", "# B").await;

    server_head_matches(&master, &replica).await;

    // Every read route answers from the replicated objects.
    let listing = replica.get(&format!("{}/files", TENANT)).await;

    listing.expect_status(StatusCode::OK);

    assert_eq!(listing.json()["files"][0]["name"], "docs");

    let commits = replica.get(&format!("{}/commits", TENANT)).await;

    commits.expect_status(StatusCode::OK);

    // Full history, not a shallow copy — which is what makes promotion a
    // config swap rather than a migration.
    assert_eq!(
        commits.json()["commits"].as_array().unwrap().len(),
        master.get(&format!("{}/commits", TENANT)).await.json()["commits"]
            .as_array()
            .unwrap()
            .len()
    );
}

/// Waits until the replica's HEAD for the tenant matches the master's.
async fn server_head_matches(master: &TestServer, replica: &TestServer) {
    let expected = master.head_sha(TENANT).await;
    let deadline = Instant::now() + CONVERGE_TIMEOUT;

    loop {
        let response = replica.get(&format!("{}/commits?per_page=1", TENANT)).await;

        if response.status == StatusCode::OK
            && response.json()["commits"][0]["sha"].as_str() == Some(expected.as_str())
        {
            return;
        }

        assert!(
            Instant::now() < deadline,
            "the replica never reached the master's HEAD {}",
            expected
        );

        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Waits until the replica's own peer listing names the master's HEAD for
/// the tenant.
///
/// Reads answer from the ref the moment it moves, and the listing is updated
/// from the sync that moved it right after — so "the replica reads the new
/// head" is not yet "the replica lists it". A test that goes on to change the
/// replica's repository behind its back, or that asserts on the listing,
/// waits for this rather than for [`server_head_matches`]: a sync still
/// finishing would otherwise record its head over whatever the test did.
async fn replica_lists_master_head(master: &TestServer, replica: &TestServer) {
    let expected = master.head_sha(TENANT).await;
    let deadline = Instant::now() + CONVERGE_TIMEOUT;

    loop {
        let response = peer_get(replica, "/state", Some(REPLICATION_SECRET)).await;

        if response.status() == StatusCode::OK {
            let body: serde_json::Value = response.json().await.expect("state is not JSON");

            let listed = body["repositories"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| {
                    entry["collection_id"] == "docs"
                        && entry["tenant_id"] == "acme"
                        && entry["head_sha"].as_str() == Some(expected.as_str())
                });

            if listed {
                return;
            }
        }

        assert!(
            Instant::now() < deadline,
            "the replica never listed the master's HEAD {}",
            expected
        );

        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn a_replica_pins_its_masters_identity() {
    let (master, replica) = pair().await;

    master.write_file(TENANT, "a.md", "x").await;

    server_head_matches(&master, &replica).await;

    let master_identity = master
        .get_unauthenticated("/_health/replication")
        .await
        .json()["node"]["identity"]
        .as_str()
        .expect("the master serves no identity")
        .to_string();

    let replica_identity = replica
        .get_unauthenticated("/_health/replication")
        .await
        .json()["node"]["identity"]
        .as_str()
        .expect("the replica pinned no identity")
        .to_string();

    // Both nodes of one data set carry the same identity, which is what lets
    // a follower re-point at a promoted replica without re-pairing.
    assert_eq!(master_identity, replica_identity);

    // And it is pinned on disk, so a restart does not re-pair.
    assert!(replica.repos_path.join(".replication.json").is_file());
}

#[tokio::test]
async fn a_caught_up_replica_reports_itself_synced_and_takes_traffic() {
    let (master, replica) = pair().await;

    master.write_file(TENANT, "a.md", "x").await;

    server_head_matches(&master, &replica).await;

    // Reading the master's head is not yet the end of the sync pass that
    // landed it: the pending count and the bootstrap gate settle when the
    // pass finishes, so this is polled rather than read once.
    let deadline = Instant::now() + CONVERGE_TIMEOUT;

    loop {
        let ping = replica.get("").await;

        ping.expect_status(StatusCode::OK);

        let status = ping.json()["replica"].clone();

        if status["state"] == "ready"
            && status["sync"] == "synced"
            && status["pending_repositories"] == 0
        {
            assert!(status["last_reconcile_at"].is_string());

            break;
        }

        assert!(
            Instant::now() < deadline,
            "the replica never reported itself caught up: {}",
            status
        );

        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let health = replica.get_unauthenticated("/_health/status").await;

    assert_eq!(health.json()["status"], "healthy");
    assert_eq!(health.json()["role"], "replica");
    // Still never writable, however caught up it is.
    assert_eq!(health.json()["writable"], false);
}

#[tokio::test]
async fn a_master_reports_the_replicas_following_it() {
    let (master, replica) = pair().await;

    master.write_file(TENANT, "a.md", "x").await;

    server_head_matches(&master, &replica).await;

    // One probe against the master covers the whole set.
    let deadline = Instant::now() + CONVERGE_TIMEOUT;

    loop {
        let body = master
            .get_unauthenticated("/_health/replication")
            .await
            .json();

        let replicas = body["replicas"].as_array().unwrap().clone();

        if replicas
            .iter()
            .any(|entry| entry["node_id"] == "test-replica")
        {
            assert_eq!(body["status"], "healthy");

            break;
        }

        assert!(
            Instant::now() < deadline,
            "the master never saw its replica: {:?}",
            replicas
        );

        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn a_replica_chains_its_own_peer_surface() {
    // A replica exposes the peer routes too, which is what lets replicas
    // chain — its `state` describes what *it* holds.
    let (master, replica) = pair().await;

    master.write_file(TENANT, "a.md", "x").await;

    replica_lists_master_head(&master, &replica).await;

    let body: serde_json::Value = peer_get(&replica, "/state", Some(REPLICATION_SECRET))
        .await
        .json()
        .await
        .expect("replica state is not JSON");

    assert_eq!(body["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(
        body["repositories"][0]["head_sha"].as_str().unwrap(),
        master.head_sha(TENANT).await
    );
}

#[tokio::test]
async fn a_cold_replica_refuses_to_describe_a_state_it_does_not_have() {
    // It has not finished its first catch-up, so it cannot honestly answer
    // what it holds — and a chained replica must not treat that as a
    // complete listing.
    let replica = TestServer::builder().replica().start().await;

    let response = peer_get(&replica, "/state", Some(REPLICATION_SECRET)).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn a_second_node_claiming_a_connected_node_id_is_refused_on_the_stream() {
    // The node id is how a master keys its roster, so two processes claiming
    // one would merge into a single row.
    let (master, replica) = pair().await;

    master.write_file(TENANT, "a.md", "x").await;

    server_head_matches(&master, &replica).await;

    let response = reqwest::Client::new()
        .get(format!(
            "{}{}/events",
            master.replication_url.as_ref().unwrap(),
            URL_PREFIX
        ))
        .bearer_auth(REPLICATION_SECRET)
        .header(NODE_ID_HEADER, "test-replica")
        .header("x-replication-instance", "0123456789abcdef")
        .send()
        .await
        .expect("stream request failed");

    assert_eq!(response.status(), StatusCode::CONFLICT);

    // Only the stream is refused: `state` and `pack` are what convergence
    // rests on and stay served, so a misnamed replica keeps its data current
    // while its operator is told.
    assert_eq!(
        peer_get(&master, "/state", Some(REPLICATION_SECRET))
            .await
            .status(),
        StatusCode::OK
    );
}

// --- Safety: a replica never destroys its own data ------------------------

/// Commits on top of a repository's HEAD directly, bypassing the API — the
/// way a promoted node, or a node written to out of band, ends up holding
/// history its upstream does not.
fn commit_on_top(repo_path: &std::path::Path, message: &str) -> String {
    let repo = git2::Repository::open(repo_path).expect("cannot open repository");

    let head = repo.head().expect("no HEAD");
    let parent = head.peel_to_commit().expect("HEAD is not a commit");
    let tree = parent.tree().expect("HEAD has no tree");

    // A fixed timestamp rather than `now`, so the same call on two stores
    // produces the same sha — which is how a test can put one node's history
    // onto another.
    let signature = git2::Signature::new(
        "Out Of Band",
        "oob@example.com",
        &git2::Time::new(1_780_000_000, 0),
    )
    .expect("bad signature");

    let oid = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[&parent],
        )
        .expect("cannot commit");

    oid.to_string()
}

/// Waits until the node's public replication health carries an issue of
/// `kind`, and returns the whole body.
async fn wait_for_issue(server: &TestServer, kind: &str) -> serde_json::Value {
    let deadline = Instant::now() + CONVERGE_TIMEOUT;

    loop {
        let body = server
            .get_unauthenticated("/_health/replication")
            .await
            .json();

        if body["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == kind)
        {
            return body;
        }

        assert!(
            Instant::now() < deadline,
            "issue {} was never raised (issues: {})",
            kind,
            body["issues"]
        );

        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn a_replica_holding_history_its_master_lacks_is_locked_not_wiped() {
    // The previous behaviour — discard and re-clone — is exactly what this
    // codebase must not do on its own: the replica cannot know which side
    // holds the history that matters.
    let (master, replica) = pair().await;

    master.write_file(TENANT, "a.md", "# Master").await;

    replica_lists_master_head(&master, &replica).await;

    // The replica gains a commit its master will never announce.
    let local_head = commit_on_top(&replica.repo_path("docs", "acme"), "out of band");

    // Nothing notified the repository index of a commit made behind its
    // back, so stand in for the pass that would notice: the periodic rescan
    // (every 10 minutes), or the boot scan of a restarted node — which is
    // how this state is actually reached in a deployment.
    replica.state.repository_index.rescan();

    let health = wait_for_issue(&replica, "replica_ahead").await;

    // An open issue means a human is needed, so the node is halted rather
    // than merely degraded.
    assert_eq!(health["status"], "halted");

    let issue = health["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|issue| issue["kind"] == "replica_ahead")
        .unwrap()
        .clone();

    // The issue names the repository so an operator knows what to look at.
    assert_eq!(issue["collection_id"], "docs");
    assert_eq!(issue["tenant_id"], "acme");

    // The local copy is kept and still served.
    assert_eq!(
        replica.read_file(TENANT, "a.md").await.as_deref(),
        Some("# Master")
    );
    assert_eq!(replica.head_sha(TENANT).await, local_head);

    // The replica's own sync verdict says it is waiting on a person.
    assert_eq!(replica.get("").await.json()["replica"]["sync"], "halted");
}

#[tokio::test]
async fn removing_the_local_copy_is_the_operators_exit_from_a_lock() {
    // A lock is not a dead end, and clearing it is deliberately an
    // operator's act rather than something the node decides: remove the
    // local copy — the side whose history is being given up — and the next
    // reconcile clones the repository afresh and lifts the lock. The index
    // cannot see that removal (nothing announced it), so the reconcile
    // checks the disk for exactly this case.
    let (master, replica) = pair().await;

    master.write_file(TENANT, "a.md", "# Master").await;

    replica_lists_master_head(&master, &replica).await;

    commit_on_top(&replica.repo_path("docs", "acme"), "out of band");

    replica.state.repository_index.rescan();

    wait_for_issue(&replica, "replica_ahead").await;

    // The operator discards the replica's copy.
    std::fs::remove_dir_all(replica.repo_path("docs", "acme"))
        .expect("cannot remove the replica's copy");

    let deadline = Instant::now() + CONVERGE_TIMEOUT;

    loop {
        let body = replica
            .get_unauthenticated("/_health/replication")
            .await
            .json();

        if body["issues"].as_array().unwrap().is_empty() {
            assert_eq!(body["status"], "healthy");

            break;
        }

        assert!(
            Instant::now() < deadline,
            "the lock never lifted after the local copy was removed: {}",
            body["issues"]
        );

        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Re-cloned from the master, at the master's history.
    wait_for_content(&replica, "a.md", "# Master").await;
    assert_eq!(
        replica.head_sha(TENANT).await,
        master.head_sha(TENANT).await
    );
}

#[tokio::test]
async fn a_master_that_only_commits_more_leaves_a_diverged_replica_locked() {
    // The other half of the rule above, and the case an operator actually
    // hits: once a replica holds a commit of its own, the master's later
    // commits fork away from it. Neither side descends from the other, so
    // the local copy is kept and a human is asked.
    let (master, replica) = pair().await;

    master.write_file(TENANT, "a.md", "# One").await;

    replica_lists_master_head(&master, &replica).await;

    commit_on_top(&replica.repo_path("docs", "acme"), "out of band");

    replica.state.repository_index.rescan();

    wait_for_issue(&replica, "replica_ahead").await;

    master.write_file(TENANT, "a.md", "# Two").await;

    let health = wait_for_issue(&replica, "history_diverged").await;

    assert_eq!(health["status"], "halted");

    // Still serving its own copy rather than having discarded anything.
    assert_eq!(
        read_when_ready(&replica, TENANT, "a.md").await.as_deref(),
        Some("# One")
    );
}

#[tokio::test]
async fn a_listing_that_would_delete_most_of_a_replica_is_refused() {
    // A replica deleting what its master deleted is ordinary replication; a
    // replica deleting most of itself because a listing went empty is the
    // one thing the feature must never do on its own.
    let (master, replica) = pair().await;

    master.write_file("/docs/one", "a.md", "x").await;
    master.write_file("/docs/two", "b.md", "x").await;

    // Both, as the replica lists them: the guard measures what the listing
    // holds, and a replica that had landed only one would hold too little to
    // trip it.
    wait_for_repository_count(&replica, 2).await;

    // The master's whole store goes at once — an unmounted or swapped
    // directory, which is the shape of the accident this guard exists for.
    // Deleting the tenants one by one through the API would *not* trip it:
    // each single deletion is not "more than half" of what was held when the
    // replica saw it, which is the rule working as documented.
    std::fs::remove_dir_all(master.repos_path.join("docs"))
        .expect("cannot empty the master's store");

    master.state.repository_index.rescan();

    wait_for_repository_count(&master, 0).await;

    let health = wait_for_issue(&replica, "deletion_refused").await;

    assert_eq!(health["status"], "halted");

    let issue = health["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|issue| issue["kind"] == "deletion_refused")
        .unwrap()
        .clone();

    assert_eq!(issue["would_delete"], 2);
    assert_eq!(issue["held"], 2);

    // Every local copy is kept and still served.
    assert_eq!(
        replica.read_file("/docs/one", "a.md").await.as_deref(),
        Some("x")
    );
    assert_eq!(
        replica.read_file("/docs/two", "b.md").await.as_deref(),
        Some("x")
    );
}

#[tokio::test]
async fn an_ordinary_deletion_below_the_guard_still_replicates() {
    // The guard draws its line at half, so a minority deletion is ordinary
    // replication and must still be applied.
    let (master, replica) = pair().await;

    for tenant in ["/docs/one", "/docs/two", "/docs/three"] {
        master.write_file(tenant, "a.md", "x").await;
    }

    wait_for_repository_count(&replica, 3).await;

    master
        .request(reqwest::Method::DELETE, "/docs/one", None, true)
        .await
        .expect_status(StatusCode::OK);

    let deadline = Instant::now() + CONVERGE_TIMEOUT;

    while read_when_ready(&replica, "/docs/one", "a.md")
        .await
        .is_some()
    {
        assert!(
            Instant::now() < deadline,
            "a one-of-three deletion never replicated"
        );

        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Nothing was raised, and the others are untouched.
    let health = replica
        .get_unauthenticated("/_health/replication")
        .await
        .json();

    assert!(health["issues"].as_array().unwrap().is_empty());
    assert!(read_when_ready(&replica, "/docs/two", "a.md")
        .await
        .is_some());
}

#[tokio::test]
async fn head_relation_classifies_the_three_cases_a_replica_must_tell_apart() {
    // The classification `apply_pack` refuses on, tested directly because
    // two of its three cases are reachable only from a repository state no
    // API call can produce.
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "one").await;

    let repo_path = server.repo_path("docs", "acme");
    let base = server.head_sha(TENANT).await;

    assert!(matches!(
        crate::git::GitReplication::relation_to(&repo_path, &base),
        crate::git::HeadRelation::Same
    ));

    // A sha this repository has never seen: nothing can be said without a
    // pack.
    assert!(matches!(
        crate::git::GitReplication::relation_to(&repo_path, &"0".repeat(40)),
        crate::git::HeadRelation::Unknown
    ));

    // Local history moves on; the announced commit is now an ancestor.
    server.write_file(TENANT, "a.md", "two").await;

    match crate::git::GitReplication::relation_to(&repo_path, &base) {
        crate::git::HeadRelation::Ahead { local } => {
            assert_eq!(local, server.head_sha(TENANT).await);
        }
        other => panic!("expected Ahead, got {:?}", other),
    }

    // A commit held locally that is neither HEAD nor one of its ancestors —
    // a forked history, which only a pack whose ref move never happened (or
    // a hand-edited repository) can produce.
    let forked = {
        let repo = git2::Repository::open(&repo_path).expect("cannot open repository");
        let base_commit = repo
            .find_commit(git2::Oid::from_str(&base).unwrap())
            .expect("base commit is gone");
        let signature =
            git2::Signature::now("Forked", "forked@example.com").expect("bad signature");

        // Bound rather than returned directly: the tree borrows `repo`, and
        // that borrow must end before `repo` does.
        let oid = repo
            .commit(
                None,
                &signature,
                &signature,
                "a fork",
                &base_commit.tree().unwrap(),
                &[&base_commit],
            )
            .expect("cannot commit")
            .to_string();

        oid
    };

    assert!(matches!(
        crate::git::GitReplication::relation_to(&repo_path, &forked),
        crate::git::HeadRelation::Diverged { .. }
    ));
}
