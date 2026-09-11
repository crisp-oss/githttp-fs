// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Read-only replication: keeping replica nodes serving while the master is
//! unavailable.
//!
//! The problem this solves is narrow on purpose. Writes stay single-master —
//! two nodes accepting commits would fork two histories that this codebase
//! has no way to merge — so what replication buys is **read availability**:
//! when the master is down, replicas keep answering every read route with
//! the last state they pulled.
//!
//! ## Why replicate git objects rather than events
//!
//! Every read in this service is answered from HEAD's tree and the object
//! database (`git.rs` — "HEAD is authoritative; the working tree is a
//! courtesy"), and reads never take the tenant write lock. So a replica that
//! holds the same objects and the same HEAD serves *byte-identical* results
//! on every read route — listing, seek, count, order index, commits, batch
//! read — including routes that do not exist yet. Rebuilding state from the
//! webhook event stream instead would lose commit authorship and history,
//! and would need per-route work forever.
//!
//! ## The shape: pull is the truth, push is a hint
//!
//! A replica converges by **comparing its own on-disk HEAD shas against the
//! master's repository listing** and fetching a packfile for whatever
//! differs. That comparison is stateless and idempotent, which is the
//! property the whole design rests on:
//!
//! - **Notifications carry no data.** A notification says "this repository
//!   moved"; it never carries the change. Missing one — or missing a week of
//!   them while the replica was down — costs nothing, because the next
//!   reconcile computes exactly the same work list either way. That is why
//!   notifications may be dropped freely under backpressure and why nothing
//!   accumulates on the master while a replica is offline.
//! - **The replica's refs are its cursor.** There is no persisted sync
//!   position to checkpoint, and therefore none to be wrong after a crash.
//! - **Each repository applies atomically** (objects first, ref last — see
//!   [`crate::git::GitReplication::apply_pack`]), so an interrupted catch-up
//!   leaves every repository either fully at the old commit or fully at the
//!   new one. Restarting just re-runs the diff.
//!
//! Push exists purely for latency: the master broadcasts a notification the
//! moment a commit lands, so a replica is current in milliseconds instead of
//! within the poll interval. The poll interval is what bounds correctness.
//!
//! ## Why the notification stream is a streaming HTTP response
//!
//! Notifications flow master → replica, but the *connection* is dialled
//! replica → master: the replica holds open a `GET
//! /_replication/events` that never completes. That inversion is what
//! keeps replicas free of any inbound network exposure — no public URL, no
//! certificate, no firewall hole, and no replica list in the master's
//! config, so a replica joins by connecting. Holding the connection open
//! also makes liveness intrinsic in both directions, which is why there is
//! no circuit breaker here: a dead peer is a closed socket, not a retry
//! budget to exhaust.
//!
//! Spelling that channel as a long-lived HTTP response rather than a bespoke
//! TCP protocol keeps one listener, one credential, one TLS story and one
//! tracing layer — and leaves it debuggable with `curl -N`.
//!
//! ## Deliberate non-goals
//!
//! - **No automatic promotion.** Split-brain forks history irreparably;
//!   promoting a replica is a manual, fenced operation.
//! - **No hooks on a replica.** Hook delivery belongs to the node that
//!   accepted the commit; a replica firing them would duplicate every event.
//!   During a master outage no hooks fire at all, and downstream mirrors are
//!   repaired afterwards with `POST /batch/replay/hook`, which already
//!   exists for exactly that drift.
//! - **No working-tree checkout on a replica** (reads never consult it).

use dashmap::DashMap;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, Notify, Semaphore};
use tokio::time::{sleep, Duration};

use crate::config::{Config, ReplicationConfig};
use crate::git::{GitReplication, PackApply, RepositoryHead, RepositoryScan};
use crate::state::AppState;
use crate::util::run_blocking;
use crate::validate;

/// How many notifications the master buffers per connected replica before
/// that replica's slowest-consumer backlog is dropped. Dropping is safe and
/// intended: a replica told it lagged responds with a full reconcile, which
/// is strictly more thorough than the notifications it lost.
const NOTIFICATION_CHANNEL_CAPACITY: usize = 1024;

/// Gap between keep-alive frames on the notification stream. Keeps idle
/// connections alive through proxies and gives the replica a positive signal
/// that the master is still there between commits.
pub const EVENT_HEARTBEAT_SECS: u64 = 20;

/// A replica treats a stream with no bytes for this long as dead. Must be
/// comfortably above the heartbeat interval.
const EVENT_READ_TIMEOUT_SECS: u64 = 60;

/// Ceiling on the reconnect backoff after repeated failures.
const MAXIMUM_RECONNECT_BACKOFF_MS: u64 = 60_000;

const CONNECT_TIMEOUT_SECS: u64 = 10;
const STATE_TIMEOUT_SECS: u64 = 30;

/// Read timeout on a packfile download. Far longer than the state timeout on
/// purpose: the master emits no body bytes until it has *prepared* the pack
/// (walked the objects and delta-compressed them), and on a large tenant
/// that preparation alone can outlast a 30 s read timeout — which would make
/// exactly the repositories that need a full clone the ones that can never
/// get one. The connect timeout still catches a master that is down.
const PACK_READ_TIMEOUT_SECS: u64 = 600;

/// How often a node re-walks its repositories directory to refresh the
/// in-memory index. The index is kept current by every commit and deletion
/// this process makes, so the rescan is a safety net for anything that
/// happened to the disk behind its back, not the mechanism.
const INDEX_RESCAN_SECS: u64 = 600;

/// Where a node keeps its replication identity, under `repos_path`. A JSON
/// object so that later fields can be added without a migration; today it
/// holds the single key `identity`.
const IDENTITY_FILE: &str = ".replication.json";

/// Size of a freshly generated identity, before hex encoding.
const IDENTITY_BYTES: usize = 32;

/// Where the replication surface is mounted, and the single place the prefix
/// is written down — the routes are nested on it in `main`, and the follower
/// builds every request URL from it, so the two cannot drift apart.
///
/// **No version segment, deliberately.** A URL version forces every peer to
/// agree on the version before it can say anything, which is backwards for a
/// node-to-node protocol: what a peer actually needs is to state its version
/// *in the message* and have the other side dispatch on it. So the wire
/// carries [`PROTOCOL_VERSION`] — in every JSON body, in the stream's opening
/// frame, and as a header on the binary pack response — and a future version
/// is a polymorphic payload rather than a second URL tree to mount, route and
/// keep alive. The path stays stable forever, which also means an operator's
/// firewall and proxy rules never need revisiting for a protocol bump.
///
/// The `_` prefix marks it as internal plumbing rather than a resource anyone
/// should be calling by hand — and it lives on its own server and port
/// (`[replication] host`/`port`), not on the content API's, so it cannot be
/// exposed by a proxy that was only ever meant to publish `/v1`.
pub const URL_PREFIX: &str = "/_replication";

/// The replication wire protocol this build speaks.
///
/// Sent by both sides — a replica stamps it on every request, and every
/// response carries it back — so either end can recognise a peer it does not
/// fully understand and say so, instead of failing on a field it did not
/// expect. There is only one version today; the field exists so that adding a
/// second one is a branch on a value rather than a new URL space.
pub const PROTOCOL_VERSION: u32 = 1;

/// Header carrying [`PROTOCOL_VERSION`] on requests and on responses whose
/// body has nowhere to put it (the packfile, which is raw binary).
pub const PROTOCOL_HEADER: &str = "x-replication-protocol";

/// Response header carrying the commit a pack brings the caller up to. The
/// body is a binary stream with nowhere to put it, and the replica
/// fast-forwards to this sha once the import succeeds — it is the pack's
/// actual head, which may be past the sha the sync was queued for.
pub const HEAD_SHA_HEADER: &str = "x-replication-head-sha";

/// Directory (under `repos_path`) where in-flight packfile downloads land
/// before being imported. Dot-prefixed, so it can never collide with a
/// collection directory — `validate::collection_id` rejects a leading dot.
const INCOMING_DIRECTORY: &str = ".replication-incoming";

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// One frame of the notification stream, serialised as a single NDJSON line.
///
/// Internally tagged on `event`, matching the naming style of the webhook
/// payloads (`file.created`, `order.updated`). A repository event carries an
/// identity and, at most, a sha — never content. That is the whole point:
/// frames are disposable hints, so they must never be the only place a piece
/// of state exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum ReplicationEvent {
    /// First frame of every stream: which protocol this master speaks, and
    /// who it is.
    ///
    /// The version belongs here rather than on every frame because a stream is
    /// a *session* — one negotiation at the top, then frames — and it saves
    /// the replica a round trip for the master's identity, which it would
    /// otherwise only learn from the next health fetch.
    #[serde(rename = "hello")]
    Hello {
        protocol: u32,
        node_id: String,
        /// The data-set identity this node serves (see
        /// [`ReplicationIdentity`]). `null` only on a replica that has not
        /// paired with its own master yet.
        #[serde(default)]
        identity: Option<String>,
    },

    #[serde(rename = "heartbeat")]
    Heartbeat,

    #[serde(rename = "repository.updated")]
    RepositoryUpdated {
        collection_id: String,
        tenant_id: String,
        head_sha: String,
    },

    #[serde(rename = "repository.deleted")]
    RepositoryDeleted {
        collection_id: String,
        tenant_id: String,
    },
}

