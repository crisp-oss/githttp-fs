// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! TOML configuration types and startup validation.
//!
//! The structs here mirror the sections of `config.toml` one-to-one
//! (`[server]`, `[limits]`, `[hooks]`, `[hooks.auth]`, `[maintenance]`)
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

/// The four webhook event kinds, matching the `FileChange` variants in
/// `git.rs` one-to-one. Serialised with dotted names (`"file.created"`)
/// because that is the wire format used both in `config.toml` and in the
/// delivered JSON payloads.
#[allow(clippy::enum_variant_names)]
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
