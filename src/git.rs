// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Every git operation in the system, built on libgit2 (the `git2` crate,
//! compiled with `vendored-libgit2` so no system git is needed).
//!
//! All functions here are **synchronous** and must be called through
//! `util::run_blocking`; mutating functions must additionally be called
//! while holding the tenant write lock (see `state.rs`).
//!
//! The module is organised into stateless namespace structs:
//!
//! - `GitUtils` — private low-level helpers (signatures, repo open/init,
//!   blob reads, tree building)
//! - `GitLocks` — stale `.git/index.lock` cleanup
//! - `GitMaintenance` — consolidating repack, optional prune, reflog
//!   expiry, index refresh
//! - `GitFiles` — file CRUD (list / read / exists / write / delete / move)
//! - `GitCommits` — history listing, commit detail, revert
//! - `GitTenant` — tenant repository deletion
//!
//! Two invariants shape everything below:
//!
//! **HEAD is authoritative; the working tree is a courtesy.** Every
//! existence check, content read, and commit tree is derived from HEAD's
//! tree — never from files on disk. The working tree is still kept in sync
//! (so a human can `ls` and inspect a repo), but if a past operation died
//! halfway and left stray files behind, they can never alter an operation's
//! outcome or get silently swept into a later commit. Each commit contains
//! exactly the intended change and nothing else.
//!
//! **Commits are built with `TreeUpdateBuilder`, not the git index.** The
//! classic index route (`add_path` → `write_tree`) costs O(repository size)
//! per commit because the whole index is rewritten. `TreeUpdateBuilder`
//! instead grafts a single change onto HEAD's existing tree, costing
//! O(touched path depth): only the trees along the changed path are
//! rewritten, everything else is shared with the previous commit by oid.
//! Large repositories therefore commit as fast as small ones, and moves and
//! reverts reuse existing blob oids outright (no content rehash).

use chrono::{DateTime, Utc};
use git2::build::TreeUpdateBuilder;
use git2::{
    Delta, DiffFindOptions, DiffFormat, DiffOptions, FileMode, Oid, Repository, Signature, Sort,
};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::error::AppError;

/// A node in the repository file tree returned by the list endpoint.
/// Serialises with a `"type"` discriminant field so clients can distinguish
/// files from directories without inspecting the presence of `children`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TreeNode {
    File {
        name: String,
    },
    Directory {
        name: String,
        children: Vec<TreeNode>,
    },
}

#[derive(Debug, Serialize)]
pub struct CommitAuthor {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Serialize)]
pub struct CommitSummary {
    pub sha: String,
    pub message: String,
    pub author: CommitAuthor,
    pub committed_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct CommitDetail {
    pub sha: String,
    pub message: String,
    pub author: CommitAuthor,
    pub committed_at: DateTime<Utc>,
    pub files: Vec<CommitFileDetail>,
}

#[derive(Debug, Serialize)]
pub struct CommitFileDetail {
    pub path: String,
    /// "created" | "updated" | "deleted" | "moved"
    pub change: String,
    /// Only present for moved files — the previous path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_path: Option<String>,
    /// Full file content at this commit. Empty string for deleted files.
    pub content: String,
    /// Unified diff for this file.
    pub diff: String,
}

/// Describes a single file change that occurred in a commit.
/// Used internally to drive hook delivery.
///
/// `Created`/`Updated`/`Moved` carry the resulting content so the hook
/// payload can be built later without re-opening the repository — by the
/// time the hook consumer runs, further commits may already have landed.
#[derive(Debug, Clone)]
pub enum FileChange {
    Created {
        path: String,
        content: String,
    },
    Updated {
        path: String,
        content: String,
    },
    Deleted {
        path: String,
    },
    Moved {
        from_path: String,
        to_path: String,
        content: String,
    },
}

/// Internal record used while building per-file commit details.
/// An owned snapshot of one `git2::DiffDelta` — copied out because the
/// diff object cannot be borrowed while also being consumed by `print`.
struct DeltaRecord {
    status: Delta,
    old_oid: Oid,
    new_oid: Oid,
    old_path: Option<PathBuf>,
    new_path: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// GitUtils — private low-level helpers shared across all operation groups
// ---------------------------------------------------------------------------

struct GitUtils;

impl GitUtils {
    /// Builds the git author/committer signature from the caller-supplied
    /// identity, timestamped "now". This is the single place where the
    /// non-empty checks on `author.name`/`author.email` are enforced — every
    /// commit path funnels through here.
    fn git_signature<'a>(
        author_name: &'a str,
        author_email: &'a str,
    ) -> Result<Signature<'a>, AppError> {
        if author_name.trim().is_empty() {
            return Err(AppError::InvalidOperation {
                reason: "author.name must not be empty".to_string(),
            });
        }
        if author_email.trim().is_empty() {
            return Err(AppError::InvalidOperation {
                reason: "author.email must not be empty".to_string(),
            });
        }

        tracing::trace!(author_name = %author_name, author_email = %author_email, "creating git signature");

        Signature::now(author_name, author_email).map_err(AppError::Git)
    }

    /// Converts a git commit timestamp (unix seconds + offset) into the UTC
    /// `DateTime` used in API responses. An out-of-range value — only
    /// possible with a corrupted repository — degrades to the epoch rather
    /// than failing the whole request.
    fn timestamp_from_git_time(git_time: git2::Time) -> DateTime<Utc> {
        DateTime::from_timestamp(git_time.seconds(), 0).unwrap_or(DateTime::UNIX_EPOCH)
    }

    /// Opens an existing tenant repository, mapping a missing directory to a
    /// 404-friendly `TenantNotFound` error rather than a generic git failure.
    fn open_tenant_repo(repo_path: &Path, tenant_id: &str) -> Result<Repository, AppError> {
        if !repo_path.exists() {
            tracing::debug!(tenant_id = %tenant_id, "tenant repository not found");

            return Err(AppError::TenantNotFound {
                tenant_id: tenant_id.to_string(),
            });
        }

        tracing::trace!(tenant_id = %tenant_id, path = %repo_path.display(), "opening tenant repository");

        Repository::open(repo_path).map_err(AppError::Git)
    }

    /// Opens an existing repo or initialises a new one with an empty root commit
    /// so that HEAD is always valid for subsequent operations.
    ///
    /// This is what makes tenant provisioning implicit: the first PUT to a
    /// brand-new tenant lands here and creates the repository on the fly.
    /// The immediate `"chore: initialize"` root commit matters — every other
    /// function in this module assumes `repo.head()` resolves to a commit,
    /// and an initialised-but-commitless repository would break that.
    fn open_or_init_repo(
        repo_path: &Path,
        author_name: &str,
        author_email: &str,
    ) -> Result<Repository, AppError> {
        if repo_path.join(".git").exists() {
            tracing::trace!(path = %repo_path.display(), "opening existing repository");

            return Repository::open(repo_path).map_err(AppError::Git);
        }

        tracing::info!(path = %repo_path.display(), "initialising new tenant repository");

        std::fs::create_dir_all(repo_path)?;

        let repo = Repository::init(repo_path)?;
        let signature = Self::git_signature(author_name, author_email)?;

        // An empty tree is required for the root commit so that HEAD is valid.
        tracing::trace!(path = %repo_path.display(), "writing empty tree for root commit");

        let empty_tree_id = repo.treebuilder(None)?.write()?;
        let empty_tree = repo.find_tree(empty_tree_id)?;

        let root_oid = repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "chore: initialize",
            &empty_tree,
            &[],
        )?;

        tracing::debug!(path = %repo_path.display(), sha = %root_oid, "root commit created");

        drop(empty_tree);