/// Response body of `GET /_replication/state`.
#[derive(Debug, Serialize, Deserialize)]
pub struct RepositoryListing {
    /// The protocol this body is written in. Defaulted on the way in so a
    /// reader never fails on a peer that predates the field.
    #[serde(default)]
    pub protocol: u32,
    /// The data-set identity of the node answering. A replica pins the first
    /// one it receives and refuses every later listing that states another
    /// — see [`ReplicationIdentity`].
    #[serde(default)]
    pub identity: Option<String>,
    pub repositories: Vec<RepositoryEntry>,
    /// Whether this listing describes the master's *entire* repository set.
    ///
    /// A replica may only infer deletions — repositories it holds and the
    /// master does not — from a complete listing. It is `false` whenever the
    /// scan behind it could not read a directory or open a repository, and
    /// it also exists so that adding pagination later cannot silently turn
    /// "page two" into "delete everything on page one".
    pub complete: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RepositoryEntry {
    pub collection_id: String,
    pub tenant_id: String,
    pub head_sha: String,
}

/// Headers a replica stamps on the replication requests it already makes, so
/// the node serving them can build a roster.
///
/// Headers rather than query parameters or a registration call because they
/// are pure *metadata about the caller* riding on requests that exist anyway:
/// no new route, no extra round trip, and a peer that sends none simply stays
/// anonymous — which keeps the zero-registration property intact. All of it
/// is self-asserted telemetry; the Bearer key is what grants access.
pub const NODE_ID_HEADER: &str = "x-replication-node-id";
pub const REPOSITORIES_HEADER: &str = "x-replication-repositories";
pub const PENDING_HEADER: &str = "x-replication-pending";

// ---------------------------------------------------------------------------
// Health — what a node can honestly say about the set it belongs to
// ---------------------------------------------------------------------------

/// The replication picture as one node sees it, served by both health routes.
///
/// One struct and one builder behind two doors: `GET /v1/replication` for the
/// operator (content API key) and `GET /_replication/health` for peers
/// (replication key). The audiences and credentials differ, the answer does
/// not — and the peer route is what lets a replica learn the roster at all,
/// since it holds the replication key and not necessarily the content one.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReplicationHealth {
    /// The protocol this body is written in.
    #[serde(default)]
    pub protocol: u32,
    /// The node answering this request.
    pub node: NodeHealth,
    /// The write node of this set, as far as the answering node knows.
    pub master: MasterHealth,
    /// Every replica known to follow the master.
    pub replicas: Vec<ReplicaPresence>,
    /// This node's own follower state — present only on a replica.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replica: Option<FollowerHealth>,
    /// When this answer was built.
    pub observed_at: i64,
    /// When `replicas` was last true. On a master that is `observed_at` — it
    /// watches those connections itself. On a replica it is when the master
    /// last told it, so a roster served while the master is down is visibly
    /// stale rather than quietly wrong. `null` when a replica has never
    /// reached its master.
    pub replicas_observed_at: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NodeHealth {
    pub node_id: String,
    /// `"master"`, `"replica"`, or `"standalone"` when `[replication]` is
    /// absent — reported rather than 404'd so one monitoring probe works
    /// against every node in a deployment, whatever its role.
    pub role: String,
    /// The data-set identity this node serves: generated on a master, pinned
    /// on a replica. `null` on a standalone node and on a replica that has
    /// not paired yet.
    pub identity: Option<String>,
    /// How many repositories this node holds right now.
    pub repositories: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MasterHealth {
    /// The master's own id. `null` on a replica that has not yet reached it.
    pub node_id: Option<String>,
    /// The URL this node follows. `null` on the master itself (it is the
    /// master) and on a standalone node.
    pub url: Option<String>,
    /// Whether the master is reachable from the answering node. Always
    /// `true` on the master: it is answering, so it is up.
    pub reachable: bool,
    /// Last successful contact with the master, as a unix timestamp.
    pub last_contact_at: Option<i64>,
    /// Why the last attempt failed, when it did. `null` while healthy.
    pub last_error: Option<String>,
}

/// One replica as the master sees it.
///
/// The split between what the master *observes* and what the replica
/// *reports* is deliberate and readable off the field names.
/// `stream_connected`, `connected_at`, `last_contact_at` and
/// `packs_delivered` are facts the master witnessed. `repositories` and
/// `pending_repositories` are numbers the replica volunteered — only the
/// replica can know how far behind it is, since lag is a property of its own
/// disk — and `reported_at` says when, so a stale claim stays visible instead
/// of being mistaken for current.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicaPresence {
    pub node_id: String,
    /// Whether this replica's notification stream is open *right now*.
    /// Presence is an open socket rather than a timeout heuristic — the same
    /// property that removes the need for a circuit breaker anywhere else in
    /// this feature.
    pub stream_connected: bool,
    pub connected_at: Option<i64>,
    /// Last request of any kind from this replica.
    pub last_contact_at: Option<i64>,
    pub packs_delivered: u64,
    /// Repositories the replica says it holds.
    pub repositories: Option<usize>,
    /// Repositories the replica says it knows are behind.
    pub pending_repositories: Option<usize>,
    pub reported_at: Option<i64>,
}

/// A replica's own view of how its following is going.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FollowerHealth {
    /// `"ready"` once this node holds content worth serving, else
    /// `"bootstrapping"`.
    pub state: String,
    pub stream_connected: bool,
    pub last_reconcile_at: Option<i64>,
    pub pending_repositories: usize,
    /// Times this node discarded a local repository and cloned it afresh
    /// because its history no longer descended from the master's. Each one
    /// destroyed a local copy, which is why it is counted where a dashboard
    /// can see it.
    pub reclones: u64,
}

// ---------------------------------------------------------------------------
// ReplicationIdentity — which data set a node serves
// ---------------------------------------------------------------------------

/// On-disk shape of `.replication.json`. Unknown keys survive a read and a
/// rewrite untouched, so the file can grow fields later without a migration.
#[derive(Debug, Default, Serialize, Deserialize)]
struct IdentityFile {
    identity: String,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

/// The identity of the *data set* a node serves, pinned so that a replica
/// can refuse to follow the wrong master.
///
/// The failure this guards against is human, not adversarial (the protocol
/// runs on trusted networks): a replica whose `master_url` was pointed at
/// the wrong deployment, or a master rebuilt from an empty disk, would
/// otherwise make every replica quietly delete its tenants and re-clone
/// whatever the new master holds. So a master generates a random identity
/// once and states it in every listing and at the top of every stream; a
/// replica stores the first one it sees and, from then on, refuses to sync
/// with anything stating another — loudly, and until an operator deletes
/// the file, which is the deliberate cost of a change that is never
/// supposed to happen.
///
/// It is deliberately *not* `node_id`. That is an operator-chosen label
/// (defaulting to `host:port`) which a rebuilt master presents unchanged,
/// so it cannot catch the very case that matters. A generated value stored
/// beside the repositories is wiped together with them.
///
/// One file, one shape, every role: a master holds the identity it
/// generated, a replica the one it pinned, and a replica also serves its
/// pinned one to anything chained below it. That is what lets a replica be
/// promoted by flipping `role` without re-pairing its followers — and what
/// makes a demoted master refuse a rebuilt deployment until told otherwise.
pub struct ReplicationIdentity {
    path: PathBuf,
    identity: Mutex<Option<String>>,
}

impl ReplicationIdentity {
    /// Reads `.replication.json`, generating it on a master that has none.
    ///
    /// A standalone node reads nothing and writes nothing: the file exists
    /// only where `[replication]` does. A file that is present but cannot
    /// be parsed or holds an invalid identity is an error rather than a
    /// regeneration — on a master, regenerating would fork every replica
    /// off it; on a replica, it would silently drop the guard.
    pub fn load(config: &Config) -> Result<Self, String> {
        let path = config.server.repos_path.join(IDENTITY_FILE);

        let Some(replication) = &config.replication else {
            return Ok(Self {
                path,
                identity: Mutex::new(None),
            });
        };

        let identity = match (Self::read_file(&path)?, replication.is_replica()) {
            (Some(file), _) => {
                tracing::info!(identity = %file.identity, path = %path.display(), "replication identity loaded");

                Some(file.identity)
            }
            (None, true) => {
                tracing::info!(path = %path.display(), "no replication identity pinned yet, will pin the master's on first reconcile");

                None
            }
            (None, false) => {
                let generated = Self::generate()?;

                Self::write_file(&path, &generated, serde_json::Map::new())?;

                tracing::info!(identity = %generated, path = %path.display(), "replication identity generated");

                Some(generated)
            }
        };

        Ok(Self {
            path,
            identity: Mutex::new(identity),
        })
    }

    /// The identity this node serves, if it has one.
    pub fn current(&self) -> Option<String> {
        self.identity
            .lock()
            .ok()
            .and_then(|identity| identity.clone())
    }

    /// Where the identity is stored — named in the mismatch error so an
    /// operator knows exactly what to delete.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stores the master's identity on a replica that has none yet. Written
    /// before it is adopted in memory, so a replica that crashes in between
    /// simply pins again on its next reconcile.
    pub fn pin(&self, identity: &str) -> Result<(), String> {
        validate::replication_identity(identity).map_err(|err| err.to_string())?;

        // Carry over anything a later version may have stored beside it.
        let extra = Self::read_file(&self.path)?
            .map(|file| file.extra)
            .unwrap_or_default();

        Self::write_file(&self.path, identity, extra)?;

        if let Ok(mut current) = self.identity.lock() {
            *current = Some(identity.to_string());
        }

        tracing::info!(identity = %identity, path = %self.path.display(), "replication identity pinned");

        Ok(())
    }

