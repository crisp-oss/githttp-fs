// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Asynchronous, ordered, retried webhook delivery.
//!
//! Webhooks exist so a downstream system (typically a read-optimised SQL
//! mirror of the content) can stay in sync with every commit. Four
//! properties drive the whole design:
//!
//! 1. **Writes are never delayed by the receiver.** The HTTP handler only
//!    *enqueues* a job and returns; actual delivery happens on a background
//!    task. A slow or down receiver cannot slow down the write API.
//! 2. **Per-repository ordering is absolute; repositories run concurrently.**
//!    Each `"{collection_id}/{tenant_id}"` gets its own queue with a single
//!    consumer task, and jobs are enqueued while the tenant write lock is
//!    still held. Queue order therefore equals commit order, and because one
//!    consumer processes jobs strictly one at a time — awaiting every file,
//!    retry and backoff sleep of a job before taking the next — a later
//!    commit's hooks can never overtake an earlier commit's, even if the
//!    earlier one is stuck in retries. A receiver that applies events as
//!    they arrive always converges to the correct state. Different
//!    repositories are different tokio tasks, so one dead receiver never
//!    holds up another repository. The cost is unbounded latency rather than
//!    lost ordering: a recursive folder delete of N files occupies its
//!    repository's queue for N sequential POSTs.
//! 3. **One event per file, never batched.** A commit's change set becomes
//!    one job, and the consumer sends one POST per change in it — so
//!    multi-file operations (revert, rollback, recursive folder delete and
//!    move) fan out to one event per file rather than one summary event.
//! 4. **Failures retry with exponential backoff, then give up loudly.**
//!    After the configured attempts are exhausted the event is dropped and
//!    a CRITICAL log line is emitted — at that point the receiver may be
//!    out of sync and needs reconciliation. Blocking the queue forever on
//!    one poisoned event would be worse: it would silently stall every
//!    subsequent event for the repository.
//!
//! The queue is deliberately unbounded and in-memory: payload volume is
//! bounded by write traffic (which is itself serialised per tenant), and
//! durability across restarts is explicitly out of scope — the downstream
//! system is expected to have a reconciliation path anyway.

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use reqwest::Client;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

use crate::config::{Config, HookEvent, HooksConfig};
use crate::git::{FileChange, GitFiles};
use crate::order;

/// Cap on the exponential backoff exponent to avoid `1 << n` overflow when an
/// operator configures a very high retry count.
const MAX_BACKOFF_EXPONENT: u32 = 20;

/// How many files a replay reads from the repository in one go.
///
/// A replay deliberately does *not* materialise every file's content when the
/// job is built — a whole-repository replay would then hold the entire corpus
/// in memory for as long as delivery takes, which an inter-hook delay can
/// stretch to hours. Reading strictly one file at a time would fix that but
/// re-open the repository per file, so content is fetched in small chunks
/// instead: resident memory is bounded by the chunk, not by the repository,
/// and the repository is opened once per chunk rather than once per file.
const REPLAY_READ_CHUNK: usize = 64;

/// A change to one directory's file-order index, as delivered to a receiver.
///
/// `Updated` carries the whole resulting order rather than a diff: a snapshot
/// is idempotent, so a receiver replaces that directory's positions and is
/// done. There is deliberately **no `Moved` variant**, even though a folder
/// move relocates the indexes inside it and `file.moved` exists for exactly
/// that identity-preserving reason: a file is an entity a receiver hangs
/// metadata off, whereas a directory's order list is just positions, with no
/// identity worth carrying across a rename. A relocated index therefore
/// arrives as `Deleted` at the old directory plus `Updated` at the new one,
/// and the receiver re-keys by replacing.
pub enum OrderChange {
    Updated {
        /// Repo-root-relative directory the order applies to; `""` is the
        /// repository root.
        directory: String,
        order: Vec<String>,
    },
    Deleted {
        directory: String,
    },
}

/// Which kind of event a replay re-emits. Deliberately not a third event
/// name: a replay exists so the receiver's *existing* handler runs again, so
/// it re-uses the ordinary event kinds (and the ordinary `[hooks] events`
/// subscription) and only marks itself with a `replay` flag in the payload.
#[derive(Clone, Copy, Debug)]
pub enum ReplayKind {
    /// One `file.created` per path — the files git holds are pushed downstream
    /// again, so a receiver missing rows re-inserts them. `created` rather
    /// than `updated` because the case a replay is usually run for is a row
    /// the receiver never got: an `UPDATE` handler would silently do nothing
    /// for exactly those. The trade is that a receiver must treat
    /// `file.created` as insert-or-replace, since the replay set includes
    /// files it already holds.
    Created,
    /// One `file.deleted` per path — the paths a caller believes it holds that
    /// git no longer has, so a receiver drops the rows git cannot account for.
    Deleted,
}

