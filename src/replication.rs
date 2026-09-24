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
//! - **A replica's working tree is mirrored by checkout, not file by file.**
//!   Nothing on a replica reads it — every route answers from HEAD's tree —
//!   but a tenant a human can `ls` on the master and not on the replica is a
//!   difference with no upside, so a landed pack checks the repository out to
//!   HEAD ([`crate::git::GitReplication::mirror_working_tree`]). A full
//!   checkout rather than a delta because it converges from any prior state,
//!   including the no-working-tree-at-all one a store replicated by an older
//!   build is in. Switched off with `server.checkout_files`, which a replica
//!   is the likeliest node to want off; healing an already-filled store is
//!   [`crate::checkout`]'s job.

use dashmap::DashMap;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{broadcast, Notify, Semaphore};
use tokio::time::{sleep, Duration};

use crate::config::{Config, ReplicationConfig};
use crate::git::{
    GitReplication, GitStaging, HeadRelation, PackApply, RepositoryHead, RepositoryScan,
};
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

/// Floor between two rescans forced by an *incomplete* index. An index that
/// cannot be completed (an unreadable collection directory, a missing
/// identity file) would otherwise rescan on every state poll and every
/// health probe, turning a fault into a disk walk per request.
const INCOMPLETE_RESCAN_MIN_SECS: u64 = 5;

/// A replica whose last reconcile-or-drain failed this many times in a row is
/// `stalled`: three consecutive misses is past the point where a single
/// transient error explains it, and short enough to page before an outage
/// has aged. Below it the replica is merely `lagging`.
const STALLED_AFTER_FAILURES: u32 = 3;

/// How many of a replica's own poll intervals of silence make a master call
/// that replica `degraded`. Two, so that one missed or slow poll — a retry, a
/// long pack, a blip — never flips a node that is converging perfectly well.
const REPLICA_SILENT_INTERVALS: i64 = 2;

/// The floor under that threshold, and what it is for a replica that does not
/// say how often it polls: an older peer predating [`POLL_INTERVAL_HEADER`],
/// or one whose header was unreadable. Two intervals at the default
/// `poll_interval_secs`, which is what this rule was before it was derived.
const REPLICA_SILENT_MINIMUM_SECS: i64 = 120;

/// The ceiling over it. The threshold is sized from a number the *replica*
/// asserts, and a self-asserted number that can grow without bound could
/// switch off the only thing on a master that notices a follower going dark.
/// Fifteen minutes is past any sane polling cadence and still well inside the
/// window in which an operator wants to hear about a silent node.
const REPLICA_SILENT_MAXIMUM_SECS: i64 = 900;

/// The mass-deletion guard. A complete listing that would have a replica
/// delete *more than half* of the repositories it holds is refused outright
/// and reported instead of applied. A master genuinely deleting most of its
/// tenants in one poll interval is rare; a master whose store was swapped,
/// emptied, or mis-mounted is not, and the difference is not one a replica
/// can tell on its own. A replica holding fewer than this many repositories
/// is exempt, since "more than half of one" is every ordinary deletion.
const DELETION_GUARD_MINIMUM_HELD: usize = 2;

/// How long after its last refused connection a node id collision stays
/// reported on the *master*. A colliding replica re-dials with backoff that
/// caps at 60 s, so a collision still in progress refreshes itself well
/// inside this; one that has gone quiet for longer was fixed (renamed and
/// restarted), and there is no other event a master could learn that from.
const COLLISION_EXPIRY_SECS: i64 = 150;

/// How long a node id collision may persist before it is reported as an
/// issue — on the replica being refused, and on the master refusing it.
///
/// A restarted replica collides with the socket of its own previous process
/// until the node serving the stream notices that socket is dead. Normally
/// that is immediate (the stream task watches for its peer going away), but
/// a connection whose `FIN` never arrives — a proxy in between, a partition
/// that left the old socket half-open — is only discovered when a heartbeat
/// write finally fails, which takes up to two of them. The stream is refused
/// throughout either way, since the two cases are indistinguishable from the
/// serving side; this is purely how long the refusal has to last before an
/// operator is told it is a misconfiguration rather than a restart.
const COLLISION_REPORT_AFTER_SECS: u64 = 2 * EVENT_HEARTBEAT_SECS;

/// Size of the per-process instance token, before hex encoding.
const INSTANCE_BYTES: usize = 16;

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
// Timestamps on the wire
// ---------------------------------------------------------------------------

/// Serde adapters that put a unix-seconds `i64` on the wire as an RFC 3339
/// date-time (`2026-06-16T10:00:00Z`) and read one back.
///
/// Every timestamp this API emits is RFC 3339 — `committed_at` on the commit
/// routes set the convention — and the health and replication bodies follow
/// it rather than leaking the integer the atomics hold internally. Whole
/// seconds only, matching `committed_at` (git times are whole seconds too),
/// so the two spell identical instants identically.
///
/// Internally everything stays `i64`: atomics cannot hold a `DateTime`, and a
/// single conversion at the edge beats threading chrono through every
/// status struct.
pub mod rfc3339 {
    use chrono::{DateTime, SecondsFormat, Utc};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    /// `unix` seconds as an RFC 3339 string with a `Z` suffix.
    pub fn format(unix: i64) -> String {
        DateTime::<Utc>::from_timestamp(unix, 0)
            .map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true))
            .unwrap_or_default()
    }

    /// The inverse of [`format`], tolerant of any RFC 3339 offset.
    pub fn parse(raw: &str) -> Option<i64> {
        DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|at| at.timestamp())
    }

    pub fn serialize<S: Serializer>(unix: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        format(*unix).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
        let raw = String::deserialize(deserializer)?;

        parse(&raw).ok_or_else(|| serde::de::Error::custom("expected an RFC 3339 date-time"))
    }

    /// The same for `Option<i64>`: `null` stays `null`.
    pub mod option {
        use serde::{Deserialize, Deserializer, Serialize, Serializer};

        pub fn serialize<S: Serializer>(
            unix: &Option<i64>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            unix.map(super::format).serialize(serializer)
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<i64>, D::Error> {
            let raw = Option::<String>::deserialize(deserializer)?;

            match raw {
                None => Ok(None),
                Some(raw) => super::parse(&raw)
                    .map(Some)
                    .ok_or_else(|| serde::de::Error::custom("expected an RFC 3339 date-time")),
            }
        }
    }
}

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
/// The replica's own [`SyncStatus`], so the master's roster shows every
/// follower's condition in one place.
pub const SYNC_HEADER: &str = "x-replication-sync";
/// A random token generated once per replica *process*. Two connections
/// bearing the same node id and the same instance are one replica
/// reconnecting; the same node id with a different instance is two replicas
/// sharing a name, which the master refuses.
pub const INSTANCE_HEADER: &str = "x-replication-instance";
/// How often the replica polls, in seconds — its own `poll_interval_secs`.
///
/// The master cannot know this: it is a replica-only config key, meaningless
/// on the node reading it, and on a chained replica it describes that node's
/// *own* upstream cadence rather than its followers'. So the replica states
/// it, next to the other facts only it can know, and the master sizes each
/// row's silence threshold from the row's own cadence — see
/// [`REPLICA_SILENT_INTERVALS`]. Absent on peers predating this header, which
/// simply fall back to [`REPLICA_SILENT_MINIMUM_SECS`].
pub const POLL_INTERVAL_HEADER: &str = "x-replication-poll-interval";

// ---------------------------------------------------------------------------
// Health — what a node can honestly say about the set it belongs to
// ---------------------------------------------------------------------------

