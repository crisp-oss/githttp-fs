// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! File CRUD routes: listing, read, existence check, write, delete, move.
//!
//! See `routes/mod.rs` for the shared handler shape (validate → lock →
//! blocking git op → hook enqueue → maintenance arm) that every write
//! handler here follows.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

use std::collections::HashSet;

use crate::{
    error::AppError,
    git,
    hooks::HookJob,
    routes::AuthorRequest,
    seek::{SeekBody, SeekFilter, SeekOptions},
    state::AppState,
    util::run_blocking,
    validate,
};

/// Query parameters for the listing endpoint. All optional: the bare
/// endpoint returns page 1 of the full recursive tree.
#[derive(Deserialize)]
pub struct ListFilesQuery {
    /// Folder to root the listing at (e.g. `/docs`); repo root if omitted.
    pub prefix_path: Option<String>,
    /// How many directory levels to descend from the listing root; full
    /// recursion if omitted.
    pub maximum_depth: Option<u32>,
    /// When true, hidden entries (names starting with `.`, per the Unix
    /// convention) are included in the listing; excluded if omitted or false.
    pub include_hidden_files: Option<bool>,
    /// When set, narrows the listing to files *and directories* whose leaf
    /// name begins with the given prefix(es), compared case-insensitively (a
    /// matched directory brings its subtree along). Accepts either a bare
    /// string (a single prefix) or a JSON-array string of prefixes, e.g.
    /// `["intro", "readme"]` — an entry matches if its leaf name begins with
    /// *any* of them. Empty is rejected (`400`).
    pub file_name_starts_with: Option<String>,
    /// Lower bound (inclusive) of the created/updated date-range filter, as an
    /// RFC 3339 date-time (e.g. `2026-06-16T10:00:00Z`).
    pub include_date_from: Option<String>,
    /// Upper bound (exclusive) of the created/updated date-range filter, as an
    /// RFC 3339 date-time.
    pub include_date_to: Option<String>,
    /// Which date the range applies to: `updated` (default) or `created`.
    /// Its value is always validated, but the filter is only active — and the
    /// history walk only paid for — when at least one bound is set.
    pub include_date_type: Option<String>,
    pub page: Option<usize>,
    pub per_page: Option<usize>,
}

// Pagination bounds shared with the commits endpoint: a generous default,
// and a hard cap so a caller cannot request unbounded response sizes.
const DEFAULT_PER_PAGE: usize = 100;
const MAX_PER_PAGE: usize = 500;

/// Query parameters for the count endpoint. All optional: the bare endpoint
/// counts the full recursive tree. The first three carry the exact
/// semantics of their `ListFilesQuery` namesakes.
#[derive(Deserialize)]
pub struct CountFilesQuery {
    /// Folder to root the count at (e.g. `/docs`); repo root if omitted.
    pub prefix_path: Option<String>,
    /// How many directory levels to descend from the count root; full
    /// recursion if omitted.
    pub maximum_depth: Option<u32>,
    /// When true, hidden entries (names starting with `.`, per the Unix
    /// convention) are included in the count; excluded if omitted or false.
    pub include_hidden_files: Option<bool>,
    /// A JSON array of file extensions as a string (query parameters are
    /// strings), e.g. `["md", "mdx"]`; when set, only files carrying one of
    /// these extensions are counted. Omitted: every file counts.
    pub restrict_file_extensions: Option<String>,
}

/// Body of the batch read endpoint: the entries to read, plus an optional
/// seek window applied to every file that does not carry its own (see
/// `seek.rs` for the field formats).
#[derive(Deserialize)]
pub struct BatchReadFilesRequest {
    pub files: Vec<BatchReadFileRequest>,
    pub seek: Option<SeekBody>,
}

/// One entry of the batch read `files` array: either a bare path string,
/// or an object holding the path plus an optional per-file seek window.
/// When the per-file `seek` is set it *replaces* the request-level `seek`
/// for that file (no field-by-field merge).
#[derive(Deserialize)]
#[serde(untagged)]
pub enum BatchReadFileRequest {
    Path(String),
    Options {
        path: String,
        seek: Option<SeekBody>,
    },
}

impl BatchReadFileRequest {
    /// The raw path and optional per-file seek, whichever spelling was used.
    fn parts(&self) -> (&str, Option<&SeekBody>) {
        match self {
            Self::Path(path) => (path, None),
            Self::Options { path, seek } => (path, seek.as_ref()),
        }
    }
}