    fn read_file(path: &Path) -> Result<Option<IdentityFile>, String> {
        let raw = match std::fs::read(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(format!("cannot read {}: {}", path.display(), err)),
        };

        let file: IdentityFile = serde_json::from_slice(&raw)
            .map_err(|err| format!("cannot parse {}: {}", path.display(), err))?;

        validate::replication_identity(&file.identity)
            .map_err(|err| format!("{} holds an invalid identity: {}", path.display(), err))?;

        Ok(Some(file))
    }

    /// Writes the file through a temporary sibling and a rename, so a crash
    /// mid-write leaves either the old file or the new one, never a torn one.
    fn write_file(
        path: &Path,
        identity: &str,
        extra: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), String> {
        let file = IdentityFile {
            identity: identity.to_string(),
            extra,
        };

        let encoded = serde_json::to_vec_pretty(&file)
            .map_err(|err| format!("cannot encode {}: {}", path.display(), err))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create {}: {}", parent.display(), err))?;
        }

        let temporary = path.with_extension("json.tmp");

        std::fs::write(&temporary, encoded)
            .map_err(|err| format!("cannot write {}: {}", temporary.display(), err))?;

        std::fs::rename(&temporary, path)
            .map_err(|err| format!("cannot move {} into place: {}", temporary.display(), err))
    }

    fn generate() -> Result<String, String> {
        let mut bytes = [0_u8; IDENTITY_BYTES];

        getrandom::fill(&mut bytes)
            .map_err(|err| format!("cannot generate replication identity: {}", err))?;

        Ok(bytes.iter().map(|byte| format!("{:02x}", byte)).collect())
    }
}

// ---------------------------------------------------------------------------
// RepositoryIndex — every repository this node holds, without a disk walk
// ---------------------------------------------------------------------------

/// One indexed repository, plus when the index learned its head.
struct IndexEntry {
    head: RepositoryHead,
    /// Index sequence number at which this entry was last set by a live
    /// announcement; `0` when it came from a disk scan. Lets a scan that
    /// started before an announcement lose to it, rather than overwrite it.
    announced_at: u64,
}

struct IndexInner {
    heads: HashMap<String, IndexEntry>,
    complete: bool,
    scanned: bool,
    sequence: u64,
}

/// The repository listing, served from memory.
///
/// Walking `repos_path` costs one `Repository::open` per tenant, and the
/// listing is asked for by every replica on every poll, by every health
/// probe, and by the replica's own reconcile — so it is computed once, at
/// first use, and kept current from then on by the same notifier every
/// commit and deletion already goes through. A periodic rescan covers
/// anything that reached the disk behind this process's back.
///
/// **Merging a scan with live updates.** A scan takes seconds on a large
/// node and commits keep landing meanwhile. Each live update is stamped with
/// a sequence number; a scan remembers the sequence at which it *started*
/// and, when it finishes, yields to any entry announced after that point.
/// So a scan can never roll a repository back to the head it had when the
/// walk passed its directory.
pub struct RepositoryIndex {
    repos_path: PathBuf,
    inner: Mutex<IndexInner>,
}

impl RepositoryIndex {
    pub fn new(config: &Config) -> Self {
        Self {
            repos_path: config.server.repos_path.clone(),
            inner: Mutex::new(IndexInner {
                heads: HashMap::new(),
                complete: false,
                scanned: false,
                sequence: 0,
            }),
        }
    }

    fn key(collection_id: &str, tenant_id: &str) -> String {
        format!("{}/{}", collection_id, tenant_id)
    }

    /// Records that a repository is at `head_sha` (creating its entry on the
    /// first commit to a new tenant).
    pub fn record_head(&self, collection_id: &str, tenant_id: &str, head_sha: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };

        inner.sequence += 1;

        let announced_at = inner.sequence;

        inner.heads.insert(
            Self::key(collection_id, tenant_id),
            IndexEntry {
                head: RepositoryHead {
                    collection_id: collection_id.to_string(),
                    tenant_id: tenant_id.to_string(),
                    head_sha: head_sha.to_string(),
                },
                announced_at,
            },
        );
    }

    pub fn remove(&self, collection_id: &str, tenant_id: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };

        inner.sequence += 1;
        inner.heads.remove(&Self::key(collection_id, tenant_id));
    }

    /// The current listing, sorted by identity. **Blocking**: runs the
    /// initial scan on first use, and re-runs it while the last one was
    /// incomplete — so a node whose disk is misbehaving keeps probing it
    /// rather than serving an incomplete listing until the next scheduled
    /// rescan. Call from the blocking pool.
    pub fn snapshot(&self) -> RepositoryScan {
        let needs_scan = self
            .inner
            .lock()
            .map(|inner| !inner.scanned || !inner.complete)
            .unwrap_or(true);

        if needs_scan {
            self.rescan();
        }

        let Ok(inner) = self.inner.lock() else {
            return RepositoryScan::default();
        };

        let mut repositories: Vec<RepositoryHead> = inner
            .heads
            .values()
            .map(|entry| RepositoryHead {
                collection_id: entry.head.collection_id.clone(),
                tenant_id: entry.head.tenant_id.clone(),
                head_sha: entry.head.head_sha.clone(),
            })
            .collect();

        repositories.sort_by(|left, right| {
            (&left.collection_id, &left.tenant_id).cmp(&(&right.collection_id, &right.tenant_id))
        });

        RepositoryScan {
            repositories,
            complete: inner.complete,
        }
    }

    /// Walks the disk and folds the result into the index. **Blocking.**
    pub fn rescan(&self) {
        let started_at = match self.inner.lock() {
            Ok(inner) => inner.sequence,
            Err(_) => return,
        };

        let scan = GitReplication::list_repositories(&self.repos_path);

        let Ok(mut inner) = self.inner.lock() else {
            return;
        };

        let mut scanned: HashMap<String, RepositoryHead> = scan
            .repositories
            .into_iter()
            .map(|head| (Self::key(&head.collection_id, &head.tenant_id), head))
            .collect();

        // Entries the scan did not see are gone — unless they were announced
        // after the scan started, in which case the scan simply passed their
        // directory too early. Deletions are only trusted from a complete
        // scan, for the same reason a replica only trusts them from a
        // complete listing.
        if scan.complete {
            inner
                .heads
                .retain(|key, entry| scanned.contains_key(key) || entry.announced_at > started_at);
        }

        for (key, head) in scanned.drain() {
            let announced_since = inner
                .heads
                .get(&key)
                .map(|entry| entry.announced_at > started_at)
                .unwrap_or(false);

            if announced_since {
                continue;
            }

            inner.heads.insert(
                key,
                IndexEntry {
                    head,
                    announced_at: 0,
                },
            );
        }

        inner.complete = scan.complete;
        inner.scanned = true;

        tracing::debug!(
            repositories = inner.heads.len(),
            complete = inner.complete,
            "repository index rescanned"
        );
    }
}

/// `url` with any embedded credentials removed, for log lines and health
/// bodies. Nothing here sends credentials that way, but an operator might.
pub fn redact_url(raw: &str) -> String {
    match reqwest::Url::parse(raw) {
        Ok(mut url) => {
            if !url.username().is_empty() || url.password().is_some() {
                let _ = url.set_username("");
                let _ = url.set_password(None);
            }

            url.to_string().trim_end_matches('/').to_string()
        }
        Err(_) => "<invalid url>".to_string(),
    }
}

// ---------------------------------------------------------------------------
// ReplicaRegistry — the master side of the roster
// ---------------------------------------------------------------------------

/// Tracks the replicas that have introduced themselves.
///
/// Rows are **kept after a replica disconnects**, flagged
/// `stream_connected: false` with a `last_contact_at`. That is the row an
/// operator most needs — "replica-2 was last here three hours ago" — and
/// dropping it would make a dead replica indistinguishable from one that was
/// never configured. Growth is bounded by the number of distinct node ids,
/// which is why the default id is deterministic across restarts.
pub struct ReplicaRegistry {
    replicas: DashMap<String, ReplicaPresence>,
}

impl ReplicaRegistry {
    pub fn new() -> Self {
        Self {
            replicas: DashMap::new(),
        }
    }

    /// Records a request from a replica, creating its row on first sight.
    /// `repositories` and `pending` are whatever it volunteered this time.
    pub fn note_request(&self, node_id: &str, repositories: Option<usize>, pending: Option<usize>) {
        let now = chrono::Utc::now().timestamp();
        let mut presence = self.entry(node_id);

        presence.last_contact_at = Some(now);

        // Only stamp `reported_at` when something was actually reported, so
        // the timestamp always describes the numbers sitting next to it.
        if repositories.is_some() || pending.is_some() {
            presence.repositories = repositories;
            presence.pending_repositories = pending;
            presence.reported_at = Some(now);
        }
    }

    pub fn note_pack_delivered(&self, node_id: &str) {
        let now = chrono::Utc::now().timestamp();
        let mut presence = self.entry(node_id);

        presence.packs_delivered += 1;
        presence.last_contact_at = Some(now);
    }

    pub fn stream_opened(&self, node_id: &str) {
        let now = chrono::Utc::now().timestamp();
        let mut presence = self.entry(node_id);

        presence.stream_connected = true;
        presence.connected_at = Some(now);
        presence.last_contact_at = Some(now);
    }

    pub fn stream_closed(&self, node_id: &str) {
        let mut presence = self.entry(node_id);

        presence.stream_connected = false;
        presence.last_contact_at = Some(chrono::Utc::now().timestamp());
    }

