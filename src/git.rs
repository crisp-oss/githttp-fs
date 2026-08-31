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
//! - `GitOrder` — the per-directory file-order index (read / write / reorder
//!   one entry / delete, plus the implicit upkeep that keeps an index honest
//!   across file deletes and moves)
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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::error::AppError;
use crate::order;
use crate::seek::SeekFilter;

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

impl TreeNode {
    /// The node's leaf name, whichever kind it is. Used to rank a node against
    /// a directory's stored order.
    fn name(&self) -> &str {
        match self {
            Self::File { name } | Self::Directory { name, .. } => name,
        }
    }
}

/// File and directory totals returned by the count endpoint. Directories
/// are counted as visited — the extension restriction only narrows which
/// files count, never which directories are entered.
#[derive(Debug, Default)]
pub struct FileCounts {
    pub files: usize,
    pub directories: usize,
}

/// What a path resolves to in HEAD's tree. Answered from tree objects alone
/// — a tree entry already carries its kind, so no blob is opened and the
/// working tree is never consulted (HEAD is authoritative everywhere in this
/// API, and a folder is a `Tree` entry exactly like a file is a `Blob` one).
///
/// `Missing` covers both "no such entry" and "an entry of some other kind"
/// (a submodule, a symlink target git records as a commit): neither a file
/// nor a folder this API can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    File,
    Directory,
    Missing,
}

/// Which git date a listing's date-range filter keys off. Both are derived
/// from commit history (a tree object carries no timestamp), so any listing
/// carrying a date bound pays for a history walk — see
/// [`GitFiles::file_dates`].
#[derive(Debug, Clone, Copy)]
pub enum DateKind {
    /// The oldest commit that introduced the file under its current path
    /// (renames are *not* followed). Requires walking to the root of history.
    Created,
    /// The most recent commit that touched the file. The walk can stop as
    /// soon as every in-scope file has been dated.
    Updated,
}

/// A date-range filter applied to a file listing: only files whose
/// [`DateKind`] date falls inside the window survive. The window is
/// half-open — `from` inclusive, `to` exclusive (`[from, to)`) — and each
/// bound is independently optional (an open-ended range). A file's date is
/// compared at whole-second (git commit) resolution, in UTC.
#[derive(Debug, Clone, Copy)]
pub struct DateFilter {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub kind: DateKind,
}

impl DateFilter {
    /// Whether `date` falls inside the half-open window.
    fn matches(&self, date: DateTime<Utc>) -> bool {
        if let Some(from) = self.from {
            if date < from {
                return false;
            }
        }

        if let Some(to) = self.to {
            if date >= to {
                return false;
            }
        }

        true
    }
}

/// How a listing applies the per-directory file-order index. Its presence is
/// the opt-in itself — `None` means "do not order at all", which is what keeps
/// the blob-free listing contract for every caller that never asks.
///
/// `implicit_default_index` decides where the entries an index does *not* name
/// land. Left unset they rank behind every listed entry, which is the original
/// (and default) behaviour: a sparse index pins what it names and everything
/// else follows. Set, it is the index unlisted entries are treated as holding —
/// so `0` (or any negative value) lifts everything unordered *above* the
/// ordered entries, and `2` slots them between the index's second and third
/// entries. An unlisted entry always sorts before a listed entry holding the
/// same index, which is what makes `0` mean "on top" rather than "tied with the
/// first".
#[derive(Debug, Clone, Copy)]
pub struct OrderOptions {
    pub implicit_default_index: Option<i64>,
}

/// Sort key of one entry against a directory's stored order: its index, plus
/// whether the index actually names it. The `bool` is the tie-break — `false`
/// sorts first, so an unlisted entry falling on an occupied index goes above
/// it rather than after it.
type OrderRank = (i64, bool);

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statistics: Option<CommitStatistics>,
}

/// Aggregate insertion/deletion/file counts for a single commit, computed
/// against its first parent (or, for the root commit, against an empty
/// tree). Renames are similarity-detected first so a pure rename does not
/// register as a full delete+add of the file's content.
#[derive(Debug, Serialize)]
pub struct CommitStatistics {
    pub insertions: usize,
    pub deletions: usize,
    pub files_changed: usize,
}

#[derive(Debug, Serialize)]
pub struct CommitDetail {
    pub sha: String,
    pub message: String,
    pub author: CommitAuthor,
    pub committed_at: DateTime<Utc>,
    pub files: Vec<CommitFileDetail>,
    pub statistics: CommitStatistics,
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

    /// Resolves `file_path` to its blob oid in `tree`, or `None` when the
    /// path is absent or resolves to a folder — "not a file" either way.
    fn blob_oid_in_tree(tree: &git2::Tree<'_>, file_path: &str) -> Option<Oid> {
        let tree_entry = tree.get_path(Path::new(file_path)).ok()?;

        if tree_entry.kind() != Some(git2::ObjectType::Blob) {
            return None;
        }

        Some(tree_entry.id())
    }

    /// Reads a blob's content with the seek window applied — or whole when
    /// the filter is a no-op. Shared by the single and batch read paths.
    ///
    /// Windowed reads prefer a streaming ODB read (`git_odb_open_rstream`),
    /// so on loose objects — every blob written since the last maintenance
    /// repack — inflation stops as soon as the window is complete. Packed
    /// objects cannot be streamed by libgit2 (the packfile backend stores
    /// them delta'd, so it implements no `readstream`); those fall back to
    /// scanning the blob borrowed from the object cache. Either way only
    /// the selected window is allocated — the full content is never copied
    /// into a `String`.
    fn windowed_blob_content(
        repo: &Repository,
        oid: Oid,
        file_path: &str,
        seek: &SeekFilter,
    ) -> Result<String, AppError> {
        if seek.is_noop() {
            let blob = repo.find_blob(oid)?;

            return std::str::from_utf8(blob.content())
                .map(|text| text.to_string())
                .map_err(|_err| AppError::InvalidUtf8 {
                    path: file_path.to_string(),
                });
        }

        let odb = repo.odb()?;

        // Bound to a local so the stream (which borrows `odb`) is dropped
        // before `odb` itself at the end of the function.
        let window = match odb.reader(oid) {
            Ok((reader, _size, _object_type)) => {
                tracing::trace!(path = %file_path, blob_id = %oid, "seek-reading blob via odb stream");

                seek.apply_reader(std::io::BufReader::new(reader), file_path)
            }

            // The backend holding this object does not support streaming
            // reads (packed objects) — scan the whole inflated blob instead.
            Err(_stream_unsupported) => {
                tracing::trace!(path = %file_path, blob_id = %oid, "seek-reading blob in memory (streaming unsupported)");

                let blob = repo.find_blob(oid)?;

                seek.apply_reader(std::io::Cursor::new(blob.content()), file_path)
            }
        };

        window
    }

    /// Resolves `dir_path` to a tree in `tree` and collects every blob
    /// beneath it, recursively, as `(full path from the repo root, blob oid)`
    /// pairs in git's own tree order.
    ///
    /// Drives the recursive delete and move: the oids are what let the new
    /// commit reuse existing blobs verbatim (no content rehash), and the paths
    /// are what the per-file hooks are built from. Blob *content* is never
    /// read here — only entry names, kinds and oids, all already in the tree
    /// objects. Non-blob leaves (submodules) are skipped: this API has no
    /// representation for them, so a folder holding one moves or deletes its
    /// files and leaves the submodule entry to be dropped with the subtree.
    fn subtree_blob_paths(
        repo: &Repository,
        tree: &git2::Tree<'_>,
        dir_path: &str,
    ) -> Result<Vec<(String, Oid)>, AppError> {
        let tree_entry =
            tree.get_path(Path::new(dir_path))
                .map_err(|_err| AppError::FileNotFound {
                    path: dir_path.to_string(),
                })?;

        let subtree = repo
            .find_tree(tree_entry.id())
            .map_err(|_err| AppError::FileNotFound {
                path: dir_path.to_string(),
            })?;

        let mut blobs: Vec<(String, Oid)> = Vec::new();

        subtree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                if let Ok(name) = entry.name() {
                    // `root` is the path relative to `subtree`, already
                    // slash-terminated ("" at the top level, "sub/" below).
                    blobs.push((format!("{}/{}{}", dir_path, root, name), entry.id()));
                }
            }