/// The replication picture as one node sees it, served by both health routes.
///
/// One struct and one builder behind two doors: `GET /v1/_health/replication`
/// for the operator (public, no credential) and `GET /_replication/health`
/// for peers (replication secret). The audiences differ, the answer does not
/// — and the peer route is what lets a replica learn the roster at all,
/// since it holds the replication secret and not necessarily the content key.
///
/// Two fields exist purely so that an alert can be written against one
/// value on any node: `status` collapses everything below it into
/// `healthy` / `degraded` / `halted`, and `issues` lists exactly what a
/// `halted` node is waiting on a human for. See [`NodeStatus`].
#[derive(Debug, Serialize, Deserialize)]
pub struct ReplicationHealth {
    /// The protocol this body is written in.
    #[serde(default)]
    pub protocol: u32,
    /// `"healthy"`, `"degraded"` (converging on its own, or a follower is),
    /// or `"halted"` (this node, or a follower it knows of, needs a human).
    #[serde(default)]
    pub status: String,
    /// Everything this node is waiting on an operator for. Empty unless
    /// `status` is `"halted"`; each entry says what happened, to which
    /// repository where that applies, and since when.
    #[serde(default)]
    pub issues: Vec<Issue>,
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
    #[serde(with = "rfc3339")]
    pub observed_at: i64,
    /// When `replicas` was last true. On a master that is `observed_at` — it
    /// watches those connections itself. On a replica it is when the master
    /// last told it, so a roster served while the master is down is visibly
    /// stale rather than quietly wrong. `null` when a replica has never
    /// reached its master.
    #[serde(default, with = "rfc3339::option")]
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
    /// Last successful contact with the master.
    #[serde(default, with = "rfc3339::option")]
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
    #[serde(default, with = "rfc3339::option")]
    pub connected_at: Option<i64>,
    /// Last request of any kind from this replica.
    #[serde(default, with = "rfc3339::option")]
    pub last_contact_at: Option<i64>,
    pub packs_delivered: u64,
    /// Repositories the replica says it holds.
    pub repositories: Option<usize>,
    /// Repositories the replica says it knows are behind.
    pub pending_repositories: Option<usize>,
    /// The replica's own [`SyncStatus`], as it last reported it — so the
    /// master's roster is one place to read every follower's condition.
    pub sync: Option<String>,
    #[serde(default, with = "rfc3339::option")]
    pub reported_at: Option<i64>,
    /// How often the replica says it polls, in seconds. Internal: it sizes
    /// the silence threshold this row is judged against, and is deliberately
    /// never serialised — it describes the replica's *configuration*, which
    /// is its operator's business, not its condition, which is what the
    /// health body is for.
    #[serde(skip)]
    pub poll_interval_secs: Option<u64>,
    /// The instance token of the process holding the stream. Internal:
    /// what tells "this replica reconnected" from "another replica claims
    /// this name". Never serialised.
    #[serde(skip)]
    pub instance: Option<String>,
    /// When a stream under this node id was last refused as a collision.
    /// Internal; what lets the collision issue expire once the second
    /// process stops dialling.
    #[serde(skip)]
    pub last_collision_at: Option<i64>,
    /// When the *current* run of refusals under this node id began, cleared
    /// the moment a stream opens. Internal; what holds the collision issue
    /// back while a restarting replica is colliding with its own dying
    /// socket.
    #[serde(skip)]
    pub collision_since: Option<i64>,
}

/// A replica's own view of how its following is going.
///
/// `state` answers "can this node serve reads" and `sync` answers "is it
/// keeping up, and will it on its own" — two questions, because a warm
/// replica cut off from its master is `ready` and `stalled` at once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FollowerHealth {
    /// `"ready"` once this node holds content worth serving, else
    /// `"bootstrapping"`.
    pub state: String,
    /// One of [`SyncStatus`]: `"synced"`, `"lagging"`, `"stalled"`, or
    /// `"halted"`. Alert on `halted`; warn on `stalled`.
    pub sync: String,
    pub stream_connected: bool,
    #[serde(default, with = "rfc3339::option")]
    pub last_reconcile_at: Option<i64>,
    /// When a reconcile-and-drain last ended with nothing failed. `null`
    /// before the first one.
    #[serde(default, with = "rfc3339::option")]
    pub last_success_at: Option<i64>,
    pub pending_repositories: usize,
    /// Repositories this replica holds but refuses to sync until an operator
    /// looks at them — one `replica_ahead` or `history_diverged` issue each.
    pub locked_repositories: usize,
    /// Worker passes in a row that ended in failure. `stalled` at
    /// [`STALLED_AFTER_FAILURES`].
    pub consecutive_failures: u32,
}

/// How a replica's following is going, as one word.
///
/// The distinction that matters to an alert is the last one: `Stalled` heals
/// on its own once whatever is failing stops failing (a master that is down,
/// a network that is flapping), whereas `Halted` never does — something
/// needs a human, and [`Issue`] says what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    /// The last pass succeeded and nothing is pending or locked.
    Synced,
    /// Work is pending, or no pass has completed yet, and passes are landing.
    Lagging,
    /// [`STALLED_AFTER_FAILURES`] passes in a row have failed.
    Stalled,
    /// At least one [`Issue`] is open.
    Halted,
}

impl SyncStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SyncStatus::Synced => "synced",
            SyncStatus::Lagging => "lagging",
            SyncStatus::Stalled => "stalled",
            SyncStatus::Halted => "halted",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "synced" => Some(SyncStatus::Synced),
            "lagging" => Some(SyncStatus::Lagging),
            "stalled" => Some(SyncStatus::Stalled),
            "halted" => Some(SyncStatus::Halted),
            _ => None,
        }
    }
}

/// The one-word verdict on a whole node, for `ReplicationHealth::status`.
///
/// On a replica it follows its own [`SyncStatus`] plus its bootstrap gate.
/// On a master it folds in every roster row: a follower reporting `halted`
/// makes the master say `halted` too, because the master's health route is
/// where an operator with one probe looks, and a replica serving stale
/// content while refusing to sync is exactly the thing that probe must not
/// hide. A standalone node is `healthy` by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NodeStatus {
    Healthy,
    Degraded,
    Halted,
}

impl NodeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeStatus::Healthy => "healthy",
            NodeStatus::Degraded => "degraded",
            NodeStatus::Halted => "halted",
        }
    }
}

/// Something a node has stopped doing on its own and is waiting on an
/// operator for. Serialised with a `kind` tag so an alert can match on it;
/// every variant carries `since` so an operator can tell a fresh problem
/// from one that has been ignored for a week.
///
/// These are the *only* conditions under which replication holds back, and
/// every one of them exists so that the alternative — a replica acting on
/// its own guess, up to and including deleting its data — never happens.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Issue {
    /// This replica holds more history than its upstream announced for the
    /// repository: the announced head is an ancestor of the local one. The
    /// local copy is intact and still served; syncing it is suspended until
    /// the upstream moves past it or an operator removes the local copy.
    #[serde(rename = "replica_ahead")]
    ReplicaAhead {
        collection_id: String,
        tenant_id: String,
        local: String,
        remote: String,
        #[serde(with = "rfc3339")]
        since: i64,
    },
    /// Neither the local nor the announced head descends from the other — a
    /// tenant deleted and re-created under the same name, or a forked
    /// history. Same handling as `replica_ahead`.
    #[serde(rename = "history_diverged")]
    HistoryDiverged {
        collection_id: String,
        tenant_id: String,
        local: String,
        remote: String,
        #[serde(with = "rfc3339")]
        since: i64,
    },
    /// The upstream states a data-set identity other than the one pinned in
    /// this replica's `.replication.json`. Nothing is pulled until it does.
    #[serde(rename = "identity_mismatch")]
    IdentityMismatch {
        pinned: String,
        stated: String,
        #[serde(with = "rfc3339")]
        since: i64,
    },
    /// A complete upstream listing would have this replica delete more than
    /// half of what it holds. Refused; updates keep flowing.
    #[serde(rename = "deletion_refused")]
    DeletionRefused {
        would_delete: usize,
        held: usize,
        #[serde(with = "rfc3339")]
        since: i64,
    },
    /// This node's own `.replication.json` is gone from `repos_path` — the
    /// store this process started with is not the store it sees now. Its
    /// listing is served as incomplete, so no follower infers deletions.
    #[serde(rename = "identity_file_missing")]
    IdentityFileMissing {
        path: String,
        #[serde(with = "rfc3339")]
        since: i64,
    },
    /// This node's `.replication.json` no longer holds the identity loaded
    /// at startup. Same consequence as `identity_file_missing`.
    #[serde(rename = "identity_file_changed")]
    IdentityFileChanged {
        path: String,
        expected: String,
        found: String,
        #[serde(with = "rfc3339")]
        since: i64,
    },
    /// Two processes are presenting the same `node_id`. On a master: it
    /// refused the second one's stream. On a replica: its own stream is
    /// being refused.
    #[serde(rename = "node_id_collision")]
    NodeIdCollision {
        node_id: String,
        #[serde(with = "rfc3339")]
        since: i64,
    },
}