/// A replay of past events, carrying **paths rather than content**.
///
/// This is the one job shape whose payloads are not fully known when it is
/// enqueued. A whole-repository replay of `file.created` would otherwise pin
/// every file's content in memory until the job drained, and with a
/// `delay_ms` throttle that can be hours — so the content of each file is
/// read from the repository just before its own POST (see
/// [`REPLAY_READ_CHUNK`]). A file that disappeared between the snapshot and
/// its delivery is simply skipped: its real `file.deleted` is already queued
/// behind this job, so the receiver still converges.
pub struct ReplayJob {
    /// Where the repository lives, so the delivery task can re-open it to read
    /// content. Only `Updated` replays ever use it — a deletion payload
    /// carries no content at all.
    pub repo_path: PathBuf,
    pub kind: ReplayKind,
    /// Repo-root-relative paths, in the order they should be delivered.
    pub paths: Vec<String>,
    /// Optional pause between consecutive deliveries. Delivery is already
    /// strictly sequential per repository, so this is a *throttle* for the
    /// receiver, not an ordering device — and it holds this repository's queue
    /// for its whole duration, delaying the hooks of any commit accepted after
    /// the replay was enqueued.
    pub delay_ms: Option<u64>,
}

/// Where a job's payloads come from: a commit that already happened, or a
/// replay that reconstructs events from the current state of the repository.
pub enum HookSource {
    /// All changes produced by one commit, materialised at commit time.
    Commit {
        file_changes: Vec<FileChange>,
        /// Order-index changes, always delivered *after* every file change of
        /// the same commit — so an order snapshot never references a file the
        /// receiver has not been told about yet.
        order_changes: Vec<OrderChange>,
    },
    /// A reconciliation replay, resolved lazily at delivery time.
    Replay(ReplayJob),
}

/// A single unit of hook work: all changes produced by one commit, or one
/// replay. Single-file operations carry one change; a revert, a rollback, or a
/// recursive folder delete/move can carry many, and its changes are delivered
/// one hook at a time, in order.
pub struct HookJob {
    /// Both halves of the repository's identity travel in every payload.
    /// `tenant_id` alone is ambiguous — the same tenant id can exist under
    /// several collections, and those are separate repositories with
    /// separately-ordered queues, so a receiver needs both to key its rows.
    pub collection_id: String,
    pub tenant_id: String,
    pub commit_sha: String,
    pub committed_at: DateTime<Utc>,
    pub source: HookSource,
}

impl HookJob {
    /// Builds a replay job. Unlike [`HookJob::new`] no commit produced this —
    /// `commit_sha` is the repository's current HEAD, which is the honest
    /// answer to "what state is being replayed", and `committed_at` is the
    /// moment the replay was requested.
    pub fn replay(
        collection_id: String,
        tenant_id: String,
        commit_sha: String,
        committed_at: DateTime<Utc>,
        replay: ReplayJob,
    ) -> Self {
        Self {
            collection_id,
            tenant_id,
            commit_sha,
            committed_at,
            source: HookSource::Replay(replay),
        }
    }
    /// Builds a job from a commit's raw change set, splitting the changes that
    /// landed on an order index out of the file changes.
    ///
    /// This is the single place where an order event is recognised, and it
    /// classifies on the **path**, not on the route that produced the change.
    /// That is what makes every producer work without a special case: an
    /// explicit order write, a delete or move that rewrote an index alongside
    /// the file, a recursive folder operation carrying indexes inside its
    /// subtree, and a revert or rollback restoring an index out of history all
    /// arrive here as plain file changes and leave correctly classified.
    pub fn new(
        collection_id: String,
        tenant_id: String,
        commit_sha: String,
        committed_at: DateTime<Utc>,
        changes: Vec<FileChange>,
    ) -> Self {
        let mut file_changes: Vec<FileChange> = Vec::with_capacity(changes.len());
        let mut order_changes: Vec<OrderChange> = Vec::new();

        for change in changes {
            // A move is classified by its destination: leaf names are
            // preserved by every move this API performs, so an index can only
            // ever move to another index path.
            let touches_index = match &change {
                FileChange::Created { path, .. }
                | FileChange::Updated { path, .. }
                | FileChange::Deleted { path } => order::is_order_file(path),
                FileChange::Moved { to_path, .. } => order::is_order_file(to_path),
            };

            if !touches_index {
                file_changes.push(change);

                continue;
            }

            match change {
                FileChange::Created { path, content } | FileChange::Updated { path, content } => {
                    Self::push_order_updated(&mut order_changes, &path, &content);
                }

                FileChange::Deleted { path } => {
                    if let Some(directory) = order::directory_of_order_file(&path) {
                        order_changes.push(OrderChange::Deleted {
                            directory: directory.to_string(),
                        });
                    }
                }

                FileChange::Moved {
                    from_path,
                    to_path,
                    content,
                } => {
                    if let Some(directory) = order::directory_of_order_file(&from_path) {
                        order_changes.push(OrderChange::Deleted {
                            directory: directory.to_string(),
                        });
                    }

                    Self::push_order_updated(&mut order_changes, &to_path, &content);
                }
            }
        }

        Self {
            collection_id,
            tenant_id,
            commit_sha,
            committed_at,
            source: HookSource::Commit {
                file_changes,
                order_changes,
            },
        }
    }