            git2::TreeWalkResult::Ok
        })?;

        tracing::trace!(path = %dir_path, file_count = blobs.len(), "collected subtree blobs");

        Ok(blobs)
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
    /// When `file_name_starts_with` is set, the listing is narrowed to files
    /// whose leaf name begins with any of those prefixes (case-insensitively);
    /// see `search_by_file_name` for the exact semantics. In that mode the
    /// "off-page directories are never walked" optimisation below does not
    /// apply — matches can be nested anywhere, so the whole in-scope tree is
    /// walked before pagination.
    ///
    /// When `date_filter` is set, the listing is narrowed to files whose git
    /// created/updated date falls inside the window (see `file_dates`). Like
    /// name search it forgoes the off-page optimisation — a file's date can
    /// move it in or out, so the whole in-scope tree must be examined before
    /// pagination — and it additionally walks commit history (still no blob
    /// opened, but cost scales with history length rather than page size).
    ///
    /// When `order_options` is set, each level of the result is reordered by the
    /// stored order index of the directory it belongs to (see
    /// [`Self::apply_order`]), with its `implicit_default_index` deciding where
    /// unlisted entries land. This is the one mode that opens blobs — one small
    /// index per directory actually rendered.
    ///
    /// The performance contract (search, date-filter and order modes aside):
    /// **no blob is ever opened**. Names and entry kinds come entirely from
    /// git tree objects, so listing cost scales with the number of tree
    /// entries actually visited — and the pagination below is designed to keep
    /// that number small even on huge repositories.
    ///
    /// Order indexes themselves are never listed, whatever
    /// `include_hidden_files` says: they are a separate resource with their own
    /// route, not content.
    #[allow(clippy::too_many_arguments)]
    pub fn list_files(
        repo_path: &Path,
        tenant_id: &str,
        path_prefix: Option<&str>,
        maximum_depth: Option<usize>,
        include_hidden_files: bool,
        file_name_starts_with: Option<&[String]>,
        date_filter: Option<DateFilter>,
        order_options: Option<OrderOptions>,
        page: usize,
        per_page: usize,
    ) -> Result<(Vec<TreeNode>, bool), AppError> {
        tracing::debug!(tenant_id = %tenant_id, path_prefix = ?path_prefix, maximum_depth = ?maximum_depth, include_hidden_files = include_hidden_files, file_name_starts_with = ?file_name_starts_with, date_filter = ?date_filter, order_options = ?order_options, page = page, per_page = per_page, "listing files");

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

        // Name search takes a different route entirely: matches may be nested
        // arbitrarily deep, so the "decide the page window before opening any
        // subtree" optimisation cannot hold — the in-scope tree is walked in
        // full, then the *filtered* result is paginated.
        if let Some(needles) = file_name_starts_with {
            return Self::search_by_file_name(
                &repo,
                &walk_tree,
                path_prefix,
                needles,
                maximum_depth,
                include_hidden_files,
                date_filter,
                order_options,
                page,
                per_page,
            );
        }

        // A date filter (without a name search) likewise walks the whole
        // in-scope tree, dates each file against commit history, and paginates
        // the surviving set — the off-page optimisation cannot hold.
        if let Some(date_filter) = date_filter {
            return Self::list_with_date_filter(
                &repo,
                &walk_tree,
                path_prefix,
                maximum_depth,
                include_hidden_files,
                date_filter,
                order_options,
                page,
                per_page,
            );
        }

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

            // Hidden entries (Unix dot convention) are dropped before the
            // page window is computed, so pagination counts visible entries
            // only. Hidden directories are pruned wholesale: their subtrees
            // are never opened.
            if !include_hidden_files && name.starts_with('.') {
                continue;
            }

            match entry.kind() {
                Some(git2::ObjectType::Tree) => root_dirs.push((name.to_string(), entry.id())),

                // The order index is a separate resource, not content: it is
                // dropped unconditionally, even when hidden entries are asked
                // for, so it can only ever be reached through `/order`.
                Some(git2::ObjectType::Blob) if name != order::ORDER_FILE_NAME => {
                    root_files.push(name.to_string())
                }

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

        let mut root_entries: Vec<RootEntry> = root_dirs
            .into_iter()
            .map(|(name, oid)| RootEntry::Directory(name, oid))
            .chain(root_files.into_iter().map(RootEntry::File))
            .collect();

        // The listing root's own order is applied *before* the page window is
        // sliced — pagination is over root-level entries, so ordering them
        // afterwards would page over the wrong sequence. Only one index is
        // read here; off-page directories are still never opened.
        if let Some(order_options) = order_options {
            if let Some(order) = GitOrder::stored_order(&repo, &walk_tree, "") {
                let ranks = Self::order_ranks(&order);

                root_entries.sort_by_key(|root_entry| {
                    let name = match root_entry {
                        RootEntry::Directory(name, _oid) => name.as_str(),
                        RootEntry::File(name) => name.as_str(),
                    };

                    Self::order_rank(&ranks, name, &order_options)
                });
            }
        }

        let page_entries: Vec<RootEntry> = root_entries
            .into_iter()
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
                    let children =
                        Self::collect_subtree(&subtree, subtree_max_depth, include_hidden_files)?;

                    nodes.push(TreeNode::Directory { name, children });
                }
            }
        }

        // The root level was ordered before pagination; every level *below* it
        // is ordered here, walking only the subtrees that made the page.
        if let Some(order_options) = order_options {
            for node in nodes.iter_mut() {
                if let TreeNode::Directory { name, children } = node {
                    Self::apply_order(&repo, &walk_tree, name, children, &order_options);
                }
            }
        }

        tracing::debug!(tenant_id = %tenant_id, page = page, returned = nodes.len(), has_more = has_more, "file listing complete");

        Ok((nodes, has_more))
    }

    /// Position of each name in a stored order, so a *stable* sort by rank
    /// puts listed entries in index order and leaves everything else in the
    /// order it already had — which is what makes a sparse index pin only what
    /// it names.
    ///
    /// Names are compared with any directory-marking trailing slash stripped.
    /// A duplicate keeps its first position; writes reject duplicates, but a
    /// hand-edited index must still rank deterministically.
    fn order_ranks(order: &[String]) -> HashMap<&str, i64> {
        let mut ranks: HashMap<&str, i64> = HashMap::with_capacity(order.len());

        for (position, entry) in order.iter().enumerate() {
            ranks
                .entry(order::entry_name(entry))
                .or_insert(position as i64);
        }

        ranks
    }

    /// Sort key of `name` against `ranks`: its stored index when the index
    /// names it, and `order_options.implicit_default_index` when it does not —
    /// falling back to "behind everything listed" when no implicit index is
    /// configured, which is the default sparse-index behaviour.
    ///
    /// The `false` on the unlisted side is what makes an implicit index of `0`
    /// mean "above the ordered entries" instead of "tied with the first one":
    /// on an equal index, unlisted sorts before listed.
    fn order_rank(
        ranks: &HashMap<&str, i64>,
        name: &str,
        order_options: &OrderOptions,
    ) -> OrderRank {
        match ranks.get(name) {
            Some(position) => (*position, true),
            None => (
                order_options.implicit_default_index.unwrap_or(i64::MAX),
                false,
            ),
        }
    }

    /// Reorders `nodes` — the rendered children of `directory`, itself
    /// relative to `base_tree` — by that directory's stored order index, then
    /// recurses into every directory child.
    ///
    /// Best-effort by design: a directory with no index, or with one that
    /// cannot be parsed, is left in its ordinary order, and an index entry
    /// naming something absent from `nodes` simply ranks nothing. That is what
    /// makes a stale index harmless — a revert can restore an index older than
    /// the files it names, and no listing should fail because of it.
    ///
    /// This is the only place in the listing path that opens a blob: one index
    /// per directory actually rendered.
    fn apply_order(
        repo: &Repository,
        base_tree: &git2::Tree<'_>,
        directory: &str,
        nodes: &mut [TreeNode],
        order_options: &OrderOptions,
    ) {
        // Nothing rendered at this level, so no index is worth reading — a
        // depth-limited stub costs no blob read.
        if nodes.is_empty() {
            return;
        }

        // A directory with no index is left in its ordinary order, whatever
        // `implicit_default_index` says: with nothing listed, every entry is
        // implicit, so a common fallback index cannot reorder any of them.
        if let Some(order) = GitOrder::stored_order(repo, base_tree, directory) {
            let ranks = Self::order_ranks(&order);

            nodes.sort_by_key(|node| Self::order_rank(&ranks, node.name(), order_options));
        }

        for node in nodes.iter_mut() {
            if let TreeNode::Directory { name, children } = node {
                let child_directory = if directory.is_empty() {
                    name.clone()
                } else {
                    format!("{}/{}", directory, name)
                };

                Self::apply_order(repo, base_tree, &child_directory, children, order_options);
            }
        }
    }

    /// Recursively walks one paged root directory and builds its child nodes.
    /// Only directories inside the requested page window ever reach this
    /// point. Blob objects are never opened — names and kinds come from the
    /// tree objects alone.
    fn collect_subtree(
        subtree: &git2::Tree<'_>,
        max_depth: Option<usize>,
        include_hidden_files: bool,
    ) -> Result<Vec<TreeNode>, AppError> {
        let mut flat: Vec<String> = Vec::new();
        let mut dir_stubs: Vec<String> = Vec::new();

        subtree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            // Hidden entries (Unix dot convention) are excluded at the walk
            // level: `Skip` prunes a hidden directory's whole subtree, so
            // libgit2 never descends into it.
            if !include_hidden_files && entry.name().is_ok_and(|name| name.starts_with('.')) {
                return if entry.kind() == Some(git2::ObjectType::Tree) {
                    git2::TreeWalkResult::Skip
                } else {
                    git2::TreeWalkResult::Ok
                };
            }

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

            // Order indexes are a separate resource, never listed as content.
            if name == order::ORDER_FILE_NAME {
                return git2::TreeWalkResult::Ok;
            }

            flat.push(format!("{}{}", root, name));

            git2::TreeWalkResult::Ok
        })?;

        Ok(GitUtils::build_tree(flat, dir_stubs, max_depth))
    }

    /// Walks `walk_tree` in full and returns the tree of entries whose *leaf
    /// name* begins with any of `needles`, compared case-insensitively (Unicode
    /// lower-casing, so `Intro` matches `intro.md`). Both files *and*
    /// directories are matched:
    ///
    /// - a matching **file** is returned as a leaf, with its ancestor
    ///   directories present purely as the structure leading to it;
    /// - a matching **directory** is returned with its whole subtree expanded
    ///   (every descendant file, whether or not its own name matches), so the
    ///   caller sees what is inside the folder they searched for.
    ///
    /// A directory that neither matches nor contains a match is pruned, so the
    /// result never carries a dead-end empty directory (a matched directory
    /// whose only visible content is filtered out still shows, as a childless
    /// node — it is itself the match).
    ///
    /// `maximum_depth` and `include_hidden_files` carry the same meaning as on
    /// the plain listing, and bound the whole operation uniformly: descent
    /// stops at the depth limit — a match deeper than it is never found, and a
    /// directory sitting *at* the limit renders as a childless stub even when
    /// it matched (exactly as the plain listing stubs depth-limited
    /// directories) — and hidden entries are skipped, a hidden directory's
    /// whole subtree along with them. Pagination is parent-based, as
    /// everywhere else: `page`/`per_page` window over the matched tree's
    /// root-level entries. Same performance contract otherwise — **no blob is
    /// ever opened**, matching is on names alone.
    #[allow(clippy::too_many_arguments)]
    fn search_by_file_name(
        repo: &Repository,
        walk_tree: &git2::Tree<'_>,
        path_prefix: Option<&str>,
        needles: &[String],
        maximum_depth: Option<usize>,
        include_hidden_files: bool,
        date_filter: Option<DateFilter>,
        order_options: Option<OrderOptions>,
        page: usize,
        per_page: usize,
    ) -> Result<(Vec<TreeNode>, bool), AppError> {
        let needles: Vec<String> = needles.iter().map(|needle| needle.to_lowercase()).collect();
        let mut flat: Vec<String> = Vec::new();
        let mut dir_stubs: Vec<String> = Vec::new();

        // While walking inside a directory whose name matched, this holds that
        // directory's path with a trailing slash. Every descendant is then
        // collected unconditionally (the whole matched subtree is expanded);
        // the trailing slash keeps the prefix test from leaking across sibling
        // names (`docs/` must not swallow `docs2/`). Cleared the moment the
        // pre-order walk steps back out of that subtree.
        let mut inside_matched: Option<String> = None;

        walk_tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            // Left the matched subtree? (Pre-order visits all of a directory's
            // descendants contiguously, so one prefix test per entry suffices.)
            if let Some(prefix) = &inside_matched {
                if !root.starts_with(prefix.as_str()) {
                    inside_matched = None;
                }
            }

            // Hidden entries (Unix dot convention) are excluded at the walk
            // level, even inside a matched subtree: `Skip` prunes a hidden
            // directory's whole subtree, so libgit2 never descends into it.
            if !include_hidden_files && entry.name().is_ok_and(|name| name.starts_with('.')) {
                return if entry.kind() == Some(git2::ObjectType::Tree) {
                    git2::TreeWalkResult::Skip
                } else {
                    git2::TreeWalkResult::Ok
                };
            }

            let Ok(name) = entry.name() else {
                return git2::TreeWalkResult::Ok;
            };
            let full_path = format!("{}{}", root, name);
            let in_matched = inside_matched.is_some();
            let lower_name = name.to_lowercase();
            let self_matches = needles.iter().any(|needle| lower_name.starts_with(needle));

            match entry.kind() {
                Some(git2::ObjectType::Tree) => {
                    let matched = in_matched || self_matches;

                    // Depth of this entry relative to the listing root:
                    // "" = depth 1, "a/" = depth 2, … A directory sitting at
                    // the depth limit is not descended into; when it is part
                    // of the result (it or an ancestor matched) it renders as
                    // a childless stub, mirroring the plain listing.
                    if let Some(max) = maximum_depth {
                        let entry_depth = root.chars().filter(|c| *c == '/').count() + 1;

                        if entry_depth >= max {
                            if matched {
                                dir_stubs.push(full_path);
                            }

                            return git2::TreeWalkResult::Skip;
                        }
                    }

                    // Entering a freshly matched directory: record it so its
                    // descendants are collected wholesale, and stub it so it
                    // still appears if every descendant turns out hidden.
                    if self_matches && !in_matched {
                        inside_matched = Some(format!("{}/", full_path));
                        dir_stubs.push(full_path);
                    }
                }
                // Order indexes are a separate resource, never listed as
                // content — and so never searchable by name either.
                Some(git2::ObjectType::Blob)
                    if name != order::ORDER_FILE_NAME && (in_matched || self_matches) =>
                {
                    flat.push(full_path);
                }
                _ => {}
            }

            git2::TreeWalkResult::Ok
        })?;

        // A date filter, when present, composes with the name search: only
        // name matches that also fall inside the date window survive. Because
        // directories carry no date, the matched tree is rebuilt from the
        // surviving *files* alone — a name-matched directory left with no
        // in-window file is pruned (and depth stubs, whose contents we never
        // walked and so cannot date, are dropped).
        let (flat, dir_stubs) = match date_filter {
            Some(date_filter) => (
                Self::retain_by_date(repo, path_prefix, flat, date_filter)?,
                Vec::new(),
            ),
            None => (flat, dir_stubs),
        };

        // The walk already bounded depth, so every collected path is within
        // scope and `build_tree` needs no depth handling of its own (`None`);
        // the stubs it receives are matched directories that were not (or
        // could not be) expanded. The matched tree is then paginated over its
        // root-level entries, exactly like the plain listing.
        let mut tree = GitUtils::build_tree(flat, dir_stubs, None);

        // The whole matched tree is in hand before pagination here, so
        // ordering is applied to it in full and the page window is sliced from
        // the ordered result.
        if let Some(order_options) = order_options {
            Self::apply_order(repo, walk_tree, "", &mut tree, &order_options);
        }

        let total = tree.len();
        let offset = ((page - 1) * per_page).min(total);
        let has_more = total > offset + per_page;

        let nodes: Vec<TreeNode> = tree.into_iter().skip(offset).take(per_page).collect();

        Ok((nodes, has_more))
    }

    /// Lists the in-scope tree narrowed to files whose git date falls inside
    /// `date_filter`'s window. The whole in-scope tree is walked (the off-page
    /// optimisation cannot hold — a date can move any file in or out), each
    /// file is dated against commit history, and the surviving set is
    /// paginated over its root-level entries. Directories survive only as the
    /// structure leading to a surviving file, so an emptied directory is
    /// pruned. `maximum_depth` and `include_hidden_files` bound the walk
    /// exactly as elsewhere; files below the depth limit are never candidates.
    #[allow(clippy::too_many_arguments)]
    fn list_with_date_filter(
        repo: &Repository,
        walk_tree: &git2::Tree<'_>,
        path_prefix: Option<&str>,
        maximum_depth: Option<usize>,
        include_hidden_files: bool,
        date_filter: DateFilter,
        order_options: Option<OrderOptions>,
        page: usize,
        per_page: usize,
    ) -> Result<(Vec<TreeNode>, bool), AppError> {
        let flat = Self::collect_flat_files(walk_tree, maximum_depth, include_hidden_files)?;
        let matched = Self::retain_by_date(repo, path_prefix, flat, date_filter)?;

        // Depth was already applied during collection, so `build_tree` needs
        // no depth handling of its own, and there are no stubs (a directory we
        // could not descend into cannot be date-classified, so it is dropped).
        let mut tree = GitUtils::build_tree(matched, Vec::new(), None);

        // The whole surviving tree is in hand before pagination here, so
        // ordering is applied to it in full and the page window is sliced from
        // the ordered result.
        if let Some(order_options) = order_options {
            Self::apply_order(repo, walk_tree, "", &mut tree, &order_options);
        }

        let total = tree.len();
        let offset = ((page - 1) * per_page).min(total);
        let has_more = total > offset + per_page;

        let nodes: Vec<TreeNode> = tree.into_iter().skip(offset).take(per_page).collect();

        Ok((nodes, has_more))
    }

    /// Collects every in-scope file as a flat path relative to `tree`,
    /// honouring the hidden-entry and depth-limit rules (a hidden directory's
    /// subtree is skipped, and descent stops at the depth limit so files below
    /// it are never collected). Unlike `collect_subtree` it records no
    /// directory stubs and returns bare paths rather than a built tree —
    /// callers that need a tree feed the result back through `build_tree`.
    fn collect_flat_files(
        tree: &git2::Tree<'_>,
        max_depth: Option<usize>,
        include_hidden_files: bool,
    ) -> Result<Vec<String>, AppError> {
        let mut flat: Vec<String> = Vec::new();

        tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if !include_hidden_files && entry.name().is_ok_and(|name| name.starts_with('.')) {
                return if entry.kind() == Some(git2::ObjectType::Tree) {
                    git2::TreeWalkResult::Skip
                } else {
                    git2::TreeWalkResult::Ok
                };
            }

            if entry.kind() == Some(git2::ObjectType::Tree) {
                if let Some(max) = max_depth {
                    let entry_depth = root.chars().filter(|c| *c == '/').count() + 1;

                    if entry_depth >= max {
                        return git2::TreeWalkResult::Skip;
                    }
                }

                return git2::TreeWalkResult::Ok;
            }

            if entry.kind() == Some(git2::ObjectType::Blob) {
                let name = entry.name().unwrap_or("");

                // Order indexes are a separate resource, never listed as
                // content — so they are never date-filtered either.
                if name != order::ORDER_FILE_NAME {
                    flat.push(format!("{}{}", root, name));
                }
            }

            git2::TreeWalkResult::Ok
        })?;

        Ok(flat)
    }

    /// Keeps only those `flat` paths (relative to the listing root) whose git
    /// date falls inside `date_filter`'s window. The date map returned by
    /// `file_dates` is keyed by repo-root paths, so the listing-root
    /// `path_prefix` is re-joined before each lookup. A path with no date in
    /// history (which cannot happen for a file present in HEAD) is dropped.
    fn retain_by_date(
        repo: &Repository,
        path_prefix: Option<&str>,
        flat: Vec<String>,
        date_filter: DateFilter,
    ) -> Result<Vec<String>, AppError> {
        let to_full = |leaf: &str| -> String {
            match path_prefix {
                Some(prefix) if !prefix.is_empty() => format!("{}/{}", prefix, leaf),
                _ => leaf.to_string(),
            }
        };

        let full_paths: HashSet<String> = flat.iter().map(|leaf| to_full(leaf)).collect();

        if full_paths.is_empty() {
            return Ok(Vec::new());
        }

        let dates = Self::file_dates(repo, &full_paths, date_filter.kind)?;

        Ok(flat
            .into_iter()
            .filter(|leaf| {
                dates
                    .get(&to_full(leaf))
                    .is_some_and(|date| date_filter.matches(*date))
            })
            .collect())
    }

    /// Walks commit history once (newest-first, diffing each commit against
    /// its first parent) and returns, for each requested repo-root path, the
    /// commit time that defines its `kind` date.
    ///
    /// The diff is computed from tree/oid deltas alone — no patch, no stats,
    /// no rename detection — so **no blob is opened** and a rename surfaces as
    /// an add of the new path plus a delete of the old (renames are not
    /// followed). Cost is therefore O(commits), not O(history × files).
    ///
    /// - `Updated`: the first commit seen (walking newest-first) that touched
    ///   a path is its most-recent touch; the walk stops as soon as every
    ///   requested path has a date.
    /// - `Created`: the oldest commit that *added* a path; the walk must reach
    ///   the root of history, so this is the heavier of the two.
    fn file_dates(
        repo: &Repository,
        paths: &HashSet<String>,
        kind: DateKind,
    ) -> Result<HashMap<String, DateTime<Utc>>, AppError> {
        let mut dates: HashMap<String, DateTime<Utc>> = HashMap::new();

        let mut revwalk = repo.revwalk()?;

        revwalk.push_head()?;
        revwalk.set_sorting(Sort::TIME | Sort::TOPOLOGICAL)?;

        for oid_result in revwalk {
            // Updated: once every requested path is dated, every older commit
            // can only hold an older (irrelevant) touch — nothing left to find.
            if matches!(kind, DateKind::Updated) && dates.len() == paths.len() {
                break;
            }

            let Ok(oid) = oid_result else {
                continue;
            };
            let Ok(commit) = repo.find_commit(oid) else {
                continue;
            };
            let Ok(commit_tree) = commit.tree() else {
                continue;
            };

            // The root commit has no parent — diff against an empty tree
            // (`None`), so its entries register as additions.
            let parent_tree = commit.parent(0).and_then(|parent| parent.tree()).ok();

            let mut diff_options = DiffOptions::new();

            diff_options.include_untracked(false);

            let diff = match repo.diff_tree_to_tree(
                parent_tree.as_ref(),
                Some(&commit_tree),
                Some(&mut diff_options),
            ) {
                Ok(diff) => diff,
                Err(_) => continue,
            };

            let commit_time = GitUtils::timestamp_from_git_time(commit.time());

            for delta in diff.deltas() {
                // A delete carries the path only on the old side; every other
                // status carries it on the new side.
                let Some(path) = delta
                    .new_file()
                    .path()
                    .or_else(|| delta.old_file().path())
                    .map(|path| path.to_string_lossy().into_owned())
                else {
                    continue;
                };

                if !paths.contains(&path) {
                    continue;
                }

                match kind {
                    // Newest-first, so the first touch seen is the latest.
                    DateKind::Updated => {
                        dates.entry(path).or_insert(commit_time);
                    }
                    // Keep overwriting on each older addition so the final
                    // value is the earliest introduction of the path.
                    DateKind::Created => {
                        if delta.status() == Delta::Added {
                            dates.insert(path, commit_time);
                        }
                    }
                }
            }
        }

        Ok(dates)
    }

    /// Counts files and directories reachable from the listing root, with
    /// the exact scoping semantics of `list_files`: `path_prefix` roots the
    /// count at a sub-directory (absent or non-directory prefix yields zero
    /// counts), `maximum_depth` bounds how many levels are descended
    /// (directories sitting at the limit are counted but never entered),
    /// and hidden entries are excluded unless `include_hidden_files` is set
    /// (a hidden directory's whole subtree is pruned).
    ///
    /// `restrict_file_extensions`, when set, narrows the *file* count to
    /// files carrying one of the given extensions (compared
    /// case-insensitively; extension-less files never match). Directories
    /// are counted regardless — they have no extension to compare.
    ///
    /// Same performance contract as the listing: **no blob is ever opened**.
    /// Names and entry kinds come entirely from git tree objects.
    pub fn count_files(
        repo_path: &Path,
        tenant_id: &str,
        path_prefix: Option<&str>,
        maximum_depth: Option<usize>,
        include_hidden_files: bool,
        restrict_file_extensions: Option<&[String]>,
    ) -> Result<FileCounts, AppError> {
        tracing::debug!(tenant_id = %tenant_id, path_prefix = ?path_prefix, maximum_depth = ?maximum_depth, include_hidden_files = include_hidden_files, restrict_file_extensions = ?restrict_file_extensions, "counting files");

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;
        let head_commit = repo.head()?.peel_to_commit()?;

        tracing::trace!(tenant_id = %tenant_id, head_sha = %head_commit.id(), "resolved HEAD for file counting");

        let head_tree = head_commit.tree()?;

        // Resolve the prefix subtree directly so the walk never visits unrelated
        // directories. An absent or non-directory prefix yields zero counts.
        let walk_tree: git2::Tree<'_> = match path_prefix.filter(|p| !p.is_empty()) {
            Some(prefix) => match head_tree.get_path(Path::new(prefix)) {
                Ok(entry) => match repo.find_tree(entry.id()) {
                    Ok(tree) => tree,
                    Err(_) => return Ok(FileCounts::default()),
                },
                Err(_) => return Ok(FileCounts::default()),
            },
            None => head_tree,
        };

        let mut counts = FileCounts::default();

        walk_tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            // Hidden entries (Unix dot convention) are excluded at the walk
            // level: `Skip` prunes a hidden directory's whole subtree, so
            // libgit2 never descends into it.
            if !include_hidden_files && entry.name().is_ok_and(|name| name.starts_with('.')) {
                return if entry.kind() == Some(git2::ObjectType::Tree) {
                    git2::TreeWalkResult::Skip
                } else {
                    git2::TreeWalkResult::Ok
                };
            }

            match entry.kind() {
                Some(git2::ObjectType::Tree) => {
                    counts.directories += 1;

                    // Depth of this entry relative to the listing root:
                    // "" = depth 1, "a/" = depth 2, … A directory sitting at
                    // the depth limit is counted (it exists at a visible
                    // level, matching the listing's childless stubs) but its
                    // subtree is never entered.
                    if let Some(max) = maximum_depth {
                        let entry_depth = root.chars().filter(|c| *c == '/').count() + 1;

                        if entry_depth >= max {
                            return git2::TreeWalkResult::Skip;
                        }
                    }
                }
                // Order indexes are a separate resource, not content: they are
                // excluded from the count exactly as from the listing, whatever
                // `include_hidden_files` says.
                Some(git2::ObjectType::Blob)
                    if entry
                        .name()
                        .is_ok_and(|name| name == order::ORDER_FILE_NAME) => {}

                Some(git2::ObjectType::Blob) => {
                    let counted = match restrict_file_extensions {
                        None => true,
                        Some(allowed) => entry
                            .name()
                            .ok()
                            .and_then(|name| Path::new(name).extension())
                            .and_then(|extension| extension.to_str())
                            .is_some_and(|extension| {
                                allowed
                                    .iter()
                                    .any(|entry| entry.eq_ignore_ascii_case(extension))
                            }),
                    };

                    if counted {
                        counts.files += 1;
                    }
                }
                _ => {}
            }

            git2::TreeWalkResult::Ok
        })?;

        tracing::debug!(tenant_id = %tenant_id, files = counts.files, directories = counts.directories, "file counting complete");

        Ok(counts)
    }

    /// Returns the file content as recorded in HEAD's tree (not from the working
    /// tree) so the response always reflects the last successfully committed state.
    ///
    /// A path resolving to a folder answers the same 404 as a missing file —
    /// consistent with the HEAD existence endpoint, and so does a path naming
    /// an order index, which is reachable only through `/order`. See
    /// `GitUtils::windowed_blob_content` for how seek windows are read.
    pub fn read_file(
        repo_path: &Path,
        tenant_id: &str,
        file_path: &str,
        seek: &SeekFilter,
    ) -> Result<String, AppError> {
        tracing::debug!(tenant_id = %tenant_id, path = %file_path, "reading file");

        if order::is_order_file(file_path) {
            return Err(AppError::FileNotFound {
                path: file_path.to_string(),
            });
        }

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;
        let head_commit = repo.head()?.peel_to_commit()?;

        tracing::trace!(tenant_id = %tenant_id, path = %file_path, head_sha = %head_commit.id(), "resolved HEAD for read");

        let head_tree = head_commit.tree()?;

        let blob_oid = GitUtils::blob_oid_in_tree(&head_tree, file_path).ok_or_else(|| {
            AppError::FileNotFound {
                path: file_path.to_string(),
            }
        })?;

        GitUtils::windowed_blob_content(&repo, blob_oid, file_path, seek)
    }

    /// Reads several files from HEAD's tree in one repository pass. The
    /// returned vector is index-aligned with `file_reads`: `None` marks a
    /// path that is absent (or a folder), `Some` carries the content with
    /// that entry's seek window applied (the route resolves each entry's
    /// effective window upfront). Unreadable content (invalid UTF-8) is a
    /// hard error for the whole batch, so `None` strictly means "not
    /// found".
    pub fn batch_read_files(
        repo_path: &Path,
        tenant_id: &str,
        file_reads: &[(String, SeekFilter)],
    ) -> Result<Vec<Option<String>>, AppError> {
        tracing::debug!(tenant_id = %tenant_id, count = file_reads.len(), "batch reading files");

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;
        let head_commit = repo.head()?.peel_to_commit()?;

        tracing::trace!(tenant_id = %tenant_id, head_sha = %head_commit.id(), "resolved HEAD for batch read");

        let head_tree = head_commit.tree()?;

        file_reads
            .iter()
            .map(|(file_path, seek)| {
                // An order index is invisible here for the same reason it is
                // on the single read route, and `None` already means "not
                // found" — no special slot is needed for it.
                if order::is_order_file(file_path) {
                    return Ok(None);
                }

                match GitUtils::blob_oid_in_tree(&head_tree, file_path) {
                    None => Ok(None),

                    Some(blob_oid) => {
                        GitUtils::windowed_blob_content(&repo, blob_oid, file_path, seek).map(Some)
                    }
                }
            })
            .collect()
    }

    /// Classifies what `path` resolves to in HEAD's tree — a file, a folder,
    /// or nothing at all — without reading any content.
    ///
    /// This is the single primitive behind both the existence endpoint (which
    /// decides from the kind whether to answer `200` or `404`) and the
    /// recursion-enabled delete/move routes (which decide from it whether to
    /// run the single-file or the whole-folder operation). A missing *tenant*
    /// is still an error rather than `Missing`: the caller is asking about a
    /// repository that does not exist, which is a different answer from "that
    /// path is not in this repository".
    ///
    /// An order index classifies as `Missing` whatever HEAD holds: it is a
    /// separate resource with its own route, so it is neither a file the
    /// existence endpoint should confirm nor a path delete/move may act on.
    pub fn path_kind(repo_path: &Path, tenant_id: &str, path: &str) -> Result<PathKind, AppError> {
        tracing::debug!(tenant_id = %tenant_id, path = %path, "classifying path");

        if order::is_order_file(path) {
            return Ok(PathKind::Missing);
        }

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;
        let head_commit = repo.head()?.peel_to_commit()?;

        tracing::trace!(tenant_id = %tenant_id, path = %path, head_sha = %head_commit.id(), "resolved HEAD for path classification");

        let head_tree = head_commit.tree()?;

        let kind = match head_tree.get_path(Path::new(path)) {
            Ok(entry) => match entry.kind() {
                Some(git2::ObjectType::Blob) => PathKind::File,
                Some(git2::ObjectType::Tree) => PathKind::Directory,
                _ => PathKind::Missing,
            },
            Err(_absent) => PathKind::Missing,
        };

        tracing::debug!(tenant_id = %tenant_id, path = %path, kind = ?kind, "path classified");

        Ok(kind)
    }

    /// Writes a file to disk, stages it, and creates a commit.
    /// Returns the commit SHA and the type of change (created vs updated).
    ///
    /// Writing content identical to what HEAD already holds is a no-op:
    /// no commit is created, nothing touches disk, and the change slot is
    /// `None` so the caller knows not to fire hooks. Clients that blindly
    /// re-PUT unchanged files thus cannot pollute history with empty
    /// commits. The comparison hashes the incoming content and compares
    /// blob oids, so the existing blob is never even read.
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
    ) -> Result<(String, Option<FileChange>), AppError> {
        tracing::debug!(path = %file_path, author_name = %author_name, author_email = %author_email, "writing file");

        // The order index has its own route, which owns its format and
        // validates it. Letting a client PUT it as an ordinary file would
        // bypass that validation entirely, so the path is refused outright —
        // and with a message that says where to go, since unlike a read there
        // is no sense in which a write "is not found".
        if order::is_order_file(file_path) {
            return Err(AppError::InvalidOperation {
                reason: format!(
                    "path is reserved for the file order index, write it through /order instead: {}",
                    file_path
                ),
            });
        }

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
            Ok(entry) if entry.kind() == Some(git2::ObjectType::Blob) => {
                // Hashing the incoming content yields the oid the new blob
                // *would* get; if it matches the entry already in HEAD, the
                // write changes nothing and short-circuits before any disk
                // or object-database activity.
                let incoming_oid =
                    git2::Oid::hash_object(git2::ObjectType::Blob, content.as_bytes())?;

                if entry.id() == incoming_oid {
                    tracing::debug!(path = %file_path, "content unchanged, skipping commit");

                    return Ok((parent_commit.id().to_string(), None));
                }

                false
            }
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

        Ok((commit_oid.to_string(), Some(change)))
    }

    /// Removes a file from disk, stages the deletion, and creates a commit.
    ///
    /// Unlike `write_file`, this opens the repo with `open_tenant_repo` (no
    /// auto-init): deleting a file from a tenant that never existed is a
    /// 404, not a reason to create an empty repository.
    ///
    /// The parent directory's order index, when it lists this file, is
    /// rewritten without it **in the same commit** — so a downstream order
    /// table never holds a position for a file that is gone. That upkeep is
    /// what makes the returned change list plural: the file's own deletion
    /// plus, at most, the index's change (which the hook layer turns into an
    /// `order.*` event from its path alone).
    pub fn delete_file(
        repo_path: &Path,
        tenant_id: &str,
        file_path: &str,
        commit_message: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<(String, Vec<FileChange>), AppError> {
        tracing::debug!(tenant_id = %tenant_id, path = %file_path, author_name = %author_name, author_email = %author_email, "deleting file");

        // The order index is not addressable through the file routes, so it is
        // "not a file" here exactly as a folder is.
        if order::is_order_file(file_path) {
            return Err(AppError::FileNotFound {
                path: file_path.to_string(),
            });
        }

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
        let mut tree_update = TreeUpdateBuilder::new();

        tree_update.remove(file_path);

        let mut file_changes = vec![FileChange::Deleted {
            path: file_path.to_string(),
        }];

        // Same commit, so the order index can never be left naming a file this
        // commit removed.
        let (parent_directory, leaf_name) = order::split_parent(file_path);

        file_changes.extend(GitOrder::stage_entry_removed(
            &repo,
            repo_path,
            &head_tree,
            &mut tree_update,
            parent_directory,
            leaf_name,
        )?);

        let tree_id = tree_update.create_updated(&repo, &head_tree)?;

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

        tracing::debug!(tenant_id = %tenant_id, path = %file_path, sha = %commit_oid, change_count = file_changes.len(), "file deletion committed");

        Ok((commit_oid.to_string(), file_changes))
    }

    /// Removes a whole folder — every file beneath it, recursively — in a
    /// single commit, and returns one `FileChange::Deleted` per file so
    /// downstream systems see each entity disappear individually.
    ///
    /// This is the recursive counterpart of [`Self::delete_file`], reached
    /// only when the caller opted in with `allow_prefix_path_recurse: true`
    /// *and* the path resolves to a folder. It is deliberately a separate
    /// function rather than a mode flag on `delete_file`: the two have
    /// different scopes (one path versus a whole subtree) and different blast
    /// radii, so each keeps one unambiguous meaning.
    ///
    /// The commit tree is HEAD's tree minus the single directory entry —
    /// libgit2's tree updater drops the whole subtree with it, and prunes any
    /// parent directory the removal leaves empty. Cost is therefore
    /// proportional to the path depth, not to the number of files removed;
    /// only the hook list scales with the file count.
    ///
    /// Order indexes travel with the subtree: each one inside it is a blob
    /// like any other, so it is collected and reported as a deletion, which
    /// the hook layer turns into an `order.deleted` event from its path alone.
    /// The *parent* directory's index — which sits outside the subtree — is
    /// rewritten without this folder in the same commit.
    pub fn delete_directory(
        repo_path: &Path,
        tenant_id: &str,
        dir_path: &str,
        commit_message: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<(String, Vec<FileChange>), AppError> {
        tracing::debug!(tenant_id = %tenant_id, path = %dir_path, author_name = %author_name, author_email = %author_email, "deleting directory recursively");

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;

        let parent_commit = repo.head()?.peel_to_commit()?;
        let head_tree = parent_commit.tree()?;

        // Existence and kind are decided from HEAD's tree, never from the
        // working tree — a stray on-disk folder is not a reason to commit.
        match head_tree.get_path(Path::new(dir_path)) {
            Ok(entry) if entry.kind() == Some(git2::ObjectType::Tree) => {}
            _ => {
                tracing::debug!(tenant_id = %tenant_id, path = %dir_path, "directory not found for recursive deletion");

                return Err(AppError::FileNotFound {
                    path: dir_path.to_string(),
                });
            }
        }

        let blobs = GitUtils::subtree_blob_paths(&repo, &head_tree, dir_path)?;

        // Git has no empty trees, so this only happens for a folder holding
        // nothing but entries this API cannot represent. Nothing to report
        // downstream means nothing worth committing — same contract as an
        // unchanged PUT.
        if blobs.is_empty() {
            tracing::debug!(tenant_id = %tenant_id, path = %dir_path, "directory holds no files, skipping commit");

            return Ok((parent_commit.id().to_string(), Vec::new()));
        }

        tracing::debug!(tenant_id = %tenant_id, path = %dir_path, file_count = blobs.len(), "staging recursive directory deletion");

        // A folder already missing from the working tree just means the
        // working tree had diverged from HEAD; HEAD is what counts.
        match std::fs::remove_dir_all(repo_path.join(dir_path)) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(AppError::Io(err)),
        }

        let mut tree_update = TreeUpdateBuilder::new();

        tree_update.remove(dir_path);

        let mut file_changes: Vec<FileChange> = blobs
            .iter()
            .map(|(path, _oid)| FileChange::Deleted { path: path.clone() })
            .collect();

        // The parent's index, if it pins this folder, loses that entry in the
        // same commit.
        let (parent_directory, dir_name) = order::split_parent(dir_path);

        file_changes.extend(GitOrder::stage_entry_removed(
            &repo,
            repo_path,
            &head_tree,
            &mut tree_update,
            parent_directory,
            dir_name,
        )?);

        let tree_id = tree_update.create_updated(&repo, &head_tree)?;

        let tree = repo.find_tree(tree_id)?;
        let signature = GitUtils::git_signature(author_name, author_email)?;

        // The trailing slash marks this as a folder-wide deletion, so a
        // reader of the history can tell it apart from a single-file one.
        let auto_message = format!("delete: {}/", dir_path);
        let message = commit_message.unwrap_or(&auto_message);

        tracing::trace!(tenant_id = %tenant_id, path = %dir_path, message = %message, "committing recursive directory deletion");

        let commit_oid = repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[&parent_commit],
        )?;

        tracing::debug!(tenant_id = %tenant_id, path = %dir_path, sha = %commit_oid, change_count = file_changes.len(), "recursive directory deletion committed");

        Ok((commit_oid.to_string(), file_changes))
    }

    /// Renames a file on disk, stages both sides, and creates a single commit.
    /// This preserves rename semantics so hook receivers know an entity was moved.
    ///
    /// Doing the remove and the insert in *one* commit is the whole point:
    /// two separate commits (delete + create) would fire two hooks and make
    /// the downstream receiver treat the file as a brand-new entity, losing
    /// whatever metadata it had attached to the old path.
    ///
    /// Order indexes are kept honest in the same commit, and the two shapes of
    /// move are treated differently on purpose:
    ///
    /// - **A rename inside one directory keeps the file's position** in that
    ///   directory's index. Demoting a file to the tail because its name
    ///   changed would silently reorder content the caller only renamed.
    /// - **A move across directories** drops the file from the source index
    ///   and appends it to the destination index *only when one already
    ///   exists* — creating an index would pin one file in a directory whose
    ///   siblings are all implicitly ordered, which the caller did not ask for.
    pub fn move_file(
        repo_path: &Path,
        tenant_id: &str,
        from_path: &str,
        to_path: &str,
        commit_message: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<(String, Vec<FileChange>), AppError> {
        tracing::debug!(
            tenant_id = %tenant_id,
            from_path = %from_path,
            to_path = %to_path,
            author_email = %author_email,
            "moving file"
        );

        // The order index is not addressable through the file routes: as a
        // source it is "not a file", and as a destination it is a path the
        // caller may not write (the order route owns its format).
        if order::is_order_file(from_path) {
            return Err(AppError::FileNotFound {
                path: from_path.to_string(),
            });
        }

        if order::is_order_file(to_path) {
            return Err(AppError::InvalidOperation {
                reason: format!(
                    "destination is reserved for the file order index, write it through /order instead: {}",
                    to_path
                ),
            });
        }

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
        let mut tree_update = TreeUpdateBuilder::new();

        tree_update.remove(from_path);
        tree_update.upsert(to_path, source_blob_oid, FileMode::Blob);

        let mut file_changes = vec![FileChange::Moved {
            from_path: from_path.to_string(),
            to_path: to_path.to_string(),
            content,
        }];

        let (from_directory, from_name) = order::split_parent(from_path);
        let (to_directory, to_name) = order::split_parent(to_path);

        if from_directory == to_directory {
            // A pure rename: the file keeps whatever position it held.
            file_changes.extend(GitOrder::stage_entry_renamed(
                &repo,
                repo_path,
                &head_tree,
                &mut tree_update,
                from_directory,
                from_name,
                to_name,
            )?);
        } else {
            file_changes.extend(GitOrder::stage_entry_removed(
                &repo,
                repo_path,
                &head_tree,
                &mut tree_update,
                from_directory,
                from_name,
            )?);

            file_changes.extend(GitOrder::stage_entry_appended(
                &repo,
                repo_path,
                &head_tree,
                &mut tree_update,
                to_directory,
                to_name,
            )?);
        }

        let tree_id = tree_update.create_updated(&repo, &head_tree)?;

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
            change_count = file_changes.len(),
            "file move committed"
        );

        Ok((commit_oid.to_string(), file_changes))
    }

    /// Relocates a whole folder — every file beneath it, recursively — in a
    /// single commit, and returns one `FileChange::Moved` per file so
    /// downstream systems keep each entity's identity across the rename
    /// instead of seeing a wave of deletes followed by unrelated creates.
    ///
    /// This is the recursive counterpart of [`Self::move_file`], reached only
    /// when the caller opted in with `allow_prefix_path_recurse: true` *and*
    /// the source path resolves to a folder. Each file keeps its own leaf
    /// name — only the ancestor prefix changes — so extensions are preserved
    /// by construction and the `limits.allowed_extensions` whitelist has
    /// nothing left to guard (the destination is a folder path, which carries
    /// no extension of its own).
    ///
    /// The destination must not exist in any form, and must not sit *inside*
    /// the source (which would ask the folder to be moved into itself).
    /// Blob oids are reused verbatim, so no content is rehashed; content is
    /// read once per file purely to fill that file's hook payload, exactly as
    /// the single-file move does.
    ///
    /// Order indexes need no rewriting: their entries are leaf names, so every
    /// index inside the subtree is still correct at its new location. Each one
    /// travels as a moved blob like any other file, which the hook layer turns
    /// into `order.deleted` at the old directory plus `order.updated` at the
    /// new one. The two *parent* indexes — outside the subtree — are updated
    /// in the same commit, with the same rename-keeps-its-position rule as
    /// [`Self::move_file`].
    pub fn move_directory(
        repo_path: &Path,
        tenant_id: &str,
        from_path: &str,
        to_path: &str,
        commit_message: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<(String, Vec<FileChange>), AppError> {
        tracing::debug!(
            tenant_id = %tenant_id,
            from_path = %from_path,
            to_path = %to_path,
            author_email = %author_email,
            "moving directory recursively"
        );

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;

        if from_path == to_path {
            tracing::debug!(tenant_id = %tenant_id, path = %from_path, "directory move rejected: source and destination are identical");

            return Err(AppError::InvalidOperation {
                reason: "destination must differ from source path".to_string(),
            });
        }

        // Moving a folder under itself has no coherent result: the source
        // subtree would have to contain its own relocated copy.
        if to_path.starts_with(&format!("{}/", from_path)) {
            tracing::debug!(tenant_id = %tenant_id, from_path = %from_path, to_path = %to_path, "directory move rejected: destination is inside the source");

            return Err(AppError::InvalidOperation {
                reason: format!(
                    "destination must not be inside the source directory: {}",
                    to_path
                ),
            });
        }

        let parent_commit = repo.head()?.peel_to_commit()?;
        let head_tree = parent_commit.tree()?;

        // Existence and kind are decided from HEAD's tree, never from the
        // working tree.
        match head_tree.get_path(Path::new(from_path)) {
            Ok(entry) if entry.kind() == Some(git2::ObjectType::Tree) => {}
            _ => {
                tracing::debug!(tenant_id = %tenant_id, from_path = %from_path, "source directory not found for move");

                return Err(AppError::FileNotFound {
                    path: from_path.to_string(),
                });
            }
        }

        // Refuse to merge into an existing destination, whether it is a file
        // or a folder — the caller must delete it first. This also rules out
        // the mirror case of the check above (the source sitting inside the
        // destination), since any ancestor of the source necessarily exists.
        if head_tree.get_path(Path::new(to_path)).is_ok() {
            tracing::debug!(tenant_id = %tenant_id, to_path = %to_path, "directory move rejected: destination already exists");

            return Err(AppError::InvalidOperation {
                reason: format!("destination already exists: {}", to_path),
            });
        }

        let blobs = GitUtils::subtree_blob_paths(&repo, &head_tree, from_path)?;

        // Nothing this API can represent lives under the folder, so there is
        // nothing to report downstream and nothing worth committing.
        if blobs.is_empty() {
            tracing::debug!(tenant_id = %tenant_id, from_path = %from_path, "source directory holds no files, skipping commit");

            return Ok((parent_commit.id().to_string(), Vec::new()));
        }

        tracing::debug!(
            tenant_id = %tenant_id,
            from_path = %from_path,
            to_path = %to_path,
            file_count = blobs.len(),
            "staging recursive directory move"
        );

        // The commit tree is HEAD's tree with the source folder dropped and
        // every one of its blobs re-attached under the destination prefix,
        // reusing the existing oids — no content rehash, no index round-trip.
        let mut tree_update = TreeUpdateBuilder::new();

        tree_update.remove(from_path);

        let mut file_changes: Vec<FileChange> = Vec::with_capacity(blobs.len());

        for (source_path, blob_oid) in &blobs {
            // Every collected path starts with `from_path/`, so swapping that
            // prefix for `to_path` keeps each file's leaf name — and thus its
            // extension and downstream identity — intact.
            let relative_path = source_path
                .strip_prefix(from_path)
                .and_then(|rest| rest.strip_prefix('/'))
                .unwrap_or(source_path);

            let destination_path = format!("{}/{}", to_path, relative_path);

            // Content comes from HEAD's blob — the authoritative state —
            // rather than whatever the working tree currently holds. It is
            // needed for this file's hook payload either way.
            let content = GitUtils::blob_content_from_tree(&repo, &head_tree, source_path)?;

            let absolute_destination = repo_path.join(&destination_path);

            if let Some(parent_dir) = absolute_destination.parent() {
                std::fs::create_dir_all(parent_dir)?;
            }

            std::fs::write(&absolute_destination, &content)?;

            tree_update.upsert(&destination_path, *blob_oid, FileMode::Blob);

            file_changes.push(FileChange::Moved {
                from_path: source_path.clone(),
                to_path: destination_path,
                content,
            });
        }

        // The destination is never inside the source (rejected above), so the
        // freshly written files cannot be swept away by this cleanup. A
        // folder already missing on disk just means the working tree had
        // diverged from HEAD.
        match std::fs::remove_dir_all(repo_path.join(from_path)) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(AppError::Io(err)),
        }

        // The parent indexes are settled last, once every file is staged: the
        // folder loses its entry where it came from and gains one where it
        // landed, or keeps its position outright when both are the same parent.
        let (from_parent, from_name) = order::split_parent(from_path);
        let (to_parent, to_name) = order::split_parent(to_path);

        if from_parent == to_parent {
            file_changes.extend(GitOrder::stage_entry_renamed(
                &repo,
                repo_path,
                &head_tree,
                &mut tree_update,
                from_parent,
                from_name,
                &order::directory_entry(to_name),
            )?);
        } else {
            file_changes.extend(GitOrder::stage_entry_removed(
                &repo,
                repo_path,
                &head_tree,
                &mut tree_update,
                from_parent,
                from_name,
            )?);

            file_changes.extend(GitOrder::stage_entry_appended(
                &repo,
                repo_path,
                &head_tree,
                &mut tree_update,
                to_parent,
                &order::directory_entry(to_name),
            )?);
        }

        let tree_id = tree_update.create_updated(&repo, &head_tree)?;
        let tree = repo.find_tree(tree_id)?;
        let signature = GitUtils::git_signature(author_name, author_email)?;

        // Trailing slashes mark this as a folder-wide move, so a reader of the
        // history can tell it apart from a single-file one.
        let auto_message = format!("move: {}/ -> {}/", from_path, to_path);
        let message = commit_message.unwrap_or(&auto_message);

        tracing::trace!(
            tenant_id = %tenant_id,
            from_path = %from_path,
            to_path = %to_path,
            message = %message,
            "committing recursive directory move"
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
            file_count = file_changes.len(),
            "recursive directory move committed"
        );

        Ok((commit_oid.to_string(), file_changes))
    }
}

