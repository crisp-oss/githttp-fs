// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Test harness: a real server on a real socket, and a stub hook receiver.
//!
//! Every integration test here drives the actual `build_router` output over
//! real HTTP rather than calling handlers directly. That is deliberate: the
//! API-key guard, the replica read-only guard, and the `/v1` nesting are all
//! *router* behaviour, and a test that bypassed the router would assert
//! nothing about them. The cost is one ephemeral port per test, which is
//! cheap enough that tests stay fully parallel.
//!
//! Each server gets its own `tempfile::TempDir` as `repos_path`, so tests
//! never share a repository store and cleanup is automatic.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::{net::TcpListener, sync::watch};

use crate::{
    build_replication_server, build_router, config::Config, replication::ReplicationIdentity,
    state::AppState,
};

/// How long a test waits for asynchronous work (hook delivery) before
/// failing. Generous, because it is only ever reached on a real failure —
/// the happy path polls at 5 ms and returns as soon as the work lands.
const WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_POLL: Duration = Duration::from_millis(5);

/// The replication secret every node in a test deployment shares. Distinct
/// from the API key on purpose: no caller of the product's API ever holds it.
pub const REPLICATION_SECRET: &str = "test-replication-secret";

/// Author object every write request needs. One helper so a change to the
/// shape is a one-line edit across the suite.
pub fn author() -> Value {
    json!({ "name": "Test Author", "email": "test@example.com" })
}

/// A response reduced to what assertions actually look at. Holds the body as
/// text so a test can assert on a non-JSON body (the `HEAD` routes answer
/// with none at all) without a second request shape.
pub struct TestResponse {
    pub status: StatusCode,
    pub text: String,
}

impl TestResponse {
    /// The body parsed as JSON. Panics with the raw body when it is not
    /// JSON, which turns "unexpected 500" into a readable failure instead of
    /// an opaque parse error.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or_else(|err| {
            panic!(
                "response body is not JSON ({}): status={} body={:?}",
                err, self.status, self.text
            )
        })
    }

    /// The `error` field of an error body — every `AppError` renders as
    /// `{ "error": "..." }`, so this is how a test asserts on the reason
    /// rather than only on the status code.
    pub fn error_message(&self) -> String {
        self.json()
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    /// Asserts the status, printing the body when it does not match — the
    /// body is where the reason is, and a bare `assert_eq!` on the status
    /// hides it.
    #[track_caller]
    pub fn expect_status(&self, expected: StatusCode) -> &Self {
        assert_eq!(
            self.status, expected,
            "unexpected status (body: {})",
            self.text
        );

        self
    }
}

/// A running githttp-fs node, its store, and a client holding its API key.
pub struct TestServer {
    pub base_url: String,
    pub repos_path: PathBuf,
    pub api_key: String,
    /// Base URL of this node's *replication* listener (`/_replication`),
    /// bound only when `[replication]` is configured. A replica's
    /// `master_url` points here, never at the content port. Peers
    /// authenticate against it with [`REPLICATION_SECRET`].
    pub replication_url: Option<String>,
    /// This node's own `AppState`, for the few tests that have to reach a
    /// background subsystem directly (the repository index, say) rather than
    /// through a route.
    pub state: AppState,
    client: reqwest::Client,
    /// Kept alive for the life of the server: dropping the last handle
    /// removes the store. Shared rather than owned so a test can restart a
    /// node over the store a previous one wrote — which is how the two-step
    /// `checkout_files_autoheal` procedure is exercised.
    store: Arc<TempDir>,
}