    /// Records the order an index now holds, parsed out of the content the
    /// commit wrote. An index that cannot be parsed yields no event at all
    /// (with a warning): it is treated as no index, exactly as on the read
    /// side, rather than leaking to the receiver as a file event on a path the
    /// `/files` routes do not even acknowledge.
    fn push_order_updated(order_changes: &mut Vec<OrderChange>, path: &str, content: &str) {
        let Some(directory) = order::directory_of_order_file(path) else {
            return;
        };

        match order::parse(content) {
            Some(parsed) => order_changes.push(OrderChange::Updated {
                directory: directory.to_string(),
                order: parsed,
            }),

            None => tracing::warn!(path, "order index is malformed, skipping its hook"),
        }
    }
}

// ---------------------------------------------------------------------------
// HookQueue — per-tenant ordered dispatch
// ---------------------------------------------------------------------------

/// Per-tenant ordered hook dispatch. Each tenant gets a dedicated queue and a
/// single consumer task, so payloads are delivered in the exact order commits
/// were accepted by this server — a later commit can never overtake an earlier
/// one at the receiver, even when the earlier one is stuck in retries.
pub struct HookQueue {
    client: Client,
    config: Arc<Config>,
    senders: DashMap<String, mpsc::UnboundedSender<HookJob>>,
}

impl HookQueue {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            // One HTTP client reused across all hook deliveries (connection pooling).
            client: Client::new(),
            config,
            senders: DashMap::new(),
        }
    }

    /// Enqueues a job on the tenant's ordered queue, creating the queue and its
    /// consumer task on first use. Callers must enqueue while still holding the
    /// tenant write lock so queue order always matches commit order.
    pub fn enqueue(&self, queue_key: &str, job: HookJob) {
        // No hooks configured → nothing to deliver, skip the queue entirely.
        if self.config.hooks.is_none() {
            return;
        }

        let sender = self
            .senders
            .entry(queue_key.to_string())
            .or_insert_with(|| self.spawn_consumer())
            .clone();

        // Senders live in the map forever, so the consumer loop never ends; a
        // send can only fail while the runtime is shutting down.
        if sender.send(job).is_err() {
            tracing::error!(queue_key, "hook queue consumer is gone, dropping hook job");
        }
    }

    /// Spawns the single consumer task for one tenant's queue. The loop
    /// `await`s each delivery to completion (including all its retries and
    /// backoff sleeps) before picking up the next job — this sequential
    /// processing is precisely what provides the ordering guarantee.
    fn spawn_consumer(&self) -> mpsc::UnboundedSender<HookJob> {
        let (sender, mut receiver) = mpsc::unbounded_channel::<HookJob>();
        let client = self.client.clone();
        let config = self.config.clone();

        tokio::spawn(async move {
            while let Some(job) = receiver.recv().await {
                HookDelivery::deliver_all(&client, &config, job).await;
            }
        });

        sender
    }
}

// ---------------------------------------------------------------------------
// HookDelivery — webhook dispatch and payload construction
// ---------------------------------------------------------------------------