// ---------------------------------------------------------------------------
// GitOrder — the per-directory file-order index
// ---------------------------------------------------------------------------

/// Reads and writes the per-directory order index, and keeps existing indexes
/// honest as files move and disappear.
///
/// The index is stored as a `.order.json` blob in the directory it orders (see
/// `order.rs` for the format and the reasoning), and every operation here goes
/// through the ordinary write machinery: HEAD is authoritative, the commit tree
/// is built with `TreeUpdateBuilder`, and the working tree is mirrored as a
/// courtesy. Two properties follow from that and are worth stating outright:
///
/// - **An index change is a file change internally.** Every function returns a
///   `FileChange` on the index's own path; the hook layer derives the
///   `order.*` event kind from that path. Nothing here knows about hooks, and
///   nothing else in the system needs a special case for order events —
///   reverts, rollbacks and recursive folder operations classify for free.
/// - **Upkeep rides along in the same commit** as the file operation that
///   triggered it, so a receiver never sees a window in which the order table
///   references a file that no longer exists.
pub struct GitOrder;

impl GitOrder {
    /// Returns the stored order for `directory`, or a 404 when that directory
    /// has no index.
    pub fn read_order(
        repo_path: &Path,
        tenant_id: &str,
        directory: &str,
    ) -> Result<Vec<String>, AppError> {
        tracing::debug!(tenant_id = %tenant_id, directory = %order::display_directory(directory), "reading order index");

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;
        let head_tree = repo.head()?.peel_to_commit()?.tree()?;

        Self::stored_order(&repo, &head_tree, directory).ok_or_else(|| AppError::OrderNotFound {
            directory: order::display_directory(directory).to_string(),
        })
    }

