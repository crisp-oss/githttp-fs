// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! A stub webhook receiver, running *inside the test process*.
//!
//! The node under test is out of process, but its hook receiver need not be:
//! the node POSTs over loopback like it would to any downstream system, and
//! keeping the receiver here is what lets a test read back exactly what was
//! delivered, in delivery order. It is bound before the node is started so
//! the node's config can name its URL.
//!
//! Deliberately minimal compared to the in-crate stub (`src/tests/harness.rs`)
//! — it always answers `200`. Retry, refusal and stall behaviour is queue
//! logic that the in-crate suite already covers against the same code; what
//! is only provable out of process is that a *deployed* node delivers at all.

#![allow(dead_code)]

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{extract::State, routing::post, Json, Router};
use serde_json::Value;

use crate::node::wait_until;

/// A receiver listening on an ephemeral port, recording every payload.
pub struct HookReceiver {
    pub url: String,
    payloads: Arc<Mutex<Vec<Value>>>,
}

impl HookReceiver {
    pub async fn start() -> Self {
        let payloads: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));

        let router = Router::new()
            .route("/hook", post(receive))
            .with_state(payloads.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("cannot bind hook receiver");

        let address = listener
            .local_addr()
            .expect("cannot read hook receiver address");

        tokio::spawn(async move {
            let _outcome = axum::serve(listener, router).await;
        });

        Self {
            url: format!("http://{}/hook", address),
            payloads,
        }
    }

    /// Every payload received so far, in delivery order.
    pub fn payloads(&self) -> Vec<Value> {
        self.payloads
            .lock()
            .expect("receiver lock poisoned")
            .clone()
    }

    pub fn count(&self) -> usize {
        self.payloads.lock().expect("receiver lock poisoned").len()
    }

    /// The `event` field of every payload, in order — what most assertions
    /// about delivery actually care about.
    pub fn events(&self) -> Vec<String> {
        self.payloads()
            .iter()
            .map(|payload| {
                payload
                    .get("event")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    /// Waits until at least `count` payloads have landed, then returns them.
    pub async fn wait_for(&self, count: usize) -> Vec<Value> {
        wait_until(
            &format!("{} hook payload(s)", count),
            Duration::from_secs(15),
            || async { self.count() >= count },
        )
        .await;

        self.payloads()
    }

    /// Gives any in-flight delivery a moment to arrive, for the assertions
    /// that nothing *should* be delivered. Waiting is the only way to tell
    /// "never sent" from "not sent yet", and this bounds how long a test
    /// pays for that certainty.
    pub async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(750)).await;
    }
}

async fn receive(
    State(payloads): State<Arc<Mutex<Vec<Value>>>>,
    Json(payload): Json<Value>,
) -> axum::http::StatusCode {
    payloads
        .lock()
        .expect("receiver lock poisoned")
        .push(payload);

    axum::http::StatusCode::OK
}