        Ok(repo)
    }

    /// Reads a blob's content from `tree` at `file_path` and decodes it as
    /// UTF-8. The UTF-8 requirement exists because content travels in JSON
    /// string fields (API responses and hook payloads) — binary blobs have
    /// no representation there, so they surface as a 422 instead.
    fn blob_content_from_tree(
        repo: &Repository,
        tree: &git2::Tree<'_>,
        file_path: &str,
    ) -> Result<String, AppError> {
        tracing::trace!(path = %file_path, "reading blob from tree");

        let tree_entry =
            tree.get_path(Path::new(file_path))
                .map_err(|_err| AppError::FileNotFound {
                    path: file_path.to_string(),
                })?;

        let blob = repo.find_blob(tree_entry.id())?;

        tracing::trace!(path = %file_path, blob_id = %tree_entry.id(), size = blob.size(), "blob found");

        std::str::from_utf8(blob.content())
            .map(|text| text.to_string())
            .map_err(|_err| AppError::InvalidUtf8 {
                path: file_path.to_string(),
            })
    }

    fn path_string(path: Option<&Path>) -> String {
        path.map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Builds a recursive `TreeNode` tree from a flat list of file paths plus
    /// an explicit list of directory stub paths (directories whose contents
    /// were not walked due to a depth limit). Directories are sorted before files
    /// at each level; entries within each group are sorted alphabetically.
    ///
    /// The two-phase approach (flat paths in, nested tree out) exists
    /// because the tree *walk* (`collect_subtree`) and the tree *shape*
    /// wanted by the API differ: git walks entries in its own order, while
    /// the response needs directories-before-files with alphabetical
    /// sorting within each group.
    fn build_tree(
        flat: Vec<String>,
        stubs: Vec<String>,
        max_depth: Option<usize>,
    ) -> Vec<TreeNode> {
        // Intermediate mutable representation. A BTreeMap keyed by entry
        // name gives alphabetical iteration for free; the dir-before-file
        // ordering is applied later, in `convert`.
        enum NodeBuilder {
            File,
            Dir(BTreeMap<String, NodeBuilder>),
        }

        // Threads one slash-separated path into the nested map, creating
        // intermediate directories as needed. When the depth limit is hit,
        // the directory at the limit is recorded as an empty stub and the
        // remainder of the path is dropped.
        fn insert(
            dir: &mut BTreeMap<String, NodeBuilder>,
            components: &[&str],
            max_depth: Option<usize>,
            current_depth: usize,
        ) {
            match components {
                [] => {}
                [name] => {
                    dir.insert(name.to_string(), NodeBuilder::File);
                }
                [name, rest @ ..] => {
                    if let Some(max) = max_depth {
                        if current_depth >= max {
                            dir.entry(name.to_string())
                                .or_insert_with(|| NodeBuilder::Dir(BTreeMap::new()));
                            return;
                        }
                    }
                    let child = dir
                        .entry(name.to_string())
                        .or_insert_with(|| NodeBuilder::Dir(BTreeMap::new()));

                    if let NodeBuilder::Dir(children) = child {
                        insert(children, rest, max_depth, current_depth + 1);
                    }
                }
            }
        }

        // Inserts a depth-limited directory as an (empty) directory node.
        // Kept separate from `insert` because a stub's final component is a
        // directory, whereas `insert`'s final component is always a file.
        fn insert_stub(dir: &mut BTreeMap<String, NodeBuilder>, components: &[&str]) {
            match components {
                [] => {}
                [name] => {
                    dir.entry(name.to_string())
                        .or_insert_with(|| NodeBuilder::Dir(BTreeMap::new()));
                }
                [name, rest @ ..] => {
                    let child = dir
                        .entry(name.to_string())
                        .or_insert_with(|| NodeBuilder::Dir(BTreeMap::new()));
                    if let NodeBuilder::Dir(children) = child {
                        insert_stub(children, rest);
                    }
                }
            }
        }

        // Recursively converts the builder map into the serialisable
        // `TreeNode` shape, applying the directories-first ordering at
        // every level (the BTreeMap already yields names alphabetically).
        fn convert(name: String, node: NodeBuilder) -> TreeNode {
            match node {
                NodeBuilder::File => TreeNode::File { name },
                NodeBuilder::Dir(children) => {
                    let mut dirs: Vec<TreeNode> = Vec::new();
                    let mut files: Vec<TreeNode> = Vec::new();

                    for (child_name, child_node) in children {
                        match child_node {
                            NodeBuilder::Dir(_) => dirs.push(convert(child_name, child_node)),
                            NodeBuilder::File => files.push(convert(child_name, child_node)),
                        }
                    }

                    TreeNode::Directory {
                        name,
                        children: dirs.into_iter().chain(files).collect(),
                    }
                }
            }
        }

        let mut root: BTreeMap<String, NodeBuilder> = BTreeMap::new();

        for path in flat {
            let components: Vec<&str> = path.split('/').collect();
            insert(&mut root, &components, max_depth, 1);
        }

        for stub_path in stubs {
            let components: Vec<&str> = stub_path.split('/').collect();
            insert_stub(&mut root, &components);
        }

        let mut dirs: Vec<TreeNode> = Vec::new();
        let mut files: Vec<TreeNode> = Vec::new();

        for (name, node) in root {
            match node {
                NodeBuilder::Dir(_) => dirs.push(convert(name, node)),
                NodeBuilder::File => files.push(convert(name, node)),
            }
        }

        dirs.into_iter().chain(files).collect()
    }
}

// ---------------------------------------------------------------------------
// GitLocks — stale lock file detection and cleanup
// ---------------------------------------------------------------------------

/// Cleanup of stale `.git/index.lock` files.
///
/// libgit2 creates `index.lock` while writing the index and removes it when
/// done; a process killed in between leaves the lock behind forever, and any
/// future index write fails until it is removed. The write path no longer
/// touches the index at all (commits go through `TreeUpdateBuilder`), so
/// only maintenance's index refresh can be blocked — but that still warrants
/// cleaning locks up at startup and before each refresh.
pub struct GitLocks;

impl GitLocks {
    /// Removes `.git/index.lock` if it is older than 30 seconds.
    /// A stale lock is left behind when a process is killed mid-operation.
    ///
    /// The age threshold is the safety margin: a lock younger than 30 s
    /// *could* belong to a live external process (say, an operator running
    /// `git` by hand inside the repo), so it is left alone. No internal
    /// operation holds the index lock anywhere near that long.
    pub fn cleanup_stale_index_lock(repo_path: &Path) -> Result<(), AppError> {
        const STALE_LOCK_THRESHOLD_SECS: u64 = 30;

        let lock_path = repo_path.join(".git").join("index.lock");

        let metadata = match std::fs::metadata(&lock_path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(AppError::Io(err)),
        };

        let modified_time = metadata.modified()?;

        let lock_age = std::time::SystemTime::now()
            .duration_since(modified_time)
            .unwrap_or_default();

        if lock_age.as_secs() > STALE_LOCK_THRESHOLD_SECS {
            tracing::warn!(
                "Removing stale git lock file at {:?} (age: {}s)",
                lock_path,
                lock_age.as_secs()
            );

            if let Err(err) = std::fs::remove_file(&lock_path) {
                // Another worker may have cleaned the lock in the meantime.
                if err.kind() != std::io::ErrorKind::NotFound {
                    return Err(AppError::Io(err));
                }
            }
        }

        Ok(())
    }

