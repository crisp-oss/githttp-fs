// Flavio
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

use serde::Deserialize;
use std::path::PathBuf;

/// Tracing log level. Accepts "trace", "debug", "info", "warn", "error".
/// Defaults to "info" if unset. Overridden by the RUST_LOG env var.
type LogLevel = String;

const VALID_LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub hooks: Option<HooksConfig>,
}

impl Config {
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        self.server.collect_errors(&mut errors);

        if let Some(hooks) = &self.hooks {
            hooks.collect_errors(&mut errors);
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
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

#[derive(Debug, Deserialize, Clone)]
pub struct HooksConfig {
    pub url: String,
    pub events: Vec<HookEvent>,
    pub retry_attempts: u32,
    pub retry_backoff_ms: u64,
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