/// Query parameters for the existence endpoint.
#[derive(Deserialize)]
pub struct FileExistsQuery {
    /// When true, a path resolving to a folder counts as existing too, so the
    /// endpoint answers "is there anything at this path" rather than "is there
    /// a file at this path". Defaults to false — folders stay invisible.
    pub check_prefix_path: Option<bool>,
}

#[derive(Deserialize)]
pub struct WriteFileRequest {
    pub author: AuthorRequest,
    pub content: String,
    pub message: Option<String>,
}

#[derive(Deserialize)]
pub struct DeleteFileRequest {
    pub author: AuthorRequest,
    pub message: Option<String>,
    /// When true and the path resolves to a folder, the deletion recurses over
    /// every file beneath it, in one commit. Defaults to false, in which case
    /// a folder path is simply "not a file" and answers `404` — so this heavy,
    /// destructive mode can never be entered by accident.
    ///
    /// Lives in the body rather than the query string because it changes *what
    /// the write does*, alongside `author` and `message`, instead of shaping a
    /// read.
    pub allow_prefix_path_recurse: Option<bool>,
}

#[derive(Deserialize)]
pub struct MoveFileRequest {
    pub author: AuthorRequest,
    pub destination: String,
    pub message: Option<String>,
    /// When true and the source path resolves to a folder, the whole subtree
    /// is relocated under `destination`, in one commit. Same default and same
    /// rationale for living in the body as its `DeleteFileRequest` namesake.
    pub allow_prefix_path_recurse: Option<bool>,
}

/// Decodes the `file_name_starts_with` query value into its list of prefixes.
/// A value that looks like a JSON array (its first non-whitespace character is
/// `[`) must be a valid JSON array of strings; any other value is taken
/// verbatim as a single prefix — the original bare-string spelling, kept for
/// backward compatibility. Empty arrays and empty prefixes are rejected with
/// `400`: an empty prefix would match every entry (indistinguishable from
/// omitting the parameter), an empty array none — both only caller bugs.
fn parse_file_name_prefixes(raw: &str) -> Result<Vec<String>, AppError> {
    let prefixes = if raw.trim_start().starts_with('[') {
        serde_json::from_str::<Vec<String>>(raw).map_err(|_err| AppError::InvalidOperation {
            reason:
                "file_name_starts_with must be a string or a JSON array of strings, e.g. [\"intro\", \"readme\"]"
                    .to_string(),
        })?
    } else {
        vec![raw.to_string()]
    };

    if prefixes.is_empty() {
        return Err(AppError::InvalidOperation {
            reason: "file_name_starts_with must contain at least one prefix".to_string(),
        });
    }

    if prefixes.iter().any(|prefix| prefix.is_empty()) {
        return Err(AppError::InvalidOperation {
            reason: "file_name_starts_with must not be empty".to_string(),
        });
    }

    Ok(prefixes)
}

/// Parses a single RFC 3339 date-time query value into UTC, mapping any
/// malformed value to a `400` naming the parameter (strict validation — only
/// the RFC 3339 spelling is accepted).
fn parse_rfc3339(raw: &str, parameter: &str) -> Result<DateTime<Utc>, AppError> {
    DateTime::parse_from_rfc3339(raw)
        .map(|date_time| date_time.with_timezone(&Utc))
        .map_err(|_err| AppError::InvalidOperation {
            reason: format!(
                "{} must be an RFC 3339 date-time, e.g. 2026-06-16T10:00:00Z",
                parameter
            ),
        })
}

