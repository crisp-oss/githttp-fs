// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::hooks::HookQueue;
use crate::maintenance::MaintenanceScheduler;

/// A cloneable handle to the per-tenant write lock.
/// Read operations do not acquire this lock.
pub type RepoLock = Arc<Mutex<()>>;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// Per-tenant ordered hook delivery queues.
    pub hook_queue: Arc<HookQueue>,
    /// Per-tenant background maintenance timers.
    pub maintenance: Arc<MaintenanceScheduler>,
    /// Lazily-created mutex per tenant to serialize git write operations.
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
