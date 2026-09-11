// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! TOML configuration types and startup validation.
//!
//! The structs here mirror the sections of `config.toml` one-to-one
//! (`[server]`, `[limits]`, `[hooks]`, `[hooks.auth]`, `[maintenance]`,
//! `[replication]`)
//! and are deserialised by serde. Two conventions run through the whole
//! module:
//!
//! - **Fail at startup, not at request time.** Every section implements a
//!   `collect_errors` method that appends human-readable problems to a
//!   shared `Vec` instead of returning on the first failure. `main` prints
//!   the whole list and exits, so an operator fixes every config mistake in
//!   a single edit-and-restart cycle. Nothing downstream ever needs to
//!   re-validate config values.
//! - **Optional sections have safe defaults.** `[hooks]` omitted means "no
//!   webhooks" (writes still work, nothing is delivered). `[maintenance]`
//!   omitted means "enabled, 24 h delay" via the `Default` impl.
//!   `[replication]` omitted means "standalone node".

use serde::Deserialize;
use std::path::PathBuf;

/// Tracing log level. Accepts "trace", "debug", "info", "warn", "error".
/// Defaults to "info" if unset. Overridden by the RUST_LOG env var.
type LogLevel = String;

const VALID_LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

/// Root of the parsed config file. `hooks` stays `Option` (its absence is
/// checked at every enqueue), while `maintenance` collapses to defaults so
/// the rest of the code never handles a missing section.
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    pub hooks: Option<HooksConfig>,
    #[serde(default)]
    pub maintenance: MaintenanceConfig,
    /// Read-only replication. Absent means this node is standalone: it
    /// serves no replication surface and follows no master, which is
    /// exactly how every deployment behaved before the feature existed.
    pub replication: Option<ReplicationConfig>,
}

impl Config {
    /// Validates the whole config, returning *all* problems at once rather
    /// than stopping at the first one. Called exactly once, from `main`,
    /// before the server starts.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        self.server.collect_errors(&mut errors);
        self.limits.collect_errors(&mut errors);

        if let Some(hooks) = &self.hooks {
            hooks.collect_errors(&mut errors);
        }

        self.maintenance.collect_errors(&mut errors);

        if let Some(replication) = &self.replication {
            replication.collect_errors(&mut errors, &self.server);
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Background repository maintenance (repack into a single consolidated
/// packfile, expire reflogs, refresh the index — and optionally prune
/// unreachable objects).
/// The section is optional; omitting it enables maintenance with a 24h delay.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct MaintenanceConfig {
    pub enabled: bool,
    /// Delay between the first write to a repository and its maintenance pass.
    pub delay_secs: u64,
    /// When true, the maintenance repack keeps only objects reachable from a
    /// ref, permanently dropping unreachable ones (e.g. blobs orphaned by
    /// writes that failed mid-operation). When false (the default), every
    /// object in the store is carried over into the consolidated pack, so
    /// maintenance can never destroy data under any circumstance — at the
    /// cost of orphaned garbage being retained forever.
    pub destructive_prune: bool,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // 24 hours
            delay_secs: 86_400,
            destructive_prune: false,
        }
    }
}

impl MaintenanceConfig {
    fn collect_errors(&self, errors: &mut Vec<String>) {
        if self.enabled && self.delay_secs < 1 {
            errors.push("maintenance.delay_secs must be at least 1".to_string());
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub api_key: String,
    pub repos_path: PathBuf,
    pub log_level: Option<LogLevel>,
}

/// Request-level guard rails, grouped in their own `[limits]` section.
/// The section is optional; omitting it (or any key) applies the defaults.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct LimitsConfig {
    /// Optional whitelist of file extensions accepted on writes and move
    /// destinations (e.g. `["md", "mdx"]`). Unset means all extensions.
    pub allowed_extensions: Option<Vec<String>>,
    /// Safety cap on how many files one batch read request may ask for;
    /// larger requests are rejected with a 400.
    pub batch_read_maximum_files: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            allowed_extensions: None,
            batch_read_maximum_files: 100,
        }
    }
}