    /// Replaces `directory`'s order index with `order`.
    ///
    /// Every entry is resolved against HEAD's tree and canonicalised from what
    /// it resolved to (a directory gains a trailing slash, a file does not), so
    /// an entry naming something absent is a `400`. That strictness is safe
    /// because the check runs under the tenant write lock, the same lock every
    /// write takes: the classification cannot go stale before the commit it
    /// drives. The order may still be *sparse* — entries must exist, but not
    /// every existing sibling need be listed.
    ///
    /// Writing the order the index already holds is a no-op, exactly as
    /// re-PUTting unchanged file content is: no commit, no hook, and the
    /// returned sha is HEAD's.
    pub fn write_order(
        repo_path: &Path,
        tenant_id: &str,
        directory: &str,
        order_entries: &[String],
        commit_message: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<(String, Option<FileChange>), AppError> {
        let display_directory = order::display_directory(directory);

        tracing::debug!(tenant_id = %tenant_id, directory = %display_directory, entry_count = order_entries.len(), author_name = %author_name, author_email = %author_email, "writing order index");

        // No auto-init here, unlike a file write: an order can only name
        // entries that exist, so there is nothing a brand-new repository could
        // accept — and an order route has no business creating a tenant.
        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;

        let parent_commit = repo.head()?.peel_to_commit()?;
        let head_tree = parent_commit.tree()?;

        // The repository root is always there; any other directory must be.
        if !directory.is_empty() {
            match head_tree.get_path(Path::new(directory)) {
                Ok(entry) if entry.kind() == Some(git2::ObjectType::Tree) => {}
                _ => {
                    tracing::debug!(tenant_id = %tenant_id, directory = %display_directory, "directory not found for order write");

                    return Err(AppError::FileNotFound {
                        path: directory.to_string(),
                    });
                }
            }
        }

        let mut canonical: Vec<String> = Vec::with_capacity(order_entries.len());

        for entry in order_entries {
            let name = order::entry_name(entry);

            let entry_path = if directory.is_empty() {
                name.to_string()
            } else {
                format!("{}/{}", directory, name)
            };

            match head_tree.get_path(Path::new(&entry_path)) {
                Ok(tree_entry) if tree_entry.kind() == Some(git2::ObjectType::Blob) => {
                    canonical.push(name.to_string())
                }
                Ok(tree_entry) if tree_entry.kind() == Some(git2::ObjectType::Tree) => {
                    canonical.push(order::directory_entry(name))
                }
                _ => {
                    tracing::debug!(tenant_id = %tenant_id, directory = %display_directory, entry = %name, "order write rejected: entry does not exist");

                    return Err(AppError::InvalidOperation {
                        reason: format!(
                            "order entry does not exist in '{}': {}",
                            display_directory, name
                        ),
                    });
                }
            }
        }

        let index_path = order::order_file_path(directory);
        let existing_oid = GitUtils::blob_oid_in_tree(&head_tree, &index_path);

        // Hashing the serialised document yields the oid the blob *would* get;
        // matching HEAD's entry means this write changes nothing.
        let incoming_oid = git2::Oid::hash_object(
            git2::ObjectType::Blob,
            order::serialize(&canonical).as_bytes(),
        )?;

        if existing_oid == Some(incoming_oid) {
            tracing::debug!(tenant_id = %tenant_id, directory = %display_directory, "order unchanged, skipping commit");

            return Ok((parent_commit.id().to_string(), None));
        }

        let mut tree_update = TreeUpdateBuilder::new();

        let change = Self::stage_index(
            &repo,
            repo_path,
            &mut tree_update,
            directory,
            &canonical,
            existing_oid.is_some(),
        )?;

        let tree_id = tree_update.create_updated(&repo, &head_tree)?;
        let tree = repo.find_tree(tree_id)?;
        let signature = GitUtils::git_signature(author_name, author_email)?;

        let auto_message = format!("order update: {}", display_directory);
        let message = commit_message.unwrap_or(&auto_message);

        let commit_oid = repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[&parent_commit],
        )?;

        tracing::debug!(tenant_id = %tenant_id, directory = %display_directory, sha = %commit_oid, "order write committed");

        Ok((commit_oid.to_string(), Some(change)))
    }

