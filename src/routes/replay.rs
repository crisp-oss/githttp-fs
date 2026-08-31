// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Webhook replay: re-emitting past events so a downstream mirror that fell
//! out of sync can converge again.
//!
//! Hook delivery is durable only in memory — a receiver that was down past
//! its retry budget, or that mis-applied an event, ends up holding a state
//! this server never agreed to. The obvious repair (wipe the mirror and
//! replay everything) is lossy whenever the mirror holds metadata the
//! repository does not, so this route repairs *in place* instead.
//!
//! One route, one set operation, two directions. The caller sends the paths
//! it holds; the server intersects them with what git holds and replays
//! whichever side of that intersection the direction asks for:
//!
//! - `delete` replays a `file.deleted` for everything **outside** the
//!   intersection — the caller's orphans, the rows git cannot account for.
//! - `create` replays a `file.created` for everything **inside** it — the
//!   files git can vouch for, so a receiver missing rows re-inserts them.
//!
//! Two directions of drift, one topology, and the direction chooses the side
//! rather than the route. Omitting `files` defaults it to every file git
//! holds, which makes `create` cover the whole scope and makes `delete` a
//! no-op by construction — git cannot be missing what it just listed.
//!
//! Nothing here writes. No commit is created, no file is touched, and
//! background maintenance is not armed: this is a pure read that enqueues
//! hook work. What it *does* share with every write handler is the tenant
//! write lock, and for a reason that is not cosmetic — see below.
//!
//! Three properties are worth stating explicitly:
//!
//! **The snapshot is taken under the write lock.** Reads normally skip the
//! lock, but a replay does not, because it enqueues. Without the lock a PUT
//! committing concurrently could have its `file.created` enqueued *before* a
//! replay whose snapshot predates it, and the receiver would apply a
//! `file.deleted` for a path that had just been created — precisely the drift
//! this feature exists to repair. Taking the lock makes "read HEAD" and
//! "enqueue" atomic with respect to commits, so queue order still equals the
//! order this server accepted things.
//!
//! **Content is resolved at delivery, not here.** The job carries paths; the
//! hook consumer reads each file just before its POST. That bounds memory to
//! a chunk rather than the whole corpus, which matters because a throttled
//! replay of a large repository can occupy its queue for hours.
//!
//! **`delay_ms` throttles, it does not order.** Delivery is already strictly
//! sequential per repository. The delay exists to spare a receiver from a
//! sustained burst, and its cost is that it holds that repository's queue for
//! its whole duration — every commit accepted after the replay waits behind
//! it.

use axum::{extract::Path as AxumPath, extract::State, http::StatusCode, Json};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;

use crate::{
    error::AppError,
    git,
    hooks::{HookJob, ReplayJob, ReplayKind},
    order,
    state::AppState,
    util::run_blocking,
    validate,
};

/// Upper bound on the inter-hook throttle. A replay holds its repository's
/// hook queue for `delay_ms × file_count`, so an unbounded delay is really an
/// unbounded outage for every later commit's hooks. One minute per file is
/// already far past any sane rate-limit accommodation, and a caller that
/// wants slower can replay in several `prefix_path`-scoped passes.
const MAX_DELAY_MS: u64 = 60_000;

/// Which side of the intersection to replay.
///
/// The two directions repair opposite drifts and are deliberately one
/// parameter on one route rather than two routes: they take identical inputs
/// and differ only in a single set operation, so splitting them would
/// duplicate the whole path-validation and snapshot topology to express a
/// boolean.
#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum ReplayDirection {
    /// Replay `file.deleted` for the paths **not** in the intersection: the
    /// caller holds them, git does not.
    Delete,
    /// Replay `file.created` for the paths **in** the intersection: git holds
    /// them, so the caller should too.
    Create,
}

impl ReplayDirection {
    /// Whether this direction keeps the paths git holds (`create`) or the
    /// ones it does not (`delete`). The whole difference between the two
    /// directions reduces to this one bit.
    fn keeps_present(self) -> bool {
        matches!(self, ReplayDirection::Create)
    }

    fn replay_kind(self) -> ReplayKind {
        match self {
            ReplayDirection::Delete => ReplayKind::Deleted,
            ReplayDirection::Create => ReplayKind::Created,
        }
    }
}