impl Issue {
    fn since(&self) -> i64 {
        match self {
            Issue::ReplicaAhead { since, .. }
            | Issue::HistoryDiverged { since, .. }
            | Issue::IdentityMismatch { since, .. }
            | Issue::DeletionRefused { since, .. }
            | Issue::IdentityFileMissing { since, .. }
            | Issue::IdentityFileChanged { since, .. }
            | Issue::NodeIdCollision { since, .. } => *since,
        }
    }

    fn set_since(&mut self, value: i64) {
        match self {
            Issue::ReplicaAhead { since, .. }
            | Issue::HistoryDiverged { since, .. }
            | Issue::IdentityMismatch { since, .. }
            | Issue::DeletionRefused { since, .. }
            | Issue::IdentityFileMissing { since, .. }
            | Issue::IdentityFileChanged { since, .. }
            | Issue::NodeIdCollision { since, .. } => *since = value,
        }
    }
}

/// The open issues of this node, keyed so that raising the same condition
/// twice updates one entry rather than appending a second.
///
/// One store for every role: a master raises identity-file and collision
/// issues, a replica raises those plus the follower ones. Whether anything is
/// open is what turns a node `halted`, and the list is what the health route
/// shows under `issues`.
pub struct Issues {
    inner: Mutex<BTreeMap<String, Issue>>,
}

impl Issues {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    /// Key for a repository-scoped issue, so a repository has at most one
    /// lock issue whichever kind it is.
    pub fn repository_key(collection_id: &str, tenant_id: &str) -> String {
        format!("repository:{}/{}", collection_id, tenant_id)
    }

    /// Key for a master-side node id collision, one per colliding name.
    pub fn collision_key(node_id: &str) -> String {
        format!("node_id_collision:{}", node_id)
    }

    /// Opens `issue` under `key`, or refreshes it. `since` is preserved from
    /// the entry already open under that key, so an issue that keeps being
    /// re-raised on every pass still reports when it first appeared. Returns
    /// whether the issue is *new*, so a caller can log the first occurrence
    /// at error and the repeats at debug.
    pub fn raise(&self, key: &str, mut issue: Issue) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };

        let new = match inner.get(key) {
            Some(existing) => {
                issue.set_since(existing.since());

                false
            }
            None => true,
        };

        inner.insert(key.to_string(), issue);

        new
    }

    /// Closes the issue under `key`, if any. Returns whether one was open,
    /// so a caller can log the recovery exactly once.
    pub fn clear(&self, key: &str) -> bool {
        self.inner
            .lock()
            .map(|mut inner| inner.remove(key).is_some())
            .unwrap_or(false)
    }

    pub fn get(&self, key: &str) -> Option<Issue> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.get(key).cloned())
    }

    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .map(|inner| inner.is_empty())
            .unwrap_or(true)
    }

    /// How many open issues have keys starting with `prefix`.
    pub fn count_with_prefix(&self, prefix: &str) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.keys().filter(|key| key.starts_with(prefix)).count())
            .unwrap_or(0)
    }

    /// Every open issue, oldest first.
    pub fn all(&self) -> Vec<Issue> {
        let mut issues: Vec<Issue> = self
            .inner
            .lock()
            .map(|inner| inner.values().cloned().collect())
            .unwrap_or_default();

        issues.sort_by_key(|issue| issue.since());

        issues
    }
}

impl Default for Issues {
    fn default() -> Self {
        Self::new()
    }
}

/// Issue keys that are not repository-scoped.
const ISSUE_IDENTITY_MISMATCH: &str = "identity_mismatch";
const ISSUE_DELETION_REFUSED: &str = "deletion_refused";
const ISSUE_IDENTITY_FILE: &str = "identity_file";
const ISSUE_NODE_ID_COLLISION: &str = "node_id_collision";

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

/// What [`ReplicationIdentity::verify_on_disk`] found wrong.
#[derive(Debug)]
pub enum IdentityDrift {
    Missing {
        path: String,
    },
    Changed {
        path: String,
        expected: String,
        found: String,
    },
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

    /// Re-reads the identity file and checks it still says what this process
    /// loaded at startup.
    ///
    /// The identity is loaded once, at boot, from inside `repos_path`. That
    /// makes it a canary for the store itself: if the directory this node
    /// serves is unmounted, swapped, or emptied while the process runs, the
    /// file goes with it — and a listing scanned from what is left would be
    /// *complete*, *empty*, and, on a master, an instruction to every
    /// replica to delete everything. Every rescan therefore re-checks the
    /// file and marks the scan incomplete when it is missing or different,
    /// which is the one signal replicas never infer deletions from.
    ///
    /// A node with no identity in memory (standalone, or a replica that has
    /// not pinned yet) has nothing to verify and always passes.
    pub fn verify_on_disk(&self) -> Result<(), IdentityDrift> {
        let Some(expected) = self.current() else {
            return Ok(());
        };

        let path = self.path.display().to_string();

        match Self::read_file(&self.path) {
            Ok(Some(file)) if file.identity == expected => Ok(()),
            Ok(Some(file)) => Err(IdentityDrift::Changed {
                path,
                expected,
                found: file.identity,
            }),
            Ok(None) => Err(IdentityDrift::Missing { path }),
            // Unreadable or malformed is treated as missing: either way the
            // store cannot vouch for itself right now.
            Err(err) => {
                tracing::warn!(err = %err, "replication identity file cannot be read");

                Err(IdentityDrift::Missing { path })
            }
        }
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
        generate_hex(IDENTITY_BYTES)
            .map_err(|err| format!("cannot generate replication identity: {}", err))
    }
}

/// `bytes` random bytes from the OS, hex-encoded lowercase.
fn generate_hex(bytes: usize) -> Result<String, String> {
    let mut buffer = vec![0_u8; bytes];

    getrandom::fill(&mut buffer).map_err(|err| err.to_string())?;

    Ok(buffer.iter().map(|byte| format!("{:02x}", byte)).collect())
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
    /// When the last rescan finished, whatever its outcome. Rate-limits the
    /// rescans an incomplete index keeps asking for.
    last_scan: Option<Instant>,
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
    /// Held for the duration of a disk walk, so concurrent first-use
    /// snapshots (every replica polling a master that just booted) run one
    /// scan and share it rather than each walking the disk.
    scan_lock: Mutex<()>,
    /// Re-verified on every rescan — see
    /// [`ReplicationIdentity::verify_on_disk`].
    identity: Arc<ReplicationIdentity>,
    issues: Arc<Issues>,
}

