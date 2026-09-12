// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! One githttp-fs *process*, its config file, its store, and its log.
//!
//! [`Node`] is the primitive the whole target is built on: it writes a
//! [`NodeSpec`] out as a real `config.toml` in a scratch directory, spawns
//! the binary cargo built with `-c` pointing at it, waits until the node
//! answers its own public health route, and kills it when the test ends.
//!
//! Two properties are load-bearing:
//!
//! - **Ports are chosen once and kept across restarts.** A replica's
//!   `master_url` names a port, so a master that came back on a different one
//!   would leave the replica following nothing — and the restart tests exist
//!   precisely to watch a node come back.
//! - **The store outlives the process.** [`Node::restart`] kills the process
//!   and spawns a new one over the same directory, which is what makes
//!   "content survives a restart" and "promotion is a config swap" testable
//!   at all.

#![allow(dead_code)]

use std::{
    fs::{self, File},
    io::Read,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::{json, Value};

/// The binary under test — the one `cargo test` just built, not whatever is
/// on `PATH`. Set by cargo for integration test targets, which is why these
/// tests live in `tests/` rather than in the in-crate suite.
const BINARY: &str = env!("CARGO_BIN_EXE_githttp-fs");

/// How long a node is given to bind and answer `/v1/_health/status`. Only
/// ever reached on a real failure — the happy path polls at 25 ms.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// How long [`wait_until`] keeps probing. Generous because replication
/// convergence crosses a poll interval and a process spawn.
pub const CONVERGE_TIMEOUT: Duration = Duration::from_secs(30);

const POLL: Duration = Duration::from_millis(25);

/// The replication secret every node in an e2e deployment shares. Distinct
/// from the API key on purpose: no caller of the product's API holds it.
pub const REPLICATION_SECRET: &str = "e2e-replication-secret";

/// The API key every node in an e2e deployment shares — as the shipped
/// `config.master.toml` / `config.replica.toml` pair does, so one client
/// credential reads from either node and failover is exercisable.
pub const API_KEY: &str = "e2e-api-key";

/// Author object every write request needs.
pub fn author() -> Value {
    json!({ "name": "E2E Author", "email": "e2e@example.com" })
}

/// A TOML basic string. JSON string escaping is a subset of TOML's, so this
/// is correct for the paths and URLs a spec holds.
fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("cannot encode TOML string")
}

/// Claims a free TCP port by binding it and letting it go.
///
/// Inherently racy — something else may take the port between the bind and
/// the node's own — but the window is microseconds on a loopback interface,
/// and the alternative (a fixed port range) collides with whatever else the
/// developer is running, which is worse and less obvious when it happens.
pub fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("cannot bind a probe socket");

    listener
        .local_addr()
        .expect("cannot read probe socket address")
        .port()
}

/// Everything that goes into one node's `config.toml`.
///
/// Deliberately a plain struct of owned fields rather than a builder chain:
/// a restart test mutates one field and spawns again, which reads better as
/// an assignment than as a rebuilt chain.
#[derive(Clone)]
pub struct NodeSpec {
    pub name: String,
    pub root: PathBuf,
    pub api_port: u16,
    pub replication_port: u16,
    pub api_key: String,
    pub node_id: String,
    /// `None` is a standalone node: no `[replication]` section at all.
    pub role: Option<String>,
    pub master_url: Option<String>,
    pub hooks_url: Option<String>,
    pub hook_events: Vec<String>,
    pub checkout_files: bool,
    pub checkout_files_autoheal: bool,
    pub poll_interval_secs: u64,
    pub maintenance_enabled: bool,
    pub log_level: String,
}

impl NodeSpec {
    /// A standalone node rooted at `root`, with ports already claimed.
    pub fn new(name: &str, root: &Path) -> Self {
        Self {
            name: name.to_string(),
            root: root.to_path_buf(),
            api_port: free_port(),
            replication_port: free_port(),
            api_key: API_KEY.to_string(),
            node_id: format!("e2e-{}", name),
            role: None,
            master_url: None,
            hooks_url: None,
            hook_events: vec![
                "file.created".to_string(),
                "file.updated".to_string(),
                "file.deleted".to_string(),
                "file.moved".to_string(),
                "order.updated".to_string(),
                "order.deleted".to_string(),
            ],
            checkout_files: true,
            checkout_files_autoheal: false,
            // Every write arms a maintenance timer, and nothing in these
            // tests wants a background repack mid-assertion.
            maintenance_enabled: false,
            poll_interval_secs: 1,
            log_level: "debug".to_string(),
        }
    }