    /// Every known replica, most recently seen first.
    pub fn roster(&self) -> Vec<ReplicaPresence> {
        let mut roster: Vec<ReplicaPresence> = self
            .replicas
            .iter()
            .map(|entry| entry.value().clone())
            .collect();

        roster.sort_by(|left, right| {
            right
                .last_contact_at
                .cmp(&left.last_contact_at)
                .then_with(|| left.node_id.cmp(&right.node_id))
        });

        roster
    }

    fn entry(&self, node_id: &str) -> dashmap::mapref::one::RefMut<'_, String, ReplicaPresence> {
        self.replicas
            .entry(node_id.to_string())
            .or_insert_with(|| ReplicaPresence {
                node_id: node_id.to_string(),
                stream_connected: false,
                connected_at: None,
                last_contact_at: None,
                packs_delivered: 0,
                repositories: None,
                pending_repositories: None,
                reported_at: None,
            })
    }
}

impl Default for ReplicaRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ReplicationNotifier — the master side of the hint channel
// ---------------------------------------------------------------------------

/// Fan-out of commit notifications to every connected replica.
///
/// A `broadcast` channel is exactly the right primitive here because its
/// failure mode is the one this design wants: a subscriber that cannot keep
/// up is told it lagged and loses frames, rather than applying backpressure
/// to the committing writer. Replication must never slow down or block a
/// write — and a lagging replica has a strictly better repair available to
/// it (a full reconcile) than the frames it dropped.
pub struct ReplicationNotifier {
    sender: broadcast::Sender<ReplicationEvent>,
    enabled: bool,
    /// Updated on every announcement, whatever the role — the index serves
    /// the health route on a standalone node too, and this is the one place
    /// every commit and deletion already passes through.
    index: Arc<RepositoryIndex>,
}

impl ReplicationNotifier {
    pub fn new(config: &Config, index: Arc<RepositoryIndex>) -> Self {
        let (sender, _receiver) = broadcast::channel(NOTIFICATION_CHANNEL_CAPACITY);

        Self {
            sender,
            enabled: config.replication.is_some(),
            index,
        }
    }

    /// Announces that a repository moved to a new commit.
    ///
    /// Called from every write handler right after the hook job is enqueued,
    /// while the tenant write lock is still held. Unlike the hook queue this
    /// carries no ordering guarantee and needs none: a replica reacts by
    /// asking what the current state is, so the only thing a stale or
    /// out-of-order frame can cause is a redundant no-op fetch.
    pub fn repository_updated(&self, collection_id: &str, tenant_id: &str, head_sha: &str) {
        self.index.record_head(collection_id, tenant_id, head_sha);

        self.publish(ReplicationEvent::RepositoryUpdated {
            collection_id: collection_id.to_string(),
            tenant_id: tenant_id.to_string(),
            head_sha: head_sha.to_string(),
        });
    }

    /// Announces that a repository was deleted.
    pub fn repository_deleted(&self, collection_id: &str, tenant_id: &str) {
        self.index.remove(collection_id, tenant_id);

        self.publish(ReplicationEvent::RepositoryDeleted {
            collection_id: collection_id.to_string(),
            tenant_id: tenant_id.to_string(),
        });
    }

    fn publish(&self, event: ReplicationEvent) {
        if !self.enabled {
            return;
        }

        // An error here means no replica is currently subscribed, which is
        // the normal state of a master nobody is following — not a problem.
        let _ = self.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ReplicationEvent> {
        self.sender.subscribe()
    }

    pub fn connected_replicas(&self) -> usize {
        self.sender.receiver_count()
    }
}

// ---------------------------------------------------------------------------
// ReplicaStatus — what a replica reports about itself
// ---------------------------------------------------------------------------

/// Live status of the follower, shared with the API-key ping route and the
/// read-only guard middleware.
///
/// `bootstrapped` carries the one policy decision worth stating plainly: a
/// **cold** replica (one holding no repositories at all) refuses traffic
/// until its first reconcile completes, because an empty repository is not
/// stale — it is wrong, and a 404 for content that exists is worse than a
/// 503. A **warm** replica that merely restarted starts serving immediately,
/// because content from an hour ago beats no content at all.
pub struct ReplicaStatus {
    is_replica: bool,
    bootstrapped: AtomicBool,
    stream_connected: AtomicBool,
    last_reconcile_unix: AtomicI64,
    pending_repositories: AtomicUsize,
    repositories: AtomicUsize,
    /// Whether the master last stated the identity this replica is pinned
    /// to. Nothing is pulled while this is false: a replica that cannot vouch
    /// for *who* it is following must not apply what that node hands out.
    identity_verified: AtomicBool,
    reclones: AtomicU64,
    /// What this node last learned about its master, and how that went.
    ///
    /// A replica caches the master's roster rather than fetching it on demand
    /// precisely because the case this whole feature exists for is a master
    /// that is *down*: a health route that proxied upstream would fail
    /// exactly when it is most needed. Cached-with-a-timestamp answers
    /// honestly instead — here is the last true picture, and here is when it
    /// was true.
    master_view: Mutex<MasterView>,
}

/// The master's own picture, as this replica last received it.
#[derive(Default)]
struct MasterView {
    node_id: Option<String>,
    replicas: Vec<ReplicaPresence>,
    observed_at: Option<i64>,
    last_contact_at: Option<i64>,
    last_error: Option<String>,
}

impl ReplicaStatus {
    pub fn new(config: &Config) -> Self {
        let is_replica = config
            .replication
            .as_ref()
            .map(ReplicationConfig::is_replica)
            .unwrap_or(false);

        Self {
            is_replica,
            // A node that is not a replica is trivially "bootstrapped": the
            // guard must never gate a standalone server or a master.
            bootstrapped: AtomicBool::new(!is_replica),
            stream_connected: AtomicBool::new(false),
            last_reconcile_unix: AtomicI64::new(0),
            pending_repositories: AtomicUsize::new(0),
            repositories: AtomicUsize::new(0),
            identity_verified: AtomicBool::new(false),
            reclones: AtomicU64::new(0),
            master_view: Mutex::new(MasterView::default()),
        }
    }

    pub fn set_identity_verified(&self, verified: bool) {
        self.identity_verified.store(verified, Ordering::Relaxed);
    }

    pub fn identity_verified(&self) -> bool {
        self.identity_verified.load(Ordering::Relaxed)
    }

    pub fn note_reclone(&self) {
        self.reclones.fetch_add(1, Ordering::Relaxed);
    }

    pub fn reclones(&self) -> u64 {
        self.reclones.load(Ordering::Relaxed)
    }

    pub fn is_replica(&self) -> bool {
        self.is_replica
    }

    pub fn is_bootstrapped(&self) -> bool {
        self.bootstrapped.load(Ordering::Relaxed)
    }

    pub fn mark_bootstrapped(&self) {
        if !self.bootstrapped.swap(true, Ordering::Relaxed) {
            tracing::info!("replica bootstrapped, now serving reads");
        }
    }

    pub fn set_stream_connected(&self, connected: bool) {
        self.stream_connected.store(connected, Ordering::Relaxed);
    }

    pub fn stream_connected(&self) -> bool {
        self.stream_connected.load(Ordering::Relaxed)
    }

    pub fn set_last_reconcile(&self, unix_seconds: i64) {
        self.last_reconcile_unix
            .store(unix_seconds, Ordering::Relaxed);
    }

    pub fn last_reconcile(&self) -> Option<i64> {
        match self.last_reconcile_unix.load(Ordering::Relaxed) {
            0 => None,
            seconds => Some(seconds),
        }
    }

    pub fn set_pending(&self, pending: usize) {
        self.pending_repositories.store(pending, Ordering::Relaxed);
    }

    pub fn pending(&self) -> usize {
        self.pending_repositories.load(Ordering::Relaxed)
    }

    pub fn set_repositories(&self, repositories: usize) {
        self.repositories.store(repositories, Ordering::Relaxed);
    }

    pub fn repositories(&self) -> usize {
        self.repositories.load(Ordering::Relaxed)
    }

    /// Records a successful health fetch, replacing the cached roster.
    pub fn record_master_health(&self, node_id: Option<String>, replicas: Vec<ReplicaPresence>) {
        let now = chrono::Utc::now().timestamp();

        if let Ok(mut view) = self.master_view.lock() {
            view.node_id = node_id;
            view.replicas = replicas;
            view.observed_at = Some(now);
            view.last_contact_at = Some(now);
            view.last_error = None;
        }
    }

    /// Records the master's identity, learned from the stream's hello frame
    /// before any health fetch has happened. Leaves the cached roster alone —
    /// this says who the master is, not what it sees.
    pub fn record_master_identity(&self, node_id: String) {
        if let Ok(mut view) = self.master_view.lock() {
            view.node_id = Some(node_id);
            view.last_contact_at = Some(chrono::Utc::now().timestamp());
            view.last_error = None;
        }
    }

    /// Records contact with the master that carried no roster (a successful
    /// reconcile). The cached roster is deliberately left alone — it is the
    /// last true one, and its own `observed_at` already says how old it is.
    pub fn record_master_contact(&self) {
        if let Ok(mut view) = self.master_view.lock() {
            view.last_contact_at = Some(chrono::Utc::now().timestamp());
            view.last_error = None;
        }
    }

    pub fn record_master_error(&self, error: String) {
        if let Ok(mut view) = self.master_view.lock() {
            view.last_error = Some(error);
        }
    }

