// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Integration tests for the router's two middleware layers and the public
//! surface they deliberately never see.
//!
//! The layer ordering in `build_router` is load-bearing: the health routes
//! are nested *after* both `.layer` calls, which is what leaves them
//! unauthenticated. A change that moves them above the layers would silently
//! put a key back in front of every monitor — so it is asserted here rather
//! than left to review.

use axum::http::StatusCode;
use serde_json::json;

use crate::tests::harness::{author, TestServer};

const TENANT: &str = "/docs/acme";

#[tokio::test]
async fn the_api_root_is_an_authenticated_no_op() {
    let server = TestServer::start().await;

    let response = server.get("").await;

    response.expect_status(StatusCode::OK);

    assert_eq!(response.json(), json!({ "pong": true }));

    // A master and a standalone node answer exactly that body — the
    // `replica` object exists only on a replica.
    assert!(response.json().get("replica").is_none());
}

#[tokio::test]
async fn every_content_route_requires_the_api_key() {
    let server = TestServer::start().await;

    server.write_file(TENANT, "a.md", "x").await;

    for path in [
        "",
        TENANT,
        &format!("{}/files", TENANT),
        &format!("{}/files/a.md", TENANT),
        &format!("{}/commits", TENANT),
        &format!("{}/order", TENANT),
        &format!("{}/count/files", TENANT),
    ] {
        let response = server.get_unauthenticated(path).await;

        response.expect_status(StatusCode::UNAUTHORIZED);

        assert_eq!(response.json()["error"], "missing or invalid API key");
    }
}

#[tokio::test]
async fn a_wrong_or_malformed_credential_is_refused() {
    let server = TestServer::start().await;

    for header in [
        "Bearer wrong-key",
        "Bearer ",
        "test-api-key",
        "Basic dGVzdDp0ZXN0",
        "bearer test-api-key",
    ] {
        let response = reqwest::Client::new()
            .get(&server.base_url)
            .header("Authorization", header)
            .send()
            .await
            .expect("request failed");

        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "accepted credential: {:?}",
            header
        );
    }
}

#[tokio::test]
async fn the_health_routes_are_public() {
    // Their audience is exactly the set of callers that does not hold the
    // product's API key.
    let server = TestServer::start().await;

    let status = server.get_unauthenticated("/_health/status").await;

    status.expect_status(StatusCode::OK);

    let body = status.json();

    assert_eq!(body["status"], "healthy");
    assert_eq!(body["name"], "githttp-fs");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["role"], "standalone");
    assert_eq!(body["writable"], true);
    assert!(body["uptime_secs"].is_number());

    chrono::DateTime::parse_from_rfc3339(body["started_at"].as_str().unwrap())
        .expect("started_at is not RFC 3339");

    let replication = server.get_unauthenticated("/_health/replication").await;

    replication.expect_status(StatusCode::OK);

    // It answers on every node, including a standalone one, and one probe
    // therefore works against a whole deployment.
    let body = replication.json();

    assert_eq!(body["node"]["role"], "standalone");
    assert_eq!(body["status"], "healthy");
    assert!(body["issues"].as_array().unwrap().is_empty());

    chrono::DateTime::parse_from_rfc3339(body["observed_at"].as_str().unwrap())
        .expect("observed_at is not RFC 3339");
}

#[tokio::test]
async fn health_takes_exactly_two_reserved_paths() {
    // `_health` is the one reserved collection id, and it takes one segment
    // — so `/v1/_health/{tenant_id}/...` still routes to the ordinary tenant
    // routes.
    let server = TestServer::start().await;

    server
        .get_unauthenticated("/_health/status/extra")
        .await
        .expect_status(StatusCode::NOT_FOUND);

    // An ordinary tenant route under the reserved collection id still
    // demands a key, which is what proves it is not on the public surface.
    server
        .get_unauthenticated("/_health/acme/files")
        .await
        .expect_status(StatusCode::UNAUTHORIZED);

    server
        .get("/_health/acme/files")
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_server_root_redirects_to_the_versioned_prefix() {
    let server = TestServer::start().await;

    let root = server
        .base_url
        .strip_suffix("/v1")
        .expect("base url should end in /v1")
        .to_string();

    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("cannot build client")
        .get(format!("{}/", root))
        .send()
        .await
        .expect("request failed");

    // Method-preserving, and no credential needed.
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok()),
        Some("/v1")
    );
}

// --- Replica guards ------------------------------------------------------

#[tokio::test]
async fn a_replica_refuses_writes_with_locked() {
    let server = TestServer::builder().replica().start().await;

    let response = server
        .put(
            &format!("{}/files/a.md", TENANT),
            json!({ "author": author(), "content": "x" }),
        )
        .await;

    response.expect_status(StatusCode::LOCKED);

    assert!(response.error_message().contains("read-only replica"));

    // Every write verb, not just PUT.
    server
        .delete(
            &format!("{}/files/a.md", TENANT),
            json!({ "author": author() }),
        )
        .await
        .expect_status(StatusCode::LOCKED);

    server
        .post(
            &format!("{}/files/a.md/move", TENANT),
            json!({ "author": author(), "destination": "b.md" }),
        )
        .await
        .expect_status(StatusCode::LOCKED);

    server
        .request(reqwest::Method::DELETE, TENANT, None, true)
        .await
        .expect_status(StatusCode::LOCKED);
}

#[tokio::test]
async fn a_cold_replica_refuses_content_reads_but_still_explains_itself() {
    // A node refusing every other request can still answer the one route
    // that says why — and the public health routes never reach the guard at
    // all, which is how an operator finds out.
    let server = TestServer::builder().replica().start().await;

    let read = server.get(&format!("{}/files", TENANT)).await;

    read.expect_status(StatusCode::SERVICE_UNAVAILABLE);

    assert!(read.error_message().contains("bootstrapping"));

    let ping = server.get("").await;

    ping.expect_status(StatusCode::OK);

    assert_eq!(ping.json()["pong"], true);

    // The replica object appears only here, and says what the node is doing.
    let replica = &ping.json()["replica"];

    assert_eq!(replica["state"], "bootstrapping");
    assert_eq!(replica["stream_connected"], false);
    assert!(replica["last_reconcile_at"].is_null());

    let status = server.get_unauthenticated("/_health/status").await;

    status.expect_status(StatusCode::OK);

    assert_eq!(status.json()["status"], "bootstrapping");
    assert_eq!(status.json()["role"], "replica");
    assert_eq!(status.json()["writable"], false);
}

#[tokio::test]
async fn the_batch_read_route_is_the_one_write_shaped_route_a_replica_serves() {
    // "Write-shaped" and "write" are different questions on this API: the
    // guard classifies POST /batch/files/read as a read.
    let server = TestServer::builder().replica().start().await;

    let response = server
        .post(
            &format!("{}/batch/files/read", TENANT),
            json!({ "files": ["a.md"] }),
        )
        .await;

    // Not 423: it passes the read-only guard and is stopped only by the
    // bootstrapping one.
    response.expect_status(StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn an_unauthenticated_caller_learns_nothing_about_the_node_role() {
    // The API-key check is outermost, so a 401 comes back before the
    // read-only guard can answer 423.
    let server = TestServer::builder().replica().start().await;

    let response = server
        .request(
            reqwest::Method::PUT,
            &format!("{}/files/a.md", TENANT),
            Some(json!({ "author": author(), "content": "x" })),
            false,
        )
        .await;

    response.expect_status(StatusCode::UNAUTHORIZED);
}