    /// Moves one entry to `position` inside its own parent directory's order
    /// index, shifting the entries at and after that position down by one.
    ///
    /// `allow_prefix_path` decides whether the path may name a **directory**
    /// rather than a file. It defaults off at the route, so only files are
    /// positionable unless the caller opts in — the same "the flag permits, it
    /// never forces" rule as `allow_prefix_path_recurse` on delete and move: a
    /// file path with the flag on runs exactly as it would with it off, and a
    /// directory path with it off is simply "not a file" and answers `404`. A
    /// directory entry is stored in the canonical index spelling (a trailing
    /// slash), which is what a stored order needs to be readable without
    /// resolving every name against a tree.
    ///
    /// The index is *read and shifted*, never rebuilt from the directory: a
    /// sibling that was never ordered stays unlisted, since being absent from
    /// the index is a meaningful state (it means "implicitly ordered") and not a
    /// gap to be filled. The entry itself is the one that must be there
    /// afterwards, so it is dropped from wherever it sat and re-inserted at
    /// `position` — which makes the position count against the index *without*
    /// it, the intuitive reading of "move this to slot N".
    ///
    /// A `position` beyond the end of the resulting index is clamped to the
    /// tail rather than rejected: the index is sparse, so its length is not
    /// something a caller can be expected to know, and "as far down as possible"
    /// is unambiguous. A directory with no index yet gets one holding just this
    /// entry — unlike implicit upkeep, which never creates an index, this is an
    /// explicit request for a position, exactly as `PUT /order` is.
    ///
    /// Reordering to the position the entry already holds is a no-op, exactly as
    /// re-PUTting unchanged file content is: no commit, no hook, HEAD's sha.
    #[allow(clippy::too_many_arguments)]
    pub fn reorder_entry(
        repo_path: &Path,
        tenant_id: &str,
        entry_path: &str,
        position: usize,
        allow_prefix_path: bool,
        commit_message: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<(String, Option<FileChange>), AppError> {
        tracing::debug!(tenant_id = %tenant_id, path = %entry_path, position = position, allow_prefix_path = allow_prefix_path, author_name = %author_name, author_email = %author_email, "reordering order index entry");

        // The index is not a file, so it cannot be given a position of its own
        // — to the `/files` routes it simply does not exist.
        if order::is_order_file(entry_path) {
            return Err(AppError::FileNotFound {
                path: entry_path.to_string(),
            });
        }

        // No auto-init here, as on every other order operation: a position can
        // only be given to something that exists.
        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;

        let parent_commit = repo.head()?.peel_to_commit()?;
        let head_tree = parent_commit.tree()?;

        let (directory, name) = order::split_parent(entry_path);
        let display_directory = order::display_directory(directory);

        // Classified against HEAD's tree, under the tenant write lock, so the
        // classification cannot go stale before the commit it drives. A folder
        // only counts when the caller opted in; without the flag it is "not a
        // file" and answers `404`, exactly as on a read or a plain delete.
        let canonical_entry = match head_tree.get_path(Path::new(entry_path)) {
            Ok(entry) if entry.kind() == Some(git2::ObjectType::Blob) => name.to_string(),

            Ok(entry) if entry.kind() == Some(git2::ObjectType::Tree) && allow_prefix_path => {
                order::directory_entry(name)
            }

            _ => {
                tracing::debug!(tenant_id = %tenant_id, path = %entry_path, allow_prefix_path = allow_prefix_path, "entry not found for reorder");

                return Err(AppError::FileNotFound {
                    path: entry_path.to_string(),
                });
            }
        };

        let index_path = order::order_file_path(directory);
        let existing_oid = GitUtils::blob_oid_in_tree(&head_tree, &index_path);

        let stored = Self::stored_order(&repo, &head_tree, directory).unwrap_or_default();

        // Dropped from wherever it sat first, so `position` counts against the
        // other entries alone — moving an entry to the index it already holds
        // then lands it right back where it was. Matching ignores the
        // directory-marking slash, so a stale spelling cannot leave a duplicate
        // behind.
        let mut entries: Vec<String> = stored
            .into_iter()
            .filter(|entry| order::entry_name(entry) != name)
            .collect();

        let position = position.min(entries.len());

        entries.insert(position, canonical_entry);

        // Hashing the serialised document yields the oid the blob *would* get;
        // matching HEAD's entry means this reorder changes nothing.
        let incoming_oid = git2::Oid::hash_object(
            git2::ObjectType::Blob,
            order::serialize(&entries).as_bytes(),
        )?;

        if existing_oid == Some(incoming_oid) {
            tracing::debug!(tenant_id = %tenant_id, directory = %display_directory, "order unchanged, skipping commit");

            return Ok((parent_commit.id().to_string(), None));
        }

        let mut tree_update = TreeUpdateBuilder::new();

        let change = Self::stage_index(
            &repo,
            repo_path,
            &mut tree_update,
            directory,
            &entries,
            existing_oid.is_some(),
        )?;

        let tree_id = tree_update.create_updated(&repo, &head_tree)?;
        let tree = repo.find_tree(tree_id)?;
        let signature = GitUtils::git_signature(author_name, author_email)?;

        let auto_message = format!("order position: {} -> {}", entry_path, position);
        let message = commit_message.unwrap_or(&auto_message);

        let commit_oid = repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[&parent_commit],
        )?;

        tracing::debug!(tenant_id = %tenant_id, directory = %display_directory, position = position, sha = %commit_oid, "order reorder committed");

        Ok((commit_oid.to_string(), Some(change)))
    }