impl RepositoryIndex {
    pub fn new(config: &Config, identity: Arc<ReplicationIdentity>, issues: Arc<Issues>) -> Self {
        Self {
            repos_path: config.server.repos_path.clone(),
            inner: Mutex::new(IndexInner {
                heads: HashMap::new(),
                complete: false,
                scanned: false,
                sequence: 0,
                last_scan: None,
            }),
            scan_lock: Mutex::new(()),
            identity,
            issues,
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
    /// rescan — but no more often than [`INCOMPLETE_RESCAN_MIN_SECS`], so a
    /// fault that does not clear cannot turn every probe into a disk walk.
    /// Call from the blocking pool.
    pub fn snapshot(&self) -> RepositoryScan {
        if self.needs_scan() {
            // Serialise: whoever gets the lock first scans, and everyone
            // queued behind finds the fresh result and skips.
            let _scanning = self.scan_lock.lock();

            if self.needs_scan() {
                self.rescan_locked();
            }
        }

        // The identity canary is cheap (one small file read) and it is the
        // only thing that can tell a listing served from a complete,
        // up-to-date index that the store underneath has since vanished. So
        // it runs on every listing, not only on the slow rescan cadence: a
        // master answers a replica's poll from memory, and ten minutes is
        // long enough for every replica to have acted on an empty listing.
        let identity_intact = self.verify_identity_file();

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
            complete: inner.complete && identity_intact,
        }
    }

    fn needs_scan(&self) -> bool {
        self.inner
            .lock()
            .map(|inner| {
                if !inner.scanned {
                    return true;
                }

                if inner.complete {
                    return false;
                }

                inner
                    .last_scan
                    .map(|at| at.elapsed().as_secs() >= INCOMPLETE_RESCAN_MIN_SECS)
                    .unwrap_or(true)
            })
            .unwrap_or(true)
    }

    /// Walks the disk and folds the result into the index. **Blocking.**
    ///
    /// The identity file is checked first, every time. A scan that finds
    /// the file missing or changed is folded in as *incomplete* whatever the
    /// walk itself found, and the condition is raised as an issue: the store
    /// under this process is not the store it started with, and nothing
    /// scanned from it may be used to infer a deletion.
    ///
    /// Scans never overlap: the merge below trusts `announced_at` against the
    /// sequence its own scan started at, and a second scan folding in
    /// meanwhile resets entries it saw on disk to `0` — after which the first
    /// one, finishing later, would drop an entry announced while it walked.
    pub fn rescan(&self) {
        let _scanning = self.scan_lock.lock();

        self.rescan_locked();
    }

    /// [`RepositoryIndex::rescan`], for a caller already holding `scan_lock`.
    fn rescan_locked(&self) {
        let started_at = match self.inner.lock() {
            Ok(inner) => inner.sequence,
            Err(_) => return,
        };

        let identity_intact = self.verify_identity_file();

        let mut scan = GitReplication::list_repositories(&self.repos_path);

        scan.complete &= identity_intact;

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
        inner.last_scan = Some(Instant::now());

        tracing::debug!(
            repositories = inner.heads.len(),
            complete = inner.complete,
            "repository index rescanned"
        );
    }

    /// Runs [`ReplicationIdentity::verify_on_disk`], keeping the
    /// `identity_file` issue in step with the result. Returns whether the
    /// file is intact.
    fn verify_identity_file(&self) -> bool {
        let now = chrono::Utc::now().timestamp();

        match self.identity.verify_on_disk() {
            Ok(()) => {
                if self.issues.clear(ISSUE_IDENTITY_FILE) {
                    tracing::info!("replication identity file is back, listing is complete again");
                }

                true
            }
            Err(IdentityDrift::Missing { path }) => {
                let new = self.issues.raise(
                    ISSUE_IDENTITY_FILE,
                    Issue::IdentityFileMissing {
                        path: path.clone(),
                        since: now,
                    },
                );

                // Loud once, then quiet: this runs on every listing, and a
                // fault that lasts an hour should not log an error a second.
                if new {
                    tracing::error!(
                        path = %path,
                        "replication identity file is missing: the repository store is not the one this process started with; \
                         serving the listing as incomplete so no replica infers deletions from it"
                    );
                } else {
                    tracing::debug!(path = %path, "replication identity file still missing");
                }

                false
            }
            Err(IdentityDrift::Changed {
                path,
                expected,
                found,
            }) => {
                let new = self.issues.raise(
                    ISSUE_IDENTITY_FILE,
                    Issue::IdentityFileChanged {
                        path: path.clone(),
                        expected: expected.clone(),
                        found: found.clone(),
                        since: now,
                    },
                );

                if new {
                    tracing::error!(
                        path = %path,
                        expected = %expected,
                        found = %found,
                        "replication identity file changed under this process; \
                         serving the listing as incomplete so no replica infers deletions from it"
                    );
                } else {
                    tracing::debug!(path = %path, "replication identity file still changed");
                }

                false
            }
        }
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
    /// `repositories`, `pending`, `sync` and `poll_interval_secs` are
    /// whatever it volunteered this time.
    pub fn note_request(
        &self,
        node_id: &str,
        repositories: Option<usize>,
        pending: Option<usize>,
        sync: Option<String>,
        poll_interval_secs: Option<u64>,
    ) {
        let now = chrono::Utc::now().timestamp();
        let mut presence = self.entry(node_id);

        presence.last_contact_at = Some(now);

        // Only stamp `reported_at` when something was actually reported, so
        // the timestamp always describes the numbers sitting next to it.
        if repositories.is_some() || pending.is_some() || sync.is_some() {
            presence.repositories = repositories;
            presence.pending_repositories = pending;
            presence.sync = sync;
            presence.reported_at = Some(now);
        }

        // Kept rather than overwritten when a request omits it: the cadence
        // is configuration, so the last value stated stays true until the
        // replica states another one, and a request that happens not to
        // carry the header must not widen this row's threshold back to the
        // floor.
        if poll_interval_secs.is_some() {
            presence.poll_interval_secs = poll_interval_secs;
        }
    }

    pub fn note_pack_delivered(&self, node_id: &str) {
        let now = chrono::Utc::now().timestamp();
        let mut presence = self.entry(node_id);

        presence.packs_delivered += 1;
        presence.last_contact_at = Some(now);
    }

    /// Marks a replica's stream open, unless another *process* already
    /// holds a stream under that node id.
    ///
    /// The instance token is what tells the two apart: a replica that
    /// reconnects after a drop presents the same token and simply replaces
    /// its old connection, while a second replica configured with the same
    /// name presents a different one and is refused. Refused rather than
    /// merged because a roster row shared by two nodes reports the state of
    /// whichever spoke last, and an operator reading it would never learn
    /// that the other had gone.
    pub fn stream_opened(&self, node_id: &str, instance: &str) -> Result<(), StreamCollision> {
        let now = chrono::Utc::now().timestamp();
        let mut presence = self.entry(node_id);

        if presence.stream_connected {
            if let Some(holder) = &presence.instance {
                if holder != instance {
                    // A refusal long after the previous one starts a fresh
                    // run, so a collision that was fixed and comes back
                    // months later is not reported as if it had been open
                    // the whole time. The window is the master's own issue
                    // expiry: a collision still in progress re-dials well
                    // inside it, since a refused replica's backoff caps at
                    // 60 s.
                    if presence
                        .last_collision_at
                        .map(|last| now - last > COLLISION_EXPIRY_SECS)
                        .unwrap_or(true)
                    {
                        presence.collision_since = None;
                    }

                    let since = *presence.collision_since.get_or_insert(now);

                    presence.last_collision_at = Some(now);

                    return Err(StreamCollision {
                        node_id: node_id.to_string(),
                        // A replica that restarted collides with the socket
                        // of its own previous process until this node
                        // notices that socket is dead, so a refusal is only
                        // worth reporting once it has outlived that window.
                        // The stream is refused either way: the two cases
                        // are indistinguishable from here, and letting the
                        // newcomer in would merge two nodes into one roster
                        // row.
                        persisting: now - since >= COLLISION_REPORT_AFTER_SECS as i64,
                        since,
                    });
                }
            }
        }

        presence.stream_connected = true;
        presence.connected_at = Some(now);
        presence.last_contact_at = Some(now);
        presence.instance = Some(instance.to_string());
        presence.collision_since = None;

        Ok(())
    }

    /// Marks a replica's stream closed — but only if `instance` is the one
    /// holding it. A reconnect that replaced an older connection must not be
    /// marked closed when the older connection's task finally ends.
    pub fn stream_closed(&self, node_id: &str, instance: &str) {
        let mut presence = self.entry(node_id);

        if presence.instance.as_deref() != Some(instance) {
            return;
        }

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
                sync: None,
                reported_at: None,
                poll_interval_secs: None,
                instance: None,
                last_collision_at: None,
                collision_since: None,
            })
    }