/// Builds the optional created/updated date-range filter from the three query
/// parameters. `include_date_type` is validated strictly whenever present
/// (only `updated` or `created`, defaulting to `updated`), but the filter is
/// left inactive — so the listing keeps its cheap tree-only fast path and no
/// commit history is walked — unless at least one date bound is given. When
/// both bounds are present, `from` must be strictly before `to` (the window
/// is `[from, to)`, so equal bounds would select nothing).
fn parse_date_filter(
    from: Option<&str>,
    to: Option<&str>,
    date_type: Option<&str>,
) -> Result<Option<git::DateFilter>, AppError> {
    let kind = match date_type {
        None | Some("updated") => git::DateKind::Updated,
        Some("created") => git::DateKind::Created,
        Some(other) => {
            return Err(AppError::InvalidOperation {
                reason: format!(
                    "include_date_type must be 'updated' or 'created': {}",
                    other
                ),
            })
        }
    };

    let from = from
        .map(|raw| parse_rfc3339(raw, "include_date_from"))
        .transpose()?;
    let to = to
        .map(|raw| parse_rfc3339(raw, "include_date_to"))
        .transpose()?;

    // No bound set: the type is validated but the filter stays inactive, so
    // the listing avoids the history walk entirely.
    if from.is_none() && to.is_none() {
        return Ok(None);
    }

    if let (Some(from), Some(to)) = (from, to) {
        if from >= to {
            return Err(AppError::InvalidOperation {
                reason: "include_date_from must be strictly before include_date_to".to_string(),
            });
        }
    }

    Ok(Some(git::DateFilter { from, to, kind }))
}

/// GET /:collection_id/:tenant_id/files
/// Returns the repository contents as a recursive file tree.
/// Accepts an optional `prefix_path` query parameter (e.g. `?prefix_path=/docs`) to scope
/// the listing to a specific sub-directory. The path must be a folder and must
/// not escape the repository root (`..' components are rejected).
///
/// Pagination is *parent-based*: `page`/`per_page` window over the
/// root-level entries of the listing, each carrying its full subtree.
/// Combined with `maximum_depth` this lets clients bound response size on
/// arbitrarily large repositories.
pub async fn list_files(
    State(state): State<AppState>,
    Path((collection_id, tenant_id)): Path<(String, String)>,
    Query(query): Query<ListFilesQuery>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    // An empty sanitised prefix ("/" or "") means "repo root", which is the
    // same as no prefix at all — normalise it to None here so the git layer
    // only ever sees a meaningful prefix.
    let path_prefix: Option<String> = query
        .prefix_path
        .as_deref()
        .map(validate::folder_path)
        .transpose()?
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string());

    // maximum_depth=0 would mean "list nothing", which is more likely a
    // caller bug than an intent — reject it explicitly.
    let maximum_depth: Option<usize> = match query.maximum_depth {
        Some(0) => {
            return Err(AppError::InvalidOperation {
                reason: "maximum_depth must be at least 1".to_string(),
            })
        }
        Some(d) => Some(d as usize),
        None => None,
    };

    let include_hidden_files = query.include_hidden_files.unwrap_or(false);

    // Accepts either a bare string (a single prefix) or a JSON-array string
    // of prefixes (query parameters are strings, so an array travels the same
    // way `seek_from_line_starts_with` does), so several prefixes can be
    // searched at once — an entry matches if its leaf name begins with *any*
    // of them.
    let file_name_starts_with: Option<Vec<String>> = query
        .file_name_starts_with
        .as_deref()
        .map(parse_file_name_prefixes)
        .transpose()?;

    let date_filter = parse_date_filter(
        query.include_date_from.as_deref(),
        query.include_date_to.as_deref(),
        query.include_date_type.as_deref(),
    )?;

    let page = query.page.unwrap_or(1).max(1);
    let per_page = query
        .per_page
        .unwrap_or(DEFAULT_PER_PAGE)
        .clamp(1, MAX_PER_PAGE);

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, path_prefix = ?path_prefix, maximum_depth = ?maximum_depth, include_hidden_files = include_hidden_files, file_name_starts_with = ?file_name_starts_with, date_filter = ?date_filter, page = page, per_page = per_page, "handling list files request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let tenant_id_for_task = tenant_id.clone();

    let (tree, has_more) = run_blocking(move || {
        git::GitFiles::list_files(
            &repo_path,
            &tenant_id_for_task,
            path_prefix.as_deref(),
            maximum_depth,
            include_hidden_files,
            file_name_starts_with.as_deref(),
            date_filter,
            page,
            per_page,
        )
    })
    .await?;

    tracing::debug!(tenant_id = %tenant_id, page = page, returned = tree.len(), has_more = has_more, "list files tree response ready");

    Ok(Json(json!({
        "page": page,
        "per_page": per_page,
        "has_more": has_more,
        "files": tree,
    })))
}

