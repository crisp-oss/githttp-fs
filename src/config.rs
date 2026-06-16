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

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub hooks: HooksConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub api_key: String,
    pub repos_path: PathBuf,
    pub log_level: Option<LogLevel>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct HooksConfig {
    pub url: String,
    pub events: Vec<HookEvent>,
    pub retry_attempts: u32,
    pub retry_backoff_ms: u64,
    pub auth: Option<HookAuthConfig>,
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