    /// Closes collision issues whose node id has not been refused for
    /// [`COLLISION_EXPIRY_SECS`]. Called when health is built, which is the
    /// one moment the answer matters; nothing else on a master would
    /// otherwise ever close them, since the surviving replica's stream is
    /// already open and never re-opens.
    pub fn expire_collisions(&self, issues: &Issues, now: i64) {
        for entry in self.replicas.iter() {
            let Some(last) = entry.value().last_collision_at else {
                continue;
            };

            if now - last > COLLISION_EXPIRY_SECS
                && issues.clear(&Issues::collision_key(entry.key()))
            {
                tracing::info!(node_id = %entry.key(), "node id collision cleared");
            }
        }
    }
}

/// A stream was refused because its node id is already connected from
/// another process.
#[derive(Debug)]
pub struct StreamCollision {
    pub node_id: String,
    /// Whether this run of refusals has outlived a restarting replica's own
    /// lingering socket, and is therefore worth reporting as an issue rather
    /// than only refusing.
    pub persisting: bool,
    /// When this run of refusals began.
    pub since: i64,
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
    /// Worker passes in a row that ended in failure; `stalled` past
    /// [`STALLED_AFTER_FAILURES`].
    consecutive_failures: AtomicU32,
    /// When a pass last ended with nothing failed. `0` for never.
    last_success_unix: AtomicI64,
    /// The node's open issues, which are what make it `halted`.
    issues: Arc<Issues>,
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
    pub fn new(config: &Config, issues: Arc<Issues>) -> Self {
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
            consecutive_failures: AtomicU32::new(0),
            last_success_unix: AtomicI64::new(0),
            issues,
            master_view: Mutex::new(MasterView::default()),
        }
    }

    /// Records how a worker pass ended. A failed pass counts up towards
    /// `stalled`; a clean one resets the count and stamps `last_success_at`.
    pub fn note_pass(&self, failed: bool) {
        if failed {
            self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        } else {
            self.consecutive_failures.store(0, Ordering::Relaxed);
            self.last_success_unix
                .store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
        }
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    pub fn last_success(&self) -> Option<i64> {
        match self.last_success_unix.load(Ordering::Relaxed) {
            0 => None,
            seconds => Some(seconds),
        }
    }

    /// Repositories locked out of replication pending an operator.
    pub fn locked_repositories(&self) -> usize {
        self.issues.count_with_prefix("repository:")
    }

    /// The one-word verdict on how following is going — see [`SyncStatus`].
    /// Any open issue is `halted`, since every issue is by definition a
    /// condition this node will not resolve on its own.
    pub fn sync_status(&self) -> SyncStatus {
        if !self.issues.is_empty() {
            return SyncStatus::Halted;
        }

        if self.consecutive_failures() >= STALLED_AFTER_FAILURES {
            return SyncStatus::Stalled;
        }

        if self.pending() > 0 || self.last_success().is_none() {
            return SyncStatus::Lagging;
        }

        SyncStatus::Synced
    }

    pub fn set_identity_verified(&self, verified: bool) {
        self.identity_verified.store(verified, Ordering::Relaxed);
    }

    pub fn identity_verified(&self) -> bool {
        self.identity_verified.load(Ordering::Relaxed)
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
    /// Repositories refused because they are locked out pending an operator.
    /// Neither progress nor failure: they are not retried by this loop, and
    /// they must not make a pass look stalled when everything else landed.
    locked: usize,
}

/// How one repository's sync ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncOutcome {
    /// The repository is where the upstream said it should be.
    Synced,
    /// The repository was left as it is and locked out of replication —
    /// ahead of, or diverged from, the upstream. An issue was raised.
    Locked,
    /// A transient failure; the repository stays pending.
    Failed,
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
    /// A random token for this *process*, sent with the node id so the
    /// master can tell this replica reconnecting from another replica
    /// wearing the same name — see [`INSTANCE_HEADER`].
    instance: String,
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
    ) -> Result<Self, String> {
        let node_id = replication.node_id().to_string();
        let instance = generate_hex(INSTANCE_BYTES).map_err(|err| {
            tracing::error!(err = %err, "cannot generate replication instance token");

            err
        })?;

        let transfer_client = Client::builder()
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .read_timeout(Duration::from_secs(STATE_TIMEOUT_SECS))
            .build()
            .map_err(|err| err.to_string())?;

        let pack_client = Client::builder()
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .read_timeout(Duration::from_secs(PACK_READ_TIMEOUT_SECS))
            .build()
            .map_err(|err| err.to_string())?;

        let stream_client = Client::builder()
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .read_timeout(Duration::from_secs(EVENT_READ_TIMEOUT_SECS))
            .build()
            .map_err(|err| err.to_string())?;

        let master_url_display = redact_url(&master_url);

        Ok(Self {
            state,
            replication,
            master_url,
            master_url_display,
            node_id,
            instance,
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
        // Repositories this process has landed since it started. What the
        // bootstrap gate's stall rule keys on: a cold node that has landed
        // *nothing* stays gated however long it stalls.
        let mut landed_total = 0_usize;

        loop {
            let mut reconcile_failed = false;

            if self.full_reconcile_requested.swap(false, Ordering::SeqCst) {
                match self.reconcile().await {
                    Ok(()) => reconciled_once = true,
                    Err(err) => {
                        tracing::warn!(err = %err, "replication reconcile failed, will retry");

                        self.state.replica_status.record_master_error(err.clone());

                        reconcile_failed = true;

                        // Put the request back so the next pass retries it
                        // rather than waiting for the poll timer to come round.
                        self.full_reconcile_requested.store(true, Ordering::SeqCst);
                    }
                }
            }

            let pass = self.drain_pending().await;

            landed_total += pass.succeeded;

            // A pass "failed" when the reconcile did, or when it tried to
            // sync something and nothing landed. Locked repositories are not
            // attempts: they are waiting on a human, not on a retry.
            let attempted_unlocked = pass.attempted.saturating_sub(pass.locked);
            let drain_failed = attempted_unlocked > 0 && pass.succeeded == 0;
            let pass_failed = reconcile_failed || drain_failed;

            self.state.replica_status.note_pass(pass_failed);

            if drain_failed {
                stalled_passes += 1;
            } else {
                stalled_passes = 0;
            }

            // The bootstrap gate lifts once a cold replica has *caught up*,
            // not once it has merely learned what it is missing — a node
            // that knows about a thousand repositories and holds none would
            // answer 404 for all of them. A master that holds nothing leaves
            // nothing pending, so that case opens at once. Entries that are
            // locked are not "pending" for this purpose: they are held
            // repositories an operator has to look at, and holding traffic
            // for them would never end.
            //
            // The one exception is a catch-up that has stopped making
            // progress *after* landing something: two passes in a row with
            // nothing new means waiting longer will not help, and the
            // repositories that did land are better served than refused. A
            // node that has landed nothing at all never takes that exit — an
            // empty replica answering 404 for everything is the exact state
            // the gate exists to prevent, however long the master's packs
            // keep failing.
            if reconciled_once && !self.state.replica_status.is_bootstrapped() {
                if self.pending.is_empty() {
                    self.state.replica_status.mark_bootstrapped();
                } else if stalled_passes >= 2 && landed_total > 0 {
                    tracing::error!(
                        pending = self.pending.len(),
                        landed = landed_total,
                        "replica catch-up has stalled, serving what it holds while retrying"
                    );

                    self.state.replica_status.mark_bootstrapped();
                } else if stalled_passes >= 2 {
                    tracing::error!(
                        pending = self.pending.len(),
                        "cold replica catch-up has stalled with nothing landed, still refusing reads"
                    );
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
            if drain_failed {
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

            // A locked repository is re-examined only when the upstream has
            // moved since it was locked. Re-queueing it every poll would
            // download the same refused pack every poll; leaving it alone
            // forever would miss the operator fixing the upstream — or
            // taking the other exit, removing the local copy, which the
            // index cannot see (nothing announced it) and so is checked on
            // disk here, one `stat` per locked repository per reconcile.
            if self.is_locked_at(
                &repository.collection_id,
                &repository.tenant_id,
                &repository.head_sha,
            ) {
                let removed_by_operator = !self
                    .repo_path(&repository.collection_id, &repository.tenant_id)
                    .join(".git")
                    .exists();

                if !removed_by_operator {
                    continue;
                }

                tracing::info!(
                    collection_id = %repository.collection_id,
                    tenant_id = %repository.tenant_id,
                    "locked repository was removed locally, cloning it afresh from the master"
                );

                self.unlock(&repository.collection_id, &repository.tenant_id);

                self.state
                    .repository_index
                    .remove(&repository.collection_id, &repository.tenant_id);
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
            let deletions: Vec<(String, String, String)> = local
                .keys()
                .filter(|key| !remote_keys.contains(*key))
                .filter_map(|key| {
                    key.split_once('/').map(|(collection_id, tenant_id)| {
                        (
                            key.clone(),
                            collection_id.to_string(),
                            tenant_id.to_string(),
                        )
                    })
                })
                .collect();

            // The mass-deletion guard. A replica deleting what its master
            // deleted is ordinary replication; a replica deleting most of
            // itself because its master's listing went empty is the one
            // thing this feature must never do on its own. The line between
            // them is drawn at half: refuse, report, and keep everything.
            // Nothing clears a refused deletion on its own — a restarted
            // replica still holds what it held — so an operator who really
            // is removing most tenants accepts it by turning the guard off
            // (`replication.deletion_guard = false`) for one restart.
            let mass_deletion = self.replication.deletion_guard
                && local.len() >= DELETION_GUARD_MINIMUM_HELD
                && deletions.len() * 2 > local.len();

            if mass_deletion {
                let new = self.state.issues.raise(
                    ISSUE_DELETION_REFUSED,
                    Issue::DeletionRefused {
                        would_delete: deletions.len(),
                        held: local.len(),
                        since: chrono::Utc::now().timestamp(),
                    },
                );

                if new {
                    tracing::error!(
                        would_delete = deletions.len(),
                        held = local.len(),
                        master = %self.master_url_display,
                        "master listing would delete more than half of this replica's repositories; \
                         refusing the deletions and keeping every local copy until an operator intervenes \
                         (set replication.deletion_guard = false and restart to accept them)"
                    );
                } else {
                    tracing::debug!(
                        would_delete = deletions.len(),
                        held = local.len(),
                        "master listing still asks for a mass deletion, still refusing"
                    );
                }
            } else {
                if self.state.issues.clear(ISSUE_DELETION_REFUSED) {
                    tracing::info!(
                        "master listing no longer asks for a mass deletion, deletions resume"
                    );
                }

                for (key, collection_id, tenant_id) in deletions {
                    self.pending
                        .insert(key, (collection_id, tenant_id, SyncTarget::Deleted));
                }
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
        let locked = Arc::new(AtomicUsize::new(0));
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
            let locked = locked.clone();

            tasks.spawn(async move {
                let _permit = permit;

                match follower
                    .sync_repository(&collection_id, &tenant_id, target)
                    .await
                {
                    SyncOutcome::Synced => {
                        succeeded.fetch_add(1, Ordering::Relaxed);
                    }
                    SyncOutcome::Locked => {
                        locked.fetch_add(1, Ordering::Relaxed);
                    }
                    SyncOutcome::Failed => {}
                }
            });
        }

        while tasks.join_next().await.is_some() {}

        self.state.replica_status.set_pending(self.pending.len());

        let pass = SyncPass {
            attempted,
            succeeded: succeeded.load(Ordering::Relaxed),
            locked: locked.load(Ordering::Relaxed),
        };

        tracing::info!(
            repositories = pass.attempted,
            succeeded = pass.succeeded,
            locked = pass.locked,
            "replication catch-up finished"
        );

        pass
    }

    /// Brings one repository to the state the master reported, or removes it.
    ///
    /// The worker counts the outcomes to tell a pass that made progress from
    /// one that achieved nothing — a failed sync re-queues itself, so the
    /// size of the pending set cannot answer that on its own — and to keep
    /// locked repositories out of both counts.
    ///
    /// **A refused pack never destroys anything.** A replica whose history
    /// is ahead of, or has diverged from, its upstream keeps its copy, keeps
    /// serving it, locks that one repository out of replication, and raises
    /// an issue that turns the node `halted`. Discarding and re-cloning was
    /// the previous behaviour and is exactly what this codebase must not do
    /// on its own: the replica cannot know whether the upstream or itself is
    /// the one holding the history that matters, and only a human can. The
    /// runbook is in REPLICATION.md.
    async fn sync_repository(
        &self,
        collection_id: &str,
        tenant_id: &str,
        target: SyncTarget,
    ) -> SyncOutcome {
        let head_sha = match target {
            SyncTarget::Deleted => {
                self.delete_repository(collection_id, tenant_id).await;

                return SyncOutcome::Synced;
            }
            SyncTarget::At { head_sha } => head_sha,
        };

        match self
            .fetch_and_apply(collection_id, tenant_id, &head_sha)
            .await
        {
            Ok(SyncOutcome::Synced) => {
                self.unlock(collection_id, tenant_id);

                SyncOutcome::Synced
            }
            Ok(outcome) => outcome,
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

                SyncOutcome::Failed
            }
        }
    }

    /// Downloads the delta packfile and imports it.
    ///
    /// An incremental fetch is *always* the right answer, and never a
    /// gamble: a replica keeps full history (the commit routes read it), so
    /// the delta is by construction a subset of a full clone, no matter how
    /// far behind the replica has fallen. A repository absent locally asks
    /// for everything.
    ///
    /// Before any byte is requested, the announced head is related to the
    /// local one from local objects alone: a replica that is *ahead* already
    /// holds the announced commit, and finding that out here avoids asking
    /// the upstream for a pack it cannot build incrementally (it does not
    /// know the replica's newer head) and that would be refused on arrival.
    async fn fetch_and_apply(
        &self,
        collection_id: &str,
        tenant_id: &str,
        head_sha: &str,
    ) -> Result<SyncOutcome, String> {
        let repo_path = self.repo_path(collection_id, tenant_id);

        let probe_path = repo_path.clone();
        let probe_sha = head_sha.to_string();

        let (have, relation) = run_blocking(move || {
            Ok((
                GitReplication::head_sha(&probe_path),
                GitReplication::relation_to(&probe_path, &probe_sha),
            ))
        })
        .await
        .map_err(|err| err.to_string())?;

        match relation {
            HeadRelation::Same => return Ok(SyncOutcome::Synced),
            HeadRelation::Ahead { local } => {
                return Ok(self.lock_ahead(collection_id, tenant_id, &local, head_sha));
            }
            HeadRelation::Diverged { local } => {
                return Ok(self.lock_diverged(collection_id, tenant_id, &local, head_sha));
            }
            HeadRelation::Unknown => {}
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

                // Recorded the moment the ref has moved, before the working
                // tree is mirrored: every read already answers from the new
                // head, so this node's listing — and the cascade below — must
                // not trail it by the length of a checkout. A listing behind
                // the reads is what let a sync finishing late overwrite a
                // newer head a rescan had already found.
                //
                // Cascade: a replica serves the replication surface too, so
                // anything following *this* node learns about the change
                // without waiting for its own poll interval.
                self.state
                    .replication
                    .repository_updated(collection_id, tenant_id, &to);

                self.state
                    .maintenance
                    .schedule(&lock_key, repo_path.clone(), lock.clone());

                // The courtesy that follows, still under the lock so it never
                // races a maintenance pass or the next sync of this
                // repository. It cannot fail the sync: the ref has moved.
                if self.state.config.server.checkout_files {
                    let mirror_repo_path = repo_path.clone();

                    if let Err(err) = run_blocking(move || {
                        Ok(GitReplication::mirror_working_tree(&mirror_repo_path))
                    })
                    .await
                    {
                        tracing::warn!(
                            collection_id = %collection_id,
                            tenant_id = %tenant_id,
                            err = %err,
                            "cannot mirror the working tree onto the replicated head"
                        );
                    }
                }

                Ok(SyncOutcome::Synced)
            }

            PackApply::UpToDate => Ok(SyncOutcome::Synced),

            PackApply::Ahead { local, remote } => {
                Ok(self.lock_ahead(collection_id, tenant_id, &local, &remote))
            }

            PackApply::Diverged { local, remote } => {
                Ok(self.lock_diverged(collection_id, tenant_id, &local, &remote))
            }
        }
    }

    /// Locks a repository out of replication because this replica is ahead
    /// of its upstream for it. Loud on purpose, and always `Locked`.
    fn lock_ahead(
        &self,
        collection_id: &str,
        tenant_id: &str,
        local: &str,
        remote: &str,
    ) -> SyncOutcome {
        tracing::error!(
            collection_id = %collection_id,
            tenant_id = %tenant_id,
            local = %local,
            remote = %remote,
            master = %self.master_url_display,
            "replica is AHEAD of its master for this repository (the announced head is an ancestor of the local one); \
             keeping and serving the local copy, replication of this repository is suspended until the master moves past it \
             or an operator removes the local copy — see REPLICATION.md"
        );

        self.state.issues.raise(
            &Issues::repository_key(collection_id, tenant_id),
            Issue::ReplicaAhead {
                collection_id: collection_id.to_string(),
                tenant_id: tenant_id.to_string(),
                local: local.to_string(),
                remote: remote.to_string(),
                since: chrono::Utc::now().timestamp(),
            },
        );

        SyncOutcome::Locked
    }

    /// Locks a repository out of replication because its history and the
    /// upstream's no longer share a line. Loud on purpose, and always
    /// `Locked`.
    fn lock_diverged(
        &self,
        collection_id: &str,
        tenant_id: &str,
        local: &str,
        remote: &str,
    ) -> SyncOutcome {
        tracing::error!(
            collection_id = %collection_id,
            tenant_id = %tenant_id,
            local = %local,
            remote = %remote,
            master = %self.master_url_display,
            "replica history DIVERGED from its master for this repository (neither head descends from the other); \
             keeping and serving the local copy, replication of this repository is suspended until an operator \
             decides which history to keep — see REPLICATION.md"
        );

        self.state.issues.raise(
            &Issues::repository_key(collection_id, tenant_id),
            Issue::HistoryDiverged {
                collection_id: collection_id.to_string(),
                tenant_id: tenant_id.to_string(),
                local: local.to_string(),
                remote: remote.to_string(),
                since: chrono::Utc::now().timestamp(),
            },
        );

        SyncOutcome::Locked
    }

    /// Clears a repository's lock after it synced. Logged once, since an
    /// operator who acted on the issue wants to see it close.
    fn unlock(&self, collection_id: &str, tenant_id: &str) {
        if self
            .state
            .issues
            .clear(&Issues::repository_key(collection_id, tenant_id))
        {
            tracing::info!(
                collection_id = %collection_id,
                tenant_id = %tenant_id,
                "repository converged again, replication lock lifted"
            );
        }
    }

    /// Whether a repository is locked *and* the upstream still announces the
    /// same head it was locked against. A different head means the upstream
    /// moved, which is one of the two ways a lock is meant to clear, so the
    /// repository is worth another look.
    fn is_locked_at(&self, collection_id: &str, tenant_id: &str, announced: &str) -> bool {
        match self
            .state
            .issues
            .get(&Issues::repository_key(collection_id, tenant_id))
        {
            Some(Issue::ReplicaAhead { remote, .. })
            | Some(Issue::HistoryDiverged { remote, .. }) => remote == announced,
            _ => false,
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

        // Renamed away before it is deleted, so a read racing the deletion
        // finds no tenant rather than a half-removed repository.
        let remove_repo_path = repo_path.clone();

        match run_blocking(move || GitStaging::remove(&remove_repo_path)).await {
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
        // When the master first answered 409 to this node id. A collision
        // is only reported as an issue once it has outlived the master's
        // chance to notice this process's *own* previous socket dying.
        let mut collision_since: Option<Instant> = None;

        loop {
            let attempt = self.request(&self.stream_client, &url).send().await;

            match attempt {
                Ok(response) if response.status().is_success() => {
                    tracing::info!(master = %self.master_url_display, "replication notification stream connected");

                    backoff_ms = self.replication.reconnect_backoff_ms;
                    collision_since = None;

                    if self.state.issues.clear(ISSUE_NODE_ID_COLLISION) {
                        tracing::info!(node_id = %self.node_id, "node id collision cleared");
                    }

                    self.state.replica_status.set_stream_connected(true);

                    self.request_full_reconcile();

                    self.consume_events(response).await;

                    self.state.replica_status.set_stream_connected(false);

                    tracing::warn!("replication notification stream closed");
                }

                Ok(response) if response.status() == reqwest::StatusCode::CONFLICT => {
                    let since = *collision_since.get_or_insert_with(Instant::now);

                    // Loud only once it persists, for the same reason the
                    // master reports it only then: this process restarting
                    // is refused by its own previous socket until the master
                    // reaps it, which is a normal restart rather than a
                    // misconfiguration. Polling converges throughout either
                    // way.
                    if since.elapsed().as_secs() >= COLLISION_REPORT_AFTER_SECS {
                        tracing::error!(
                            node_id = %self.node_id,
                            master = %self.master_url_display,
                            "master refused the notification stream: another process is connected under this node id; \
                             still converging by polling, but every replica needs a unique replication.node_id"
                        );

                        self.state.issues.raise(
                            ISSUE_NODE_ID_COLLISION,
                            Issue::NodeIdCollision {
                                node_id: self.node_id.clone(),
                                since: chrono::Utc::now().timestamp(),
                            },
                        );
                    } else {
                        tracing::warn!(
                            node_id = %self.node_id,
                            master = %self.master_url_display,
                            "master still holds a stream under this node id, retrying shortly \
                             (expected for a moment after this replica restarts)"
                        );
                    }
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

                // A deletion is never acted on from a hint. The frame asks
                // for a reconcile, and the listing that reconcile fetches is
                // what decides — under the mass-deletion guard, which only
                // exists there. Acting on the frame directly would let a
                // burst of deletion frames do exactly what the guard refuses
                // a listing for. One extra state fetch per deletion is the
                // whole cost; an update frame, which destroys nothing, still
                // queues its repository directly.
                tracing::debug!(
                    collection_id = %collection_id,
                    tenant_id = %tenant_id,
                    "repository.deleted frame received, reconciling to confirm it"
                );

                self.request_full_reconcile();
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
            Ok(()) => {
                status.set_identity_verified(true);

                if self.state.issues.clear(ISSUE_IDENTITY_MISMATCH) {
                    tracing::info!("master states the pinned identity again, syncing resumes");
                }
            }
            Err(err) => {
                status.set_identity_verified(false);
                status.record_master_error(err.clone());

                tracing::error!(err = %err, "refusing to sync from this master");

                if let (Some(pinned), Some(stated)) = (self.state.identity.current(), stated) {
                    if pinned != stated {
                        self.state.issues.raise(
                            ISSUE_IDENTITY_MISMATCH,
                            Issue::IdentityMismatch {
                                pinned,
                                stated: stated.to_string(),
                                since: chrono::Utc::now().timestamp(),
                            },
                        );
                    }
                }
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
    /// master's roster would then show a replica flickering in and out. The
    /// polling cadence rides here too, rather than on the reporting request
    /// below: it is fixed configuration rather than a number that moves, and
    /// stamping it on every request means the master can size this replica's
    /// silence threshold from its very first contact — including one that
    /// only ever opened a notification stream, which is precisely the node
    /// the threshold has to judge once that stream drops.
    fn request(&self, client: &Client, url: &str) -> reqwest::RequestBuilder {
        client
            .get(url)
            .bearer_auth(&self.replication.secret)
            .header(NODE_ID_HEADER, &self.node_id)
            .header(INSTANCE_HEADER, &self.instance)
            .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
            .header(
                POLL_INTERVAL_HEADER,
                self.replication.poll_interval_secs.to_string(),
            )
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
            .header(SYNC_HEADER, status.sync_status().as_str())
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
/// `repositories` is passed in rather than counted here because counting can
/// mean touching the filesystem (the index's first use is a directory scan),
/// which belongs on the blocking pool — the caller reads it from
/// `RepositoryIndex::snapshot` there.
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
    let issues = state.issues.all();

    let Some(replication) = &state.config.replication else {
        // Standalone: it accepts writes, so it *is* the write node of a set
        // of one. Reporting that uniformly beats a special case a monitor
        // would have to know about. It has no node id of its own — the
        // field only means something to peers, and it has none — so the
        // bind address stands in.
        let node_id = format!("{}:{}", server.host, server.port);

        return ReplicationHealth {
            protocol: PROTOCOL_VERSION,
            status: NodeStatus::Healthy.as_str().to_string(),
            issues,
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

    let node_id = replication.node_id().to_string();

    if !replication.is_replica() {
        state.replica_registry.expire_collisions(&state.issues, now);

        let issues = state.issues.all();
        let replicas = state.replica_registry.roster();
        let status = master_status(&replicas, issues.is_empty(), now);

        return ReplicationHealth {
            protocol: PROTOCOL_VERSION,
            status: status.as_str().to_string(),
            issues,
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
            replicas,
            replica: None,
            observed_at: now,
            replicas_observed_at: Some(now),
        };
    }

    let status = &state.replica_status;
    let (master_node_id, replicas, replicas_observed_at, last_contact_at, last_error) =
        status.master_snapshot();

    let sync = status.sync_status();

    // A replica's verdict is its own: it does not fold in the roster it
    // caches from the master, which is the master's view and possibly stale.
    let node_status = match sync {
        SyncStatus::Halted => NodeStatus::Halted,
        SyncStatus::Stalled => NodeStatus::Degraded,
        SyncStatus::Synced | SyncStatus::Lagging if !status.is_bootstrapped() => {
            NodeStatus::Degraded
        }
        SyncStatus::Synced | SyncStatus::Lagging => NodeStatus::Healthy,
    };

    ReplicationHealth {
        protocol: PROTOCOL_VERSION,
        status: node_status.as_str().to_string(),
        issues,
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
            sync: sync.as_str().to_string(),
            stream_connected: status.stream_connected(),
            last_reconcile_at: status.last_reconcile(),
            last_success_at: status.last_success(),
            pending_repositories: status.pending(),
            locked_repositories: status.locked_repositories(),
            consecutive_failures: status.consecutive_failures(),
        }),
        observed_at: now,
        replicas_observed_at,
    }
}

/// How long a roster row may go quiet before its silence means something,
/// derived from the cadence that row reported.
///
/// A replica converging by polling with its notification stream down is
/// healthy, and it is heard from once per `poll_interval_secs` — so the
/// threshold has to follow that key, which lives on the replica and is
/// ignored on the node judging it. Hence the derivation from what the replica
/// stated, between a floor (also the fallback for a peer that stated nothing)
/// and a ceiling (so a self-asserted number cannot disable the rule).
///
/// The row is refreshed by *any* request, not only the state poll — a pack
/// download and a health probe stamp it too — so this only ever fires on a
/// replica that has genuinely gone quiet.
pub(crate) fn silent_after_secs(poll_interval_secs: Option<u64>) -> i64 {
    poll_interval_secs
        // Saturating rather than checked, so a cadence too large to multiply
        // reads as the enormous number it is and meets the ceiling — the same
        // answer an ordinary large value gets. Falling back to the floor here
        // instead would make one absurd value alert sooner than a merely
        // excessive one, which is a rule no operator could predict.
        .map(|secs| {
            i64::try_from(secs)
                .unwrap_or(i64::MAX)
                .saturating_mul(REPLICA_SILENT_INTERVALS)
        })
        .unwrap_or(REPLICA_SILENT_MINIMUM_SECS)
        .clamp(REPLICA_SILENT_MINIMUM_SECS, REPLICA_SILENT_MAXIMUM_SECS)
}

/// A master's verdict folds in every replica it knows of, so the one probe
/// an operator points at the master reports the worst thing in the set.
///
/// A replica reporting `halted` makes the master `halted`. One reporting
/// `stalled`, or one whose stream is down and that has not been heard from
/// for [`silent_after_secs`] of its own polling cadence, makes it `degraded`.
/// `lagging` is normal operation and changes nothing. A master's own open
/// issues make it `halted` regardless of its replicas.
fn master_status(replicas: &[ReplicaPresence], no_issues: bool, now: i64) -> NodeStatus {
    if !no_issues {
        return NodeStatus::Halted;
    }

    let mut status = NodeStatus::Healthy;

    for replica in replicas {
        let reported = replica.sync.as_deref().and_then(SyncStatus::parse);

        let verdict = match reported {
            Some(SyncStatus::Halted) => NodeStatus::Halted,
            Some(SyncStatus::Stalled) => NodeStatus::Degraded,
            _ => {
                let threshold = silent_after_secs(replica.poll_interval_secs);

                let silent = replica
                    .last_contact_at
                    .map(|at| now - at > threshold)
                    .unwrap_or(true);

                if !replica.stream_connected && silent {
                    NodeStatus::Degraded
                } else {
                    NodeStatus::Healthy
                }
            }
        };

        status = status.max(verdict);
    }

    status
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