    /// A snapshot of the cached master view: its id, its roster, when that
    /// roster was learned, last contact, and the last error.
    #[allow(clippy::type_complexity)]
    pub fn master_snapshot(
        &self,
    ) -> (
        Option<String>,
        Vec<ReplicaPresence>,
        Option<i64>,
        Option<i64>,
        Option<String>,
    ) {
        match self.master_view.lock() {
            Ok(view) => (
                view.node_id.clone(),
                view.replicas.clone(),
                view.observed_at,
                view.last_contact_at,
                view.last_error.clone(),
            ),
            Err(_) => (None, Vec::new(), None, None, None),
        }
    }
}

// ---------------------------------------------------------------------------
// ReplicaFollower — the replica side
// ---------------------------------------------------------------------------

/// What one drain of the stale set achieved.
///
/// `succeeded` is the load-bearing half: a failed sync puts itself back on
/// the pending set, so "the set is non-empty" says nothing about whether the
/// pass accomplished anything — and only real progress justifies going round
/// again with no pause.
#[derive(Debug, Default)]
struct SyncPass {
    attempted: usize,
    succeeded: usize,
}

/// A repository this replica knows it needs to look at.
#[derive(Debug, Clone)]
enum SyncTarget {
    /// The master says this repository is at `head_sha`.
    At { head_sha: String },
    /// The master no longer holds this repository.
    Deleted,
}

/// Pulls repositories from a master and keeps this node's copies current.
///
/// Two tasks feed one worker:
///
/// - the **event stream task** holds a long-lived connection open and marks
///   repositories stale as notifications arrive,
/// - the **worker** drains that stale set, and on every poll interval (and
///   on every stream reconnect) first runs a full reconcile against the
///   master's listing, which is what catches everything the stream missed.
pub struct ReplicaFollower {
    state: AppState,
    replication: ReplicationConfig,
    master_url: String,
    /// `master_url` with any embedded credentials stripped, for log lines.
    master_url_display: String,
    /// How this node names itself upstream, so the master's roster shows a
    /// name rather than an anonymous row.
    node_id: String,
    /// Client for the state and health requests: small JSON answers, so a
    /// short read timeout is right.
    transfer_client: Client,
    /// Client for pack downloads. No overall timeout, and a long read
    /// timeout (`PACK_READ_TIMEOUT_SECS`): a full clone of a large tenant is
    /// a legitimately long response whose first byte can itself take a
    /// while, and cutting it off at the state timeout would make big
    /// repositories permanently unsyncable.
    pack_client: Client,
    /// Client for the notification stream, whose read timeout is tuned to
    /// the heartbeat rather than to a transfer.
    stream_client: Client,
    /// Repositories known to need a look, keyed `"collection_id/tenant_id"`.
    pending: DashMap<String, (String, String, SyncTarget)>,
    /// Wakes the worker when the pending set gains an entry.
    wake: Arc<Notify>,
    /// Set when something (startup, the poll timer, a stream reconnect, a
    /// dropped-frame notice) means the pending set can no longer be trusted
    /// to be the whole story.
    full_reconcile_requested: AtomicBool,
    /// Distinguishes concurrent downloads of the same repository, so their
    /// temporary files cannot collide.
    download_sequence: AtomicU64,
}

/// Spawns the follower for this node, if it is a replica.
///
/// Returns immediately; all work happens on background tasks. A failure to
/// reach the master is never fatal — the node keeps serving whatever it
/// already holds, which is the entire point of the feature.
pub fn spawn(state: AppState) {
    let Some(replication) = state.config.replication.clone() else {
        return;
    };

    // Every replicating node keeps its repository index fresh against the
    // disk on a slow cadence, master and replica alike. The index is
    // maintained live by the notifier; this catches whatever bypassed it.
    let index = state.repository_index.clone();

    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(INDEX_RESCAN_SECS)).await;

            let index = index.clone();

            if let Err(err) = run_blocking(move || {
                index.rescan();

                Ok(())
            })
            .await
            {
                tracing::warn!(err = %err, "repository index rescan failed");
            }
        }
    });

    if !replication.is_replica() {
        tracing::info!("replication role is master, serving the replication surface");

        return;
    }

    let Some(master_url) = replication.master_url.clone() else {
        return;
    };

    let master_url = master_url.trim_end_matches('/').to_string();

    let follower = match ReplicaFollower::new(state, replication, master_url) {
        Ok(follower) => Arc::new(follower),
        Err(err) => {
            tracing::error!(err = %err, "failed to start replication follower");

            return;
        }
    };

    tokio::spawn(follower.clone().run_event_stream());
    tokio::spawn(follower.run_worker());
}

impl ReplicaFollower {
    fn new(
        state: AppState,
        replication: ReplicationConfig,
        master_url: String,
    ) -> Result<Self, reqwest::Error> {
        let node_id = replication.node_id(&state.config.server);

        let transfer_client = Client::builder()
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .read_timeout(Duration::from_secs(STATE_TIMEOUT_SECS))
            .build()?;

        let pack_client = Client::builder()
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .read_timeout(Duration::from_secs(PACK_READ_TIMEOUT_SECS))
            .build()?;

        let stream_client = Client::builder()
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .read_timeout(Duration::from_secs(EVENT_READ_TIMEOUT_SECS))
            .build()?;

        let master_url_display = redact_url(&master_url);

        Ok(Self {
            state,
            replication,
            master_url,
            master_url_display,
            node_id,
            transfer_client,
            pack_client,
            stream_client,
            pending: DashMap::new(),
            wake: Arc::new(Notify::new()),
            full_reconcile_requested: AtomicBool::new(true),
            download_sequence: AtomicU64::new(0),
        })
    }

    // -----------------------------------------------------------------------
    // Worker
    // -----------------------------------------------------------------------

    /// The convergence loop. Runs a full reconcile whenever one is
    /// requested, drains whatever is stale, and otherwise sleeps until the
    /// poll interval elapses or a notification arrives.
    async fn run_worker(self: Arc<Self>) {
        // A replica that already holds repositories is serving usable — if
        // stale — content, so it takes traffic immediately rather than
        // blacking out for the length of a catch-up. A genuinely cold one
        // stays gated until its first catch-up has actually landed content.
        let index = self.state.repository_index.clone();

        let already_holds_repositories =
            run_blocking(move || Ok(!index.snapshot().repositories.is_empty()))
                .await
                .unwrap_or(false);

        if already_holds_repositories {
            tracing::info!(
                "replica restarted with existing repositories, serving while it catches up"
            );

            self.state.replica_status.mark_bootstrapped();
        } else {
            tracing::info!("cold replica, holding traffic until the first catch-up completes");
        }

        let mut reconciled_once = false;
        let mut stalled_passes = 0_u32;

        loop {
            if self.full_reconcile_requested.swap(false, Ordering::SeqCst) {
                match self.reconcile().await {
                    Ok(()) => reconciled_once = true,
                    Err(err) => {
                        tracing::warn!(err = %err, "replication reconcile failed, will retry");

                        self.state.replica_status.record_master_error(err.clone());

                        // Put the request back so the next pass retries it
                        // rather than waiting for the poll timer to come round.
                        self.full_reconcile_requested.store(true, Ordering::SeqCst);
                    }
                }
            }

            let pass = self.drain_pending().await;

            if pass.attempted > 0 && pass.succeeded == 0 {
                stalled_passes += 1;
            } else {
                stalled_passes = 0;
            }

            // The bootstrap gate lifts once a cold replica has *caught up*,
            // not once it has merely learned what it is missing — a node
            // that knows about a thousand repositories and holds none would
            // answer 404 for all of them. A master that holds nothing leaves
            // nothing pending, so that case opens at once. The one exception
            // is a catch-up that has stopped making progress: two passes in
            // a row with nothing landing means waiting longer will not help,
            // and the repositories that did land are better served than
            // refused.
            if reconciled_once && !self.state.replica_status.is_bootstrapped() {
                if self.pending.is_empty() {
                    self.state.replica_status.mark_bootstrapped();
                } else if stalled_passes >= 2 {
                    tracing::error!(
                        pending = self.pending.len(),
                        "replica catch-up has stalled, serving what it holds while retrying"
                    );

                    self.state.replica_status.mark_bootstrapped();
                }
            }

            // Loop straight round only on *progress*. Counting attempts
            // instead would spin against an unreachable master: every failed
            // sync re-queues itself, so a wholly-failed pass always leaves
            // the pending set non-empty and would be retried immediately.
            if pass.succeeded > 0 && !self.pending.is_empty() {
                continue;
            }

            // A pass where nothing succeeded gets a backoff of its own,
            // rather than falling through to the select below — whose `wake`
            // may already hold a permit from a notification that arrived
            // while the pass was running, and would return at once.
            if pass.attempted > 0 && pass.succeeded == 0 {
                sleep(Duration::from_millis(self.replication.reconnect_backoff_ms)).await;
            }

            let poll = Duration::from_secs(self.replication.poll_interval_secs);

            tokio::select! {
                _ = sleep(poll) => {
                    self.full_reconcile_requested.store(true, Ordering::SeqCst);
                }
                _ = self.wake.notified() => {}
            }
        }
    }