impl LimitsConfig {
    fn collect_errors(&self, errors: &mut Vec<String>) {
        if self.batch_read_maximum_files < 1 {
            errors.push("limits.batch_read_maximum_files must be at least 1".to_string());
        }

        if let Some(extensions) = &self.allowed_extensions {
            if extensions.is_empty() {
                errors.push(
                    "limits.allowed_extensions must contain at least one extension".to_string(),
                );
            }

            for extension in extensions {
                let normalized = extension.trim_start_matches('.');

                if normalized.is_empty()
                    || !normalized.bytes().all(|byte| byte.is_ascii_alphanumeric())
                {
                    errors.push(format!(
                        "limits.allowed_extensions entry '{}' is invalid; must be alphanumeric like \"md\"",
                        extension
                    ));
                }
            }
        }
    }
}

impl ServerConfig {
    fn collect_errors(&self, errors: &mut Vec<String>) {
        if self.host.trim().is_empty() {
            errors.push("server.host must not be empty".to_string());
        }

        if self.api_key.trim().is_empty() {
            errors.push("server.api_key must not be empty".to_string());
        }

        if let Some(level) = &self.log_level {
            if !VALID_LOG_LEVELS.contains(&level.as_str()) {
                errors.push(format!(
                    "server.log_level '{}' is invalid; must be one of: {}",
                    level,
                    VALID_LOG_LEVELS.join(", ")
                ));
            }
        }

        // Validating repos_path doubles as provisioning: if the directory
        // does not exist yet it is created here, so a fresh deployment works
        // without a manual `mkdir` step. Failure to create it (permissions,
        // read-only filesystem, ...) is a config error like any other.
        if self.repos_path.as_os_str().is_empty() {
            errors.push("server.repos_path must not be empty".to_string());
        } else if self.repos_path.exists() {
            if !self.repos_path.is_dir() {
                errors.push(format!(
                    "server.repos_path '{}' exists but is not a directory",
                    self.repos_path.display()
                ));
            }
        } else if let Err(create_err) = std::fs::create_dir_all(&self.repos_path) {
            errors.push(format!(
                "server.repos_path '{}' could not be created: {}",
                self.repos_path.display(),
                create_err
            ));
        }
    }
}

/// Which side of a replication pair this node is.
///
/// The two roles are not symmetric in what they *serve* — a replica serves
/// the replication surface too, so replicas can chain off one another — but
/// only a replica runs a follower that pulls from somewhere else.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationRole {
    #[serde(rename = "master")]
    Master,
    #[serde(rename = "replica")]
    Replica,
}

