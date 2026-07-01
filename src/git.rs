// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

use chrono::{DateTime, Utc};
use git2::{Delta, DiffFindOptions, DiffFormat, DiffOptions, Oid, Repository, Signature, Sort};
use serde::Serialize;
use std::collections::BTreeMap;
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
        size: usize,
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

        let empty_tree_id = {
            let mut index = repo.index()?;
            index.write_tree()?
        };
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

    /// Reads a blob's content from `tree` at `file_path` and decodes it as UTF-8.
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

    /// Builds a recursive `TreeNode` tree from a flat list of (path, size) pairs.
    /// Directories are sorted before files at each level; entries within each
    /// group are sorted alphabetically. Uses `BTreeMap` to keep insertion order
    /// deterministic regardless of the order git walks the tree.
    fn build_tree(flat: Vec<(String, usize)>) -> Vec<TreeNode> {
        enum NodeBuilder {
            File(usize),
            Dir(BTreeMap<String, NodeBuilder>),
        }

        fn insert(dir: &mut BTreeMap<String, NodeBuilder>, components: &[&str], size: usize) {
            match components {
                [] => {}
                [name] => {
                    dir.insert(name.to_string(), NodeBuilder::File(size));
                }
                [name, rest @ ..] => {
                    let child = dir
                        .entry(name.to_string())
                        .or_insert_with(|| NodeBuilder::Dir(BTreeMap::new()));

                    if let NodeBuilder::Dir(children) = child {
                        insert(children, rest, size);
                    }
                }
            }
        }

        fn convert(name: String, node: NodeBuilder) -> TreeNode {
            match node {
                NodeBuilder::File(size) => TreeNode::File { name, size },
                NodeBuilder::Dir(children) => {
                    // Directories first, then files; each group sorted by name.
                    let mut dirs: Vec<TreeNode> = Vec::new();
                    let mut files: Vec<TreeNode> = Vec::new();

                    for (child_name, child_node) in children {
                        match child_node {
                            NodeBuilder::Dir(_) => dirs.push(convert(child_name, child_node)),
                            NodeBuilder::File(_) => files.push(convert(child_name, child_node)),
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

        for (path, size) in flat {
            let components: Vec<&str> = path.split('/').collect();
            insert(&mut root, &components, size);
        }

        let mut dirs: Vec<TreeNode> = Vec::new();
        let mut files: Vec<TreeNode> = Vec::new();

        for (name, node) in root {
            match node {
                NodeBuilder::Dir(_) => dirs.push(convert(name, node)),
                NodeBuilder::File(_) => files.push(convert(name, node)),
            }
        }

        dirs.into_iter().chain(files).collect()
    }
}

// ---------------------------------------------------------------------------
// GitLocks — stale lock file detection and cleanup
// ---------------------------------------------------------------------------

pub struct GitLocks;

impl GitLocks {
    /// Removes `.git/index.lock` if it is older than 30 seconds.
    /// A stale lock is left behind when a process is killed mid-operation.
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
// GitFiles — file CRUD operations
// ---------------------------------------------------------------------------

pub struct GitFiles;

impl GitFiles {
    pub fn list_files(
        repo_path: &Path,
        tenant_id: &str,
        path_prefix: Option<&str>,
    ) -> Result<Vec<TreeNode>, AppError> {
        tracing::debug!(tenant_id = %tenant_id, path_prefix = ?path_prefix, "listing files");

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;
        let head_commit = repo.head()?.peel_to_commit()?;

        tracing::trace!(tenant_id = %tenant_id, head_sha = %head_commit.id(), "resolved HEAD for file listing");

        let head_tree = head_commit.tree()?;

        // When a prefix is given, only entries whose path starts with `prefix/`
        // are kept, and the prefix is stripped before the tree is built so the
        // result is rooted at the requested folder.
        let prefix_filter: Option<String> = path_prefix
            .filter(|p| !p.is_empty())
            .map(|p| format!("{}/", p));

        let mut flat: Vec<(String, usize)> = Vec::new();

        head_tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if entry.kind() != Some(git2::ObjectType::Blob) {
                return git2::TreeWalkResult::Ok;
            }

            let name = entry.name().unwrap_or("");
            let path = format!("{}{}", root, name);

            if let Some(ref prefix) = prefix_filter {
                if !path.starts_with(prefix.as_str()) {
                    return git2::TreeWalkResult::Ok;
                }
            }

            let size = repo
                .find_blob(entry.id())
                .map(|blob| blob.size())
                .unwrap_or(0);

            tracing::trace!(tenant_id = %tenant_id, path = %path, size = size, "indexed file entry");

            flat.push((path, size));

            git2::TreeWalkResult::Ok
        })?;

        tracing::debug!(tenant_id = %tenant_id, count = flat.len(), "file listing complete, building tree");

        let flat = match prefix_filter {
            Some(ref prefix) => flat
                .into_iter()
                .map(|(path, size)| (path[prefix.len()..].to_string(), size))
                .collect(),
            None => flat,
        };

        Ok(GitUtils::build_tree(flat))
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

    /// Writes a file to disk, stages it, and creates a commit.
    /// Returns the commit SHA and the type of change (created vs updated).
    pub fn write_file(
        repo_path: &Path,
        file_path: &str,
        content: &str,
        commit_message: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<(String, FileChange), AppError> {
        tracing::debug!(path = %file_path, author_name = %author_name, author_email = %author_email, "writing file");

        GitLocks::cleanup_stale_index_lock(repo_path)?;

        let repo = GitUtils::open_or_init_repo(repo_path, author_name, author_email)?;

        let absolute_path = repo_path.join(file_path);
        let is_new_file = !absolute_path.exists();

        tracing::debug!(path = %file_path, is_new_file = is_new_file, "staging file write");

        if let Some(parent_dir) = absolute_path.parent() {
            std::fs::create_dir_all(parent_dir)?;
        }

        std::fs::write(&absolute_path, content)?;

        let mut index = repo.index()?;

        tracing::trace!(path = %file_path, "adding path to git index");

        index.add_path(Path::new(file_path))?;
        index.write()?;

        let tree_id = index.write_tree()?;
        let tree = repo.find_tree(tree_id)?;
        let parent_commit = repo.head()?.peel_to_commit()?;
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
    pub fn delete_file(
        repo_path: &Path,
        tenant_id: &str,
        file_path: &str,
        commit_message: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<(String, FileChange), AppError> {
        tracing::debug!(tenant_id = %tenant_id, path = %file_path, author_name = %author_name, author_email = %author_email, "deleting file");

        GitLocks::cleanup_stale_index_lock(repo_path)?;

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;

        let absolute_path = repo_path.join(file_path);

        if !absolute_path.exists() {
            tracing::debug!(tenant_id = %tenant_id, path = %file_path, "file not found for deletion");

            return Err(AppError::FileNotFound {
                path: file_path.to_string(),
            });
        }

        let mut index = repo.index()?;

        // Remove from the index first; if the path was never tracked, surface that
        // before we touch the working tree.
        tracing::trace!(tenant_id = %tenant_id, path = %file_path, "removing path from git index");

        index.remove_path(Path::new(file_path))?;

        std::fs::remove_file(&absolute_path)?;

        index.write()?;

        let tree_id = index.write_tree()?;
        let tree = repo.find_tree(tree_id)?;
        let parent_commit = repo.head()?.peel_to_commit()?;
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

        GitLocks::cleanup_stale_index_lock(repo_path)?;

        let repo = GitUtils::open_tenant_repo(repo_path, tenant_id)?;

        if from_path == to_path {
            tracing::debug!(tenant_id = %tenant_id, path = %from_path, "move rejected: source and destination are identical");

            return Err(AppError::InvalidOperation {
                reason: "destination must differ from source path".to_string(),
            });
        }

        let absolute_from = repo_path.join(from_path);
        let absolute_to = repo_path.join(to_path);

        if !absolute_from.exists() {
            tracing::debug!(tenant_id = %tenant_id, from_path = %from_path, "source file not found for move");

            return Err(AppError::FileNotFound {
                path: from_path.to_string(),
            });
        }

        // Refuse to clobber an existing destination — the user must delete first.
        if absolute_to.exists() {
            tracing::debug!(tenant_id = %tenant_id, to_path = %to_path, "move rejected: destination already exists");

            return Err(AppError::InvalidOperation {
                reason: format!("destination already exists: {}", to_path),
            });
        }

        if let Some(parent_dir) = absolute_to.parent() {
            std::fs::create_dir_all(parent_dir)?;
        }

        std::fs::rename(&absolute_from, &absolute_to)?;

        let content = std::fs::read_to_string(&absolute_to)?;

        let mut index = repo.index()?;

        tracing::trace!(
            tenant_id = %tenant_id,
            from_path = %from_path,
            to_path = %to_path,
            "updating git index for move"
        );

        index.remove_path(Path::new(from_path))?;
        index.add_path(Path::new(to_path))?;
        index.write()?;

        let tree_id = index.write_tree()?;
        let tree = repo.find_tree(tree_id)?;
        let parent_commit = repo.head()?.peel_to_commit()?;
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

                let mut diff_opts = DiffOptions::new();

                diff_opts.include_untracked(false);

                let mut diff = match repo.diff_tree_to_tree(
                    Some(&parent_tree),
                    Some(&commit_tree),
                    Some(&mut diff_opts),
                ) {
                    Ok(d) => d,
                    Err(_) => continue,
                };

                let mut find_opts = DiffFindOptions::new();

                find_opts.renames(true);

                let _ = diff.find_similar(Some(&mut find_opts));

                let mut matched = false;
                let mut rename_from: Option<String> = None;

                for i in 0..diff.deltas().count() {
                    let delta = match diff.get_delta(i) {
                        Some(d) => d,
                        None => continue,
                    };

                    let old = delta
                        .old_file()
                        .path()
                        .map(|p| p.to_string_lossy().into_owned());

                    let new = delta
                        .new_file()
                        .path()
                        .map(|p| p.to_string_lossy().into_owned());

                    match delta.status() {
                        Delta::Renamed => {
                            if new.as_deref() == Some(current_path.as_str()) {
                                tracing::trace!(
                                    tenant_id = %tenant_id,
                                    sha = %commit.id(),
                                    from = ?old,
                                    to = %current_path,
                                    "rename detected, following path backward"
                                );

                                matched = true;
                                rename_from = old;
                                break;
                            }
                        }
                        Delta::Added | Delta::Modified => {
                            if new.as_deref() == Some(current_path.as_str()) {
                                matched = true;
                                break;
                            }
                        }
                        Delta::Deleted => {
                            if old.as_deref() == Some(current_path.as_str()) {
                                matched = true;
                                break;
                            }
                        }
                        _ => {}
                    }
                }

                (matched, rename_from)
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
        let mut per_file_diffs: Vec<String> = vec![String::new(); records.len()];

        diff.print(DiffFormat::Patch, |delta, _hunk, line| {
            let key = (delta.old_file().id(), delta.new_file().id());

            if let Some(idx) = records
                .iter()
                .position(|record| (record.old_oid, record.new_oid) == key)
            {
                let bucket = &mut per_file_diffs[idx];

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
    pub fn revert_commit(
        repo_path: &Path,
        tenant_id: &str,
        sha: &str,
        commit_message: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<(String, Vec<FileChange>), AppError> {
        tracing::debug!(tenant_id = %tenant_id, sha = %sha, author_name = %author_name, author_email = %author_email, "reverting commit");

        GitLocks::cleanup_stale_index_lock(repo_path)?;

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

        let mut index = repo.index()?;
        let mut file_changes: Vec<FileChange> = Vec::new();

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

                        index.remove_path(new_path)?;

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
                        index.add_path(old_path)?;

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
                        index.add_path(old_path)?;

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

                        index.remove_path(new_path)?;
                        index.add_path(old_path)?;

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

        tracing::trace!(tenant_id = %tenant_id, sha = %sha, "writing revert index and committing");

        index.write()?;

        let tree_id = index.write_tree()?;
        let tree = repo.find_tree(tree_id)?;
        let head_commit = repo.head()?.peel_to_commit()?;
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