    /// Compares the master's whole repository listing against this node's own
    /// disk, queueing everything that differs and deleting what the master no
    /// longer holds.
    ///
    /// This is the path that makes long downtime a non-event: it does not
    /// care how many notifications were missed, only what the two sides hold
    /// right now.
    async fn reconcile(&self) -> Result<(), String> {
        let listing = self.fetch_state().await?;

        // Before anything in the listing is acted on: is this the master we
        // are pinned to? The first successful listing is where a fresh
        // replica pins, since the listing is the authority on state.
        self.verify_master_identity(listing.identity.as_deref(), true)?;

        let index = self.state.repository_index.clone();

        let local = run_blocking(move || {
            let mut local = std::collections::HashMap::new();

            for repository in index.snapshot().repositories {
                local.insert(
                    format!("{}/{}", repository.collection_id, repository.tenant_id),
                    repository.head_sha,
                );
            }

            Ok(local)
        })
        .await
        .map_err(|err| err.to_string())?;

        let mut remote_keys = std::collections::HashSet::new();
        let mut stale = 0_usize;

        for repository in &listing.repositories {
            // Every identifier the master states becomes a path on this disk
            // and a segment of a URL, so it is held to the same rules as one
            // arriving through the content API — a peer of another build, or
            // a stray directory on the master, must not be able to name a
            // path this node would never have created itself.
            if let Err(reason) = Self::validate_entry(
                &repository.collection_id,
                &repository.tenant_id,
                &repository.head_sha,
            ) {
                tracing::warn!(reason = %reason, "ignoring invalid repository in master listing");

                continue;
            }

            let key = format!("{}/{}", repository.collection_id, repository.tenant_id);

            remote_keys.insert(key.clone());

            if local.get(&key) == Some(&repository.head_sha) {
                continue;
            }

            stale += 1;

            self.pending.insert(
                key,
                (
                    repository.collection_id.clone(),
                    repository.tenant_id.clone(),
                    SyncTarget::At {
                        head_sha: repository.head_sha.clone(),
                    },
                ),
            );
        }

        // Deletions are only inferable from a listing that describes the
        // master's entire repository set.
        if !listing.complete {
            tracing::warn!("master listing is incomplete, deletions will not be inferred from it");
        } else {
            for key in local.keys() {
                if remote_keys.contains(key) {
                    continue;
                }

                let Some((collection_id, tenant_id)) = key.split_once('/') else {
                    continue;
                };

                self.pending.insert(
                    key.clone(),
                    (
                        collection_id.to_string(),
                        tenant_id.to_string(),
                        SyncTarget::Deleted,
                    ),
                );
            }
        }

        let status = &self.state.replica_status;

        status.set_repositories(local.len());
        status.set_last_reconcile(chrono::Utc::now().timestamp());
        status.record_master_contact();

        // Reaching the master is *not* the same as holding its content, so a
        // successful reconcile does not open a cold replica for traffic; the
        // worker does that once the pending set this pass filled has drained.

        tracing::debug!(
            remote_repositories = listing.repositories.len(),
            local_repositories = local.len(),
            stale = stale,
            pending = self.pending.len(),
            "replication reconcile complete"
        );

        // The roster is refreshed on the back of a successful reconcile — the
        // one moment this node is known to be able to reach its master — so
        // the health route always answers from the freshest picture available
        // without a polling loop of its own.
        self.refresh_master_health().await;

        Ok(())
    }

    /// Fetches the master's own health and caches its roster locally.
    ///
    /// Failure is logged at debug and otherwise ignored: a roster is
    /// observability, and losing it must never hold up convergence, which is
    /// the job that actually matters. The previously cached roster stays, with
    /// its own `observed_at` to say how old it now is.
    async fn refresh_master_health(&self) {
        let url = format!("{}{}/health", self.master_url, URL_PREFIX);

        let fetched = self
            .reporting_request(&self.transfer_client, &url)
            .send()
            .await;

        let response = match fetched {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                tracing::debug!(status = %response.status(), "master health request rejected");

                return;
            }
            Err(err) => {
                tracing::debug!(err = %err, "master health request failed");

                return;
            }
        };

