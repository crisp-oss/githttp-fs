// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Shared application state, cloned into every request handler.
//!
//! `AppState` is axum's "state" object: one instance is created at startup
//! and a clone is handed to each handler invocation. Every field is wrapped
//! in an `Arc`, so cloning is just a handful of atomic reference-count bumps
//! — all handlers share the *same* config, hook queues, maintenance
//! scheduler, and lock map.
//!
//! The most important piece here is the **per-tenant write lock**. githttp-fs
//! serialises all mutating git operations (write / delete / move / revert /
//! tenant delete) on a given repository through one `tokio::sync::Mutex`.
//! This is what makes each repository a single-writer system:
//!
//! - commits never race (git has no built-in concurrent-commit safety when
//!   driven through libgit2 the way we drive it),
//! - hook jobs can be enqueued *while the lock is held*, guaranteeing hook
//!   order matches commit order,
//! - background maintenance can freeze the object store by simply taking the
//!   same lock.
//!
//! Reads never touch the lock: they operate on immutable git objects
//! (HEAD's tree and blobs), which are safe to read concurrently with a
//! writer appending new objects.

use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::hooks::HookQueue;
use crate::maintenance::MaintenanceScheduler;

/// A cloneable handle to the per-tenant write lock.
/// Read operations do not acquire this lock.
///
/// The mutex guards nothing (`()`): it is used purely for its exclusion
/// property. It is a `tokio::sync::Mutex` (not `std`) because holders keep it
/// across `.await` points — e.g. while a git operation runs on the blocking
/// thread pool — which a std mutex guard cannot legally do.
pub type RepoLock = Arc<Mutex<()>>;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// Per-tenant ordered hook delivery queues.
    pub hook_queue: Arc<HookQueue>,
    /// Per-tenant background maintenance timers.
    pub maintenance: Arc<MaintenanceScheduler>,
    /// Lazily-created mutex per tenant to serialize git write operations.
    /// Keyed as `"collection_id/tenant_id"` — the same composite key used for
    /// hook queues and maintenance slots, so all three subsystems agree on
    /// what "one tenant" means. `DashMap` gives lock-free-ish concurrent
    /// access without a global mutex around the map itself.
    repo_locks: Arc<DashMap<String, RepoLock>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let config = Arc::new(config);

        Self {
            hook_queue: Arc::new(HookQueue::new(config.clone())),
            maintenance: Arc::new(MaintenanceScheduler::new(config.clone())),
            config,
            repo_locks: Arc::new(DashMap::new()),
        }
    }

    /// Returns the write lock for a tenant, creating it if this is the first access.
    ///
    /// Entries are deliberately never removed — not even on tenant deletion.
    /// Removing an entry would let a writer that fetched the old `Arc` run
    /// concurrently with a writer holding a freshly-created mutex for the same
    /// repository. The cost of a retained entry is a few dozen bytes.
    pub fn get_repo_lock(&self, tenant_id: &str) -> RepoLock {
        self.repo_locks
            .entry(tenant_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}