/// Read-only replication.
///
/// The section is optional, and its absence is the historical behaviour:
/// no replication routes are mounted, no follower runs, nothing changes.
///
/// `api_key` is deliberately separate from `server.api_key`. The
/// replication surface hands out whole repositories as packfiles and, on a
/// master, a live notification stream — a different grant with a different
/// blast radius from the content API, so it gets a credential that can be
/// rotated and network-restricted on its own.
#[derive(Debug, Deserialize, Clone)]
pub struct ReplicationConfig {
    pub role: ReplicationRole,
    /// Guards the replication server on this node, and is sent as the Bearer
    /// token when this node is a replica pulling from its master.
    ///
    /// Named `secret` rather than `api_key` because it is not an API key in
    /// the sense `server.api_key` is: no caller of the product's API ever
    /// holds it, it is never handed to an application, and it authenticates
    /// githttp-fs to githttp-fs. Two keys in one config file called the same
    /// thing is an invitation to paste the wrong one.
    pub secret: String,
    /// How this node names itself to its peers, so an operator reading the
    /// health route sees meaningful names instead of anonymous rows.
    ///
    /// This is **telemetry, not authentication** — `api_key` is what guards
    /// the replication surface. A node id only ever labels a row in a
    /// roster, so a node that lies about its own can mislead a dashboard and
    /// nothing more. Keeping it out of the auth path is what lets it stay
    /// optional and self-asserted, which preserves the property that a
    /// replica needs no registration on its master: it joins by connecting.
    ///
    /// Defaults to `"host:port"`, which is deterministic across restarts (so
    /// a roster row survives a reboot instead of forking into two) and is
    /// usually already meaningful. Override it where the bind address is not
    /// how peers see this node — behind NAT, in a container, or when several
    /// nodes share a host.
    pub node_id: Option<String>,
    /// Base URL of the master's **replication server** — its `[replication]
    /// host`/`port`, not the content API's. Required on a replica, rejected
    /// on a master (a master that names a master is a config mistake worth
    /// failing at startup rather than silently ignoring).
    pub master_url: Option<String>,
    /// Address the replication server binds. Defaults to `server.host`.
    ///
    /// Set it to `127.0.0.1` where every replica is local, or to a private
    /// interface address where they are not: the separate port already keeps
    /// this surface off whatever proxies the content API, and binding narrowly
    /// closes the gap the rest of the way.
    pub host: Option<String>,
    /// Port the replication server binds. Defaults to 5356.
    #[serde(default = "default_replication_port")]
    pub port: u16,
    /// How often a replica runs a full reconcile pass against the master's
    /// repository listing. This is the *convergence* path: notifications are
    /// disposable hints, so this interval — not the notification stream — is
    /// what bounds how long a replica can stay wrong.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// How many repositories a replica syncs concurrently during catch-up.
    /// Repositories are independent, so this fans out freely; the bound
    /// exists to stop a returning replica from stampeding its master.
    #[serde(default = "default_replication_parallelism")]
    pub parallelism: usize,
    /// Base delay before a replica re-dials a dropped notification stream.
    /// Backoff doubles up to a minute.
    #[serde(default = "default_reconnect_backoff_ms")]
    pub reconnect_backoff_ms: u64,
}

fn default_poll_interval_secs() -> u64 {
    60
}

fn default_replication_parallelism() -> usize {
    4
}

fn default_reconnect_backoff_ms() -> u64 {
    1_000
}

fn default_replication_port() -> u16 {
    5356
}

impl ReplicationConfig {
    pub fn is_replica(&self) -> bool {
        self.role == ReplicationRole::Replica
    }

    /// This node's id, falling back to `host:port` when unset.
    pub fn node_id(&self, server: &ServerConfig) -> String {
        match &self.node_id {
            Some(node_id) => node_id.clone(),
            None => format!("{}:{}", server.host, server.port),
        }
    }

    /// Address the replication server binds, falling back to `server.host`.
    pub fn host(&self, server: &ServerConfig) -> String {
        self.host.clone().unwrap_or_else(|| server.host.clone())
    }

    fn collect_errors(&self, errors: &mut Vec<String>, server: &ServerConfig) {
        if self.secret.trim().is_empty() {
            errors.push("replication.secret must not be empty".to_string());
        }

        // Caught here rather than left to a confusing "address already in
        // use" on the second bind — and the whole point of the split is that
        // these are two different doors.
        if self.port == server.port {
            errors.push(format!(
                "replication.port ({}) must differ from server.port",
                self.port
            ));
        }

        // Held to the same rule peers apply on receipt (`validate::node_id`),
        // so a node never announces a name its master would discard and show
        // as anonymous.
        if let Some(node_id) = &self.node_id {
            if let Err(err) = crate::validate::node_id(node_id) {
                errors.push(format!("replication.node_id is invalid: {}", err));
            }
        }

        match (self.role, &self.master_url) {
            (ReplicationRole::Replica, None) => {
                errors
                    .push("replication.master_url is required when role is 'replica'".to_string());
            }
            (ReplicationRole::Master, Some(_)) => {
                errors.push(
                    "replication.master_url must not be set when role is 'master'".to_string(),
                );
            }
            _ => {}
        }

        if let Some(url) = &self.master_url {
            match reqwest::Url::parse(url) {
                Ok(parsed) if parsed.scheme() == "http" || parsed.scheme() == "https" => {}
                Ok(parsed) => errors.push(format!(
                    "replication.master_url scheme '{}' is invalid; must be http or https",
                    parsed.scheme()
                )),
                Err(_) => errors.push(format!(
                    "replication.master_url '{}' is not a valid URL",
                    url
                )),
            }
        }

        if self.poll_interval_secs < 1 {
            errors.push("replication.poll_interval_secs must be at least 1".to_string());
        }

        if self.parallelism < 1 {
            errors.push("replication.parallelism must be at least 1".to_string());
        }

        if self.reconnect_backoff_ms < 1 {
            errors.push("replication.reconnect_backoff_ms must be at least 1".to_string());
        }
    }
}