        match response.json::<ReplicationHealth>().await {
            Ok(health) => {
                self.state
                    .replica_status
                    .record_master_health(Some(health.node.node_id), health.replicas);
            }
            Err(err) => {
                tracing::debug!(err = %err, "master health response is not valid");
            }
        }
    }

    /// Syncs every repository currently marked stale, bounded by
    /// `replication.parallelism`.
    ///
    /// Repositories are independent — there is no cross-repository ordering
    /// requirement the way there is for hook delivery — so this fans out
    /// freely. Work is ordered by nothing in particular beyond what the map
    /// yields; the bound exists so a replica returning from a long outage
    /// cannot stampede its master.
    async fn drain_pending(self: &Arc<Self>) -> SyncPass {
        // Nothing is pulled from a master whose identity has not checked
        // out. The pending set is kept: it is drained the moment a listing
        // states the right identity again.
        if !self.state.replica_status.identity_verified() {
            if !self.pending.is_empty() {
                tracing::debug!(
                    pending = self.pending.len(),
                    "holding replication catch-up until the master's identity is verified"
                );
            }

            return SyncPass::default();
        }

        let targets: Vec<(String, (String, String, SyncTarget))> = self
            .pending
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();

        if targets.is_empty() {
            self.state.replica_status.set_pending(0);

            return SyncPass::default();
        }

        self.state.replica_status.set_pending(targets.len());

        tracing::info!(
            repositories = targets.len(),
            "replication catch-up starting"
        );

        let semaphore = Arc::new(Semaphore::new(self.replication.parallelism));
        let succeeded = Arc::new(AtomicUsize::new(0));
        let mut tasks = tokio::task::JoinSet::new();
        let mut attempted = 0_usize;

        for (key, (collection_id, tenant_id, target)) in targets {
            // Remove before syncing: a notification arriving mid-sync
            // re-inserts the key and the next pass picks it up, rather than
            // being swallowed by this one.
            self.pending.remove(&key);

            attempted += 1;

            let permit = semaphore.clone().acquire_owned().await;

            if permit.is_err() {
                break;
            }

            let follower = Arc::clone(self);
            let succeeded = succeeded.clone();

            tasks.spawn(async move {
                let _permit = permit;

                if follower
                    .sync_repository(&collection_id, &tenant_id, target)
                    .await
                {
                    succeeded.fetch_add(1, Ordering::Relaxed);
                }
            });
        }

        while tasks.join_next().await.is_some() {}

        self.state.replica_status.set_pending(self.pending.len());

        let pass = SyncPass {
            attempted,
            succeeded: succeeded.load(Ordering::Relaxed),
        };

        tracing::info!(
            repositories = pass.attempted,
            succeeded = pass.succeeded,
            "replication catch-up finished"
        );

        pass
    }

    /// Brings one repository to the state the master reported, or removes it.
    ///
    /// Returns whether the repository ended up where the master said it
    /// should be. The worker counts those to tell a pass that made progress
    /// from one that achieved nothing — a failed sync re-queues itself, so
    /// the size of the pending set cannot answer that on its own.
    async fn sync_repository(
        &self,
        collection_id: &str,
        tenant_id: &str,
        target: SyncTarget,
    ) -> bool {
        let head_sha = match target {
            SyncTarget::Deleted => {
                self.delete_repository(collection_id, tenant_id).await;

                return true;
            }
            SyncTarget::At { head_sha } => head_sha,
        };

        match self
            .fetch_and_apply(collection_id, tenant_id, &head_sha, false)
            .await
        {
            Ok(true) => true,
            Ok(false) => {
                // Non-fast-forward: with the master's identity pinned, the
                // ways this happens are a tenant deleted and re-created
                // under the same identity (an unrelated history), or this
                // replica being *ahead* of its master — a master restored
                // from backup, or a failback after a manual promotion. Either
                // way the local copy is discarded and cloned afresh, which
                // destroys it, so this is logged as an error and counted.
                tracing::error!(
                    collection_id = %collection_id,
                    tenant_id = %tenant_id,
                    "replica history diverged from master, discarding the local copy and re-cloning"
                );

                self.state.replica_status.note_reclone();

                self.delete_repository(collection_id, tenant_id).await;

                match self
                    .fetch_and_apply(collection_id, tenant_id, &head_sha, true)
                    .await
                {
                    Ok(applied) => applied,
                    Err(err) => {
                        tracing::error!(
                            collection_id = %collection_id,
                            tenant_id = %tenant_id,
                            err = %err,
                            "replication re-clone failed"
                        );

                        false
                    }
                }
            }
            Err(err) => {
                tracing::warn!(
                    collection_id = %collection_id,
                    tenant_id = %tenant_id,
                    err = %err,
                    "replication sync failed, will retry on the next pass"
                );

                // Leave it stale so the next pass picks it up again.
                self.pending.insert(
                    format!("{}/{}", collection_id, tenant_id),
                    (
                        collection_id.to_string(),
                        tenant_id.to_string(),
                        SyncTarget::At { head_sha },
                    ),
                );

                false
            }
        }
    }

    /// Downloads the delta packfile and imports it. Returns `false` when the
    /// import was refused as non-fast-forward.
    ///
    /// `force_full` skips the `have` parameter, asking for every object
    /// reachable from HEAD. It is only used for the re-clone path — in normal
    /// operation an incremental fetch is *always* the right answer, and never
    /// a gamble: a replica keeps full history (the commit routes read it), so
    /// the delta is by construction a subset of a full clone, no matter how
    /// far behind the replica has fallen.
    async fn fetch_and_apply(
        &self,
        collection_id: &str,
        tenant_id: &str,
        head_sha: &str,
        force_full: bool,
    ) -> Result<bool, String> {
        let repo_path = self.repo_path(collection_id, tenant_id);

        let have = if force_full {
            None
        } else {
            let probe_path = repo_path.clone();

            run_blocking(move || Ok(GitReplication::head_sha(&probe_path)))
                .await
                .map_err(|err| err.to_string())?
        };

        if have.as_deref() == Some(head_sha) {
            return Ok(true);
        }

        let (pack_path, pack_head_sha) = self
            .download_pack(collection_id, tenant_id, have.as_deref())
            .await?;

        // The pack was built at whatever HEAD the master had when it was
        // asked, which may already be past the sha this sync was queued for.
        // Fast-forwarding to the pack's own head lands everything it carried
        // instead of stopping one commit short and fetching the rest again.
        let apply_head = pack_head_sha.unwrap_or_else(|| head_sha.to_string());

        // The download happens outside the lock — it is the slow part, and
        // nothing about it touches the repository. Only the import, which is
        // local and fast, needs exclusion (from maintenance, and from a
        // concurrent sync of the same repository).
        let lock_key = format!("{}/{}", collection_id, tenant_id);
        let lock = self.state.get_repo_lock(&lock_key);
        let _lock_guard = lock.lock().await;

        let apply_repo_path = repo_path.clone();
        let apply_pack_path = pack_path.clone();

        let outcome = run_blocking(move || {
            GitReplication::apply_pack(&apply_repo_path, &apply_pack_path, &apply_head)
        })
        .await;

        let _ = std::fs::remove_file(&pack_path);

        match outcome.map_err(|err| err.to_string())? {
            PackApply::FastForwarded { from, to } => {
                tracing::info!(
                    collection_id = %collection_id,
                    tenant_id = %tenant_id,
                    from = %from.unwrap_or_else(|| "(new)".to_string()),
                    to = %to,
                    "replicated"
                );

                self.state
                    .maintenance
                    .schedule(&lock_key, repo_path, lock.clone());

                // Cascade: a replica serves the replication surface too, so
                // anything following *this* node learns about the change
                // without waiting for its own poll interval.
                self.state
                    .replication
                    .repository_updated(collection_id, tenant_id, &to);

                Ok(true)
            }

            PackApply::UpToDate => Ok(true),

            PackApply::NonFastForward { local, remote } => {
                tracing::warn!(
                    collection_id = %collection_id,
                    tenant_id = %tenant_id,
                    local = %local,
                    remote = %remote,
                    "non-fast-forward replication result"
                );

                Ok(false)
            }
        }
    }

    /// Streams a packfile from the master into a temporary file.
    ///
    /// Landing it on disk rather than in memory is what keeps a full clone
    /// of a large tenant bounded: the import reads the file back through
    /// libgit2's pack writer, so at no point does the whole transfer have to
    /// be resident.
    async fn download_pack(
        &self,
        collection_id: &str,
        tenant_id: &str,
        have: Option<&str>,
    ) -> Result<(PathBuf, Option<String>), String> {
        let mut url = format!(
            "{}{}/{}/{}/pack",
            self.master_url, URL_PREFIX, collection_id, tenant_id
        );

        if let Some(have) = have {
            url.push_str(&format!("?have={}", have));
        }

        let mut response = self
            .request(&self.pack_client, &url)
            .send()
            .await
            .map_err(|err| format!("pack request failed: {}", err))?;

        if !response.status().is_success() {
            return Err(format!("pack request returned {}", response.status()));
        }

        // The commit this pack brings us to. Absent or malformed, the sync
        // falls back to the sha it was queued for — a master of another
        // build is not a reason to refuse its pack.
        let pack_head_sha = response
            .headers()
            .get(HEAD_SHA_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| validate::commit_sha(value).ok())
            .map(str::to_string);

        let incoming_directory = self.state.config.server.repos_path.join(INCOMING_DIRECTORY);

        std::fs::create_dir_all(&incoming_directory)
            .map_err(|err| format!("cannot create incoming directory: {}", err))?;

        let sequence = self.download_sequence.fetch_add(1, Ordering::Relaxed);

        let pack_path =
            incoming_directory.join(format!("{}_{}_{}.pack", collection_id, tenant_id, sequence));

        let mut pack_file = tokio::fs::File::create(&pack_path)
            .await
            .map_err(|err| format!("cannot create pack file: {}", err))?;

        let mut written = 0_u64;

        loop {
            let chunk = response
                .chunk()
                .await
                .map_err(|err| format!("pack download failed: {}", err))?;

            let Some(chunk) = chunk else {
                break;
            };

            written += chunk.len() as u64;

            tokio::io::AsyncWriteExt::write_all(&mut pack_file, &chunk)
                .await
                .map_err(|err| format!("cannot write pack file: {}", err))?;
        }

        tokio::io::AsyncWriteExt::flush(&mut pack_file)
            .await
            .map_err(|err| format!("cannot flush pack file: {}", err))?;

        tracing::debug!(
            collection_id = %collection_id,
            tenant_id = %tenant_id,
            bytes = written,
            incremental = have.is_some(),
            "replication pack downloaded"
        );

        Ok((pack_path, pack_head_sha))
    }

    /// Removes a repository the master no longer holds.
    async fn delete_repository(&self, collection_id: &str, tenant_id: &str) {
        let repo_path = self.repo_path(collection_id, tenant_id);

        if !repo_path.exists() {
            return;
        }

        let lock_key = format!("{}/{}", collection_id, tenant_id);
        let lock = self.state.get_repo_lock(&lock_key);
        let _lock_guard = lock.lock().await;

        match tokio::fs::remove_dir_all(&repo_path).await {
            Ok(()) => {
                self.state.maintenance.cancel(&lock_key);

                self.state
                    .replication
                    .repository_deleted(collection_id, tenant_id);

                tracing::info!(
                    collection_id = %collection_id,
                    tenant_id = %tenant_id,
                    "replicated tenant deletion"
                );
            }
            Err(err) => {
                tracing::error!(
                    collection_id = %collection_id,
                    tenant_id = %tenant_id,
                    err = %err,
                    "failed to remove replicated tenant"
                );
            }
        }
    }

    async fn fetch_state(&self) -> Result<RepositoryListing, String> {
        let url = format!("{}{}/state", self.master_url, URL_PREFIX);

        let response = self
            .reporting_request(&self.transfer_client, &url)
            .send()
            .await
            .map_err(|err| format!("state request failed: {}", err))?;

        if !response.status().is_success() {
            return Err(format!("state request returned {}", response.status()));
        }

        response
            .json::<RepositoryListing>()
            .await
            .map_err(|err| format!("state response is not valid: {}", err))
    }

    // -----------------------------------------------------------------------
    // Event stream
    // -----------------------------------------------------------------------

    /// Holds the notification stream open, reconnecting with backoff.
    ///
    /// Every successful connection triggers a full reconcile before any
    /// frame is read: whatever happened while the stream was down was not
    /// announced to us, and the reconcile is the only thing that can say
    /// what it was.
    async fn run_event_stream(self: Arc<Self>) {
        let url = format!("{}{}/events", self.master_url, URL_PREFIX);
        let mut backoff_ms = self.replication.reconnect_backoff_ms;

        loop {
            let attempt = self.request(&self.stream_client, &url).send().await;

            match attempt {
                Ok(response) if response.status().is_success() => {
                    tracing::info!(master = %self.master_url_display, "replication notification stream connected");

                    backoff_ms = self.replication.reconnect_backoff_ms;

                    self.state.replica_status.set_stream_connected(true);

                    self.request_full_reconcile();

                    self.consume_events(response).await;

                    self.state.replica_status.set_stream_connected(false);

                    tracing::warn!("replication notification stream closed");
                }

                Ok(response) => {
                    tracing::warn!(
                        status = %response.status(),
                        "replication notification stream rejected"
                    );
                }

                Err(err) => {
                    tracing::warn!(err = %err, "replication notification stream unreachable");
                }
            }

            self.state.replica_status.set_stream_connected(false);

            sleep(Duration::from_millis(backoff_ms)).await;

            backoff_ms = (backoff_ms * 2).min(MAXIMUM_RECONNECT_BACKOFF_MS);
        }
    }

    /// Reads NDJSON frames until the stream ends.
    ///
    /// Unparseable lines are skipped rather than fatal, so a replica running
    /// against a newer master that emits an event kind it does not know
    /// keeps working — it simply learns nothing from those frames, and its
    /// poll-driven reconcile covers the gap.
    async fn consume_events(&self, mut response: reqwest::Response) {
        let mut buffer = String::new();

        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(err) => {
                    tracing::debug!(err = %err, "replication stream read failed");

                    break;
                }
            };

            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(newline) = buffer.find('\n') {
                let line = buffer[..newline].trim().to_string();

                buffer.drain(..=newline);

                if line.is_empty() {
                    continue;
                }

                match serde_json::from_str::<ReplicationEvent>(&line) {
                    // A frame can ask for the stream to be dropped — the
                    // master identifying itself as the wrong one does.
                    Ok(event) => {
                        if !self.handle_event(event) {
                            return;
                        }
                    }
                    Err(err) => {
                        tracing::debug!(err = %err, "ignoring unrecognised replication frame")
                    }
                }
            }
        }
    }

    /// Acts on one frame. Returns whether the stream should stay open.
    fn handle_event(&self, event: ReplicationEvent) -> bool {
        match event {
            ReplicationEvent::Hello {
                protocol,
                node_id,
                identity,
            } => {
                if protocol != PROTOCOL_VERSION {
                    // Logged, not fatal: the payloads are versioned precisely
                    // so a mismatch can be handled rather than fatal, and for
                    // one known version the honest move is to carry on and
                    // let any field we cannot parse be skipped.
                    tracing::warn!(
                        master_protocol = protocol,
                        our_protocol = PROTOCOL_VERSION,
                        "master speaks a different replication protocol"
                    );
                }

                tracing::debug!(master = %node_id, protocol = protocol, "master identified itself");

                // The stream is not where a fresh replica pins (the listing
                // is), but it is a place a wrong master can be caught early:
                // drop the stream rather than queue syncs against it. A
                // peer stating no identity is left to the listing check.
                if identity.is_some() {
                    if let Err(err) = self.verify_master_identity(identity.as_deref(), false) {
                        tracing::warn!(err = %err, "closing notification stream");

                        return false;
                    }
                }

                self.state.replica_status.record_master_identity(node_id);
            }

            ReplicationEvent::Heartbeat => {}

            ReplicationEvent::RepositoryUpdated {
                collection_id,
                tenant_id,
                head_sha,
            } => {
                if let Err(reason) = Self::validate_entry(&collection_id, &tenant_id, &head_sha) {
                    tracing::warn!(reason = %reason, "ignoring invalid repository.updated frame");

                    return true;
                }

                let key = format!("{}/{}", collection_id, tenant_id);

                self.pending
                    .insert(key, (collection_id, tenant_id, SyncTarget::At { head_sha }));

                self.wake.notify_one();
            }

            ReplicationEvent::RepositoryDeleted {
                collection_id,
                tenant_id,
            } => {
                if let Err(reason) = Self::validate_identifiers(&collection_id, &tenant_id) {
                    tracing::warn!(reason = %reason, "ignoring invalid repository.deleted frame");

                    return true;
                }

                let key = format!("{}/{}", collection_id, tenant_id);

                self.pending
                    .insert(key, (collection_id, tenant_id, SyncTarget::Deleted));

                self.wake.notify_one();
            }
        }

        true
    }

    fn request_full_reconcile(&self) {
        self.full_reconcile_requested.store(true, Ordering::SeqCst);

        self.wake.notify_one();
    }

    /// Checks the identity the master just stated against the one this
    /// replica is pinned to, pinning it when `may_pin` and nothing is pinned
    /// yet. On success the follower is cleared to pull; on failure it is
    /// blocked, the error is recorded where the health route shows it, and
    /// an operator gets told exactly what to do.
    fn verify_master_identity(&self, stated: Option<&str>, may_pin: bool) -> Result<(), String> {
        let status = &self.state.replica_status;

        let outcome = self.check_master_identity(stated, may_pin);

        match &outcome {
            Ok(()) => status.set_identity_verified(true),
            Err(err) => {
                status.set_identity_verified(false);
                status.record_master_error(err.clone());

                tracing::error!(err = %err, "refusing to sync from this master");
            }
        }

        outcome
    }

    fn check_master_identity(&self, stated: Option<&str>, may_pin: bool) -> Result<(), String> {
        let Some(stated) = stated else {
            return Err(format!(
                "master {} stated no replication identity; a node that cannot say which data set it serves is not followed",
                self.master_url_display
            ));
        };

        validate::replication_identity(stated).map_err(|err| {
            format!(
                "master {} stated an invalid identity: {}",
                self.master_url_display, err
            )
        })?;

        let identity = &self.state.identity;

        match identity.current() {
            Some(pinned) if pinned == stated => Ok(()),
            Some(pinned) => {
                let (master_node_id, ..) = self.state.replica_status.master_snapshot();

                Err(format!(
                    "master identity mismatch: this replica ({}) is pinned to {} in {} but the master {} at {} states {}; \
                     refusing to sync. If the master was legitimately rebuilt or replaced, stop this replica, \
                     delete {} and restart it to pair with the new master",
                    self.node_id,
                    pinned,
                    identity.path().display(),
                    master_node_id.as_deref().unwrap_or("(unknown)"),
                    self.master_url_display,
                    stated,
                    identity.path().display(),
                ))
            }
            None if may_pin => identity.pin(stated),
            None => Ok(()),
        }
    }

    fn validate_identifiers(collection_id: &str, tenant_id: &str) -> Result<(), String> {
        validate::collection_id(collection_id).map_err(|err| format!("collection id: {}", err))?;
        validate::tenant_id(tenant_id).map_err(|err| format!("tenant id: {}", err))?;

        Ok(())
    }

    fn validate_entry(collection_id: &str, tenant_id: &str, head_sha: &str) -> Result<(), String> {
        Self::validate_identifiers(collection_id, tenant_id)?;
        validate::commit_sha(head_sha).map_err(|err| format!("head sha: {}", err))?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Builds an authenticated upstream request carrying this node's identity.
    ///
    /// Every request the follower makes goes through here, so identity can
    /// never be attached to some requests and forgotten on others — the
    /// master's roster would then show a replica flickering in and out.
    fn request(&self, client: &Client, url: &str) -> reqwest::RequestBuilder {
        client
            .get(url)
            .bearer_auth(&self.replication.secret)
            .header(NODE_ID_HEADER, &self.node_id)
            .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
    }

    /// The same, plus the two numbers only this node can know: how many
    /// repositories it holds and how many it knows are behind. Sent on the
    /// state poll because that request is already the heartbeat of the
    /// convergence loop, so the master learns this node's lag with no extra
    /// round trip.
    fn reporting_request(&self, client: &Client, url: &str) -> reqwest::RequestBuilder {
        let status = &self.state.replica_status;

        self.request(client, url)
            .header(REPOSITORIES_HEADER, status.repositories().to_string())
            .header(PENDING_HEADER, status.pending().to_string())
    }

    fn repo_path(&self, collection_id: &str, tenant_id: &str) -> PathBuf {
        self.state
            .config
            .server
            .repos_path
            .join(collection_id)
            .join(tenant_id)
    }
}