    /// Drops `directory`'s order index, so the directory falls back to the
    /// ordinary listing order. A directory with no index is a 404 — there is
    /// nothing to delete, and reporting success would hide a caller mistake.
    pub fn delete_order(
        repo_path: &Path,
        tenant_id: &str,
        directory: &str,
        commit_message: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<(String, FileChange), AppError> {
        let display_directory = order::display_directory(directory);

        tracing::debug!(tenant_id = %tenant_id, directory = %display_directory, author_name = %author_name, author_email = %author_email, "deleting order index");

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;

        let parent_commit = repo.head()?.peel_to_commit()?;
        let head_tree = parent_commit.tree()?;

        let index_path = order::order_file_path(directory);

        if GitUtils::blob_oid_in_tree(&head_tree, &index_path).is_none() {
            tracing::debug!(tenant_id = %tenant_id, directory = %display_directory, "order index not found for deletion");

            return Err(AppError::OrderNotFound {
                directory: display_directory.to_string(),
            });
        }

        let mut tree_update = TreeUpdateBuilder::new();

        // An empty order stages a removal — an index holding nothing and no
        // index at all are the same state.
        let change = Self::stage_index(&repo, repo_path, &mut tree_update, directory, &[], true)?;

        let tree_id = tree_update.create_updated(&repo, &head_tree)?;
        let tree = repo.find_tree(tree_id)?;
        let signature = GitUtils::git_signature(author_name, author_email)?;

        let auto_message = format!("order delete: {}", display_directory);
        let message = commit_message.unwrap_or(&auto_message);

        let commit_oid = repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[&parent_commit],
        )?;

        tracing::debug!(tenant_id = %tenant_id, directory = %display_directory, sha = %commit_oid, "order deletion committed");

        Ok((commit_oid.to_string(), change))
    }