    /// Walks `repos_root` once on startup and removes any leftover `.git/index.lock`
    /// regardless of age — no live operation can hold a lock at boot.
    ///
    /// The directory layout being walked is `repos_root/<collection>/<tenant>/.git`.
    /// Every error along the way is swallowed on purpose: lock cleanup is
    /// best-effort hygiene and must never prevent the server from starting.
    pub fn cleanup_all_stale_locks(repos_root: &Path) {
        let collections_dir = match std::fs::read_dir(repos_root) {
            Ok(d) => d,
            Err(_) => return,
        };

        for collection_entry_result in collections_dir {
            let Ok(collection_entry) = collection_entry_result else {
                continue;
            };

            let collection_path = collection_entry.path();

            if !collection_path.is_dir() {
                continue;
            }

            let tenants_dir = match std::fs::read_dir(&collection_path) {
                Ok(d) => d,
                Err(_) => continue,
            };

            for tenant_entry_result in tenants_dir {
                let Ok(tenant_entry) = tenant_entry_result else {
                    continue;
                };

                let lock_path = tenant_entry.path().join(".git").join("index.lock");

                if lock_path.exists() {
                    tracing::warn!(
                        "Removing stale git lock file found on startup: {:?}",
                        lock_path
                    );

                    if let Err(remove_err) = std::fs::remove_file(&lock_path) {
                        tracing::error!(
                            "Failed to remove stale lock {:?}: {}",
                            lock_path,
                            remove_err
                        );
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GitMaintenance — background repack, optional prune, reflog expiry, index refresh
// ---------------------------------------------------------------------------

/// Summary of what one maintenance pass did, consumed by the scheduler's log
/// line in `maintenance.rs`.
#[derive(Debug, Default)]
pub struct MaintenanceReport {
    /// Reachable objects written into the consolidated packfile
    /// (0 when the repack was skipped because nothing had changed).
    pub packed_objects: usize,
    /// Loose object files deleted. All of them live on in the new pack —
    /// except, when `destructive_prune` is enabled, unreachable ones, which
    /// are thereby pruned.
    pub loose_objects_removed: usize,
    /// Superseded packfiles deleted after consolidation.
    pub old_packs_removed: usize,
}

/// The maintenance pass itself (the *when* lives in `maintenance.rs`; this
/// is the *what*). Equivalent in spirit to `git repack -a -d` + `git reflog
/// expire --all` + `git read-tree HEAD` — plus `git prune` when
/// `destructive_prune` is enabled — implemented directly against libgit2 so
/// the binary stays self-contained.
pub struct GitMaintenance;

impl GitMaintenance {
    /// Runs one full housekeeping pass over a tenant repository:
    ///
    /// 1. **Reflog expiry** — reflogs are pure bloat here: history is never
    ///    rewritten and the API never exposes them, yet every commit appends
    ///    an entry, so they grow without bound. They are simply deleted (and
    ///    start accumulating again until the next pass).
    /// 2. **Consolidating repack** — one new packfile is written, then *all*
    ///    loose objects and *all* superseded packfiles are deleted. What
    ///    goes into that pack depends on `destructive_prune`:
    ///    - `false` (default): every object in the store, reachable or not.
    ///      Maintenance can then never destroy data under any circumstance;
    ///      orphaned garbage (e.g. blobs from writes that failed between
    ///      blob creation and commit) is retained forever.
    ///    - `true`: only objects reachable from a ref. Orphaned garbage is
    ///      permanently pruned. Note this never touches *history*: commits
    ///      are append-only in this system, so every past file version —
    ///      including versions of since-deleted files — stays reachable
    ///      through its commit and is always carried over.
    ///    Skipped entirely when the repository is already consolidated (no
    ///    loose objects, at most one pack).
    /// 3. **Index refresh** — the on-disk index is reset to HEAD so
    ///    `git status` stays meaningful for humans (the write path never
    ///    touches the index).
    ///
    /// Must be called while holding the tenant write lock. That lock is also
    /// why pruning needs no grace period, unlike `git gc` with its two-week
    /// default: objects are only ever created under the same lock, so there
    /// can be no in-flight object that is "not referenced *yet*" — anything
    /// unreachable now is unreachable forever (the write path derives every
    /// commit from HEAD, never from pre-existing stray objects).
    ///
    /// Concurrent reads are safe throughout: readers hold their own
    /// `Repository` handles, deleting a pack file that a reader has mapped
    /// does not invalidate the mapping (POSIX unlink semantics), and on a
    /// missed lookup libgit2 rescans the pack directory and finds the new
    /// consolidated pack.
    pub fn run(repo_path: &Path, destructive_prune: bool) -> Result<MaintenanceReport, AppError> {
        // The tenant may have been deleted while the timer was armed.
        if !repo_path.join(".git").exists() {
            tracing::debug!(path = %repo_path.display(), "repository gone, skipping maintenance");

            return Ok(MaintenanceReport::default());
        }

        let repo = Repository::open(repo_path)?;

        Self::expire_reflogs(&repo);

        let loose_objects = Self::enumerate_loose_objects(repo_path)?;
        let packs_before = Self::enumerate_pack_stems(repo_path)?;

        tracing::debug!(
            path = %repo_path.display(),
            loose_objects = loose_objects.len(),
            packs = packs_before.len(),
            destructive_prune = destructive_prune,
            "running repository maintenance"
        );

        // Already consolidated (single pack, no loose objects) means no write
        // has landed since the previous pass — every write creates loose
        // objects — so the repack would just rebuild the identical pack.
        let report = if loose_objects.is_empty() && packs_before.len() <= 1 {
            MaintenanceReport::default()
        } else {
            Self::repack(
                &repo,
                repo_path,
                destructive_prune,
                &loose_objects,
                &packs_before,
            )?
        };

        // Refresh the on-disk index to HEAD. Clean a stale index.lock first —
        // this is the only code path left that writes the index.
        GitLocks::cleanup_stale_index_lock(repo_path)?;

        let head_tree = repo.head()?.peel_to_commit()?.tree()?;
        let mut index = repo.index()?;

        index.read_tree(&head_tree)?;
        index.write()?;

        Ok(report)
    }

    /// Writes one consolidated packfile — holding either every object or
    /// only the reachable ones, per `destructive_prune` — then deletes the
    /// now-redundant loose objects and superseded packs.
    ///
    /// Crash safety: the new pack is fully written and committed to the ODB
    /// *before* anything is deleted, so a failure at any point never loses
    /// objects — at worst it leaves redundant copies that the next pass
    /// cleans up.
    fn repack(
        repo: &Repository,
        repo_path: &Path,
        destructive_prune: bool,
        loose_objects: &[(Oid, PathBuf)],
        packs_before: &HashSet<PathBuf>,
    ) -> Result<MaintenanceReport, AppError> {
        let odb = repo.odb()?;
        let mut pack_builder = repo.packbuilder()?;

        if destructive_prune {
            Self::insert_reachable_objects(repo, &mut pack_builder)?;
        } else {
            // Non-destructive mode: carry over every object in the store —
            // loose and packed, reachable or not. The ODB iterator visits
            // all backends; duplicates are deduplicated by the packbuilder.
            odb.foreach(|oid| pack_builder.insert_object(*oid, None).is_ok())?;
        }

        // Stream the pack straight into the ODB — this writes both the
        // .pack and its .idx under .git/objects/pack.
        let mut pack_writer = odb.packwriter()?;

        pack_builder.foreach(|chunk| {
            use std::io::Write;

            pack_writer.write_all(chunk).is_ok()
        })?;

        pack_writer.commit()?;

        let mut report = MaintenanceReport {
            packed_objects: pack_builder.object_count(),
            ..Default::default()
        };

        // From here on the new pack is durable; deletions are best-effort
        // (a leftover file is redundant data, not corruption).
        for (_, loose_path) in loose_objects {
            let _ = std::fs::remove_file(loose_path);
        }

        report.loose_objects_removed = loose_objects.len();

        // Empty fan-out directories are pruned best-effort (`remove_dir`
        // refuses non-empty directories).
        let fanout_dirs: HashSet<PathBuf> = loose_objects
            .iter()
            .filter_map(|(_, loose_path)| loose_path.parent().map(PathBuf::from))
            .collect();

        for fanout_dir in fanout_dirs {
            let _ = std::fs::remove_dir(fanout_dir);
        }

        // Superseded packs are identified by directory diff rather than by
        // predicting the new pack's name (a libgit2 implementation detail).
        // If no new stem appeared, the consolidated pack was byte-identical
        // to an existing one — possible when only unreachable garbage
        // accumulated since the last pass — and the ODB just rewrote that
        // file in place. In that case nothing is deleted: we cannot tell
        // which old pack is the keeper, and redundant packs are merely
        // wasteful, never wrong. The next pass after a real write (distinct
        // pack name guaranteed by the new commit) sweeps them.
        let packs_after = Self::enumerate_pack_stems(repo_path)?;
        let new_pack_appeared = packs_after.difference(packs_before).next().is_some();

        if new_pack_appeared {
            for stem in packs_before {
                // libgit2 writes only .pack/.idx, but a human running `git
                // repack` inside the repo may have produced auxiliary files
                // sharing the stem — remove those too, not just the pair.
                for extension in ["pack", "idx", "rev", "mtimes", "keep", "bitmap"] {
                    let _ = std::fs::remove_file(stem.with_extension(extension));
                }

                report.old_packs_removed += 1;
            }
        }

        Ok(report)
    }

    /// Feeds the packbuilder with the complete reachable object set: every
    /// commit reachable from any ref, plus all trees and blobs those commits
    /// reference. Objects *not* collected here are the ones a destructive
    /// prune drops, so this must err on the side of keeping things.
    fn insert_reachable_objects(
        repo: &Repository,
        pack_builder: &mut git2::PackBuilder<'_>,
    ) -> Result<(), AppError> {
        let mut revwalk = repo.revwalk()?;

        // Reachability roots: HEAD plus every ref. The service itself only
        // ever creates HEAD/master, but a human may have added branches or
        // tags while inspecting a repo — a destructive prune must honour
        // those, not silently corrupt them.
        revwalk.push_head()?;

        for reference in repo.references()?.flatten() {
            if let Ok(name) = reference.name() {
                let _ = revwalk.push_ref(name);
            }

            // `push_ref` peels an annotated tag down to its commit for the
            // walk; the tag *object* itself must be packed separately or the
            // ref would dangle after the prune.
            if let Some(oid) = reference.target() {
                if let Ok(object) = repo.find_object(oid, None) {
                    if object.kind() == Some(git2::ObjectType::Tag) {
                        let _ = pack_builder.insert_object(oid, None);
                    }
                }
            }
        }

        // Inserts every commit in the walk plus all trees and blobs they
        // reference, deduplicated — the complete reachable object set.
        pack_builder.insert_walk(&mut revwalk)?;

        Ok(())
    }

    /// Deletes the reflogs for HEAD and the branch it points at. Best-effort:
    /// a missing reflog is fine, and reflog loss is never worth failing a
    /// maintenance pass over.
    fn expire_reflogs(repo: &Repository) {
        // Resolve the branch name before deleting anything (`repo.head()`
        // resolves the HEAD symref to e.g. `refs/heads/master`).
        let branch_ref_name = repo
            .head()
            .ok()
            .and_then(|head_ref| head_ref.name().ok().map(str::to_owned));

        let _ = repo.reflog_delete("HEAD");

        if let Some(name) = branch_ref_name {
            tracing::trace!(reference = %name, "expiring reflog");

            let _ = repo.reflog_delete(&name);
        }
    }

    /// Lists the packfiles under `.git/objects/pack` as extension-less path
    /// stems (each pack is a family of files — `.pack`, `.idx`, ... —
    /// sharing one stem).
    fn enumerate_pack_stems(repo_path: &Path) -> Result<HashSet<PathBuf>, AppError> {
        let pack_dir = repo_path.join(".git").join("objects").join("pack");
        let mut stems: HashSet<PathBuf> = HashSet::new();

        // A repository that has never been packed has no pack directory.
        let entries = match std::fs::read_dir(&pack_dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(stems),
        };

        for entry in entries.flatten() {
            let path = entry.path();

            if path.extension().and_then(|extension| extension.to_str()) == Some("pack") {
                stems.insert(path.with_extension(""));
            }
        }

        Ok(stems)
    }

    /// Walks `.git/objects/` and returns every loose object with its file
    /// path. Non-object entries (`pack/`, `info/`, temporary files) are
    /// skipped by the hex-name filters.
    ///
    /// Loose objects live at `.git/objects/<2-hex-chars>/<38-hex-chars>` —
    /// the object's SHA split after two characters (the "fan-out" scheme
    /// that keeps any single directory from holding every object).
    /// Re-joining the directory name and file name reconstructs the oid.
    fn enumerate_loose_objects(repo_path: &Path) -> Result<Vec<(Oid, PathBuf)>, AppError> {
        let objects_dir = repo_path.join(".git").join("objects");
        let mut loose_objects: Vec<(Oid, PathBuf)> = Vec::new();

        let fanout_entries = match std::fs::read_dir(&objects_dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(loose_objects),
        };

        for fanout_entry in fanout_entries.flatten() {
            let fanout_name = fanout_entry.file_name();

            let Some(prefix) = fanout_name.to_str() else {
                continue;
            };

            if prefix.len() != 2 || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }

            let Ok(object_entries) = std::fs::read_dir(fanout_entry.path()) else {
                continue;
            };

            for object_entry in object_entries.flatten() {
                let object_name = object_entry.file_name();

                let Some(suffix) = object_name.to_str() else {
                    continue;
                };

                if let Ok(oid) = Oid::from_str(&format!("{}{}", prefix, suffix)) {
                    loose_objects.push((oid, object_entry.path()));
                }
            }
        }

        Ok(loose_objects)
    }
}

// ---------------------------------------------------------------------------
// GitFiles — file CRUD operations
// ---------------------------------------------------------------------------

pub struct GitFiles;

impl GitFiles {
    /// Lists the repository as a tree of `TreeNode`s, rooted at
    /// `path_prefix` (or the repo root), paginated over root-level entries,
    /// and optionally depth-limited.
    ///
    /// The performance contract: **no blob is ever opened**. Names and
    /// entry kinds come entirely from git tree objects, so listing cost
    /// scales with the number of tree entries actually visited — and the
    /// pagination below is designed to keep that number small even on huge
    /// repositories.
    pub fn list_files(
        repo_path: &Path,
        tenant_id: &str,
        path_prefix: Option<&str>,
        maximum_depth: Option<usize>,
        page: usize,
        per_page: usize,
    ) -> Result<(Vec<TreeNode>, bool), AppError> {
        tracing::debug!(tenant_id = %tenant_id, path_prefix = ?path_prefix, maximum_depth = ?maximum_depth, page = page, per_page = per_page, "listing files");

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;
        let head_commit = repo.head()?.peel_to_commit()?;

        tracing::trace!(tenant_id = %tenant_id, head_sha = %head_commit.id(), "resolved HEAD for file listing");

        let head_tree = head_commit.tree()?;

        // Resolve the prefix subtree directly so the walk never visits unrelated
        // directories. An absent or non-directory prefix yields an empty result.
        let walk_tree: git2::Tree<'_> = match path_prefix.filter(|p| !p.is_empty()) {
            Some(prefix) => match head_tree.get_path(Path::new(prefix)) {
                Ok(entry) => match repo.find_tree(entry.id()) {
                    Ok(tree) => tree,
                    Err(_) => return Ok((vec![], false)),
                },
                Err(_) => return Ok((vec![], false)),
            },
            None => head_tree,
        };

        // The listing root's immediate entries are already in memory as part
        // of the tree object — no further object reads are needed to
        // enumerate them. Sorting mirrors the response order (directories
        // first, then files, both alphabetical), so the page window is
        // decided before a single subtree is opened: off-page directories
        // are never visited at all.
        let mut root_dirs: Vec<(String, Oid)> = Vec::new();
        let mut root_files: Vec<String> = Vec::new();

        for entry in walk_tree.iter() {
            let Ok(name) = entry.name() else {
                continue;
            };

            match entry.kind() {
                Some(git2::ObjectType::Tree) => root_dirs.push((name.to_string(), entry.id())),
                Some(git2::ObjectType::Blob) => root_files.push(name.to_string()),
                _ => {}
            }
        }

        root_dirs.sort_by(|left, right| left.0.cmp(&right.0));
        root_files.sort();

        // Page arithmetic over the combined (dirs-then-files) root sequence.
        // `has_more` comes from the total count directly — no "fetch one
        // extra" trick needed here since all root names are already known.
        let total = root_dirs.len() + root_files.len();
        let offset = ((page - 1) * per_page).min(total);
        let has_more = total > offset + per_page;

        enum RootEntry {
            Directory(String, Oid),
            File(String),
        }

        let page_entries: Vec<RootEntry> = root_dirs
            .into_iter()
            .map(|(name, oid)| RootEntry::Directory(name, oid))
            .chain(root_files.into_iter().map(RootEntry::File))
            .skip(offset)
            .take(per_page)
            .collect();

        let mut nodes: Vec<TreeNode> = Vec::with_capacity(page_entries.len());

        for root_entry in page_entries {
            match root_entry {
                RootEntry::File(name) => nodes.push(TreeNode::File { name }),
                RootEntry::Directory(name, oid) => {
                    // maximum_depth counts levels from the listing root, so a
                    // depth-1 listing renders every directory as a childless
                    // stub without opening its subtree.
                    if maximum_depth == Some(1) {
                        nodes.push(TreeNode::Directory {
                            name,
                            children: Vec::new(),
                        });

                        continue;
                    }

                    let subtree = repo.find_tree(oid)?;

                    // Depth limits below are relative to this subtree, which
                    // sits one level down from the listing root.
                    let subtree_max_depth = maximum_depth.map(|max| max - 1);
                    let children = Self::collect_subtree(&subtree, subtree_max_depth)?;

                    nodes.push(TreeNode::Directory { name, children });
                }
            }
        }

        tracing::debug!(tenant_id = %tenant_id, page = page, returned = nodes.len(), has_more = has_more, "file listing complete");

        Ok((nodes, has_more))
    }

    /// Recursively walks one paged root directory and builds its child nodes.
    /// Only directories inside the requested page window ever reach this
    /// point. Blob objects are never opened — names and kinds come from the
    /// tree objects alone.
    fn collect_subtree(
        subtree: &git2::Tree<'_>,
        max_depth: Option<usize>,
    ) -> Result<Vec<TreeNode>, AppError> {
        let mut flat: Vec<String> = Vec::new();
        let mut dir_stubs: Vec<String> = Vec::new();

        subtree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            // Depth of this entry relative to the subtree: "" = depth 1, "a/" = depth 2, …
            let entry_depth = root.chars().filter(|c| *c == '/').count() + 1;

            if entry.kind() == Some(git2::ObjectType::Tree) {
                if let Some(max) = max_depth {
                    if entry_depth >= max {
                        // Record as a stub and skip descending.
                        let name = entry.name().unwrap_or("");
                        dir_stubs.push(format!("{}{}", root, name));
                        return git2::TreeWalkResult::Skip;
                    }
                }
                return git2::TreeWalkResult::Ok;
            }

            if entry.kind() != Some(git2::ObjectType::Blob) {
                return git2::TreeWalkResult::Ok;
            }

            let name = entry.name().unwrap_or("");
            flat.push(format!("{}{}", root, name));

            git2::TreeWalkResult::Ok
        })?;

        Ok(GitUtils::build_tree(flat, dir_stubs, max_depth))
    }

    /// Returns the file content as recorded in HEAD's tree (not from the working
    /// tree) so the response always reflects the last successfully committed state.
    pub fn read_file(
        repo_path: &Path,
        tenant_id: &str,
        file_path: &str,
    ) -> Result<String, AppError> {
        tracing::debug!(tenant_id = %tenant_id, path = %file_path, "reading file");

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;
        let head_commit = repo.head()?.peel_to_commit()?;

        tracing::trace!(tenant_id = %tenant_id, path = %file_path, head_sha = %head_commit.id(), "resolved HEAD for read");

        let head_tree = head_commit.tree()?;

        GitUtils::blob_content_from_tree(&repo, &head_tree, file_path)
    }

    /// Checks that a file exists in HEAD's tree without reading its content.
    /// Returns `FileNotFound` when the path is absent or resolves to a folder.
    pub fn file_exists(repo_path: &Path, tenant_id: &str, file_path: &str) -> Result<(), AppError> {
        tracing::debug!(tenant_id = %tenant_id, path = %file_path, "checking file existence");

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;
        let head_commit = repo.head()?.peel_to_commit()?;

        tracing::trace!(tenant_id = %tenant_id, path = %file_path, head_sha = %head_commit.id(), "resolved HEAD for existence check");

        let head_tree = head_commit.tree()?;

        let tree_entry =
            head_tree
                .get_path(Path::new(file_path))
                .map_err(|_err| AppError::FileNotFound {
                    path: file_path.to_string(),
                })?;

        if tree_entry.kind() != Some(git2::ObjectType::Blob) {
            return Err(AppError::FileNotFound {
                path: file_path.to_string(),
            });
        }

        Ok(())
    }

    /// Writes a file to disk, stages it, and creates a commit.
    /// Returns the commit SHA and the type of change (created vs updated).
    ///
    /// Order of operations matters: the working-tree write happens *before*
    /// the commit, so if the process dies in between, HEAD still points at
    /// the last good commit and the stray on-disk file is harmless (it will
    /// simply be overwritten or ignored — never committed — because commit
    /// trees are built from HEAD, not from disk).
    pub fn write_file(
        repo_path: &Path,
        file_path: &str,
        content: &str,
        commit_message: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<(String, FileChange), AppError> {
        tracing::debug!(path = %file_path, author_name = %author_name, author_email = %author_email, "writing file");

        let repo = GitUtils::open_or_init_repo(repo_path, author_name, author_email)?;

        let parent_commit = repo.head()?.peel_to_commit()?;
        let head_tree = parent_commit.tree()?;

        // Existence is decided from HEAD's tree, never from the working tree,
        // so leftovers from a previously failed operation cannot change the
        // outcome (created vs updated) or the hook event that is emitted.
        // Writing *onto* a directory path is rejected outright — git would
        // technically allow replacing a tree with a blob, but for a CMS that
        // is almost certainly a caller mistake that would delete a whole
        // folder of content in one PUT.
        let is_new_file = match head_tree.get_path(Path::new(file_path)) {
            Ok(entry) if entry.kind() == Some(git2::ObjectType::Blob) => false,
            Ok(_) => {
                return Err(AppError::InvalidOperation {
                    reason: format!("path is a folder: {}", file_path),
                })
            }
            Err(_) => true,
        };

        tracing::debug!(path = %file_path, is_new_file = is_new_file, "staging file write");

        let absolute_path = repo_path.join(file_path);

        if let Some(parent_dir) = absolute_path.parent() {
            std::fs::create_dir_all(parent_dir)?;
        }

        std::fs::write(&absolute_path, content)?;

        tracing::trace!(path = %file_path, "building updated tree");

        // The commit tree is HEAD's tree plus this single change — O(path
        // depth) instead of the O(repository size) an index round-trip costs,
        // and stray state from a failed past operation can never leak in.
        let blob_oid = repo.blob(content.as_bytes())?;

        let tree_id = TreeUpdateBuilder::new()
            .upsert(file_path, blob_oid, FileMode::Blob)
            .create_updated(&repo, &head_tree)?;

        let tree = repo.find_tree(tree_id)?;
        let signature = GitUtils::git_signature(author_name, author_email)?;

        let auto_message = if is_new_file {
            format!("create: {}", file_path)
        } else {
            format!("update: {}", file_path)
        };
        let message = commit_message.unwrap_or(&auto_message);

        tracing::trace!(path = %file_path, message = %message, "committing file write");

        let commit_oid = repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[&parent_commit],
        )?;

        tracing::debug!(path = %file_path, sha = %commit_oid, is_new_file = is_new_file, "file write committed");

        let change = if is_new_file {
            FileChange::Created {
                path: file_path.to_string(),
                content: content.to_string(),
            }
        } else {
            FileChange::Updated {
                path: file_path.to_string(),
                content: content.to_string(),
            }
        };

        Ok((commit_oid.to_string(), change))
    }

    /// Removes a file from disk, stages the deletion, and creates a commit.
    ///
    /// Unlike `write_file`, this opens the repo with `open_tenant_repo` (no
    /// auto-init): deleting a file from a tenant that never existed is a
    /// 404, not a reason to create an empty repository.
    pub fn delete_file(
        repo_path: &Path,
        tenant_id: &str,
        file_path: &str,
        commit_message: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<(String, FileChange), AppError> {
        tracing::debug!(tenant_id = %tenant_id, path = %file_path, author_name = %author_name, author_email = %author_email, "deleting file");

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;

        let parent_commit = repo.head()?.peel_to_commit()?;
        let head_tree = parent_commit.tree()?;

        // Existence is decided from HEAD's tree, never from the working tree.
        match head_tree.get_path(Path::new(file_path)) {
            Ok(entry) if entry.kind() == Some(git2::ObjectType::Blob) => {}
            _ => {
                tracing::debug!(tenant_id = %tenant_id, path = %file_path, "file not found for deletion");

                return Err(AppError::FileNotFound {
                    path: file_path.to_string(),
                });
            }
        }

        tracing::trace!(tenant_id = %tenant_id, path = %file_path, "building updated tree without path");

        // A file already missing from the working tree just means the working
        // tree had diverged from HEAD; there is nothing left to clean up.
        let absolute_path = repo_path.join(file_path);

        match std::fs::remove_file(&absolute_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(AppError::Io(err)),
        }

        // The commit tree is HEAD's tree minus this single entry — O(path
        // depth) instead of the O(repository size) an index round-trip costs.
        let tree_id = TreeUpdateBuilder::new()
            .remove(file_path)
            .create_updated(&repo, &head_tree)?;

        let tree = repo.find_tree(tree_id)?;
        let signature = GitUtils::git_signature(author_name, author_email)?;

        let auto_message = format!("delete: {}", file_path);
        let message = commit_message.unwrap_or(&auto_message);

        tracing::trace!(tenant_id = %tenant_id, path = %file_path, message = %message, "committing file deletion");

        let commit_oid = repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[&parent_commit],
        )?;

        tracing::debug!(tenant_id = %tenant_id, path = %file_path, sha = %commit_oid, "file deletion committed");

        Ok((
            commit_oid.to_string(),
            FileChange::Deleted {
                path: file_path.to_string(),
            },
        ))
    }

    /// Renames a file on disk, stages both sides, and creates a single commit.
    /// This preserves rename semantics so hook receivers know an entity was moved.
    ///
    /// Doing the remove and the insert in *one* commit is the whole point:
    /// two separate commits (delete + create) would fire two hooks and make
    /// the downstream receiver treat the file as a brand-new entity, losing
    /// whatever metadata it had attached to the old path.
    pub fn move_file(
        repo_path: &Path,
        tenant_id: &str,
        from_path: &str,
        to_path: &str,
        commit_message: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<(String, FileChange), AppError> {
        tracing::debug!(
            tenant_id = %tenant_id,
            from_path = %from_path,
            to_path = %to_path,
            author_email = %author_email,
            "moving file"
        );

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;

        if from_path == to_path {
            tracing::debug!(tenant_id = %tenant_id, path = %from_path, "move rejected: source and destination are identical");

            return Err(AppError::InvalidOperation {
                reason: "destination must differ from source path".to_string(),
            });
        }

        let parent_commit = repo.head()?.peel_to_commit()?;
        let head_tree = parent_commit.tree()?;

        // Existence is decided from HEAD's tree, never from the working tree.
        // The blob oid is kept so the destination entry reuses it verbatim.
        let source_blob_oid = match head_tree.get_path(Path::new(from_path)) {
            Ok(entry) if entry.kind() == Some(git2::ObjectType::Blob) => entry.id(),
            _ => {
                tracing::debug!(tenant_id = %tenant_id, from_path = %from_path, "source file not found for move");

                return Err(AppError::FileNotFound {
                    path: from_path.to_string(),
                });
            }
        };

        // Refuse to clobber an existing destination — the user must delete first.
        if head_tree.get_path(Path::new(to_path)).is_ok() {
            tracing::debug!(tenant_id = %tenant_id, to_path = %to_path, "move rejected: destination already exists");

            return Err(AppError::InvalidOperation {
                reason: format!("destination already exists: {}", to_path),
            });
        }

        // The moved content comes from HEAD's blob — the authoritative state —
        // rather than whatever the working tree currently holds.
        let content = GitUtils::blob_content_from_tree(&repo, &head_tree, from_path)?;

        let absolute_from = repo_path.join(from_path);
        let absolute_to = repo_path.join(to_path);

        match std::fs::remove_file(&absolute_from) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(AppError::Io(err)),
        }

        if let Some(parent_dir) = absolute_to.parent() {
            std::fs::create_dir_all(parent_dir)?;
        }

        std::fs::write(&absolute_to, &content)?;

        tracing::trace!(
            tenant_id = %tenant_id,
            from_path = %from_path,
            to_path = %to_path,
            "building updated tree for move"
        );

        // The commit tree is HEAD's tree with the entry relocated, reusing the
        // existing blob — no content rehash, no index round-trip.
        let tree_id = TreeUpdateBuilder::new()
            .remove(from_path)
            .upsert(to_path, source_blob_oid, FileMode::Blob)
            .create_updated(&repo, &head_tree)?;

        let tree = repo.find_tree(tree_id)?;
        let signature = GitUtils::git_signature(author_name, author_email)?;

        let auto_message = format!("move: {} -> {}", from_path, to_path);
        let message = commit_message.unwrap_or(&auto_message);

        tracing::trace!(
            tenant_id = %tenant_id,
            from_path = %from_path,
            to_path = %to_path,
            message = %message,
            "committing file move"
        );

        let commit_oid = repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[&parent_commit],
        )?;

        tracing::debug!(
            tenant_id = %tenant_id,
            from_path = %from_path,
            to_path = %to_path,
            sha = %commit_oid,
            "file move committed"
        );

        Ok((
            commit_oid.to_string(),
            FileChange::Moved {
                from_path: from_path.to_string(),
                to_path: to_path.to_string(),
                content,
            },
        ))
    }
}

// ---------------------------------------------------------------------------
// GitCommits — commit history and revert
// ---------------------------------------------------------------------------

pub struct GitCommits;

impl GitCommits {
    /// Lists commits newest-first, paginated. With a `file_path` filter the
    /// work is delegated to [`Self::list_commits_by_file`], which follows
    /// the file backward through renames.
    pub fn list_commits(
        repo_path: &Path,
        tenant_id: &str,
        page: usize,
        per_page: usize,
        file_path: Option<&str>,
    ) -> Result<(Vec<CommitSummary>, bool), AppError> {
        if let Some(path) = file_path {
            return Self::list_commits_by_file(repo_path, tenant_id, page, per_page, path);
        }

        tracing::debug!(tenant_id = %tenant_id, page = page, per_page = per_page, "listing commits");

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;

        let mut revwalk = repo.revwalk()?;

        revwalk.push_head()?;

        // TIME | TOPOLOGICAL gives stable ordering across commits sharing a timestamp.
        revwalk.set_sorting(Sort::TIME | Sort::TOPOLOGICAL)?;

        let skip_count = page.saturating_sub(1).saturating_mul(per_page);

        tracing::trace!(tenant_id = %tenant_id, skip_count = skip_count, per_page = per_page, "walking commit graph");

        // Fetch one extra to detect whether a next page exists without a full count.
        let mut commits: Vec<CommitSummary> = revwalk
            .skip(skip_count)
            .take(per_page + 1)
            .filter_map(|oid_result| oid_result.ok())
            .filter_map(|oid| repo.find_commit(oid).ok())
            .map(|commit| CommitSummary {
                sha: commit.id().to_string(),
                message: commit.message().unwrap_or("").to_string(),
                author: CommitAuthor {
                    name: commit.author().name().unwrap_or("").to_string(),
                    email: commit.author().email().unwrap_or("").to_string(),
                },
                committed_at: GitUtils::timestamp_from_git_time(commit.time()),
            })
            .collect();

        let has_more = commits.len() > per_page;

        commits.truncate(per_page);

        tracing::debug!(tenant_id = %tenant_id, page = page, returned = commits.len(), has_more = has_more, "commit listing complete");

        Ok((commits, has_more))
    }