/// GET /:collection_id/:tenant_id/count/files
/// Returns file and directory count statistics for the repository.
///
/// `prefix_path`, `maximum_depth` and `include_hidden_files` scope the
/// count exactly like the listing endpoint (sub-directory root, depth
/// bound, hidden filter). `restrict_file_extensions` — a JSON-array string,
/// same wire spelling as the `seek_*` prefix lists — narrows the file count
/// to files carrying one of the given extensions, compared
/// case-insensitively; directories are counted regardless.
pub async fn count_files(
    State(state): State<AppState>,
    Path((collection_id, tenant_id)): Path<(String, String)>,
    Query(query): Query<CountFilesQuery>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    // An empty sanitised prefix ("/" or "") means "repo root", which is the
    // same as no prefix at all — normalise it to None here so the git layer
    // only ever sees a meaningful prefix.
    let path_prefix: Option<String> = query
        .prefix_path
        .as_deref()
        .map(validate::folder_path)
        .transpose()?
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string());

    // maximum_depth=0 would mean "count nothing", which is more likely a
    // caller bug than an intent — reject it explicitly.
    let maximum_depth: Option<usize> = match query.maximum_depth {
        Some(0) => {
            return Err(AppError::InvalidOperation {
                reason: "maximum_depth must be at least 1".to_string(),
            })
        }
        Some(d) => Some(d as usize),
        None => None,
    };

    let include_hidden_files = query.include_hidden_files.unwrap_or(false);

    let restrict_file_extensions: Option<Vec<String>> = query
        .restrict_file_extensions
        .as_deref()
        .map(parse_restrict_file_extensions)
        .transpose()?;

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, path_prefix = ?path_prefix, maximum_depth = ?maximum_depth, include_hidden_files = include_hidden_files, restrict_file_extensions = ?restrict_file_extensions, "handling count files request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let tenant_id_for_task = tenant_id.clone();

    let counts = run_blocking(move || {
        git::GitFiles::count_files(
            &repo_path,
            &tenant_id_for_task,
            path_prefix.as_deref(),
            maximum_depth,
            include_hidden_files,
            restrict_file_extensions.as_deref(),
        )
    })
    .await?;

    tracing::debug!(tenant_id = %tenant_id, files = counts.files, directories = counts.directories, "count files response ready");

    Ok(Json(json!({
        "files": counts.files,
        "directories": counts.directories,
    })))
}

/// Decodes and validates the `restrict_file_extensions` query parameter
/// from its JSON-array-string spelling. Entries are normalised by trimming
/// leading dots (`".md"` and `"md"` name the same extension); a non-array
/// value, an empty array, or an entry left empty after trimming are all
/// caller bugs and rejected with a `400`.
fn parse_restrict_file_extensions(raw: &str) -> Result<Vec<String>, AppError> {
    let extensions: Vec<String> =
        serde_json::from_str(raw).map_err(|_err| AppError::InvalidOperation {
            reason:
                "restrict_file_extensions must be a JSON array of strings, e.g. [\"md\", \"mdx\"]"
                    .to_string(),
        })?;

    if extensions.is_empty() {
        return Err(AppError::InvalidOperation {
            reason: "restrict_file_extensions must contain at least one extension".to_string(),
        });
    }

    let normalized: Vec<String> = extensions
        .iter()
        .map(|extension| extension.trim_start_matches('.').to_string())
        .collect();

    if normalized.iter().any(|extension| extension.is_empty()) {
        return Err(AppError::InvalidOperation {
            reason: "restrict_file_extensions extensions must not be empty".to_string(),
        });
    }

    Ok(normalized)
}

/// GET /:collection_id/:tenant_id/files/*path
/// Returns the file content and path as JSON.
///
/// Optional `seek_*` query parameters (`seek_from_line_starts_with`,
/// `seek_to_line_starts_with`, `seek_lines_maximum`) narrow `content` to a
/// line window — see `seek.rs` for the exact semantics and accepted
/// formats. The seek runs inside the git read so it can scan the blob as a
/// line stream instead of decoding it whole.
pub async fn read_file(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, file_path)): Path<(String, String, String)>,
    Query(seek): Query<SeekOptions>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();
    let file_path = validate::file_path(&file_path)?.to_string();

    let seek = seek.parse()?;

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, path = %file_path, seek = ?seek, "handling read file request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let file_path_for_task = file_path.clone();
    let tenant_id_for_task = tenant_id.clone();

    let content = run_blocking(move || {
        git::GitFiles::read_file(&repo_path, &tenant_id_for_task, &file_path_for_task, &seek)
    })
    .await?;

    Ok(Json(json!({
        "path": file_path,
        "content": content,
    })))
}