    /// The order stored for `directory` (relative to `tree`), or `None` when
    /// there is no index or it cannot be read as one.
    ///
    /// A malformed index degrades to "no index" rather than to an error: only
    /// this server's order route writes the file, so a broken one can only
    /// come from a hand-edited commit, and that must not be able to fail every
    /// listing of the directory.
    fn stored_order(
        repo: &Repository,
        tree: &git2::Tree<'_>,
        directory: &str,
    ) -> Option<Vec<String>> {
        let index_path = order::order_file_path(directory);
        let index_oid = GitUtils::blob_oid_in_tree(tree, &index_path)?;
        let blob = repo.find_blob(index_oid).ok()?;

        let parsed = order::parse(std::str::from_utf8(blob.content()).ok()?);

        if parsed.is_none() {
            tracing::warn!(path = %index_path, "order index is malformed, ignoring it");
        }

        parsed
    }

    /// Stages `directory`'s index to hold `order_entries` — or removes the
    /// index when they are empty — mirroring the change onto the working tree,
    /// and returns the change recorded for the index blob itself.
    ///
    /// `index_existed` only picks between a created and an updated change; both
    /// become the same `order.updated` event, but the distinction keeps the
    /// change list honest for anything else reading it.
    fn stage_index(
        repo: &Repository,
        repo_path: &Path,
        tree_update: &mut TreeUpdateBuilder,
        directory: &str,
        order_entries: &[String],
        index_existed: bool,
    ) -> Result<FileChange, AppError> {
        let index_path = order::order_file_path(directory);
        let absolute_path = repo_path.join(&index_path);

        if order_entries.is_empty() {
            tracing::trace!(path = %index_path, "staging order index removal");

            // An index already missing from the working tree just means the
            // working tree had diverged from HEAD; HEAD is what counts.
            match std::fs::remove_file(&absolute_path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(AppError::Io(err)),
            }

            tree_update.remove(&index_path);

            return Ok(FileChange::Deleted { path: index_path });
        }

        tracing::trace!(path = %index_path, entry_count = order_entries.len(), "staging order index write");

        let content = order::serialize(order_entries);

        if let Some(parent_dir) = absolute_path.parent() {
            std::fs::create_dir_all(parent_dir)?;
        }

        std::fs::write(&absolute_path, &content)?;

        let blob_oid = repo.blob(content.as_bytes())?;

        tree_update.upsert(&index_path, blob_oid, FileMode::Blob);

        Ok(if index_existed {
            FileChange::Updated {
                path: index_path,
                content,
            }
        } else {
            FileChange::Created {
                path: index_path,
                content,
            }
        })
    }

    /// Drops `name` from `directory`'s index. `None` when there is no index
    /// there, or when it does not list `name` — upkeep never creates an index
    /// and never rewrites one it has nothing to say about.
    fn stage_entry_removed(
        repo: &Repository,
        repo_path: &Path,
        head_tree: &git2::Tree<'_>,
        tree_update: &mut TreeUpdateBuilder,
        directory: &str,
        name: &str,
    ) -> Result<Option<FileChange>, AppError> {
        let Some(stored) = Self::stored_order(repo, head_tree, directory) else {
            return Ok(None);
        };

        let entry_count = stored.len();

        let retained: Vec<String> = stored
            .into_iter()
            .filter(|entry| order::entry_name(entry) != name)
            .collect();

        if retained.len() == entry_count {
            return Ok(None);
        }

        Self::stage_index(repo, repo_path, tree_update, directory, &retained, true).map(Some)
    }

    /// Appends `canonical_entry` to `directory`'s index, at the tail.
    ///
    /// `None` when there is no index there — an index is only ever *edited* by
    /// upkeep, never created: pinning one file in a directory whose siblings
    /// are all implicitly ordered would be a surprise the caller did not ask
    /// for. Also `None` when the entry is already listed (which a stale entry
    /// left by a revert can make true), so upkeep never duplicates a name.
    fn stage_entry_appended(
        repo: &Repository,
        repo_path: &Path,
        head_tree: &git2::Tree<'_>,
        tree_update: &mut TreeUpdateBuilder,
        directory: &str,
        canonical_entry: &str,
    ) -> Result<Option<FileChange>, AppError> {
        let Some(mut stored) = Self::stored_order(repo, head_tree, directory) else {
            return Ok(None);
        };

        let name = order::entry_name(canonical_entry);

        if stored.iter().any(|entry| order::entry_name(entry) == name) {
            return Ok(None);
        }

        stored.push(canonical_entry.to_string());

        Self::stage_index(repo, repo_path, tree_update, directory, &stored, true).map(Some)
    }

    /// Replaces `from_name` with `canonical_entry` **in place**, keeping the
    /// entry's position. `None` when there is no index there, or when it does
    /// not list `from_name` — an unlisted entry stays unlisted, since a rename
    /// changes a name and should not change an ordering.
    fn stage_entry_renamed(
        repo: &Repository,
        repo_path: &Path,
        head_tree: &git2::Tree<'_>,
        tree_update: &mut TreeUpdateBuilder,
        directory: &str,
        from_name: &str,
        canonical_entry: &str,
    ) -> Result<Option<FileChange>, AppError> {
        let Some(mut stored) = Self::stored_order(repo, head_tree, directory) else {
            return Ok(None);
        };

        let Some(position) = stored
            .iter()
            .position(|entry| order::entry_name(entry) == from_name)
        else {
            return Ok(None);
        };

        stored[position] = canonical_entry.to_string();

        Self::stage_index(repo, repo_path, tree_update, directory, &stored, true).map(Some)
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
        include_statistics: bool,
    ) -> Result<(Vec<CommitSummary>, bool), AppError> {
        if let Some(path) = file_path {
            return Self::list_commits_by_file(
                repo_path,
                tenant_id,
                page,
                per_page,
                path,
                include_statistics,
            );
        }

        tracing::debug!(tenant_id = %tenant_id, page = page, per_page = per_page, include_statistics = include_statistics, "listing commits");

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;

        let mut revwalk = repo.revwalk()?;

        revwalk.push_head()?;

        // TIME | TOPOLOGICAL gives stable ordering across commits sharing a timestamp.
        revwalk.set_sorting(Sort::TIME | Sort::TOPOLOGICAL)?;

        let skip_count = page.saturating_sub(1).saturating_mul(per_page);

        tracing::trace!(tenant_id = %tenant_id, skip_count = skip_count, per_page = per_page, "walking commit graph");

        // Fetch one extra to detect whether a next page exists without a full count.
        // When line stats are requested, that extra commit is diffed too — one
        // wasted diff per page is an acceptable trade for keeping this branch
        // free of a second, stats-only pass.
        let mut commits: Vec<CommitSummary> = revwalk
            .skip(skip_count)
            .take(per_page + 1)
            .filter_map(|oid_result| oid_result.ok())
            .filter_map(|oid| repo.find_commit(oid).ok())
            .map(|commit| {
                let statistics = if include_statistics {
                    Some(Self::statistics_for_commit(&repo, &commit)?)
                } else {
                    None
                };

                Ok(CommitSummary {
                    sha: commit.id().to_string(),
                    message: commit.message().unwrap_or("").to_string(),
                    author: CommitAuthor {
                        name: commit.author().name().unwrap_or("").to_string(),
                        email: commit.author().email().unwrap_or("").to_string(),
                    },
                    committed_at: GitUtils::timestamp_from_git_time(commit.time()),
                    statistics,
                })
            })
            .collect::<Result<Vec<CommitSummary>, AppError>>()?;

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
        include_statistics: bool,
    ) -> Result<(Vec<CommitSummary>, bool), AppError> {
        tracing::debug!(
            tenant_id = %tenant_id,
            page = page,
            per_page = per_page,
            file_path = %file_path,
            include_statistics = include_statistics,
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
                    statistics: None,
                });