/// Builder for a node with non-default configuration. The plain
/// [`TestServer::start`] covers most tests; this covers the ones that need
/// hooks, an extension whitelist, or a replica role.
pub struct TestServerBuilder {
    hooks_url: Option<String>,
    hook_events: Vec<&'static str>,
    hook_retry_attempts: u32,
    hook_retry_backoff_ms: u64,
    hook_auth: Option<(String, String)>,
    allowed_extensions: Option<Vec<&'static str>>,
    batch_read_maximum_files: Option<usize>,
    replica: bool,
    replication_role: Option<&'static str>,
    master_url: Option<String>,
    node_id: Option<String>,
    poll_interval_secs: u64,
    checkout_files: Option<bool>,
    checkout_files_autoheal: Option<bool>,
    maintenance: bool,
    maintenance_delay_secs: u64,
    maintenance_maximum_packs: Option<usize>,
    store: Option<Arc<TempDir>>,
}

impl TestServerBuilder {
    fn new() -> Self {
        Self {
            hooks_url: None,
            hook_retry_attempts: 1,
            hook_retry_backoff_ms: 1,
            hook_auth: None,
            hook_events: vec![
                "file.created",
                "file.updated",
                "file.deleted",
                "file.moved",
                "order.updated",
                "order.deleted",
            ],
            allowed_extensions: None,
            batch_read_maximum_files: None,
            replica: false,
            replication_role: None,
            master_url: None,
            node_id: None,
            poll_interval_secs: 1,
            checkout_files: None,
            checkout_files_autoheal: None,
            maintenance: false,
            maintenance_delay_secs: 86_400,
            maintenance_maximum_packs: None,
            store: None,
        }
    }

    /// Enables background maintenance, with the given delay between a
    /// repository's first write and its pass. Off in tests by default: it
    /// would otherwise hold a timer per tenant for the life of the process.
    pub fn maintenance(mut self, delay_secs: u64) -> Self {
        self.maintenance = true;
        self.maintenance_delay_secs = delay_secs;

        self
    }

    /// Runs a repository's maintenance pass at once, rather than after the
    /// delay, once it holds this many packfiles.
    pub fn maximum_packs(mut self, maximum: usize) -> Self {
        self.maintenance_maximum_packs = Some(maximum);

        self
    }

    /// Makes this node a replication master: it binds a peer listener and
    /// serves `/_replication/*` to replicas.
    pub fn master(mut self) -> Self {
        self.replication_role = Some("master");

        self
    }

    /// Makes this node a replica following `master_replication_url` — the
    /// master's *replication* listener, as a real deployment configures it.
    pub fn replica_of(mut self, master_replication_url: &str) -> Self {
        self.replication_role = Some("replica");
        self.master_url = Some(master_replication_url.to_string());

        self
    }

    /// How this node names itself to its peers. Must be unique within a
    /// deployment — a master refuses a second stream for a node id already
    /// connected from another process.
    pub fn node_id(mut self, node_id: &str) -> Self {
        self.node_id = Some(node_id.to_string());

        self
    }

    /// How often this replica polls its master. The default of one second
    /// is what keeps convergence tests quick; a test that cares about the
    /// cadence a replica *reports* rather than how fast it converges sets
    /// its own.
    pub fn poll_interval_secs(mut self, secs: u64) -> Self {
        self.poll_interval_secs = secs;

        self
    }

    /// Whether this node keeps a working tree on disk.
    pub fn checkout_files(mut self, enabled: bool) -> Self {
        self.checkout_files = Some(enabled);

        self
    }

    /// Whether this node heals drifted working trees once at startup.
    pub fn checkout_files_autoheal(mut self, enabled: bool) -> Self {
        self.checkout_files_autoheal = Some(enabled);

        self
    }

    /// Starts over a store another server already wrote — a restart with a
    /// different config, over the same repositories.
    pub fn reuse_store(mut self, store: Arc<TempDir>) -> Self {
        self.store = Some(store);

        self
    }

    /// Delivers hooks to `url`, subscribing to every event kind unless
    /// [`hook_events`](Self::hook_events) narrows it.
    pub fn hooks(mut self, url: &str) -> Self {
        self.hooks_url = Some(url.to_string());

        self
    }