// ---------------------------------------------------------------------------
// build_health — the one answer both health routes serve
// ---------------------------------------------------------------------------

/// Builds this node's replication picture.
///
/// `repositories` is passed in rather than counted here because counting means
/// touching the filesystem, which belongs on the blocking pool — the caller
/// already has it from `GitReplication::list_repositories`.
///
/// The three roles answer with the same shape, which is the point: one probe
/// works against every node in a deployment. What differs is where the
/// contents come from.
///
/// - A **master** (and a standalone node, which is its own write node) names
///   itself as the master and reports a *live* roster, since it is the node
///   watching those connections.
/// - A **replica** names the master it follows and reports the roster it last
///   received, stamped with when that was. It deliberately does not fetch
///   upstream on demand: the situation this feature exists for is a master
///   that is down, so a proxying health route would fail exactly when it is
///   needed most. A timestamped cache degrades instead of breaking.
pub fn build_health(state: &AppState, repositories: usize) -> ReplicationHealth {
    let now = chrono::Utc::now().timestamp();
    let server = &state.config.server;

    let Some(replication) = &state.config.replication else {
        // Standalone: it accepts writes, so it *is* the write node of a set
        // of one. Reporting that uniformly beats a special case a monitor
        // would have to know about.
        let node_id = format!("{}:{}", server.host, server.port);

        return ReplicationHealth {
            protocol: PROTOCOL_VERSION,
            node: NodeHealth {
                node_id: node_id.clone(),
                role: "standalone".to_string(),
                identity: None,
                repositories,
            },
            master: MasterHealth {
                node_id: Some(node_id),
                url: None,
                reachable: true,
                last_contact_at: Some(now),
                last_error: None,
            },
            replicas: Vec::new(),
            replica: None,
            observed_at: now,
            replicas_observed_at: Some(now),
        };
    };

    let node_id = replication.node_id(server);

    if !replication.is_replica() {
        return ReplicationHealth {
            protocol: PROTOCOL_VERSION,
            node: NodeHealth {
                node_id: node_id.clone(),
                role: "master".to_string(),
                identity: state.identity.current(),
                repositories,
            },
            master: MasterHealth {
                node_id: Some(node_id),
                url: None,
                reachable: true,
                last_contact_at: Some(now),
                last_error: None,
            },
            replicas: state.replica_registry.roster(),
            replica: None,
            observed_at: now,
            replicas_observed_at: Some(now),
        };
    }

    let status = &state.replica_status;
    let (master_node_id, replicas, replicas_observed_at, last_contact_at, last_error) =
        status.master_snapshot();

    ReplicationHealth {
        protocol: PROTOCOL_VERSION,
        node: NodeHealth {
            node_id,
            role: "replica".to_string(),
            identity: state.identity.current(),
            repositories,
        },
        master: MasterHealth {
            node_id: master_node_id,
            url: replication.master_url.as_deref().map(redact_url),
            // "The last attempt to talk to the master succeeded." Kept
            // separate from the stream's state below, so neither fact has to
            // stand in for the other: a replica can be converging by polling
            // with its notification stream down, and an operator should see
            // both.
            reachable: last_error.is_none() && last_contact_at.is_some(),
            last_contact_at,
            last_error,
        },
        replicas,
        replica: Some(FollowerHealth {
            state: if status.is_bootstrapped() {
                "ready".to_string()
            } else {
                "bootstrapping".to_string()
            },
            stream_connected: status.stream_connected(),
            last_reconcile_at: status.last_reconcile(),
            pending_repositories: status.pending(),
            reclones: status.reclones(),
        }),
        observed_at: now,
        replicas_observed_at,
    }
}

/// Removes stale packfile downloads left behind by a previous process.
///
/// Called at startup, where no download can be in flight, so everything
/// found is by definition abandoned — the same reasoning as the stale
/// `.git/index.lock` sweep next to it.
pub fn cleanup_incoming_packs(repos_root: &Path) {
    let incoming_directory = repos_root.join(INCOMING_DIRECTORY);

    if !incoming_directory.exists() {
        return;
    }

    if let Err(err) = std::fs::remove_dir_all(&incoming_directory) {
        tracing::warn!(
            path = %incoming_directory.display(),
            err = %err,
            "failed to clear abandoned replication downloads"
        );
    }
}
