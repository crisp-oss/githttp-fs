// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Startup healing of working trees that do not match HEAD.
//!
//! The working tree is a courtesy copy of HEAD kept on disk so a human can
//! `ls` a tenant repository (see `WorkingTree` in `git.rs`). Every ordinary
//! operation keeps it current *as it goes*: a write mirrors the file it
//! commits, a delete removes it, a replicated pack checks its repository out.
//! None of that is retroactive, which leaves exactly one gap — a repository
//! whose files were never written in the first place, or were written and
//! then left to drift:
//!
//! - a node that ran with `server.checkout_files = false` and now runs with it
//!   on, including every tenant created while it was off,
//! - a replica whose store was filled before replication checked anything out,
//!   and whose tenants will not be touched again until their master commits,
//! - a tenant left half-written by a process killed mid-operation.
//!
//! So this pass runs once per boot and checks every repository out to its
//! HEAD. A checkout converges from *any* prior state, which is what makes one
//! pass enough and makes the pass idempotent: on an already-correct store it
//! writes nothing, and it is reported as having healed nothing.
//!
//! **Startup is the only sane trigger.** Healing on read would mean writing to
//! disk from a read path — which would have to take the tenant write lock that
//! reads in this codebase deliberately never take, so one `GET` could block
//! behind a commit. Healing on write would make whichever request happens to
//! touch a long-disabled tenant first pay for checking out the whole of it
//! while holding that tenant's lock, and would never converge for the tenants
//! nobody writes to — which are precisely the ones an operator goes looking at
//! on disk. A boot pass is off every request path and bounded by the store.
//!
//! **It is opt-in** (`server.checkout_files_autoheal`, default `false`), which
//! is the one place this feature's two keys disagree on a default. Keeping
//! files on disk is what every deployment already did, so `checkout_files`
//! defaults to `true`; this pass never ran before, touches every repository in
//! the store, and *removes* files HEAD no longer names, so it waits to be
//! asked. Its cost is also the only part of the feature that scales with the
//! whole store rather than with one operation: roughly a `stat` per file per
//! boot even when nothing needs repairing.
//!
//! Leaving it off stops *only* this pass — mirroring as work arrives carries on
//! regardless — so healing a store that ran without `checkout_files` is a
//! deliberate two-step: enable this for one restart, then turn it back off.

use std::time::Instant;

use crate::git::GitReplication;
use crate::state::AppState;
use crate::util::run_blocking;

/// Spawns the heal pass, or does nothing when this node is not keeping files
/// on disk (nothing to heal) or has not asked for it (`checkout_files_autoheal`,
/// off unless set).
///
/// Spawned rather than awaited: nothing waits on the working tree, so the
/// listener binds and serves while the pass runs behind it. Every route
/// answers from HEAD's tree, so a tenant whose turn has not come yet is
/// fully readable throughout.
pub fn spawn(state: AppState) {
    if !state.config.server.checkout_files {
        return;
    }

    if !state.config.server.checkout_files_autoheal {
        tracing::debug!("working tree heal on start is not enabled");

        return;
    }

    tokio::spawn(run(state));
}

/// Checks every repository this node holds out to its HEAD, one at a time.
///
/// Sequential on purpose: it is disk work nothing is waiting for, and running
/// it in parallel would turn a boot into an I/O storm on exactly the node an
/// operator just restarted. Each repository is mirrored under its own write
/// lock, so the pass can never run against a repository a write or a
/// replicated pack apply is in the middle of, nor against a maintenance pass.
async fn run(state: AppState) {
    let repos_path = state.config.server.repos_path.clone();

    // The same disk walk replication uses to enumerate tenants; it lives
    // there because that is what needed it first, and it knows nothing about
    // replication beyond the name of its module.
    let scan = match run_blocking(move || Ok(GitReplication::list_repositories(&repos_path))).await
    {
        Ok(scan) => scan,
        Err(err) => {
            tracing::warn!(err = %err, "cannot list repositories to heal working trees");

            return;
        }
    };

    if scan.repositories.is_empty() {
        return;
    }

    let started_at = Instant::now();
    let total = scan.repositories.len();
    let mut healed_repositories = 0_usize;
    let mut healed_files = 0_usize;

    for repository in scan.repositories {
        let repo_path = state
            .config
            .server
            .repos_path
            .join(&repository.collection_id)
            .join(&repository.tenant_id);

        let lock_key = format!("{}/{}", repository.collection_id, repository.tenant_id);
        let lock = state.get_repo_lock(&lock_key);
        let _lock_guard = lock.lock().await;

        let touched = run_blocking(move || Ok(GitReplication::mirror_working_tree(&repo_path)))
            .await
            .unwrap_or(0);

        if touched > 0 {
            tracing::info!(
                repository = %lock_key,
                files = touched,
                "healed working tree"
            );

            healed_repositories += 1;
            healed_files += touched;
        }
    }

    // Logged either way, including the nothing-to-do case: an operator who
    // turned `checkout_files` on wants to see that the pass ran and found the
    // store already correct, not silence.
    tracing::info!(
        repositories = total,
        healed_repositories = healed_repositories,
        healed_files = healed_files,
        elapsed_ms = started_at.elapsed().as_millis(),
        "working tree heal on start complete"
    );
}