    /// Total delivery attempts per payload (the first try included), and
    /// the base delay its exponential backoff doubles from.
    pub fn hook_retries(mut self, attempts: u32, backoff_ms: u64) -> Self {
        self.hook_retry_attempts = attempts;
        self.hook_retry_backoff_ms = backoff_ms;

        self
    }

    /// Configures `[hooks.auth]`: a static header on every delivery.
    pub fn hook_auth(mut self, header: &str, value: &str) -> Self {
        self.hook_auth = Some((header.to_string(), value.to_string()));

        self
    }

    /// Narrows the `[hooks] events` subscription list.
    pub fn hook_events(mut self, events: &[&'static str]) -> Self {
        self.hook_events = events.to_vec();

        self
    }

    pub fn allowed_extensions(mut self, extensions: &[&'static str]) -> Self {
        self.allowed_extensions = Some(extensions.to_vec());

        self
    }

    pub fn batch_read_maximum_files(mut self, maximum: usize) -> Self {
        self.batch_read_maximum_files = Some(maximum);

        self
    }

    /// Makes this node a replica. It follows an unreachable master, which is
    /// exactly what the read-only and bootstrapping guards need: the node
    /// must refuse writes without ever succeeding at replication.
    pub fn replica(mut self) -> Self {
        self.replica = true;

        self
    }

    pub async fn start(self) -> TestServer {
        let store = match self.store {
            Some(store) => store,
            None => {
                Arc::new(tempfile::tempdir().expect("cannot create temporary repository store"))
            }
        };

        let repos_path = store.path().to_path_buf();

        let mut toml_text = format!(
            "[server]\n\
             host = \"127.0.0.1\"\n\
             port = 0\n\
             api_key = \"test-api-key\"\n\
             repos_path = {}\n",
            toml_string(&repos_path.to_string_lossy())
        );

        if let Some(enabled) = self.checkout_files {
            toml_text.push_str(&format!("checkout_files = {}\n", enabled));
        }

        if let Some(enabled) = self.checkout_files_autoheal {
            toml_text.push_str(&format!("checkout_files_autoheal = {}\n", enabled));
        }

        toml_text.push_str("\n[limits]\n");

        if let Some(extensions) = &self.allowed_extensions {
            toml_text.push_str(&format!(
                "allowed_extensions = [{}]\n",
                extensions
                    .iter()
                    .map(|extension| toml_string(extension))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        if let Some(maximum) = self.batch_read_maximum_files {
            toml_text.push_str(&format!("batch_read_maximum_files = {}\n", maximum));
        }

        if let Some(url) = &self.hooks_url {
            toml_text.push_str(&format!(
                "\n[hooks]\n\
                 url = {}\n\
                 events = [{}]\n\
                 retry_attempts = {}\n\
                 retry_backoff_ms = {}\n",
                toml_string(url),
                self.hook_events
                    .iter()
                    .map(|event| toml_string(event))
                    .collect::<Vec<_>>()
                    .join(", "),
                self.hook_retry_attempts,
                self.hook_retry_backoff_ms
            ));

            if let Some((header, value)) = &self.hook_auth {
                toml_text.push_str(&format!(
                    "\n[hooks.auth]\nheader = {}\nvalue = {}\n",
                    toml_string(header),
                    toml_string(value)
                ));
            }
        }

        // Maintenance is armed by every write and would otherwise hold a
        // timer per tenant for the life of the test process. Tests that want
        // it exercise `GitMaintenance` directly instead.
        toml_text.push_str(&format!(
            "\n[maintenance]\nenabled = {}\ndelay_secs = {}\n",
            self.maintenance, self.maintenance_delay_secs
        ));

        if let Some(maximum) = self.maintenance_maximum_packs {
            toml_text.push_str(&format!("maximum_packs = {}\n", maximum));
        }

        // `.replica()` is the degenerate case: a replica whose master is
        // unreachable, which is all the read-only and bootstrapping guards
        // need. `.master()` / `.replica_of()` build a real pair.
        let role = if self.replica {
            Some("replica")
        } else {
            self.replication_role
        };

        if let Some(role) = role {
            let node_id = self
                .node_id
                .clone()
                .unwrap_or_else(|| format!("test-{}", role));

            toml_text.push_str(&format!(
                "\n[replication]\nrole = {}\nsecret = {}\nnode_id = {}\n",
                toml_string(role),
                toml_string(REPLICATION_SECRET),
                toml_string(&node_id)
            ));

            if role == "replica" {
                let master_url = self
                    .master_url
                    .clone()
                    .unwrap_or_else(|| "http://127.0.0.1:1".to_string());

                toml_text.push_str(&format!(
                    "master_url = {}\npoll_interval_secs = {}\n",
                    toml_string(&master_url),
                    self.poll_interval_secs
                ));
            }

            // Never bound as configured — the peer listener below takes an
            // ephemeral port like the content one. The key still has to be
            // present and distinct from `server.port`, which validation
            // checks, so the config stays a realistic one.
            toml_text.push_str("port = 5356\n");
        }

        let config: Config = toml::from_str(&toml_text)
            .unwrap_or_else(|err| panic!("test config does not parse: {}\n{}", err, toml_text));

        config
            .validate()
            .unwrap_or_else(|errors| panic!("test config is invalid: {:?}", errors));

        let identity =
            ReplicationIdentity::load(&config).expect("cannot load test replication identity");

        let state = AppState::new(config, identity);

        // The startup heal pass, exactly as `main` runs it: a no-op unless
        // this node keeps files on disk and was asked to heal them.
        crate::checkout::spawn(state.clone());

        // The peer surface, on its own listener — two listeners, never one,
        // exactly as `main` binds them.
        let replication_url = if role.is_some() {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("cannot bind replication listener");

            let address = listener
                .local_addr()
                .expect("cannot read replication listener port");

            let replication_router = build_replication_server(state.clone());

            tokio::spawn(async move {
                let _outcome = axum::serve(listener, replication_router).await;
            });

            Some(format!("http://{}", address))
        } else {
            None
        };

        // Starts the follower when this node is a replica; inert otherwise.
        crate::replication::spawn(state.clone());

        let router = build_router(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("cannot bind test listener");

        let address = listener
            .local_addr()
            .expect("cannot read test listener port");

        tokio::spawn(async move {
            let _outcome = axum::serve(listener, router).await;
        });

        TestServer {
            base_url: format!("http://{}/v1", address),
            repos_path,
            api_key: "test-api-key".to_string(),
            replication_url,
            state,
            client: reqwest::Client::new(),
            store,
        }
    }
}

impl TestServer {
    /// A standalone node with no hooks, no whitelist, and default limits.
    pub async fn start() -> Self {
        TestServerBuilder::new().start().await
    }

    pub fn builder() -> TestServerBuilder {
        TestServerBuilder::new()
    }

    /// A handle on this server's store, for restarting a node over it.
    pub fn store(&self) -> Arc<TempDir> {
        self.store.clone()
    }

    /// On-disk location of one tenant's repository, for the few assertions
    /// that are about the working tree rather than the API.
    pub fn repo_path(&self, collection_id: &str, tenant_id: &str) -> PathBuf {
        self.repos_path.join(collection_id).join(tenant_id)
    }

    pub async fn get(&self, path: &str) -> TestResponse {
        self.request(reqwest::Method::GET, path, None, true).await
    }

    pub async fn head(&self, path: &str) -> TestResponse {
        self.request(reqwest::Method::HEAD, path, None, true).await
    }

    pub async fn put(&self, path: &str, body: Value) -> TestResponse {
        self.request(reqwest::Method::PUT, path, Some(body), true)
            .await
    }

    pub async fn post(&self, path: &str, body: Value) -> TestResponse {
        self.request(reqwest::Method::POST, path, Some(body), true)
            .await
    }

    pub async fn delete(&self, path: &str, body: Value) -> TestResponse {
        self.request(reqwest::Method::DELETE, path, Some(body), true)
            .await
    }

    /// Same as [`get`](Self::get) but without the `Authorization` header, for
    /// the routes that must answer anyway (and the ones that must not).
    pub async fn get_unauthenticated(&self, path: &str) -> TestResponse {
        self.request(reqwest::Method::GET, path, None, false).await
    }

    pub async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        authenticate: bool,
    ) -> TestResponse {
        let mut request = self
            .client
            .request(method, format!("{}{}", self.base_url, path));

        if authenticate {
            request = request.bearer_auth(&self.api_key);
        }

        if let Some(body) = body {
            request = request.json(&body);
        }

        let response = request.send().await.expect("test request failed");

        TestResponse {
            status: response.status(),
            text: response.text().await.unwrap_or_default(),
        }
    }

    // --- Convenience shorthands used across many tests -------------------

    /// Writes a file and asserts it committed, returning the commit sha.
    pub async fn write_file(&self, tenant_path: &str, file_path: &str, content: &str) -> String {
        let response = self
            .put(
                &format!("{}/files/{}", tenant_path, file_path),
                json!({ "author": author(), "content": content }),
            )
            .await;

        response.expect_status(StatusCode::OK);

        response.json()["commit_sha"]
            .as_str()
            .expect("write response carries no commit_sha")
            .to_string()
    }

    /// The content of a file, or `None` when it does not exist.
    pub async fn read_file(&self, tenant_path: &str, file_path: &str) -> Option<String> {
        let response = self
            .get(&format!("{}/files/{}", tenant_path, file_path))
            .await;

        if response.status == StatusCode::NOT_FOUND {
            return None;
        }

        response.expect_status(StatusCode::OK);

        Some(
            response.json()["content"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        )
    }

    /// Current HEAD sha, read from the commit list.
    pub async fn head_sha(&self, tenant_path: &str) -> String {
        let response = self
            .get(&format!("{}/commits?per_page=1", tenant_path))
            .await;

        response.expect_status(StatusCode::OK);

        response.json()["commits"][0]["sha"]
            .as_str()
            .expect("no commits in repository")
            .to_string()
    }
}

/// A stub webhook receiver: records every payload it is POSTed, in order,
/// and can be made to fail or to stall so the delivery path's failure and
/// concurrency behaviour is observable.
pub struct HookReceiver {
    pub url: String,
    deliveries: Arc<Mutex<Vec<Delivery>>>,
    /// How many further deliveries answer 500 before the receiver recovers.
    failures: Arc<AtomicUsize>,
    /// Which deliveries are held open rather than answered — what a slow or
    /// wedged receiver looks like from the server's side. `None` answers
    /// everything; `Some(tenant_id)` holds only that repository's, which is
    /// how the per-repository concurrency of the queues becomes observable.
    gate: watch::Sender<Option<String>>,
}

/// One delivery as the receiver saw it.
#[derive(Clone)]
pub struct Delivery {
    pub payload: Value,
    /// Value of the header `[hooks.auth]` configures, when one is set.
    pub auth_header: Option<String>,
    /// When the receiver was reached, for asserting on throttling.
    pub at: Instant,
}

#[derive(Clone)]
struct ReceiverState {
    deliveries: Arc<Mutex<Vec<Delivery>>>,
    failures: Arc<AtomicUsize>,
    gate: watch::Receiver<Option<String>>,
}

impl HookReceiver {
    pub async fn start() -> Self {
        let deliveries = Arc::new(Mutex::new(Vec::new()));
        let failures = Arc::new(AtomicUsize::new(0));
        let (gate, gate_receiver) = watch::channel(None);

        let router = Router::new()
            .route("/hook", post(receive_hook))
            .with_state(ReceiverState {
                deliveries: deliveries.clone(),
                failures: failures.clone(),
                gate: gate_receiver,
            });

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("cannot bind hook receiver");

        let address = listener.local_addr().expect("cannot read receiver port");

        tokio::spawn(async move {
            let _outcome = axum::serve(listener, router).await;
        });

        Self {
            url: format!("http://{}/hook", address),
            deliveries,
            failures,
            gate,
        }
    }

    /// Makes the next `count` deliveries answer `500`, after which the
    /// receiver recovers. Every attempt is still recorded, so a test can
    /// count retries.
    pub fn fail_next(&self, count: usize) {
        self.failures.store(count, Ordering::SeqCst);
    }

    /// Holds open every delivery whose payload names `tenant_id`, without
    /// answering — one wedged repository, while the rest stay responsive.
    pub fn stall_tenant(&self, tenant_id: &str) {
        self.gate
            .send(Some(tenant_id.to_string()))
            .expect("hook receiver gate closed");
    }

    /// Lets held deliveries complete, and stops holding new ones.
    pub fn resume(&self) {
        self.gate.send(None).expect("hook receiver gate closed");
    }

    pub fn deliveries(&self) -> Vec<Delivery> {
        self.deliveries
            .lock()
            .expect("hook receiver poisoned")
            .clone()
    }

    /// Everything received so far, in delivery order.
    pub fn events(&self) -> Vec<Value> {
        self.deliveries()
            .into_iter()
            .map(|delivery| delivery.payload)
            .collect()
    }

    /// The `event` field of everything received so far — the shorthand most
    /// ordering assertions want.
    pub fn event_names(&self) -> Vec<String> {
        self.events()
            .iter()
            .map(|payload| payload["event"].as_str().unwrap_or("<missing>").to_string())
            .collect()
    }

    /// Waits until at least `count` payloads have arrived, then returns them
    /// all. Panics on timeout, naming what did arrive.
    pub async fn wait_for(&self, count: usize) -> Vec<Value> {
        let deadline = Instant::now() + WAIT_TIMEOUT;

        loop {
            let events = self.events();

            if events.len() >= count {
                return events;
            }

            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {} hook(s); received {:?}",
                    count,
                    self.event_names()
                );
            }

            tokio::time::sleep(WAIT_POLL).await;
        }
    }

    /// Waits for `count` payloads and then asserts no further one arrives —
    /// the way a test states "and nothing else fired". The settle window is
    /// short because delivery is immediate on the happy path.
    pub async fn wait_for_exactly(&self, count: usize) -> Vec<Value> {
        let events = self.wait_for(count).await;

        tokio::time::sleep(Duration::from_millis(150)).await;

        let settled = self.events();

        assert_eq!(
            settled.len(),
            count,
            "expected exactly {} hook(s), received {:?}",
            count,
            self.event_names()
        );

        events
    }
}

async fn receive_hook(
    State(state): State<ReceiverState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> StatusCode {
    let held_tenant = payload["tenant_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    state
        .deliveries
        .lock()
        .expect("hook receiver poisoned")
        .push(Delivery {
            payload,
            auth_header: headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
            at: Instant::now(),
        });

    // Held open while this delivery's repository is stalled, so the server's
    // queue for it stays occupied and a test can watch another repository's
    // queue drain past it.
    let mut gate = state.gate.clone();

    loop {
        let stalled = gate
            .borrow_and_update()
            .as_deref()
            .map(|tenant_id| tenant_id == held_tenant)
            .unwrap_or(false);

        if !stalled {
            break;
        }

        if gate.changed().await.is_err() {
            break;
        }
    }

    // Fail a bounded number of times, so a test can count retries and still
    // see the delivery eventually succeed.
    if state
        .failures
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
            left.checked_sub(1)
        })
        .is_ok()
    {
        return StatusCode::INTERNAL_SERVER_ERROR;
    }

    StatusCode::OK
}

/// Quotes a value as a TOML basic string, escaping what TOML requires. Test
/// inputs are paths and URLs, but a temporary directory can hold anything the
/// OS allows, so the escaping is done properly rather than assumed away.
fn toml_string(raw: &str) -> String {
    let escaped = raw
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");

    format!("\"{}\"", escaped)
}
