// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

use dashmap::DashMap;
use reqwest::Client;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::Config;

/// A cloneable handle to the per-tenant write lock.
/// Read operations do not acquire this lock.
pub type RepoLock = Arc<Mutex<()>>;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// One HTTP client reused across all hook deliveries (connection pooling).
    pub http_client: Client,
    /// Lazily-created mutex per tenant to serialize git write operations.
    repo_locks: Arc<DashMap<String, RepoLock>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            http_client: Client::new(),
            repo_locks: Arc::new(DashMap::new()),
        }
    }

    /// Returns the write lock for a tenant, creating it if this is the first access.
    pub fn get_repo_lock(&self, tenant_id: &str) -> RepoLock {
        self.repo_locks
            .entry(tenant_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Removes the in-memory lock entry for a tenant after its repo is deleted.
    pub fn remove_repo_lock(&self, tenant_id: &str) {
        self.repo_locks.remove(tenant_id);
    }
}