/// Webhook receiver configuration. When present, every commit produces one
/// HTTP POST per changed file (see `hooks.rs`), filtered down to the events
/// listed in `events`.
#[derive(Debug, Deserialize, Clone)]
pub struct HooksConfig {
    pub url: String,
    /// Only these event kinds are delivered; changes producing other kinds
    /// are silently skipped. Lets a receiver subscribe to e.g. deletions only.
    pub events: Vec<HookEvent>,
    /// Total delivery attempts per payload (first try included).
    pub retry_attempts: u32,
    /// Base delay for exponential backoff: attempt N waits
    /// `retry_backoff_ms * 2^(N-1)` before retrying.
    pub retry_backoff_ms: u64,
    /// Optional static header (e.g. `Authorization`) attached to every
    /// delivery so the receiver can authenticate this server.
    pub auth: Option<HookAuthConfig>,
}

impl HooksConfig {
    fn collect_errors(&self, errors: &mut Vec<String>) {
        match reqwest::Url::parse(&self.url) {
            Ok(parsed) if parsed.scheme() == "http" || parsed.scheme() == "https" => {}
            Ok(parsed) => errors.push(format!(
                "hooks.url scheme '{}' is invalid; must be http or https",
                parsed.scheme()
            )),
            Err(_) => errors.push(format!("hooks.url '{}' is not a valid URL", self.url)),
        }

        if self.events.is_empty() {
            errors.push("hooks.events must contain at least one event".to_string());
        }

        if self.retry_attempts < 1 {
            errors.push("hooks.retry_attempts must be at least 1".to_string());
        }

        if self.retry_backoff_ms < 1 {
            errors.push("hooks.retry_backoff_ms must be at least 1".to_string());
        }

        if let Some(auth) = &self.auth {
            auth.collect_errors(errors);
        }
    }
}

/// The webhook event kinds: four for file changes, matching the `FileChange`
/// variants in `git.rs` one-to-one, and two for changes to a directory's file
/// order index, matching the `OrderChange` variants in `hooks.rs`. Serialised
/// with dotted names (`"file.created"`) because that is the wire format used
/// both in `config.toml` and in the delivered JSON payloads.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    #[serde(rename = "file.created")]
    FileCreated,
    #[serde(rename = "file.updated")]
    FileUpdated,
    #[serde(rename = "file.deleted")]
    FileDeleted,
    #[serde(rename = "file.moved")]
    FileMoved,
    #[serde(rename = "order.updated")]
    OrderUpdated,
    #[serde(rename = "order.deleted")]
    OrderDeleted,
}

#[derive(Debug, Deserialize, Clone)]
pub struct HookAuthConfig {
    pub header: String,
    pub value: String,
}

impl HookAuthConfig {
    fn collect_errors(&self, errors: &mut Vec<String>) {
        if self.header.trim().is_empty() {
            errors.push("hooks.auth.header must not be empty".to_string());
        }

        if self.value.trim().is_empty() {
            errors.push("hooks.auth.value must not be empty".to_string());
        }
    }
}