/// POST /:collection_id/:tenant_id/batch/files/read
/// Reads several files in one request. The response array is index-aligned
/// with the request's `files` array: each slot is either the same
/// `{ path, content }` object the single read route returns, or `null`
/// when that path does not exist (or is a folder). Each entry is a bare
/// path string or a `{ path, seek? }` object; an optional request-level
/// `seek` object applies the same line window to every file, and an
/// entry-level `seek` replaces it for that file.
///
/// The whole request is rejected upfront (400) when a path is invalid,
/// paths are duplicated, or more than `limits.batch_read_maximum_files`
/// paths are asked for — a safety cap against unbounded response sizes.
/// A file that exists but cannot be represented (invalid UTF-8) fails the
/// whole batch with a 422, so `null` strictly means "not found".
pub async fn batch_read_files(
    State(state): State<AppState>,
    Path((collection_id, tenant_id)): Path<(String, String)>,
    Json(body): Json<BatchReadFilesRequest>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    if body.files.is_empty() {
        return Err(AppError::InvalidOperation {
            reason: "files must contain at least one path".to_string(),
        });
    }

    let maximum_files = state.config.limits.batch_read_maximum_files;

    if body.files.len() > maximum_files {
        return Err(AppError::InvalidOperation {
            reason: format!(
                "files must not contain more than {} paths ({} requested)",
                maximum_files,
                body.files.len()
            ),
        });
    }

    let global_seek = body.seek.unwrap_or_default().parse()?;

    // Sanitise every path with the same rules as the single read route,
    // *before* the uniqueness check so that two spellings of the same file
    // (e.g. `a.md` and `/a.md`) are caught as duplicates. Each entry's
    // effective seek is resolved here too: its own window when it carries
    // one, the request-level window otherwise.
    let mut seen_paths = HashSet::new();
    let mut file_reads: Vec<(String, SeekFilter)> = Vec::with_capacity(body.files.len());

    for (index, entry) in body.files.iter().enumerate() {
        let (raw_path, entry_seek) = entry.parts();

        let file_path = validate::file_path(raw_path)?.to_string();

        if !seen_paths.insert(file_path.clone()) {
            return Err(AppError::InvalidOperation {
                reason: format!("files must be unique: '{}' is requested twice", file_path),
            });
        }

        let seek = match entry_seek {
            None => global_seek.clone(),

            // Prefix validation errors with the entry's index so the caller
            // knows which per-file seek is malformed (the request-level one
            // reports without a prefix).
            Some(entry_seek) => entry_seek.parse().map_err(|err| match err {
                AppError::InvalidOperation { reason } => AppError::InvalidOperation {
                    reason: format!("files[{}]: {}", index, reason),
                },
                other => other,
            })?,
        };

        file_reads.push((file_path, seek));
    }

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, count = file_reads.len(), seek = ?global_seek, "handling batch read files request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let file_reads_for_task = file_reads.clone();

    let contents = run_blocking(move || {
        git::GitFiles::batch_read_files(&repo_path, &tenant_id, &file_reads_for_task)
    })
    .await?;

    let files: Vec<_> = file_reads
        .iter()
        .zip(contents)
        .map(|((path, _seek), content)| {
            content.map(|content| json!({ "path": path, "content": content }))
        })
        .collect();

    Ok(Json(json!({ "files": files })))
}

/// HEAD /:collection_id/:tenant_id/files/*path
/// Returns 200 with no body when the file exists in HEAD, 404 otherwise.
/// Cheaper than GET as the blob content is never loaded.
///
/// With `?check_prefix_path=true` a folder at that path counts as existing
/// too — the answer becomes "is there anything here", which is what a caller
/// about to recurse a delete or a move needs to know. Both kinds are read
/// from HEAD's tree entry, so the check stays a single tree lookup either way.
pub async fn file_exists(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, file_path)): Path<(String, String, String)>,
    Query(query): Query<FileExistsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    let check_prefix_path = query.check_prefix_path.unwrap_or(false);

    // Folders are naturally spelled with a trailing slash, so tolerate one
    // once folders are in scope at all.
    let file_path = if check_prefix_path {
        validate::file_or_folder_path(&file_path)?
    } else {
        validate::file_path(&file_path)?
    }
    .to_string();

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, path = %file_path, check_prefix_path = check_prefix_path, "handling file existence request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let file_path_for_task = file_path.clone();

    let kind =
        run_blocking(move || git::GitFiles::path_kind(&repo_path, &tenant_id, &file_path_for_task))
            .await?;

    match kind {
        git::PathKind::File => Ok(StatusCode::OK),
        git::PathKind::Directory if check_prefix_path => Ok(StatusCode::OK),
        _ => Err(AppError::FileNotFound { path: file_path }),
    }
}