/// Stateless namespace for the delivery machinery: payload construction,
/// event filtering, the retry loop, and the actual HTTP POST. Kept separate
/// from `HookQueue` so queueing concerns and wire concerns don't mix.
struct HookDelivery;

impl HookDelivery {
    /// Delivers one hook payload per file change, in order. Delivery is
    /// sequential so the receiver can process changes synchronously without
    /// needing to sort by commit order itself.
    async fn deliver_all(client: &Client, config: &Config, job: HookJob) {
        let hooks = match config.hooks.as_ref() {
            Some(hooks) => hooks,
            None => return,
        };

        // A zero/negative configuration is treated as a single attempt so events
        // are never silently dropped because of a misconfiguration.
        let attempts = hooks.retry_attempts.max(1);

        // Log lines name the repository, not just the tenant: a tenant id can
        // repeat across collections, and an operator reading a CRITICAL
        // permanent-failure line needs to know which repository is now out of
        // sync. Same composite key the queue itself is keyed by.
        let repository = format!("{}/{}", job.collection_id, job.tenant_id);

        let (file_changes, order_changes) = match job.source {
            HookSource::Commit {
                file_changes,
                order_changes,
            } => (file_changes, order_changes),

            // A replay resolves its own payloads as it goes, so it never
            // reaches the commit-shaped loops below.
            HookSource::Replay(replay) => {
                Self::deliver_replay(
                    client,
                    hooks,
                    attempts,
                    &job.collection_id,
                    &job.tenant_id,
                    &job.commit_sha,
                    &job.committed_at,
                    &repository,
                    replay,
                )
                .await;

                return;
            }
        };

        for file_change in file_changes {
            let required_event = Self::event_for_change(&file_change);

            if !hooks.events.contains(&required_event) {
                continue;
            }

            let payload = Self::build_payload(
                &job.collection_id,
                &job.tenant_id,
                &job.commit_sha,
                &job.committed_at,
                &file_change,
                false,
            );
            let description = Self::change_description(&file_change);

            Self::deliver_with_retries(
                client,
                hooks,
                attempts,
                payload,
                &repository,
                &job.commit_sha,
                &description,
            )
            .await;
        }

        // Order events come after every file event of the same commit, so an
        // order snapshot never names a file the receiver has not seen yet.
        for order_change in order_changes {
            let required_event = Self::event_for_order_change(&order_change);

            if !hooks.events.contains(&required_event) {
                continue;
            }

            let payload = Self::build_order_payload(
                &job.collection_id,
                &job.tenant_id,
                &job.commit_sha,
                &job.committed_at,
                &order_change,
            );
            let description = Self::order_change_description(&order_change);

            Self::deliver_with_retries(
                client,
                hooks,
                attempts,
                payload,
                &repository,
                &job.commit_sha,
                &description,
            )
            .await;
        }
    }

