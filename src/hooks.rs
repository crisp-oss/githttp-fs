// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use reqwest::Client;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

use crate::config::{Config, HookEvent, HooksConfig};
use crate::git::FileChange;

/// Cap on the exponential backoff exponent to avoid `1 << n` overflow when an
/// operator configures a very high retry count.
const MAX_BACKOFF_EXPONENT: u32 = 20;

/// A single unit of hook work: all file changes produced by one commit.
pub struct HookJob {
    pub tenant_id: String,
    pub commit_sha: String,
    pub committed_at: DateTime<Utc>,
    pub file_changes: Vec<FileChange>,
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

        for file_change in job.file_changes {
            let required_event = Self::event_for_change(&file_change);

            if !hooks.events.contains(&required_event) {
                continue;
            }

            let payload = Self::build_payload(
                &job.tenant_id,
                &job.commit_sha,
                &job.committed_at,
                &file_change,
            );
            let description = Self::change_description(&file_change);

            Self::deliver_with_retries(
                client,
                hooks,
                attempts,
                payload,
                &job.tenant_id,
                &job.commit_sha,
                &description,
            )
            .await;
        }
    }

    async fn deliver_with_retries(
        client: &Client,
        hooks: &HooksConfig,
        attempts: u32,
        payload: Value,
        tenant_id: &str,
        commit_sha: &str,
        change_description: &str,
    ) {
        for attempt in 1..=attempts {
            match Self::send(client, hooks, &payload).await {
                Ok(()) => {
                    tracing::debug!(
                        tenant_id,
                        commit_sha,
                        change = change_description,
                        "hook delivered"
                    );

                    return;
                }
                Err(delivery_err) => {
                    tracing::error!(
                        tenant_id,
                        commit_sha,
                        change = change_description,
                        attempt,
                        total = attempts,
                        "hook delivery failed: {}",
                        delivery_err
                    );

                    if attempt == attempts {
                        tracing::error!(
                            tenant_id, commit_sha, change = change_description,
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

    fn event_for_change(file_change: &FileChange) -> HookEvent {
        match file_change {
            FileChange::Created { .. } => HookEvent::FileCreated,
            FileChange::Updated { .. } => HookEvent::FileUpdated,
            FileChange::Deleted { .. } => HookEvent::FileDeleted,
            FileChange::Moved { .. } => HookEvent::FileMoved,
        }
    }

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

    fn build_payload(
        tenant_id: &str,
        commit_sha: &str,
        committed_at: &DateTime<Utc>,
        file_change: &FileChange,
    ) -> Value {
        let mut payload = Map::with_capacity(5);

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