/// PUT /:collection_id/:tenant_id/files/*path
/// Creates or updates a file, commits the change, and fires a hook.
///
/// PUT is idempotent by design: the caller does not need to know whether the
/// file exists. The server decides created-vs-updated from HEAD's tree and
/// reflects that in both the auto-generated commit message and the hook
/// event kind. Idempotency extends to content: re-PUTting a file with the
/// exact content HEAD already holds creates no commit and fires no hook —
/// the response carries HEAD's sha instead. This is also the endpoint that
/// lazily initialises a tenant repository on first use — there is no
/// explicit "create tenant" call.
pub async fn write_file(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, file_path)): Path<(String, String, String)>,
    Json(body): Json<WriteFileRequest>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();
    let file_path = validate::file_path(&file_path)?.to_string();

    validate::file_extension(
        &file_path,
        state.config.limits.allowed_extensions.as_deref(),
    )?;

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, path = %file_path, "handling write file request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let lock_key = format!("{}/{}", collection_id, tenant_id);
    let lock = state.get_repo_lock(&lock_key);
    let _lock_guard = lock.lock().await;

    let WriteFileRequest {
        author,
        content,
        message,
    } = body;

    let repo_path_for_maintenance = repo_path.clone();

    let (commit_sha, file_change) = run_blocking(move || {
        git::GitFiles::write_file(
            &repo_path,
            &file_path,
            &content,
            message.as_deref(),
            &author.name,
            &author.email,
        )
    })
    .await?;

    // A `None` change means the content already matched HEAD: no commit was
    // created, so there is nothing for downstream systems to sync and no new
    // objects for maintenance to consolidate. The returned sha is HEAD's —
    // the commit whose tree already contains exactly this content.
    let Some(file_change) = file_change else {
        tracing::debug!(tenant_id = %tenant_id, sha = %commit_sha, "content unchanged, no commit created");

        return Ok((StatusCode::OK, Json(json!({ "commit_sha": commit_sha }))));
    };

    tracing::debug!(tenant_id = %tenant_id, sha = %commit_sha, "file write committed, enqueuing hook delivery");

    // Enqueued while the tenant write lock is still held, so per-tenant hook
    // order always matches commit order.
    state.hook_queue.enqueue(
        &lock_key,
        HookJob {
            collection_id,
            tenant_id,
            commit_sha: commit_sha.clone(),
            committed_at: Utc::now(),
            file_changes: vec![file_change],
        },
    );

    state
        .maintenance
        .schedule(&lock_key, repo_path_for_maintenance, lock.clone());

    Ok((StatusCode::OK, Json(json!({ "commit_sha": commit_sha }))))
}