    /// Delivers one replay: a `file.updated` or `file.deleted` per path, in
    /// order, optionally throttled by `delay_ms`.
    ///
    /// Two things make this different from a commit-shaped job:
    ///
    /// - **Content is resolved here, not at enqueue time.** Paths are read in
    ///   chunks of [`REPLAY_READ_CHUNK`], so a whole-repository replay holds a
    ///   chunk in memory rather than the whole corpus — which matters because
    ///   a throttled replay can occupy this repository's queue for hours. A
    ///   deletion replay needs no content at all and reads nothing.
    /// - **A path can vanish mid-replay.** The snapshot was taken under the
    ///   write lock, but delivery outlives it, so a file deleted in the
    ///   meantime is skipped rather than treated as an error: its real
    ///   `file.deleted` is already queued behind this job, so the receiver
    ///   still converges.
    #[allow(clippy::too_many_arguments)]
    async fn deliver_replay(
        client: &Client,
        hooks: &HooksConfig,
        attempts: u32,
        collection_id: &str,
        tenant_id: &str,
        commit_sha: &str,
        committed_at: &DateTime<Utc>,
        repository: &str,
        replay: ReplayJob,
    ) {
        let required_event = match replay.kind {
            ReplayKind::Created => HookEvent::FileCreated,
            ReplayKind::Deleted => HookEvent::FileDeleted,
        };

        // Subscriptions apply to a replay exactly as to a live event. An
        // unsubscribed replay delivers nothing, which is correct but silent
        // enough to look like a bug from the outside — so it is logged at warn
        // rather than skipped quietly.
        if !hooks.events.contains(&required_event) {
            tracing::warn!(
                repository,
                event = ?replay.kind,
                path_count = replay.paths.len(),
                "replay requested but its event kind is not listed in [hooks] events, delivering nothing"
            );

            return;
        }

        let total = replay.paths.len();
        let delay = replay
            .delay_ms
            .filter(|milliseconds| *milliseconds > 0)
            .map(Duration::from_millis);

        tracing::info!(
            repository,
            event = ?replay.kind,
            path_count = total,
            delay_ms = ?replay.delay_ms,
            "replay starting"
        );

        let mut delivered: usize = 0;
        let mut skipped: usize = 0;

        for chunk in replay.paths.chunks(REPLAY_READ_CHUNK) {
            let changes =
                match Self::replay_chunk_changes(&replay.repo_path, tenant_id, replay.kind, chunk)
                    .await
                {
                    Some(changes) => changes,

                    // The repository could not be read at all (deleted mid-replay,
                    // or a git failure). Continuing would emit nothing useful for
                    // every remaining chunk, so the replay stops here.
                    None => {
                        tracing::error!(
                            repository,
                            delivered,
                            remaining = total - delivered - skipped,
                            "replay aborted: repository could not be read"
                        );

                        return;
                    }
                };

            for change in changes {
                let Some(change) = change else {
                    skipped += 1;

                    continue;
                };

                // Sleeping *before* each delivery but the first is what keeps
                // the throttle strictly between deliveries — no trailing pause
                // holding the queue after the final POST.
                if delivered > 0 {
                    if let Some(delay) = delay {
                        sleep(delay).await;
                    }
                }

                let payload = Self::build_payload(
                    collection_id,
                    tenant_id,
                    commit_sha,
                    committed_at,
                    &change,
                    true,
                );
                let description = Self::change_description(&change);

                Self::deliver_with_retries(
                    client,
                    hooks,
                    attempts,
                    payload,
                    repository,
                    commit_sha,
                    &description,
                )
                .await;

                delivered += 1;
            }
        }

        tracing::info!(
            repository,
            event = ?replay.kind,
            delivered,
            skipped,
            "replay finished"
        );
    }

    /// Resolves one chunk of replay paths into the changes to deliver.
    ///
    /// `None` means the repository itself could not be read (the caller aborts
    /// the whole replay); an inner `None` means that single path is no longer
    /// deliverable — absent from HEAD, or holding content JSON cannot carry —
    /// and is skipped. A deletion replay needs no repository access at all, so
    /// it never leaves the async context.
    async fn replay_chunk_changes(
        repo_path: &std::path::Path,
        tenant_id: &str,
        kind: ReplayKind,
        chunk: &[String],
    ) -> Option<Vec<Option<FileChange>>> {
        if let ReplayKind::Deleted = kind {
            return Some(
                chunk
                    .iter()
                    .map(|path| Some(FileChange::Deleted { path: path.clone() }))
                    .collect(),
            );
        }

        let repo_path = repo_path.to_path_buf();
        let tenant_id = tenant_id.to_string();
        let paths = chunk.to_vec();

        // libgit2 is synchronous, so the read goes to the blocking pool for the
        // same reason every other git call in this codebase does.
        let read = tokio::task::spawn_blocking(move || {
            GitFiles::replay_read_files(&repo_path, &tenant_id, &paths)
        })
        .await;

        let contents = match read {
            Ok(Ok(contents)) => contents,

            Ok(Err(read_err)) => {
                tracing::error!("replay chunk read failed: {}", read_err);

                return None;
            }

            Err(join_err) => {
                tracing::error!("replay chunk read task failed: {}", join_err);

                return None;
            }
        };

        Some(
            chunk
                .iter()
                .zip(contents)
                .map(|(path, content)| {
                    content.map(|content| FileChange::Created {
                        path: path.clone(),
                        content,
                    })
                })
                .collect(),
        )
    }