    pub fn store_path(&self) -> PathBuf {
        self.root.join("store")
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn log_path(&self) -> PathBuf {
        self.root.join("node.log")
    }

    /// This node's content API base, as a client addresses it.
    pub fn api_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.api_port)
    }

    /// This node's peer listener base — what a replica's `master_url` points
    /// at, never the content port.
    pub fn replication_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.replication_port)
    }

    /// Renders the spec as the config file the node is actually started with.
    pub fn to_toml(&self) -> String {
        let mut text = format!(
            "[server]\n\
             host = \"127.0.0.1\"\n\
             port = {}\n\
             api_key = {}\n\
             repos_path = {}\n\
             log_level = {}\n\
             checkout_files = {}\n\
             checkout_files_autoheal = {}\n",
            self.api_port,
            toml_string(&self.api_key),
            toml_string(&self.store_path().to_string_lossy()),
            toml_string(&self.log_level),
            self.checkout_files,
            self.checkout_files_autoheal
        );

        if let Some(url) = &self.hooks_url {
            text.push_str(&format!(
                "\n[hooks]\nurl = {}\nevents = [{}]\nretry_attempts = 2\nretry_backoff_ms = 50\n",
                toml_string(url),
                self.hook_events
                    .iter()
                    .map(|event| toml_string(event))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        if let Some(role) = &self.role {
            text.push_str(&format!(
                "\n[replication]\nrole = {}\nsecret = {}\nnode_id = {}\nport = {}\n",
                toml_string(role),
                toml_string(REPLICATION_SECRET),
                toml_string(&self.node_id),
                self.replication_port
            ));

            if let Some(master_url) = &self.master_url {
                text.push_str(&format!(
                    "master_url = {}\npoll_interval_secs = {}\n",
                    toml_string(master_url),
                    self.poll_interval_secs
                ));
            }
        }

        text.push_str(&format!(
            "\n[maintenance]\nenabled = {}\n",
            self.maintenance_enabled
        ));

        text
    }
}

/// A response reduced to what assertions look at, holding the body as text so
/// a bodiless answer (`HEAD`) needs no second shape.
pub struct Reply {
    pub status: reqwest::StatusCode,
    pub text: String,
}

impl Reply {
    /// The body parsed as JSON, panicking with the raw body when it is not —
    /// which turns an unexpected `500` into a readable failure.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or_else(|err| {
            panic!(
                "response body is not JSON ({}): status={} body={:?}",
                err, self.status, self.text
            )
        })
    }

    #[track_caller]
    pub fn expect_status(&self, expected: u16) -> &Self {
        assert_eq!(
            self.status.as_u16(),
            expected,
            "unexpected status (body: {})",
            self.text
        );

        self
    }

    /// Convenience for the one field every `AppError` renders.
    pub fn error_message(&self) -> String {
        self.json()
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }
}

/// A running githttp-fs process.
pub struct Node {
    pub spec: NodeSpec,
    child: Option<Child>,
    client: reqwest::Client,
}

impl Node {
    /// Writes the config, spawns the binary, and waits until it answers.
    pub async fn start(spec: NodeSpec) -> Self {
        let mut node = Self {
            spec,
            child: None,
            client: reqwest::Client::new(),
        };

        node.spawn_process();
        node.wait_ready().await;

        node
    }

    /// Kills the process, leaving the store and the config on disk.
    ///
    /// `SIGKILL` rather than a graceful shutdown on purpose: this is the
    /// crash a supervisor restarts after, and the boot-time repairs
    /// (`.git/index.lock`, abandoned incoming packs) exist for exactly that
    /// case. A test that wants them exercised has to be able to produce one.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Stops the node and starts it again over the **same store**, with
    /// whatever `spec` now says. The config file is rewritten, so this is a
    /// config swap plus a restart — the operation promotion is documented as.
    pub async fn restart(&mut self) {
        self.stop();

        // Nothing holds the port once the process is reaped, but the OS may
        // still be tearing the listener down; the readiness poll absorbs it.
        self.spawn_process();
        self.wait_ready().await;
    }

    /// The node's whole log, for a failing assertion to print.
    pub fn logs(&self) -> String {
        let mut text = String::new();

        if let Ok(mut file) = File::open(self.spec.log_path()) {
            let _ = file.read_to_string(&mut text);
        }

        text
    }

    fn spawn_process(&mut self) {
        let root = &self.spec.root;

        fs::create_dir_all(root).expect("cannot create node scratch directory");
        fs::create_dir_all(self.spec.store_path()).expect("cannot create node store");

        fs::write(self.spec.config_path(), self.spec.to_toml()).expect("cannot write node config");

        // Both streams go to one file, appended across restarts so a
        // promotion test can read the whole deployment's story afterwards.
        let log = File::options()
            .create(true)
            .append(true)
            .open(self.spec.log_path())
            .expect("cannot open node log");

        let log_err = log.try_clone().expect("cannot clone node log handle");

        let child = Command::new(BINARY)
            .arg("-c")
            .arg(self.spec.config_path())
            // The developer's own RUST_LOG must not reach the node: the
            // config's log_level is what these tests set, and an inherited
            // env var would override it.
            .env_remove("RUST_LOG")
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn()
            .unwrap_or_else(|err| panic!("cannot spawn {}: {}", BINARY, err));

        self.child = Some(child);
    }