/// DELETE /:collection_id/:tenant_id/files/*path
/// Deletes a file, commits the removal, and fires a hook.
///
/// With `allow_prefix_path_recurse: true` in the body the path may also name a
/// folder, in which case every file beneath it is removed in one commit and
/// one `file.deleted` hook fires per file. Without the opt-in a folder path is
/// simply not a file and answers `404`, so this destructive mode is never
/// entered by accident.
pub async fn delete_file(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, file_path)): Path<(String, String, String)>,
    Json(body): Json<DeleteFileRequest>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    let allow_prefix_path_recurse = body.allow_prefix_path_recurse.unwrap_or(false);

    // Folders are naturally spelled with a trailing slash, so tolerate one
    // once folders are in scope at all.
    let file_path = if allow_prefix_path_recurse {
        validate::file_or_folder_path(&file_path)?
    } else {
        validate::file_path(&file_path)?
    }
    .to_string();

    tracing::debug!(collection_id = %collection_id, tenant_id = %tenant_id, path = %file_path, allow_prefix_path_recurse = allow_prefix_path_recurse, "handling delete file request");

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let lock_key = format!("{}/{}", collection_id, tenant_id);
    let lock = state.get_repo_lock(&lock_key);
    let _lock_guard = lock.lock().await;

    let DeleteFileRequest {
        author,
        message,
        allow_prefix_path_recurse: _,
    } = body;

    let repo_path_for_maintenance = repo_path.clone();

    // Which of the two operations to run is decided from HEAD's tree, under
    // the write lock, so the classification cannot go stale before the commit
    // it drives. The lookup is only paid for when the caller opted in — with
    // recursion off the single-file path is unchanged, folder and all.
    let recurse_directory = if allow_prefix_path_recurse {
        let repo_path_for_kind = repo_path.clone();
        let tenant_id_for_kind = tenant_id.clone();
        let file_path_for_kind = file_path.clone();

        let kind = run_blocking(move || {
            git::GitFiles::path_kind(
                &repo_path_for_kind,
                &tenant_id_for_kind,
                &file_path_for_kind,
            )
        })
        .await?;

        kind == git::PathKind::Directory
    } else {
        false
    };

    let tenant_id_for_task = tenant_id.clone();

    let (commit_sha, file_changes) = if recurse_directory {
        run_blocking(move || {
            git::GitFiles::delete_directory(
                &repo_path,
                &tenant_id_for_task,
                &file_path,
                message.as_deref(),
                &author.name,
                &author.email,
            )
        })
        .await?
    } else {
        let (commit_sha, file_change) = run_blocking(move || {
            git::GitFiles::delete_file(
                &repo_path,
                &tenant_id_for_task,
                &file_path,
                message.as_deref(),
                &author.name,
                &author.email,
            )
        })
        .await?;

        (commit_sha, vec![file_change])
    };

    // An empty change list means no commit was created (only reachable on the
    // recursive path, for a folder holding nothing this API represents), so
    // there is nothing to sync and no new objects to consolidate.
    if file_changes.is_empty() {
        tracing::debug!(tenant_id = %tenant_id, sha = %commit_sha, "nothing to delete, no commit created");

        return Ok((StatusCode::OK, Json(json!({ "commit_sha": commit_sha }))));
    }

    tracing::debug!(tenant_id = %tenant_id, sha = %commit_sha, file_count = file_changes.len(), "file deletion committed, enqueuing hook delivery");

    // Enqueued while the tenant write lock is still held, so per-tenant hook
    // order always matches commit order.
    state.hook_queue.enqueue(
        &lock_key,
        HookJob {
            collection_id,
            tenant_id,
            commit_sha: commit_sha.clone(),
            committed_at: Utc::now(),
            file_changes,
        },
    );

    state
        .maintenance
        .schedule(&lock_key, repo_path_for_maintenance, lock.clone());

    Ok((StatusCode::OK, Json(json!({ "commit_sha": commit_sha }))))
}