    /// Walks the commit graph from HEAD, diffing each commit against its parent
    /// with rename detection enabled, and collects only commits that touched
    /// `file_path` (following the file backward through any renames).
    ///
    /// Pagination is applied after matching: we collect up to
    /// `(page-1)*per_page + per_page + 1` matching commits, then slice.
    ///
    /// How the walk stays cheap: for each commit, two O(path depth) tree
    /// lookups (does the file exist in this commit? in its parent? same
    /// oid?) decide whether the commit touched the file. This answers the
    /// overwhelmingly common "untouched" case without ever loading content.
    /// Only when a commit *introduced* the path (present in commit, absent
    /// in parent) is a full rename-detecting diff computed, to distinguish
    /// "created here" from "renamed from an older path" — and in the rename
    /// case, `current_path` is rewritten so the walk keeps following the
    /// file under its previous name.
    fn list_commits_by_file(
        repo_path: &Path,
        tenant_id: &str,
        page: usize,
        per_page: usize,
        file_path: &str,
    ) -> Result<(Vec<CommitSummary>, bool), AppError> {
        tracing::debug!(
            tenant_id = %tenant_id,
            page = page,
            per_page = per_page,
            file_path = %file_path,
            "listing commits by file path"
        );

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;

        let mut revwalk = repo.revwalk()?;

        revwalk.push_head()?;
        revwalk.set_sorting(Sort::TIME | Sort::TOPOLOGICAL)?;

        let skip_count = page.saturating_sub(1).saturating_mul(per_page);
        // Collect one extra beyond what we need so we can detect has_more.
        let need = skip_count + per_page + 1;

        // The name of the file we are tracking. Updated when we cross a rename.
        let mut current_path = file_path.to_string();
        let mut matching: Vec<CommitSummary> = Vec::new();

        for oid_result in revwalk {
            if matching.len() >= need {
                break;
            }

            let oid = match oid_result {
                Ok(id) => id,
                Err(_) => continue,
            };

            let commit = match repo.find_commit(oid) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let commit_tree = match commit.tree() {
                Ok(t) => t,
                Err(_) => continue,
            };

            // For the root commit there is no parent tree to diff against — the
            // file is "created" here if it exists in the tree under the current name.
            let (is_match, rename_from) = if commit.parent_count() == 0 {
                let exists = commit_tree.get_path(Path::new(&current_path)).is_ok();

                tracing::trace!(
                    tenant_id = %tenant_id,
                    sha = %commit.id(),
                    path = %current_path,
                    exists = exists,
                    "checking root commit for file"
                );

                (exists, None)
            } else {
                let parent_tree = match commit.parent(0).and_then(|p| p.tree()) {
                    Ok(t) => t,
                    Err(_) => continue,
                };

                // Two O(path depth) tree lookups decide whether this commit
                // touched the file at all. A rename-detecting diff (which loads
                // blob contents to score similarity) is only computed for the
                // rare commit that introduced the file under its current name.
                let commit_entry = commit_tree.get_path(Path::new(&current_path)).ok();
                let parent_entry = parent_tree.get_path(Path::new(&current_path)).ok();

                match (commit_entry, parent_entry) {
                    // Untouched by this commit — the overwhelmingly common case.
                    (Some(in_commit), Some(in_parent))
                        if in_commit.id() == in_parent.id()
                            && in_commit.filemode() == in_parent.filemode() =>
                    {
                        (false, None)
                    }
                    // Modified in this commit.
                    (Some(_), Some(_)) => (true, None),
                    // Deleted by this commit (the file was re-created later).
                    (None, Some(_)) => (true, None),
                    // Not present under this name on either side.
                    (None, None) => (false, None),
                    // Introduced by this commit — either created, or renamed
                    // from an older path that must be followed backward.
                    (Some(_), None) => (
                        true,
                        Self::rename_source(&repo, &parent_tree, &commit_tree, &current_path),
                    ),
                }
            };

            if is_match {
                tracing::trace!(
                    tenant_id = %tenant_id,
                    sha = %commit.id(),
                    path = %current_path,
                    "commit matched file path filter"
                );

                matching.push(CommitSummary {
                    sha: commit.id().to_string(),
                    message: commit.message().unwrap_or("").to_string(),
                    author: CommitAuthor {
                        name: commit.author().name().unwrap_or("").to_string(),
                        email: commit.author().email().unwrap_or("").to_string(),
                    },
                    committed_at: GitUtils::timestamp_from_git_time(commit.time()),
                });

                if let Some(old_name) = rename_from {
                    current_path = old_name;
                }
            }
        }

        let has_more = matching.len() > skip_count + per_page;

        let commits = matching
            .into_iter()
            .skip(skip_count)
            .take(per_page)
            .collect();

        tracing::debug!(
            tenant_id = %tenant_id,
            page = page,
            returned = per_page,
            has_more = has_more,
            "commit listing by file complete"
        );

        Ok((commits, has_more))
    }