/// Body of `POST /batch/replay/hook`.
///
/// No `author`: nothing is committed, so there is no signature to record.
#[derive(Deserialize)]
pub struct ReplayHookRequest {
    direction: ReplayDirection,
    /// The paths the caller holds, repo-root-relative. Omitted defaults to
    /// every file git holds in scope.
    #[serde(default)]
    files: Option<Vec<String>>,
    /// Scopes the git-side snapshot to one folder, with the listing route's
    /// semantics (a non-existent folder scopes to nothing). When set, every
    /// entry of `files` must sit under it.
    #[serde(default)]
    prefix_path: Option<String>,
    /// Only meaningful when `files` is omitted: it shapes the default set.
    /// The snapshot the set operation runs against always includes hidden
    /// files — see [`replay_hook`].
    #[serde(default)]
    include_hidden_files: bool,
    #[serde(default)]
    delay_ms: Option<u64>,
}

/// Replays file hooks for one side of the caller's intersection with git.
///
/// The one subtlety worth knowing is how `include_hidden_files` interacts
/// with the snapshot. When `files` is given, the snapshot deliberately
/// includes hidden files no matter what the flag says, because the set
/// operation needs git's set to be **maximal**: a file hidden from the
/// snapshot would fall outside the intersection and replay a `file.deleted`
/// for a file that is very much still there. The flag only shapes the
/// *default* set used when `files` is omitted, where both sides come from the
/// same listing and the risk cannot arise.
pub async fn replay_hook(
    State(state): State<AppState>,
    AxumPath((collection_id, tenant_id)): AxumPath<(String, String)>,
    Json(body): Json<ReplayHookRequest>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    let delay_ms = validate_delay(body.delay_ms)?;

    let prefix_path = match &body.prefix_path {
        Some(raw) => Some(validate::folder_path(raw)?.to_string()),
        None => None,
    };

    // Everything judgeable from the values alone is settled before the
    // repository is opened, exactly as on the batch read and order routes.
    let files = match &body.files {
        Some(raw_files) => Some(sanitize_files(raw_files, prefix_path.as_deref())?),
        None => None,
    };

    require_hook_receiver(&state)?;

    let direction = body.direction;

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, direction = ?direction, given_count = ?files.as_ref().map(Vec::len), prefix_path = ?prefix_path, "handling hook replay request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let lock_key = format!("{}/{}", collection_id, tenant_id);
    let lock = state.get_repo_lock(&lock_key);

    // Held across the snapshot *and* the enqueue, so a commit landing
    // concurrently cannot slip its hooks in front of a replay that predates it.
    let _lock_guard = lock.lock().await;

    let repo_path_for_task = repo_path.clone();
    let tenant_id_for_task = tenant_id.clone();
    let prefix_for_task = prefix_path.clone();

    // Maximal snapshot whenever the caller supplied its own list — see the
    // doc comment above for why hidden files cannot be excluded there.
    let include_hidden_files = files.is_some() || body.include_hidden_files;

    let (head_sha, present_paths) = run_blocking(move || {
        git::GitFiles::list_all_file_paths(
            &repo_path_for_task,
            &tenant_id_for_task,
            prefix_for_task.as_deref(),
            include_hidden_files,
        )
    })
    .await?;

    let present: HashSet<&str> = present_paths.iter().map(String::as_str).collect();

    // Omitting `files` defaults it to everything git holds. The two
    // directions then fall out very differently — `create` covers the whole
    // scope, while `delete` yields nothing, since git cannot be missing what
    // it just listed — but that asymmetry is inherent to the set operation
    // rather than a special case, so it needs no branch here.
    let candidates: &[String] = files.as_deref().unwrap_or(&present_paths);
    let keeps_present = direction.keeps_present();

    let affected: Vec<String> = candidates
        .iter()
        .filter(|path| present.contains(path.as_str()) == keeps_present)
        .cloned()
        .collect();

    let file_count = affected.len();
    let scheduled = file_count > 0;

    tracing::info!(collection_id = %collection_id, tenant_id = %tenant_id, direction = ?direction, candidate_count = candidates.len(), present_count = present.len(), file_count, "computed hook replay set");

    if scheduled {
        state.hook_queue.enqueue(
            &lock_key,
            HookJob::replay(
                collection_id,
                tenant_id,
                head_sha.clone(),
                Utc::now(),
                ReplayJob {
                    repo_path,
                    kind: direction.replay_kind(),
                    paths: affected,
                    delay_ms,
                },
            ),
        );
    }

    // The affected paths themselves are not echoed: in the `create` direction
    // the set is bounded by the repository rather than by the request, so a
    // whole-tenant replay would answer with the entire file list for no
    // benefit. `commit_sha` is the HEAD the snapshot was taken from — no
    // commit was created; it is the honest answer to "which state was this
    // computed against".
    Ok((
        StatusCode::OK,
        Json(json!({
            "commit_sha": head_sha,
            "files": file_count,
        })),
    ))
}