    /// The retry loop for one payload: attempt, and on failure sleep
    /// `retry_backoff_ms * 2^(attempt-1)` before trying again. Runs inside
    /// the tenant's consumer task, so backoff sleeps intentionally hold up
    /// that tenant's queue (ordering beats latency here) without affecting
    /// any other tenant.
    async fn deliver_with_retries(
        client: &Client,
        hooks: &HooksConfig,
        attempts: u32,
        payload: Value,
        repository: &str,
        commit_sha: &str,
        change_description: &str,
    ) {
        for attempt in 1..=attempts {
            match Self::send(client, hooks, &payload).await {
                Ok(()) => {
                    tracing::debug!(
                        repository,
                        commit_sha,
                        change = change_description,
                        "hook delivered"
                    );

                    return;
                }
                Err(delivery_err) => {
                    tracing::error!(
                        repository,
                        commit_sha,
                        change = change_description,
                        attempt,
                        total = attempts,
                        "hook delivery failed: {}",
                        delivery_err
                    );

                    if attempt == attempts {
                        tracing::error!(
                            repository, commit_sha, change = change_description,
                            "CRITICAL: hook permanently failed after {} attempts — the receiver may be out of sync",
                            attempts
                        );

                        return;
                    }

                    let exponent = (attempt - 1).min(MAX_BACKOFF_EXPONENT);
                    let backoff_ms = hooks.retry_backoff_ms.saturating_mul(1u64 << exponent);

                    sleep(Duration::from_millis(backoff_ms)).await;
                }
            }
        }
    }

    /// Performs one HTTP POST. Success means any 2xx status; everything else
    /// (transport error or non-2xx) is an error string for the retry loop —
    /// including the response body, since receivers often explain rejections
    /// there and it is the only clue an operator gets in the logs.
    async fn send(client: &Client, hooks: &HooksConfig, payload: &Value) -> Result<(), String> {
        let mut request_builder = client.post(&hooks.url).json(payload);

        if let Some(hook_auth) = &hooks.auth {
            request_builder = request_builder.header(&hook_auth.header, &hook_auth.value);
        }

        let response = request_builder
            .send()
            .await
            .map_err(|send_err| send_err.to_string())?;

        let status = response.status();

        if status.is_success() {
            return Ok(());
        }

        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "(unreadable body)".to_string());