    /// Runs a rename-detecting diff of a single commit and returns the prior
    /// path when `current_path` was renamed (rather than freshly created) by
    /// it. Only invoked for commits that introduced the file under its
    /// current name, so the similarity scan stays off the hot path.
    fn rename_source(
        repo: &Repository,
        parent_tree: &git2::Tree<'_>,
        commit_tree: &git2::Tree<'_>,
        current_path: &str,
    ) -> Option<String> {
        let mut diff_opts = DiffOptions::new();

        diff_opts.include_untracked(false);

        let mut diff = repo
            .diff_tree_to_tree(Some(parent_tree), Some(commit_tree), Some(&mut diff_opts))
            .ok()?;

        let mut find_opts = DiffFindOptions::new();

        find_opts.renames(true);

        diff.find_similar(Some(&mut find_opts)).ok()?;

        for index in 0..diff.deltas().count() {
            let Some(delta) = diff.get_delta(index) else {
                continue;
            };

            if delta.status() != Delta::Renamed {
                continue;
            }

            let new = delta
                .new_file()
                .path()
                .map(|path| path.to_string_lossy().into_owned());

            if new.as_deref() == Some(current_path) {
                let old = delta
                    .old_file()
                    .path()
                    .map(|path| path.to_string_lossy().into_owned());

                tracing::trace!(
                    from = ?old,
                    to = %current_path,
                    "rename detected, following path backward"
                );

                return old;
            }
        }

        None
    }

