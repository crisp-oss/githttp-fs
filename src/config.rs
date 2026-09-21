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
    /// Opt-in pack-count trigger. When set, a repository holding at least
    /// this many packfiles gets its maintenance pass *immediately* on the
    /// next write (or replicated pack apply) instead of after `delay_secs`.
    ///
    /// Unset by default, because a master rarely needs it: its writes land
    /// as loose objects and one consolidated pack a day is plenty. A replica
    /// is the case it exists for — every applied delta arrives as its own
    /// pack, so a busy tenant polled every minute can carry hundreds of packs
    /// before a daily pass, and libgit2 consults every pack index on every
    /// object lookup. The count is one directory read per write, checked
    /// only when this is set.
    pub maximum_packs: Option<usize>,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // 24 hours
            delay_secs: 86_400,
            destructive_prune: false,
            maximum_packs: None,
        }
    }
}

impl MaintenanceConfig {
    fn collect_errors(&self, errors: &mut Vec<String>) {
        if self.enabled && self.delay_secs < 1 {
            errors.push("maintenance.delay_secs must be at least 1".to_string());
        }

        // Two packs is the smallest count at which consolidation does
        // anything: the pass already skips a repository holding one pack and
        // no loose objects, so a threshold of 1 would arm a no-op after every
        // write.
        if let Some(maximum_packs) = self.maximum_packs {
            if maximum_packs < 2 {
                errors.push("maintenance.maximum_packs must be at least 2".to_string());
            }
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
    /// Whether each tenant's files are spelled out on the working tree, so a
    /// human can `ls` a repository.
    ///
    /// Nothing this server answers ever reads them — every route resolves
    /// content from HEAD's tree and the object database — so turning this off
    /// costs only that inspectability, and saves the *uncompressed* size of
    /// all content plus one filesystem block per file and directory. Git
    /// compresses blobs, so on markdown the working tree is several times
    /// larger than the `.git` holding its whole history.
    ///
    /// Defaults to `true`, which is how every deployment behaved before this
    /// key existed.
    #[serde(default = "default_true")]
    pub checkout_files: bool,
    /// Whether startup heals tenants whose files are missing or stale on
    /// disk, by checking every one of them out.
    ///
    /// This is what makes `checkout_files = true` retroactive: a store that
    /// ran with it off, or one replicated before replication checked
    /// anything out, has working trees that no ordinary write would ever
    /// fill in. The pass is idempotent and converging, so it repairs both
    /// "never mirrored" and "went stale while mirroring was off".
    ///
    /// **Defaults to `false`**, unlike `checkout_files`, and the asymmetry is
    /// deliberate. A default is a judgement about what should happen to a
    /// deployment that said nothing, and these two keys answer different
    /// questions: files on disk were always there, so keeping them changes
    /// nothing — whereas this pass never ran before, touches *every*
    /// repository in the store, and *removes* files HEAD no longer names.
    /// Work of that blast radius is something an operator asks for, not
    /// something an upgrade starts doing. Its cost is also the one part of
    /// the feature that scales with the whole store rather than with one
    /// operation: roughly a `stat` per file per boot, even when nothing needs
    /// writing.
    ///
    /// Leaving it off stops nothing else: a master still mirrors each file as
    /// it commits it, and a replica still checks a repository out when a pack
    /// lands. Turning `checkout_files` on for a store that ran without it is
    /// therefore a deliberate two-step — enable this for one restart, then
    /// turn it back off.
    ///
    /// Inert when `checkout_files` is off, rather than a config error: there
    /// is nothing to heal, the same way `include_date_type` alone changes
    /// nothing on a listing.
    #[serde(default)]
    pub checkout_files_autoheal: bool,
}

/// Serde default for a flag whose absence must mean "as it always was".
fn default_true() -> bool {
    true
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
    /// Safety timer on the commit-history walk a date-filtered listing runs,
    /// in milliseconds; a listing that exhausts it stops walking and answers
    /// with the entries it had dated by then, flagged `partial`. Defaults to
    /// ten seconds — the one listing cost that is bounded by history length
    /// rather than by the request, so a node that never ran out of time
    /// keeps the exact answers it always gave, while one that would have
    /// spent minutes on a query now answers. `0` turns the timer off and
    /// restores the unbounded walk.
    pub date_filter_maximum_ms: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            allowed_extensions: None,
            batch_read_maximum_files: 100,
            date_filter_maximum_ms: 10_000,
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
    /// How this node names itself to its peers: the row it occupies in a
    /// master's roster, and the name every log line about it carries.
    ///
    /// **Required**, and required to be unique within a deployment. The
    /// master refuses a second notification stream claiming a node id that
    /// is already connected from a different process (`409`), because a
    /// roster keyed on a shared name would fold two replicas into one row
    /// and an operator would never learn that one of them had gone. There
    /// is no default on purpose: the obvious one, `host:port`, is
    /// `0.0.0.0:5355` on every node bound to all interfaces — identical
    /// everywhere, which is the collision this rule exists to prevent.
    ///
    /// This is **identity for telemetry, not authentication** — `secret` is
    /// what guards the replication surface. A node that lies about its name
    /// can mislead a dashboard or be refused a stream, nothing more, which is
    /// what lets a replica still need no registration on its master: it
    /// joins by connecting.
    pub node_id: String,
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
    /// The mass-deletion guard. While `true` (the default), a replica refuses
    /// a master listing that would delete more than half of the repositories
    /// it holds, and reports it instead. Set to `false` — temporarily, with
    /// a restart — to accept such a deletion on purpose; nothing else clears
    /// a refused one, since a restarted replica still holds what it held.
    #[serde(default = "default_deletion_guard")]
    pub deletion_guard: bool,
}

fn default_deletion_guard() -> bool {
    true
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

    /// This node's id, as configured.
    pub fn node_id(&self) -> &str {
        &self.node_id
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
        // so a node never announces a name its master would discard. An
        // empty name is the same error as a missing one, and it is caught
        // here rather than deserialisation so the message names the rule.
        if let Err(err) = crate::validate::node_id(&self.node_id) {
            errors.push(format!(
                "replication.node_id is required and must be valid: {}",
                err
            ));
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
