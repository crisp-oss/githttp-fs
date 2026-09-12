// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! A full replicated deployment on scratch disks: a master process, a replica
//! process following it over the peer protocol, and a hook receiver.
//!
//! This is the shape the shipped `config.master.toml` / `config.replica.toml`
//! pair describes, built from nothing in a `TempDir`. Each node gets its own
//! store, because they are two copies of the same content and not two
//! processes sharing a directory — the same reason `dev/repositories` has a
//! `master/` and a `replica/` subdirectory.
//!
//! The convergence helpers here all *poll*. A replica is eventually
//! consistent by construction: the notification stream is a latency hint and
//! the poll interval is what bounds staleness, so a test that read once and
//! asserted would be asserting on a race. Polling with a timeout asserts the
//! thing the design actually promises — that the replica gets there.

#![allow(dead_code)]

use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;

use crate::{
    node::{wait_until, Node, NodeSpec, CONVERGE_TIMEOUT},
    receiver::HookReceiver,
};

pub struct Deployment {
    pub master: Node,
    pub replica: Node,
    /// The master's hook receiver. A replica never enqueues a hook — hook
    /// delivery belongs to the node that accepted the commit — so there is
    /// only ever one.
    pub receiver: HookReceiver,
    /// Held so the scratch disks outlive both processes. Dropped last, after
    /// each `Node`'s own `Drop` has killed its process.
    scratch: TempDir,
}

impl Deployment {
    /// Brings up a master, then a replica following it, and waits until the
    /// replica reports itself caught up.
    pub async fn start() -> Self {
        Self::start_with(true).await
    }

    /// The same deployment with no `[hooks]` on the master, for tests where a
    /// receiver would only add noise.
    pub async fn start_without_hooks() -> Self {
        Self::start_with(false).await
    }

    async fn start_with(hooks: bool) -> Self {
        let scratch = tempfile::tempdir().expect("cannot create deployment scratch directory");

        let receiver = HookReceiver::start().await;

        let mut master_spec = NodeSpec::new("master", &scratch.path().join("master"));

        master_spec.role = Some("master".to_string());
        master_spec.node_id = "e2e-master".to_string();

        if hooks {
            master_spec.hooks_url = Some(receiver.url.clone());
        }

        let master = Node::start(master_spec).await;

        let mut replica_spec = NodeSpec::new("replica", &scratch.path().join("replica"));

        replica_spec.role = Some("replica".to_string());
        replica_spec.node_id = "e2e-replica".to_string();
        replica_spec.master_url = Some(master.spec.replication_url());

        let replica = Node::start(replica_spec).await;

        let deployment = Self {
            master,
            replica,
            receiver,
            scratch,
        };

        deployment.wait_for_replica_paired().await;

        deployment
    }

    /// Waits until the replica has reached its master at least once — its
    /// health reports the master reachable and an identity pinned.
    pub async fn wait_for_replica_paired(&self) {
        wait_until(
            "replica to pair with its master",
            CONVERGE_TIMEOUT,
            || async {
                let health = self.replica.health_replication().await;

                health
                    .pointer("/master/reachable")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    && health.pointer("/node/identity").map(|id| !id.is_null()) == Some(true)
            },
        )
        .await;
    }

    /// Waits until the replica reports `sync: "synced"` — the one-word
    /// verdict that following is keeping up with nothing pending or locked.
    pub async fn wait_for_replica_synced(&self) {
        wait_until(
            "replica to report itself synced",
            CONVERGE_TIMEOUT,
            || async {
                self.replica
                    .health_replication()
                    .await
                    .pointer("/replica/sync")
                    .and_then(Value::as_str)
                    == Some("synced")
            },
        )
        .await;
    }

    /// Waits until the master's roster lists the replica.
    pub async fn wait_for_roster(&self) {
        wait_until(
            "master roster to list the replica",
            CONVERGE_TIMEOUT,
            || async {
                let health = self.master.health_replication().await;

                health
                    .get("replicas")
                    .and_then(Value::as_array)
                    .map(|replicas| {
                        replicas.iter().any(|replica| {
                            replica.get("node_id").and_then(Value::as_str) == Some("e2e-replica")
                        })
                    })
                    .unwrap_or(false)
            },
        )
        .await;
    }

    /// Waits until the replica serves `tenant_path` with exactly `content`.
    pub async fn wait_for_replica_content(&self, tenant_path: &str, content: &str) {
        wait_until(
            &format!("replica to serve '{}'", tenant_path),
            CONVERGE_TIMEOUT,
            || async {
                let reply = self.replica.read_file(tenant_path).await;

                reply.status.as_u16() == 200
                    && reply.json().get("content").and_then(Value::as_str) == Some(content)
            },
        )
        .await;
    }

    /// Waits until the replica no longer serves `tenant_path`.
    pub async fn wait_for_replica_missing(&self, tenant_path: &str) {
        wait_until(
            &format!("replica to drop '{}'", tenant_path),
            CONVERGE_TIMEOUT,
            || async { self.replica.read_file(tenant_path).await.status.as_u16() == 404 },
        )
        .await;
    }

    /// Gives replication a moment to do something, for the assertions that it
    /// should *not* have.
    pub async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(750)).await;
    }
}