    /// Builds the full detail view of one commit: metadata, and for every
    /// file it touched a change label, the post-commit content, and a
    /// unified diff. The `sha` may be abbreviated — the validation layer has
    /// already guaranteed it is plain hexadecimal, so the `revparse_single`
    /// call can only ever resolve it as an object id prefix, never as a
    /// revspec expression.
    pub fn get_commit(
        repo_path: &Path,
        tenant_id: &str,
        sha: &str,
    ) -> Result<CommitDetail, AppError> {
        tracing::debug!(tenant_id = %tenant_id, sha = %sha, "fetching commit detail");

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;

        let object = repo
            .revparse_single(sha)
            .map_err(|_err| AppError::CommitNotFound {
                sha: sha.to_string(),
            })?;

        let commit = object
            .peel_to_commit()
            .map_err(|_err| AppError::CommitNotFound {
                sha: sha.to_string(),
            })?;

        let commit_tree = commit.tree()?;

        // The root commit has no parent; diffing against `None` yields every
        // file in the commit as "created", which is exactly right.
        let parent_tree = if commit.parent_count() > 0 {
            Some(commit.parent(0)?.tree()?)
        } else {
            None
        };

        tracing::trace!(
            tenant_id = %tenant_id,
            sha = %sha,
            has_parent = parent_tree.is_some(),
            "diffing commit against parent"
        );

        let mut diff_options = DiffOptions::new();

        diff_options.include_untracked(false);

        let mut diff = repo.diff_tree_to_tree(
            parent_tree.as_ref(),
            Some(&commit_tree),
            Some(&mut diff_options),
        )?;

        // Enable rename detection so moved files are identified correctly.
        let mut find_options = DiffFindOptions::new();

        find_options.renames(true);

        diff.find_similar(Some(&mut find_options))?;

        let records: Vec<DeltaRecord> = (0..diff.deltas().count())
            .filter_map(|index| diff.get_delta(index))
            .map(|delta| {
                tracing::trace!(
                    tenant_id = %tenant_id,
                    sha = %sha,
                    status = ?delta.status(),
                    old_path = ?delta.old_file().path(),
                    new_path = ?delta.new_file().path(),
                    "processing diff delta"
                );
                DeltaRecord {
                    status: delta.status(),
                    old_oid: delta.old_file().id(),
                    new_oid: delta.new_file().id(),
                    old_path: delta.old_file().path().map(PathBuf::from),
                    new_path: delta.new_file().path().map(PathBuf::from),
                }
            })
            .collect();

        tracing::trace!(tenant_id = %tenant_id, sha = %sha, delta_count = records.len(), "building per-file diffs");

        // Walk the entire patch once and route each line to its delta's bucket.
        // Linear scan via `position` is fine — commits hold a handful of files.
        // `diff.print` streams the whole patch through one callback with no
        // per-file grouping of its own, so each line is matched back to its
        // delta by the (old oid, new oid) pair.
        let mut per_file_diffs: Vec<String> = vec![String::new(); records.len()];

        diff.print(DiffFormat::Patch, |delta, _hunk, line| {
            let key = (delta.old_file().id(), delta.new_file().id());

            if let Some(idx) = records
                .iter()
                .position(|record| (record.old_oid, record.new_oid) == key)
            {
                let bucket = &mut per_file_diffs[idx];

                // Content lines get their +/-/space marker re-attached
                // (libgit2 strips it from `line.content()`); structural
                // lines (hunk headers, file headers) pass through as-is.
                match line.origin() {
                    '+' | '-' | ' ' | '\\' => bucket.push(line.origin()),
                    _ => {}
                }

                bucket.push_str(std::str::from_utf8(line.content()).unwrap_or(""));
            }

            true
        })?;

        let mut file_details: Vec<CommitFileDetail> = Vec::with_capacity(records.len());

        for (index, record) in records.iter().enumerate() {
            // Map git's Delta status onto the API's four change labels. The
            // catch-all arm collapses the exotic statuses (typechange,
            // copied, ...) into "updated" — for a store that only ever holds
            // regular text files they cannot meaningfully occur.
            let (change_label, file_path, from_path) = match record.status {
                Delta::Added => (
                    "created",
                    GitUtils::path_string(record.new_path.as_deref()),
                    None,
                ),
                Delta::Deleted => (
                    "deleted",
                    GitUtils::path_string(record.old_path.as_deref()),
                    None,
                ),
                Delta::Renamed => (
                    "moved",
                    GitUtils::path_string(record.new_path.as_deref()),
                    record
                        .old_path
                        .as_deref()
                        .map(|path| path.to_string_lossy().into_owned()),
                ),
                _ => (
                    "updated",
                    GitUtils::path_string(record.new_path.as_deref()),
                    None,
                ),
            };

            tracing::trace!(
                tenant_id = %tenant_id,
                sha = %sha,
                path = %file_path,
                change = %change_label,
                "assembling commit file detail"
            );

            let content = if record.status == Delta::Deleted {
                String::new()
            } else {
                GitUtils::blob_content_from_tree(&repo, &commit_tree, &file_path)?
            };

            file_details.push(CommitFileDetail {
                path: file_path,
                change: change_label.to_string(),
                from_path,
                content,
                diff: std::mem::take(&mut per_file_diffs[index]),
            });
        }

        // Materialise borrowed values before the struct literal so that the
        // `Signature` temporary returned by `commit.author()` is dropped while
        // `commit` (and the underlying `repo`) is still alive.
        let sha = commit.id().to_string();
        let message = commit.message().unwrap_or("").to_string();

        let author = CommitAuthor {
            name: commit.author().name().unwrap_or("").to_string(),
            email: commit.author().email().unwrap_or("").to_string(),
        };

        let committed_at = GitUtils::timestamp_from_git_time(commit.time());

        tracing::debug!(tenant_id = %tenant_id, sha = %sha, file_count = file_details.len(), "commit detail ready");

        Ok(CommitDetail {
            sha,
            message,
            author,
            committed_at,
            files: file_details,
        })
    }