/// Rejects a replay when no receiver is configured at all.
///
/// `HookQueue::enqueue` would silently drop the job, and answering `200` with
/// a file count for a reconciliation that delivered nothing is worse than an
/// error — the whole point of the route is to tell an operator that the
/// mirror has been repaired.
fn require_hook_receiver(state: &AppState) -> Result<(), AppError> {
    if state.config.hooks.is_none() {
        return Err(AppError::InvalidOperation {
            reason: "no webhook receiver is configured; a replay would deliver nothing".to_string(),
        });
    }

    Ok(())
}

/// Validates the optional throttle against [`MAX_DELAY_MS`].
fn validate_delay(delay_ms: Option<u64>) -> Result<Option<u64>, AppError> {
    if let Some(delay_ms) = delay_ms {
        if delay_ms > MAX_DELAY_MS {
            return Err(AppError::InvalidOperation {
                reason: format!(
                    "delay_ms must not exceed {} milliseconds, got {}",
                    MAX_DELAY_MS, delay_ms
                ),
            });
        }
    }

    Ok(delay_ms)
}

/// Sanitises the caller's path list with the same rules as every other `*path`
/// on this API, and rejects duplicates after sanitisation exactly as the batch
/// read route does — two spellings of one path would replay the same event
/// twice.
///
/// Paths stay **repo-root-relative**, as they are everywhere else on this API
/// and in every hook payload; `prefix_path` is a guard rail rather than a
/// join, so an entry outside it is a `400`. Rejecting is what keeps both sides
/// of the set operation on the same footing: the git snapshot is scoped to the
/// prefix, so an out-of-scope entry would sit outside the intersection for a
/// reason that has nothing to do with whether git holds it — and in the
/// `delete` direction that reads as an orphan and drops a live row.
///
/// An order-index path is likewise rejected rather than dropped. It cannot
/// legitimately be in a caller's list (the index is invisible to every
/// `/files` route, so nothing downstream should ever have mirrored one), and
/// letting it through would be actively wrong: `HookJob::new` classifies
/// events by path, so a `file.deleted` on an index path would reach the
/// receiver as an `order.deleted`, wiping a directory's stored order on the
/// strength of a caller mistake.
fn sanitize_files(
    raw_files: &[String],
    prefix_path: Option<&str>,
) -> Result<Vec<String>, AppError> {
    if raw_files.is_empty() {
        return Err(AppError::InvalidOperation {
            reason:
                "files must contain at least one path; omit it entirely to default to every file"
                    .to_string(),
        });
    }

    let mut sanitized: Vec<String> = Vec::with_capacity(raw_files.len());
    let mut seen: HashSet<&str> = HashSet::with_capacity(raw_files.len());

    for raw_file in raw_files {
        let path = validate::file_path(raw_file)?;

        if order::is_order_file(path) {
            return Err(AppError::InvalidOperation {
                reason: format!(
                    "files must not reference an order index: {}; file order is a separate resource under /order",
                    raw_file
                ),
            });
        }

        if let Some(prefix) = prefix_path.filter(|prefix| !prefix.is_empty()) {
            let inside = path
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix('/'))
                .is_some_and(|rest| !rest.is_empty());

            if !inside {
                return Err(AppError::InvalidOperation {
                    reason: format!(
                        "files entry '{}' must sit under prefix_path '{}'",
                        raw_file, prefix
                    ),
                });
            }
        }

        if !seen.insert(path) {
            return Err(AppError::InvalidOperation {
                reason: format!("files must be unique: '{}' is listed twice", path),
            });
        }

        sanitized.push(path.to_string());
    }

    Ok(sanitized)
}