                if let Some(old_name) = rename_from {
                    current_path = old_name;
                }
            }
        }

        let has_more = matching.len() > skip_count + per_page;

        // Line stats are only computed for the final page window, not for
        // every matching commit found while walking history — the match
        // scan already runs a rename-detecting diff per introduction, so
        // deferring this avoids doubling that cost across unpaginated rows.
        let commits: Vec<CommitSummary> = matching
            .into_iter()
            .skip(skip_count)
            .take(per_page)
            .map(|mut summary| {
                if include_statistics {
                    let oid = Oid::from_str(&summary.sha)?;
                    let commit = repo.find_commit(oid)?;

                    summary.statistics = Some(Self::statistics_for_commit(&repo, &commit)?);
                }

                Ok(summary)
            })
            .collect::<Result<Vec<CommitSummary>, AppError>>()?;

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

    /// Computes aggregate line-change stats for one commit against its first
    /// parent (or an empty tree for the root commit). Rename detection runs
    /// first so a pure rename doesn't count as a full delete+add of its
    /// content. Only called when a caller opts in via `include_statistics`,
    /// since it requires an actual content diff rather than the cheap
    /// oid/tree comparisons the rest of commit listing relies on.
    fn statistics_for_commit(
        repo: &Repository,
        commit: &git2::Commit,
    ) -> Result<CommitStatistics, AppError> {
        let commit_tree = commit.tree()?;

        let parent_tree = if commit.parent_count() > 0 {
            Some(commit.parent(0)?.tree()?)
        } else {
            None
        };

        let mut diff_options = DiffOptions::new();

        diff_options.include_untracked(false);

        let mut diff = repo.diff_tree_to_tree(
            parent_tree.as_ref(),
            Some(&commit_tree),
            Some(&mut diff_options),
        )?;

        let mut find_options = DiffFindOptions::new();

        find_options.renames(true);

        diff.find_similar(Some(&mut find_options))?;

        let stats = diff.stats()?;

        Ok(CommitStatistics {
            insertions: stats.insertions(),
            deletions: stats.deletions(),
            files_changed: stats.files_changed(),
        })
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

        let diff_stats = diff.stats()?;
        let statistics = CommitStatistics {
            insertions: diff_stats.insertions(),
            deletions: diff_stats.deletions(),
            files_changed: diff_stats.files_changed(),
        };

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
            statistics,
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

    /// Rolls every file the commit `sha` touched back to the exact state it
    /// had *at* that commit — point-in-time rollback, the "time machine"
    /// counterpart to the whole-commit revert above. Returns the new commit
    /// SHA and one change per file that actually moved (empty when the
    /// repository already holds that state).
    ///
    /// Which files are in scope is derived from `sha` itself — the same delta
    /// set a revert walks — so the caller passes no paths: the commit already
    /// says what it touched. What differs from `revert_commit` is which *side*
    /// of the target commit is restored. A revert restores `parent(sha)` (undo
    /// what that commit did); a rollback restores `sha` itself (make those
    /// files look the way they looked then), collapsing every later change to
    /// those paths into one commit. Rolling back to the root commit is legal —
    /// with no parent, its delta set is simply its whole tree.
    ///
    /// The target state is read from `sha`'s tree and staged onto **current
    /// HEAD**, reusing existing blob oids so no content is rehashed. Per file:
    ///
    /// - present at `sha`, absent in HEAD → re-created (a file deleted since
    ///   comes back from the dead)
    /// - present at `sha`, different in HEAD → updated
    /// - absent at `sha` (the commit deleted it), present in HEAD → deleted
    ///   again
    /// - identical on both sides → skipped entirely: no staging, no hook
    ///
    /// A rename inside the commit rolls back as a rename (one `file.moved`
    /// hook, preserving downstream entity identity) whenever HEAD still holds
    /// the pre-rename path and not the post-rename one; otherwise each side is
    /// settled on its own. When nothing at all needs to move, no commit is
    /// created and HEAD's sha is returned — same contract as an unchanged PUT.
    ///
    /// History is never rewritten: the rollback is a new commit on top, and
    /// the state it replaces stays reachable through its own commit.
    pub fn rollback_commit(
        repo_path: &Path,
        tenant_id: &str,
        sha: &str,
        commit_message: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<(String, Vec<FileChange>), AppError> {
        tracing::debug!(tenant_id = %tenant_id, sha = %sha, author_name = %author_name, author_email = %author_email, "rolling files back to commit");

        // Stages `path` holding `oid` — the state at the target commit —
        // unless HEAD already holds that exact blob, in which case there is
        // nothing to roll back for this path.
        fn restore_path(
            repo: &Repository,
            repo_path: &Path,
            head_tree: &git2::Tree<'_>,
            target_tree: &git2::Tree<'_>,
            tree_update: &mut TreeUpdateBuilder,
            path: &Path,
            oid: Oid,
        ) -> Result<Option<FileChange>, AppError> {
            let path_string = path.to_string_lossy().into_owned();
            let head_oid = GitUtils::blob_oid_in_tree(head_tree, &path_string);

            if head_oid == Some(oid) {
                tracing::trace!(path = %path_string, "rollback: file already in target state");

                return Ok(None);
            }

            tracing::trace!(
                path = %path_string,
                existed_in_head = head_oid.is_some(),
                "rollback: restoring file content from target commit"
            );

            let content = GitUtils::blob_content_from_tree(repo, target_tree, &path_string)?;

            let absolute_path = repo_path.join(path);

            if let Some(parent_dir) = absolute_path.parent() {
                std::fs::create_dir_all(parent_dir)?;
            }

            std::fs::write(&absolute_path, &content)?;

            tree_update.upsert(path, oid, FileMode::Blob);

            Ok(Some(if head_oid.is_some() {
                FileChange::Updated {
                    path: path_string,
                    content,
                }
            } else {
                FileChange::Created {
                    path: path_string,
                    content,
                }
            }))
        }

        // Stages the removal of `path` — absent at the target commit — unless
        // HEAD does not hold it as a file either.
        fn remove_path(
            repo_path: &Path,
            head_tree: &git2::Tree<'_>,
            tree_update: &mut TreeUpdateBuilder,
            path: &Path,
        ) -> Result<Option<FileChange>, AppError> {
            let path_string = path.to_string_lossy().into_owned();

            if GitUtils::blob_oid_in_tree(head_tree, &path_string).is_none() {
                tracing::trace!(path = %path_string, "rollback: file already absent");

                return Ok(None);
            }

            tracing::trace!(path = %path_string, "rollback: removing file absent from target commit");

            // A file already missing from the working tree just means the
            // working tree had diverged from HEAD; HEAD is what counts.
            match std::fs::remove_file(repo_path.join(path)) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(AppError::Io(err)),
            }

            tree_update.remove(path);

            Ok(Some(FileChange::Deleted { path: path_string }))
        }

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

        let target_tree = target_commit.tree()?;

        // Diff parent → target tells us which paths this commit touched, and
        // what each of them looked like once it landed. The root commit has no
        // parent, so it diffs against nothing: its whole tree is in scope.
        tracing::trace!(tenant_id = %tenant_id, sha = %sha, "computing diff for rollback");

        let parent_tree = match target_commit.parent_count() {
            0 => None,
            _ => Some(target_commit.parent(0)?.tree()?),
        };

        let mut diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&target_tree), None)?;

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

        tracing::trace!(tenant_id = %tenant_id, sha = %sha, delta_count = raw_deltas.len(), "applying rollback deltas");

        let head_commit = repo.head()?.peel_to_commit()?;
        let head_tree = head_commit.tree()?;

        // The rollback tree is HEAD's tree with each touched path forced back
        // to its target-commit state — no index round-trip, no rehashing.
        let mut tree_update = TreeUpdateBuilder::new();

        let mut file_changes: Vec<FileChange> = Vec::new();

        // For each delta: mirror the target state onto the working tree
        // (best-effort human-visible state), stage it into the tree builder
        // (the authoritative commit state), and record the corresponding
        // FileChange (drives one hook per file, in this order).
        for raw_delta in &raw_deltas {
            match raw_delta.status {
                // The commit created or changed this file → restore that version.
                Delta::Added | Delta::Modified => {
                    if let Some(new_path) = &raw_delta.new_path {
                        let change = restore_path(
                            &repo,
                            repo_path,
                            &head_tree,
                            &target_tree,
                            &mut tree_update,
                            new_path,
                            raw_delta.new_oid,
                        )?;

                        file_changes.extend(change);
                    }
                }

                // The commit deleted this file → at that point in time it was
                // gone, so rolling back deletes it again.
                Delta::Deleted => {
                    if let Some(old_path) = &raw_delta.old_path {
                        let change =
                            remove_path(repo_path, &head_tree, &mut tree_update, old_path)?;

                        file_changes.extend(change);
                    }
                }

                // The commit renamed old → new. Rolling back re-applies that
                // rename; when HEAD still has the file under its old name and
                // nothing at the new one, it is reported as a single move so
                // downstream receivers keep the entity's identity instead of
                // seeing a delete followed by an unrelated create.
                Delta::Renamed => {
                    if let (Some(old_path), Some(new_path)) =
                        (&raw_delta.old_path, &raw_delta.new_path)
                    {
                        let old_in_head =
                            GitUtils::blob_oid_in_tree(&head_tree, &old_path.to_string_lossy())
                                .is_some();
                        let new_in_head =
                            GitUtils::blob_oid_in_tree(&head_tree, &new_path.to_string_lossy())
                                .is_some();

                        if old_in_head && !new_in_head {
                            tracing::trace!(
                                tenant_id = %tenant_id,
                                sha = %sha,
                                from_path = %old_path.display(),
                                to_path = %new_path.display(),
                                "rollback: re-applying rename"
                            );

                            let content = GitUtils::blob_content_from_tree(
                                &repo,
                                &target_tree,
                                &new_path.to_string_lossy(),
                            )?;

                            let absolute_old = repo_path.join(old_path);
                            let absolute_new = repo_path.join(new_path);

                            match std::fs::remove_file(&absolute_old) {
                                Ok(()) => {}
                                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                                Err(err) => return Err(AppError::Io(err)),
                            }

                            if let Some(parent_dir) = absolute_new.parent() {
                                std::fs::create_dir_all(parent_dir)?;
                            }

                            std::fs::write(&absolute_new, &content)?;

                            tree_update.remove(old_path);
                            tree_update.upsert(new_path, raw_delta.new_oid, FileMode::Blob);

                            file_changes.push(FileChange::Moved {
                                from_path: old_path.to_string_lossy().into_owned(),
                                to_path: new_path.to_string_lossy().into_owned(),
                                content,
                            });
                        } else {
                            // HEAD's shape does not match a plain rename-back
                            // (the destination is occupied, or the source is
                            // already gone), so each side is settled on its
                            // own terms.
                            let restored = restore_path(
                                &repo,
                                repo_path,
                                &head_tree,
                                &target_tree,
                                &mut tree_update,
                                new_path,
                                raw_delta.new_oid,
                            )?;

                            file_changes.extend(restored);

                            let removed =
                                remove_path(repo_path, &head_tree, &mut tree_update, old_path)?;

                            file_changes.extend(removed);
                        }
                    }
                }

                _ => {}
            }
        }

        // Every staged path yields exactly one change, so an empty list means
        // the repository already holds the target state: no commit, no hook.
        if file_changes.is_empty() {
            tracing::debug!(tenant_id = %tenant_id, sha = %sha, "files already in target state, skipping commit");

            return Ok((head_commit.id().to_string(), file_changes));
        }

        tracing::trace!(tenant_id = %tenant_id, sha = %sha, "building rollback tree and committing");

        let tree_id = tree_update.create_updated(&repo, &head_tree)?;
        let tree = repo.find_tree(tree_id)?;
        let signature = GitUtils::git_signature(author_name, author_email)?;

        let auto_message = format!("rollback: {}", target_commit.message().unwrap_or("unknown"));
        let message = commit_message.unwrap_or(&auto_message);

        let new_commit_oid = repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[&head_commit],
        )?;

        tracing::debug!(
            tenant_id = %tenant_id,
            rolled_back_to_sha = %sha,
            new_sha = %new_commit_oid,
            file_change_count = file_changes.len(),
            "rollback committed"
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