        Err(format!(
            "receiver returned HTTP {}: {}",
            status.as_u16(),
            body
        ))
    }

    /// Maps a git-layer `FileChange` to its wire-level event kind, used to
    /// check the change against the configured `events` subscription list.
    fn event_for_change(file_change: &FileChange) -> HookEvent {
        match file_change {
            FileChange::Created { .. } => HookEvent::FileCreated,
            FileChange::Updated { .. } => HookEvent::FileUpdated,
            FileChange::Deleted { .. } => HookEvent::FileDeleted,
            FileChange::Moved { .. } => HookEvent::FileMoved,
        }
    }

    /// Maps an order-index change to its wire-level event kind, so it is
    /// filtered against the configured `events` list exactly like a file
    /// change.
    fn event_for_order_change(order_change: &OrderChange) -> HookEvent {
        match order_change {
            OrderChange::Updated { .. } => HookEvent::OrderUpdated,
            OrderChange::Deleted { .. } => HookEvent::OrderDeleted,
        }
    }

    /// Compact human-readable summary of a change (`"updated:docs/intro.md"`)
    /// used purely for log lines — never sent to the receiver.
    fn change_description(file_change: &FileChange) -> String {
        match file_change {
            FileChange::Created { path, .. } => format!("created:{}", path),
            FileChange::Updated { path, .. } => format!("updated:{}", path),
            FileChange::Deleted { path } => format!("deleted:{}", path),
            FileChange::Moved {
                from_path, to_path, ..
            } => {
                format!("moved:{}→{}", from_path, to_path)
            }
        }
    }

    /// Same, for an order-index change. The directory is spelled `/` here (and
    /// only here, in logs) so a root-level order does not read as a blank.
    fn order_change_description(order_change: &OrderChange) -> String {
        match order_change {
            OrderChange::Updated { directory, order } => format!(
                "order updated:{} ({} entries)",
                order::display_directory(directory),
                order.len()
            ),
            OrderChange::Deleted { directory } => {
                format!("order deleted:{}", order::display_directory(directory))
            }
        }
    }

    /// Builds the JSON payload for one order-index change, on the same
    /// envelope as every file event.
    ///
    /// `order.updated` carries the directory's complete resulting order rather
    /// than a diff, so applying it downstream is a replace and repeated
    /// delivery is harmless. The `directory` is repo-root-relative, with the
    /// repository root spelled as the empty string — unlike the log line, the
    /// wire value stays a plain path a receiver can key on directly.
    fn build_order_payload(
        collection_id: &str,
        tenant_id: &str,
        commit_sha: &str,
        committed_at: &DateTime<Utc>,
        order_change: &OrderChange,
    ) -> Value {
        let mut payload = Map::with_capacity(7);

        payload.insert(
            "collection_id".to_string(),
            Value::String(collection_id.to_string()),
        );
        payload.insert(
            "tenant_id".to_string(),
            Value::String(tenant_id.to_string()),
        );
        payload.insert(
            "commit_sha".to_string(),
            Value::String(commit_sha.to_string()),
        );
        payload.insert(
            "committed_at".to_string(),
            Value::String(committed_at.to_rfc3339()),
        );

        match order_change {
            OrderChange::Updated { directory, order } => {
                payload.insert(
                    "event".to_string(),
                    Value::String("order.updated".to_string()),
                );
                payload.insert("directory".to_string(), Value::String(directory.clone()));
                payload.insert("order".to_string(), json!(order));
            }
            OrderChange::Deleted { directory } => {
                payload.insert(
                    "event".to_string(),
                    Value::String("order.deleted".to_string()),
                );
                payload.insert("directory".to_string(), Value::String(directory.clone()));
            }
        }

        Value::Object(payload)
    }

    /// Builds the JSON payload for one file change. The envelope fields
    /// (`collection_id`, `tenant_id`, `commit_sha`, `committed_at`) are
    /// identical across all event kinds; the event-specific part is the `file`
    /// object — or, for moves, a `from`/`to` pair so the receiver can carry
    /// entity identity (and any attached metadata) across the rename instead
    /// of seeing an unrelated delete + create.
    ///
    /// `collection_id` and `tenant_id` together are the repository's identity,
    /// and together they are what a receiver must key on: hook order is
    /// guaranteed per *repository*, so two collections sharing a tenant id
    /// deliver on independent queues and would otherwise be indistinguishable.
    ///
    /// `replay` adds a single `"replayed": true` field, and *only* when true —
    /// spelled as a past participle because it states something about the
    /// event ("this event was replayed") rather than instructing anything,
    /// the same way `has_more` reads on a listing response —
    /// a live event's payload stays byte-for-byte what it has always been, so
    /// nothing an existing receiver parses changes. The event name is
    /// deliberately not varied: the whole point of a replay is that the
    /// receiver's existing handler runs again unmodified, with the flag there
    /// purely so it can log, meter, or guard on it.
    fn build_payload(
        collection_id: &str,
        tenant_id: &str,
        commit_sha: &str,
        committed_at: &DateTime<Utc>,
        file_change: &FileChange,
        replay: bool,
    ) -> Value {
        let mut payload = Map::with_capacity(7);

        payload.insert(
            "collection_id".to_string(),
            Value::String(collection_id.to_string()),
        );
        payload.insert(
            "tenant_id".to_string(),
            Value::String(tenant_id.to_string()),
        );
        payload.insert(
            "commit_sha".to_string(),
            Value::String(commit_sha.to_string()),
        );
        payload.insert(
            "committed_at".to_string(),
            Value::String(committed_at.to_rfc3339()),
        );

        if replay {
            payload.insert("replayed".to_string(), Value::Bool(true));
        }

        match file_change {
            FileChange::Created { path, content } => {
                payload.insert(
                    "event".to_string(),
                    Value::String("file.created".to_string()),
                );
                payload.insert(
                    "file".to_string(),
                    json!({ "path": path, "content": content }),
                );
            }
            FileChange::Updated { path, content } => {
                payload.insert(
                    "event".to_string(),
                    Value::String("file.updated".to_string()),
                );
                payload.insert(
                    "file".to_string(),
                    json!({ "path": path, "content": content }),
                );
            }
            FileChange::Deleted { path } => {
                payload.insert(
                    "event".to_string(),
                    Value::String("file.deleted".to_string()),
                );
                payload.insert("file".to_string(), json!({ "path": path }));
            }
            FileChange::Moved {
                from_path,
                to_path,
                content,
            } => {
                payload.insert("event".to_string(), Value::String("file.moved".to_string()));
                payload.insert("from".to_string(), json!({ "path": from_path }));
                payload.insert(
                    "to".to_string(),
                    json!({ "path": to_path, "content": content }),
                );
            }
        }

        Value::Object(payload)
    }
}
