// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Background repository maintenance scheduling.
//!
//! Why maintenance is needed at all: every commit made through the write
//! path stores its new objects (blob, trees, commit) as individual *loose*
//! files under `.git/objects/`, every commit appends reflog entries, and
//! failed writes can orphan unreachable blobs. A busy tenant accumulates
//! thousands of tiny files, which slows down object lookups and wastes
//! disk. Git normally solves this with `git gc`; githttp-fs does the
//! equivalent in-process (see `GitMaintenance` in `git.rs`) — repacking all
//! reachable objects into a single consolidated packfile, pruning
//! unreachable ones, and expiring reflogs — with no dependency on a `git`
//! binary.
//!
//! Why the scheduler works the way it does:
//!
//! - **One-shot timer armed by the first write** — repos that receive no
//!   writes are never touched (no wasted wake-ups scanning idle tenants),
//!   and repos that write constantly are packed at most once per
//!   `delay_secs`, not after every burst.
//! - **In-memory only, doesn't survive restarts** — losing a pending timer
//!   is harmless: the next write simply re-arms it. Persisting the schedule
//!   would add state for no correctness benefit.
//! - **Runs under the tenant write lock** — packing must see a frozen
//!   object store; taking the same mutex as writers is the simplest way to
//!   guarantee that, and readers remain unaffected throughout.

use dashmap::DashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

use crate::config::Config;
use crate::git::GitMaintenance;
use crate::state::RepoLock;
use crate::util::run_blocking;

/// Schedules background maintenance for tenant repositories.
///
/// The first write to a repository arms a one-shot timer; writes that land
/// while a timer is already armed do not reschedule it, so maintenance runs a
/// fixed delay after the *first* write following the previous pass. Inactive
/// repositories never trigger maintenance. The schedule is in-memory only and
/// intentionally does not survive a restart.
pub struct MaintenanceScheduler {
    config: Arc<Config>,
    /// Tenants with a maintenance pass currently armed or running. The handle
    /// is `None` only for the instant between arming and task registration.
    pending: Arc<DashMap<String, Option<tokio::task::JoinHandle<()>>>>,
}

impl MaintenanceScheduler {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            pending: Arc::new(DashMap::new()),
        }
    }

    /// Arms maintenance for a tenant repository after a successful write.
    pub fn schedule(&self, tenant_key: &str, repo_path: PathBuf, repo_lock: RepoLock) {
        if !self.config.maintenance.enabled {
            return;
        }

        // Only arm when no pass is already pending for this tenant. The
        // entry API makes check-and-insert atomic — two concurrent writers
        // cannot both arm a timer (only one sees the vacant slot). The slot
        // is first claimed with `None` and the JoinHandle is filled in
        // below, after the task exists.
        match self.pending.entry(tenant_key.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(_) => return,
            dashmap::mapref::entry::Entry::Vacant(vacant) => {
                vacant.insert(None);
            }
        }

        let delay_secs = self.config.maintenance.delay_secs;
        let pending = self.pending.clone();
        let task_tenant_key = tenant_key.to_string();

        tracing::debug!(tenant = %task_tenant_key, delay_secs = delay_secs, "maintenance scheduled");

        let task = tokio::spawn(async move {
            sleep(Duration::from_secs(delay_secs)).await;

            // Take the tenant write lock so maintenance never runs concurrently
            // with a write — packing must see a frozen object store.
            let _lock_guard = repo_lock.lock().await;

            let repo_path_for_task = repo_path.clone();

            match run_blocking(move || GitMaintenance::run(&repo_path_for_task)).await {
                Ok(report) => {
                    tracing::info!(
                        tenant = %task_tenant_key,
                        packed_objects = report.packed_objects,
                        loose_objects_removed = report.loose_objects_removed,
                        old_packs_removed = report.old_packs_removed,
                        "maintenance complete"
                    );
                }
                Err(err) => {
                    tracing::error!(tenant = %task_tenant_key, err = %err, "maintenance failed");
                }
            }

            // Clear the slot while still holding the write lock, so the very
            // next write reliably re-arms the timer.
            pending.remove(&task_tenant_key);
        });

        // Register the handle so `cancel` can abort the timer. If the tenant
        // was cancelled in the instant since arming, abort immediately.
        match self.pending.get_mut(tenant_key) {
            Some(mut slot) => *slot = Some(task),
            None => task.abort(),
        }
    }

    /// Disarms any pending maintenance pass for a tenant. Called on tenant
    /// deletion so the timer task does not linger for up to `delay_secs`
    /// (and so a re-created tenant can arm a fresh timer right away).
    ///
    /// A pass that already started running cannot be holding the repository:
    /// it runs under the tenant write lock, which the deletion handler holds
    /// while calling this.
    pub fn cancel(&self, tenant_key: &str) {
        if let Some((_, task)) = self.pending.remove(tenant_key) {
            if let Some(task) = task {
                task.abort();
            }

            tracing::debug!(tenant = %tenant_key, "maintenance canceled");
        }
    }
}