/// POST /:collection_id/:tenant_id/files/*path/move
/// Moves/renames a file to a new path in a single atomic commit, fires a
/// single hook with both the old and new paths so the receiver can
/// correlate the rename without losing attached metadata.
///
/// Axum cannot match a fixed suffix after a wildcard segment, so this handler
/// is registered on POST `/*path` and enforces the `/move` suffix itself.
///
/// With `allow_prefix_path_recurse: true` in the body the source may also name
/// a folder, in which case the whole folder is relocated in one commit and one
/// `file.moved` hook fires per file — every file keeping its own leaf name, so
/// downstream entity identity survives. Without the opt-in a folder source is
/// simply not a file and answers `404`.
pub async fn move_file(
    State(state): State<AppState>,
    Path((collection_id, tenant_id, raw_path)): Path<(String, String, String)>,
    Json(body): Json<MoveFileRequest>,
) -> Result<impl IntoResponse, AppError> {
    let collection_id = validate::collection_id(&collection_id)?.to_string();
    let tenant_id = validate::tenant_id(&tenant_id)?.to_string();

    // Enforce that the URL ends with /move — anything else on POST is not found.
    let from_path_raw = raw_path
        .strip_suffix("/move")
        .ok_or_else(|| AppError::InvalidPath {
            reason: "POST on a file path must end with /move".to_string(),
        })?;

    let allow_prefix_path_recurse = body.allow_prefix_path_recurse.unwrap_or(false);

    // Folders are naturally spelled with a trailing slash, so tolerate one on
    // both sides once folders are in scope at all.
    let (from_path, to_path) = if allow_prefix_path_recurse {
        (
            validate::file_or_folder_path(from_path_raw)?.to_string(),
            validate::file_or_folder_path(&body.destination)?.to_string(),
        )
    } else {
        (
            validate::file_path(from_path_raw)?.to_string(),
            validate::file_path(&body.destination)?.to_string(),
        )
    };

    // Only the destination is checked against the whitelist: files written
    // before the whitelist was configured must remain movable. With recursion
    // enabled the check is deferred until the source kind is known — a folder
    // destination carries no extension of its own, and the files inside keep
    // their leaf names, so there is nothing there for the whitelist to guard.
    if !allow_prefix_path_recurse {
        validate::file_extension(&to_path, state.config.limits.allowed_extensions.as_deref())?;
    }

    tracing::debug!(
        collection_id = %collection_id,
        tenant_id = %tenant_id,
        from_path = %from_path,
        to_path = %to_path,
        allow_prefix_path_recurse = allow_prefix_path_recurse,
        "handling move file request"
    );

    let repo_path = state
        .config
        .server
        .repos_path
        .join(&collection_id)
        .join(&tenant_id);

    let lock_key = format!("{}/{}", collection_id, tenant_id);
    let lock = state.get_repo_lock(&lock_key);
    let _lock_guard = lock.lock().await;

    let MoveFileRequest {
        author,
        destination: _,
        message,
        allow_prefix_path_recurse: _,
    } = body;

    let repo_path_for_maintenance = repo_path.clone();

    // Which of the two operations to run is decided from HEAD's tree, under
    // the write lock, so the classification cannot go stale before the commit
    // it drives. The lookup is only paid for when the caller opted in.
    let recurse_directory = if allow_prefix_path_recurse {
        let repo_path_for_kind = repo_path.clone();
        let tenant_id_for_kind = tenant_id.clone();
        let from_path_for_kind = from_path.clone();

        let kind = run_blocking(move || {
            git::GitFiles::path_kind(
                &repo_path_for_kind,
                &tenant_id_for_kind,
                &from_path_for_kind,
            )
        })
        .await?;

        kind == git::PathKind::Directory
    } else {
        false
    };

    // The source turned out to be a file after all (or nothing at all), so
    // the destination is a file path and the whitelist applies as usual.
    if allow_prefix_path_recurse && !recurse_directory {
        validate::file_extension(&to_path, state.config.limits.allowed_extensions.as_deref())?;
    }

    let tenant_id_for_task = tenant_id.clone();

    let (commit_sha, file_changes) = if recurse_directory {
        run_blocking(move || {
            git::GitFiles::move_directory(
                &repo_path,
                &tenant_id_for_task,
                &from_path,
                &to_path,
                message.as_deref(),
                &author.name,
                &author.email,
            )
        })
        .await?
    } else {
        let (commit_sha, file_change) = run_blocking(move || {
            git::GitFiles::move_file(
                &repo_path,
                &tenant_id_for_task,
                &from_path,
                &to_path,
                message.as_deref(),
                &author.name,
                &author.email,
            )
        })
        .await?;

        (commit_sha, vec![file_change])
    };

    // An empty change list means no commit was created (only reachable on the
    // recursive path, for a folder holding nothing this API represents), so
    // there is nothing to sync and no new objects to consolidate.
    if file_changes.is_empty() {
        tracing::debug!(tenant_id = %tenant_id, sha = %commit_sha, "nothing to move, no commit created");

        return Ok((StatusCode::OK, Json(json!({ "commit_sha": commit_sha }))));
    }

    tracing::debug!(tenant_id = %tenant_id, sha = %commit_sha, file_count = file_changes.len(), "file move committed, enqueuing hook delivery");

    // Enqueued while the tenant write lock is still held, so per-tenant hook
    // order always matches commit order.
    state.hook_queue.enqueue(
        &lock_key,
        HookJob {
            collection_id,
            tenant_id,
            commit_sha: commit_sha.clone(),
            committed_at: Utc::now(),
            file_changes,
        },
    );

    state
        .maintenance
        .schedule(&lock_key, repo_path_for_maintenance, lock.clone());

    Ok((StatusCode::OK, Json(json!({ "commit_sha": commit_sha }))))
}