    /// Polls the node's public status route until it answers.
    ///
    /// `/v1/_health/status` rather than an authenticated route because it is
    /// the one that always answers `200` — including on a replica still
    /// bootstrapping, which is exactly the state a cold node starts in.
    async fn wait_ready(&mut self) {
        let deadline = Instant::now() + READY_TIMEOUT;
        let url = format!("{}/_health/status", self.spec.api_url());

        loop {
            // A node that died is never going to answer, and its log says
            // why — far more useful than a timeout twenty seconds later.
            if let Some(child) = self.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    panic!(
                        "node '{}' exited during startup with {}\n--- log ---\n{}",
                        self.spec.name,
                        status,
                        self.logs()
                    );
                }
            }

            if let Ok(response) = self.client.get(&url).send().await {
                if response.status().is_success() {
                    return;
                }
            }

            if Instant::now() >= deadline {
                panic!(
                    "node '{}' did not become ready within {:?}\n--- log ---\n{}",
                    self.spec.name,
                    READY_TIMEOUT,
                    self.logs()
                );
            }

            tokio::time::sleep(POLL).await;
        }
    }

    // -- HTTP surface ------------------------------------------------------

    async fn send(&self, request: reqwest::RequestBuilder) -> Reply {
        let response = request
            .header("Authorization", format!("Bearer {}", self.spec.api_key))
            .send()
            .await
            .unwrap_or_else(|err| {
                panic!(
                    "request to node '{}' failed: {}\n--- log ---\n{}",
                    self.spec.name,
                    err,
                    self.logs()
                )
            });

        let status = response.status();
        let text = response.text().await.unwrap_or_default();

        Reply { status, text }
    }

    pub async fn get(&self, path: &str) -> Reply {
        self.send(self.client.get(self.url(path))).await
    }

    pub async fn head(&self, path: &str) -> Reply {
        self.send(self.client.head(self.url(path))).await
    }

    pub async fn put(&self, path: &str, body: Value) -> Reply {
        self.send(self.client.put(self.url(path)).json(&body)).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Reply {
        self.send(self.client.post(self.url(path)).json(&body))
            .await
    }

    pub async fn delete(&self, path: &str, body: Value) -> Reply {
        self.send(self.client.delete(self.url(path)).json(&body))
            .await
    }

    /// A request carrying no credential, for the two public health routes.
    pub async fn get_unauthenticated(&self, path: &str) -> Reply {
        let response = self
            .client
            .get(self.url(path))
            .send()
            .await
            .unwrap_or_else(|err| panic!("unauthenticated request failed: {}", err));

        let status = response.status();
        let text = response.text().await.unwrap_or_default();

        Reply { status, text }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.spec.api_url(), path)
    }

    // -- Shorthands used across the tests ----------------------------------

    pub async fn write_file(&self, tenant_path: &str, content: &str) -> Reply {
        self.put(
            &format!("/{}", tenant_path.trim_start_matches('/')),
            json!({ "author": author(), "content": content }),
        )
        .await
    }

    pub async fn read_file(&self, tenant_path: &str) -> Reply {
        self.get(&format!("/{}", tenant_path.trim_start_matches('/')))
            .await
    }

    pub async fn health_status(&self) -> Value {
        self.get_unauthenticated("/_health/status").await.json()
    }

    pub async fn health_replication(&self) -> Value {
        self.get_unauthenticated("/_health/replication")
            .await
            .json()
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        // A panicking test takes its node's log with it — without this the
        // interesting half of an e2e failure dies with the TempDir.
        if std::thread::panicking() {
            eprintln!(
                "--- node '{}' log ---\n{}\n--- end ---",
                self.spec.name,
                self.logs()
            );
        }

        self.stop();
    }
}

/// Spawns a node that is **expected to fail**, and returns how it exited
/// along with everything it printed.
///
/// Every config and bind failure in `main` ends in `std::process::exit(1)`
/// after an `eprintln!`, so it can only be observed from outside the process.
/// That is the whole reason this target exists.
pub fn spawn_expecting_exit(root: &Path, config_text: &str) -> (i32, String) {
    fs::create_dir_all(root).expect("cannot create scratch directory");

    let config_path = root.join("config.toml");

    fs::write(&config_path, config_text).expect("cannot write config");

    spawn_with_config_path(&config_path)
}

/// Runs the binary against `config_path` to completion, returning its exit
/// code and everything it printed. The path need not exist — "cannot read
/// config file" is itself one of the exits worth asserting.
pub fn spawn_with_config_path(config_path: &Path) -> (i32, String) {
    let output = Command::new(BINARY)
        .arg("-c")
        .arg(config_path)
        .env_remove("RUST_LOG")
        .output()
        .expect("cannot spawn binary");

    let mut printed = String::from_utf8_lossy(&output.stderr).to_string();

    printed.push_str(&String::from_utf8_lossy(&output.stdout));

    (output.status.code().unwrap_or(-1), printed)
}

/// Polls `probe` until it answers `true`, panicking with `label` on timeout.
pub async fn wait_until<F, Fut>(label: &str, timeout: Duration, mut probe: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;

    loop {
        if probe().await {
            return;
        }

        if Instant::now() >= deadline {
            panic!("timed out after {:?} waiting for: {}", timeout, label);
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