    /// Reverts all changes introduced by the given commit by applying their inverse,
    /// then records the result as a new commit. Returns the new commit SHA and
    /// the list of file changes (for hook delivery).
    ///
    /// How the inverse is computed: diff `parent(target) → target` to learn
    /// what the target commit introduced, then apply each delta *backwards*
    /// on top of the **current HEAD** (added → remove, deleted → restore,
    /// modified → restore old version, renamed → rename back). Restored
    /// content and blob oids come from the target's parent tree — the exact
    /// pre-commit state — so no content is ever rehashed.
    ///
    /// Note this is a "blind" revert of the git-revert family: if commits
    /// *after* the target modified the same files, their changes are
    /// overwritten by the restored versions (last-write-wins, consistent
    /// with the rest of the API's semantics). Reverting the root commit is
    /// rejected since there is no parent state to restore.
    pub fn revert_commit(
        repo_path: &Path,
        tenant_id: &str,
        sha: &str,
        commit_message: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<(String, Vec<FileChange>), AppError> {
        tracing::debug!(tenant_id = %tenant_id, sha = %sha, author_name = %author_name, author_email = %author_email, "reverting commit");

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;

        let object = repo
            .revparse_single(sha)
            .map_err(|_err| AppError::CommitNotFound {
                sha: sha.to_string(),
            })?;

        let target_commit = object
            .peel_to_commit()
            .map_err(|_err| AppError::CommitNotFound {
                sha: sha.to_string(),
            })?;

        if target_commit.parent_count() == 0 {
            tracing::warn!(tenant_id = %tenant_id, sha = %sha, "cannot revert root commit");

            return Err(AppError::InvalidOperation {
                reason: "cannot revert the initial commit".to_string(),
            });
        }

        let parent_commit = target_commit.parent(0)?;
        let commit_tree = target_commit.tree()?;
        let parent_tree = parent_commit.tree()?;

        // Diff from parent → commit tells us what the commit introduced.
        // Reverting means applying each change in reverse.
        tracing::trace!(tenant_id = %tenant_id, sha = %sha, "computing diff for revert");

        let mut diff = repo.diff_tree_to_tree(Some(&parent_tree), Some(&commit_tree), None)?;

        let mut find_options = DiffFindOptions::new();

        find_options.renames(true);

        diff.find_similar(Some(&mut find_options))?;

        let raw_deltas: Vec<DeltaRecord> = (0..diff.deltas().count())
            .filter_map(|index| diff.get_delta(index))
            .map(|delta| DeltaRecord {
                status: delta.status(),
                old_oid: delta.old_file().id(),
                new_oid: delta.new_file().id(),
                old_path: delta.old_file().path().map(PathBuf::from),
                new_path: delta.new_file().path().map(PathBuf::from),
            })
            .collect();

        tracing::trace!(tenant_id = %tenant_id, sha = %sha, delta_count = raw_deltas.len(), "applying revert deltas");

        let head_commit = repo.head()?.peel_to_commit()?;
        let head_tree = head_commit.tree()?;

        // The revert tree is HEAD's tree plus the inverse of each delta,
        // reusing parent-tree blob oids — no index round-trip, no rehashing.
        let mut tree_update = TreeUpdateBuilder::new();

        let mut file_changes: Vec<FileChange> = Vec::new();

        // For each delta: mirror the inverse change onto the working tree
        // (best-effort human-visible state), stage it into the tree builder
        // (the authoritative commit state), and record the corresponding
        // FileChange (drives one hook per file, in this order).
        for raw_delta in &raw_deltas {
            match raw_delta.status {
                Delta::Added => {
                    // Commit added this file → revert removes it.
                    if let Some(new_path) = &raw_delta.new_path {
                        tracing::trace!(
                            tenant_id = %tenant_id,
                            sha = %sha,
                            path = %new_path.display(),
                            "revert: removing added file"
                        );

                        let absolute_path = repo_path.join(new_path);

                        if absolute_path.exists() {
                            std::fs::remove_file(&absolute_path)?;
                        }

                        tree_update.remove(new_path);

                        file_changes.push(FileChange::Deleted {
                            path: new_path.to_string_lossy().into_owned(),
                        });
                    }
                }
                Delta::Deleted => {
                    // Commit deleted this file → revert restores it from the parent tree.
                    if let Some(old_path) = &raw_delta.old_path {
                        tracing::trace!(
                            tenant_id = %tenant_id,
                            sha = %sha,
                            path = %old_path.display(),
                            "revert: restoring deleted file"
                        );

                        let content = GitUtils::blob_content_from_tree(
                            &repo,
                            &parent_tree,
                            &old_path.to_string_lossy(),
                        )?;

                        let absolute_path = repo_path.join(old_path);

                        if let Some(parent_dir) = absolute_path.parent() {
                            std::fs::create_dir_all(parent_dir)?;
                        }

                        std::fs::write(&absolute_path, &content)?;

                        tree_update.upsert(old_path, raw_delta.old_oid, FileMode::Blob);

                        file_changes.push(FileChange::Created {
                            path: old_path.to_string_lossy().into_owned(),
                            content,
                        });
                    }
                }
                Delta::Modified => {
                    // Commit modified this file → revert restores the old version.
                    if let Some(old_path) = &raw_delta.old_path {
                        tracing::trace!(
                            tenant_id = %tenant_id,
                            sha = %sha,
                            path = %old_path.display(),
                            "revert: restoring modified file to previous version"
                        );

                        let content = GitUtils::blob_content_from_tree(
                            &repo,
                            &parent_tree,
                            &old_path.to_string_lossy(),
                        )?;

                        let absolute_path = repo_path.join(old_path);

                        std::fs::write(&absolute_path, &content)?;

                        tree_update.upsert(old_path, raw_delta.old_oid, FileMode::Blob);

                        file_changes.push(FileChange::Updated {
                            path: old_path.to_string_lossy().into_owned(),
                            content,
                        });
                    }
                }
                Delta::Renamed => {
                    // Commit renamed old → new; revert renames new → old.
                    if let (Some(old_path), Some(new_path)) =
                        (&raw_delta.old_path, &raw_delta.new_path)
                    {
                        tracing::trace!(
                            tenant_id = %tenant_id,
                            sha = %sha,
                            from_path = %new_path.display(),
                            to_path = %old_path.display(),
                            "revert: reversing rename"
                        );

                        let content = GitUtils::blob_content_from_tree(
                            &repo,
                            &parent_tree,
                            &old_path.to_string_lossy(),
                        )?;

                        let absolute_old = repo_path.join(old_path);
                        let absolute_new = repo_path.join(new_path);

                        if absolute_new.exists() {
                            std::fs::remove_file(&absolute_new)?;
                        }

                        if let Some(parent_dir) = absolute_old.parent() {
                            std::fs::create_dir_all(parent_dir)?;
                        }

                        std::fs::write(&absolute_old, &content)?;

                        tree_update.remove(new_path);
                        tree_update.upsert(old_path, raw_delta.old_oid, FileMode::Blob);

                        file_changes.push(FileChange::Moved {
                            from_path: new_path.to_string_lossy().into_owned(),
                            to_path: old_path.to_string_lossy().into_owned(),
                            content,
                        });
                    }
                }
                _ => {}
            }
        }

        tracing::trace!(tenant_id = %tenant_id, sha = %sha, "building revert tree and committing");

        let tree_id = tree_update.create_updated(&repo, &head_tree)?;
        let tree = repo.find_tree(tree_id)?;
        let signature = GitUtils::git_signature(author_name, author_email)?;

        let auto_message = format!("revert: {}", target_commit.message().unwrap_or("unknown"));
        let revert_message = commit_message.unwrap_or(&auto_message);

        let new_commit_oid = repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            revert_message,
            &tree,
            &[&head_commit],
        )?;

        tracing::debug!(
            tenant_id = %tenant_id,
            reverted_sha = %sha,
            new_sha = %new_commit_oid,
            file_change_count = file_changes.len(),
            "revert committed"
        );

        Ok((new_commit_oid.to_string(), file_changes))
    }
}

// ---------------------------------------------------------------------------
// GitTenant — tenant repository lifecycle
// ---------------------------------------------------------------------------

pub struct GitTenant;

impl GitTenant {
    /// Permanently deletes a tenant's repository — working tree, `.git`
    /// directory, full history, everything. There is no soft-delete or
    /// trash: the API contract is that tenant deletion is irreversible.
    /// Must be called under the tenant write lock (the route handler holds
    /// it) so no commit can be in flight while the directory disappears.
    pub fn delete_repo(repo_path: &Path, tenant_id: &str) -> Result<(), AppError> {
        tracing::debug!(tenant_id = %tenant_id, "deleting tenant repository");

        if !repo_path.exists() {
            tracing::debug!(tenant_id = %tenant_id, "tenant repository not found for deletion");

            return Err(AppError::TenantNotFound {
                tenant_id: tenant_id.to_string(),
            });
        }

        std::fs::remove_dir_all(repo_path).map_err(|err| {
            tracing::error!(
                tenant_id = %tenant_id,
                path = %repo_path.display(),
                err = %err,
                "failed to remove tenant repository directory"
            );

            AppError::Io(err)
        })?;

        tracing::info!(tenant_id = %tenant_id, "tenant repository deleted");

        Ok(())
    }
}
