# githttp-fs

Git-based Content Management System served over HTTP.

## What it is

githttp-fs is a single Rust binary that wraps git repositories and exposes them as a file-system-over-HTTP API. Each tenant gets its own git repository on disk. Clients can create, read, update, delete, and move `.md`/`.mdx` files via REST. Every effective write produces a git commit (re-writing a file with unchanged content is a no-op). A configurable webhook fires after each commit so downstream systems (e.g. a read-only SQL database) can stay in sync.

Git is never exposed in the API surface — no git terminology appears in requests or responses.

## Project layout

```
src/
  main.rs          — server startup, router wiring, config loading
  config.rs        — TOML config types (ServerConfig, HooksConfig, HookEvent)
  state.rs         — AppState: Arc<Config>, reqwest::Client, per-tenant DashMap<Mutex>
  error.rs         — AppError enum with axum IntoResponse (JSON error bodies)
  git.rs           — all git2 operations (write, delete, move, list, commits, revert)
  hooks.rs         — async hook delivery with exponential backoff retry
  middleware.rs    — Bearer API key guard (axum middleware)
  seek.rs          — SeekOptions: line-based content windowing for file reads
  routes/
    mod.rs         — shared request types (AuthorRequest)
    files.rs       — GET/PUT/DELETE/POST on /:collection_id/:tenant_id/files and /:collection_id/:tenant_id/files/*path, plus POST /:collection_id/:tenant_id/batch/files/read
    commits.rs     — commit list, commit detail, revert
    tenant.rs      — DELETE /:collection_id/:tenant_id
```

## HTTP API

All routes are prefixed `/v1` and require `Authorization: Bearer <api_key>`.

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1` | Check that the API key is valid (`200` with body `{ "pong": true }`, or `401`) |
| `DELETE` | `/v1/:collection_id/:tenant_id` | Delete entire tenant repository |
| `GET` | `/v1/:collection_id/:tenant_id/files?prefix_path=&maximum_depth=&page=&per_page=` | List tracked files as a tree; optional `prefix_path` scopes the listing to a sub-directory (e.g. `?prefix_path=/docs`); optional `maximum_depth` limits how many directory levels deep the listing goes; `page`/`per_page` paginate over the root-level entries of the listing (default 100, max 500) |
| `GET` | `/v1/:collection_id/:tenant_id/files/*path?seek_from_line_starts_with=&seek_to_line_starts_with=&seek_lines_maximum=` | Read file content; optional `seek_*` parameters narrow the response to a line window (see below) |
| `POST` | `/v1/:collection_id/:tenant_id/batch/files/read` | Batch-read several files in one request, with an optional shared seek window (overridable per file); capped by `limits.batch_read_maximum_files` |
| `HEAD` | `/v1/:collection_id/:tenant_id/files/*path` | Check that a file exists (`200` or `404`, no body) |
| `PUT` | `/v1/:collection_id/:tenant_id/files/*path` | Create or update a file |
| `DELETE` | `/v1/:collection_id/:tenant_id/files/*path` | Delete a file |
| `POST` | `/v1/:collection_id/:tenant_id/files/*path/move` | Move / rename a file |
| `GET` | `/v1/:collection_id/:tenant_id/commits?page=&per_page=&file_path=` | List commits, paginated (default 100, max 500); optional `file_path` filters to commits touching that file, following renames backward |
| `GET` | `/v1/:collection_id/:tenant_id/commits/:sha` | Commit detail with per-file diffs and snapshots |
| `POST` | `/v1/:collection_id/:tenant_id/commits/:sha/revert` | Revert a commit |

### Request bodies

All write requests share a required `author` object. `message` is optional everywhere — auto-generated from the operation if omitted (e.g. `"update: docs/intro.md"`).

**PUT** — create or update a file
```json
{
  "author": { "name": "Valerian Saliou", "email": "valerian@example.com" },
  "content": "# Hello",
  "message": "optional commit message"
}
```

**DELETE** `/files/*path` — delete a file
```json
{
  "author": { "name": "Valerian Saliou", "email": "valerian@example.com" },
  "message": "optional commit message"
}
```

**POST** `/files/*path/move` — move / rename a file
```json
{
  "author": { "name": "Valerian Saliou", "email": "valerian@example.com" },
  "destination": "new/path/to/file.md",
  "message": "optional commit message"
}
```

**POST** `/commits/:sha/revert` — revert a commit
```json
{
  "author": { "name": "Valerian Saliou", "email": "valerian@example.com" },
  "message": "optional commit message"
}
```

**POST** `/batch/files/read` — batch-read several files (no `author`: reads commit nothing)
```json
{
  "files": [
    "docs/a.md",
    { "path": "docs/b.md", "seek": { "lines_maximum": 10 } }
  ],
  "seek": {
    "from_line_starts_with": ["---", "+++"],
    "to_line_starts_with": ["$seek_from_line_starts_with"],
    "lines_maximum": 20
  }
}
```

`files` is required: 1 to `limits.batch_read_maximum_files` entries (more is a `400`). Each entry is polymorphic: either a bare path string, or an object `{ "path": "...", "seek": { ... } }` with an optional per-file seek. Paths (whichever spelling) are validated with the same rules as the single read route's `*path`, and must be unique after sanitisation (duplicates are a `400`). The root-level `seek` is optional and applies the same line window to every file; an entry-level `seek`, when present, *replaces* the root-level one for that file entirely (no field-by-field merge — an entry `seek` of `{ "lines_maximum": 10 }` carries no `from`/`to` filters even if the root `seek` sets them). Both seek objects share the same format: the fields carry the exact semantics of the read route's `seek_*` query parameters, but as native JSON arrays and without the `seek_` prefix (they already nest under `seek`). Exactly like the query parameter, `to_line_starts_with` also accepts the bare string `"$seek_from_line_starts_with"` as a shorthand for an array holding only the meta operator — that is the only bare string allowed (any other must be array-wrapped, else `400`); the meta value is also usable as an array element.

### Response shapes

**GET** `/v1` — authentication check. An authenticated no-op: responds `200` when the Bearer API key is valid, `401` otherwise (via the same middleware as every other route). GET-only (no implicit HEAD).
```json
{ "pong": true }
```
Touches no tenant or repository state — safe as a credential probe or liveness check for monitors that hold the key.

Any request to the bare server root `/` (any method, no auth required) is answered with a `308 Permanent Redirect` to `/v1`.

**GET** `/files` — file listing (tree rooted at the optional `?prefix_path=` folder, or the repo root if omitted)
```json
{
  "page": 1,
  "per_page": 100,
  "has_more": false,
  "files": [
    {
      "type": "directory",
      "name": "docs",
      "children": [
        { "type": "file", "name": "intro.md" }
      ]
    },
    { "type": "file", "name": "README.md" }
  ]
}
```

Directories sort before files at every level; entries within each group sort alphabetically. File sizes are intentionally not reported — the listing is served from tree objects alone and never opens a single blob. Pagination applies to the *root-level* entries of the listing (parent-based paging): each page contains up to `per_page` root nodes with their full subtrees, and directories outside the page window are never even walked. Combine with `maximum_depth` (and `prefix_path`) to bound subtree size on huge repositories.

The `prefix_path` query parameter must be a folder path (e.g. `/docs` or `docs/sub`). Leading and trailing slashes are stripped. `..`, `.`, and `.git` components are rejected with `400`. Passing `/` or omitting the parameter lists the full repository. When `prefix_path` points to a non-existent folder the response is an empty tree.

The optional `maximum_depth` query parameter (positive integer, minimum 1) restricts the listing to that many directory levels from the listing root (after `prefix_path` is applied). `maximum_depth=1` returns only items directly in the listing root: root-level files appear as `file` nodes, any directories with content deeper than the limit appear as `directory` stubs with an empty `children` array. Omitting `maximum_depth` returns the full recursive tree. Passing `maximum_depth=0` returns `400`.

**GET** `/files/*path` — read file
```json
{
  "path": "docs/intro.md",
  "content": "# Hello world\n..."
}
```

Three optional, combinable `seek_*` query parameters narrow `content` to a line window (the response shape is unchanged; `content` simply holds only the selected lines, byte-for-byte — CRLF endings and the presence or absence of a final newline are preserved):

- `seek_from_line_starts_with` — a JSON array of non-empty strings, URL-encoded (e.g. `?seek_from_line_starts_with=["---", "+++"]`); this is the only accepted spelling — a plain string, malformed JSON, a non-string array, an empty array, or an empty prefix all return `400`. The window starts at the first line whose text starts with *any* of the prefixes (that line included; on a line matching several, the first prefix in the given order wins and is what `$seek_from_line_starts_with` resolves to). When no line matches, `content` is empty (still `200`). Omitted: the window starts at line 0.
- `seek_to_line_starts_with` — the same JSON array format, or the bare meta value `$seek_from_line_starts_with` as a shorthand for an array holding only it; anything else returns `400`. The window stops *at* the first line whose text starts with any of the prefixes, that line included as the window's last line. The search begins on the line *after* the window's first line, so the window always contains at least its first line — this is what lets the same prefix be used for both bounds (e.g. from `["---"]` to `["---"]` selects a whole front-matter block, both markers included). Every occurrence of the meta value `$seek_from_line_starts_with` inside a prefix is replaced by the `seek_from_line_starts_with` prefix that actually matched, so a multi-prefix seek can stop on the same marker it started on (e.g. from `["---", "+++"]` to `$seek_from_line_starts_with` selects a front-matter block whichever marker style the file uses); using the meta without `seek_from_line_starts_with` set returns `400`. When no line matches, the window runs to the end of the file.
- `seek_lines_maximum` — caps the window to this many lines, counted from the window's first line (line 0, or the `seek_from_line_starts_with` match if set). Must be at least 1; `0` returns `400`.

Filters resolve in that order: from → to → maximum.

**POST** `/batch/files/read` — batch read result
```json
{
  "files": [
    { "path": "docs/a.md", "content": "# A" },
    null
  ]
}
```

The `files` array is index-aligned with the request's `files` array. Each slot is either the same `{ path, content }` object the single read route returns (with the seek window applied, `path` in sanitised form), or `null` when that path does not exist in HEAD (or is a folder). `null` strictly means "not found": a file that exists but cannot be represented in JSON (invalid UTF-8) fails the whole request with a `422` naming the path. The tenant not existing at all is a `404`, as on the single read route.

**HEAD** `/files/*path` — check file existence. Responds `200` with an empty body when the file exists in the last committed state, `404` when it doesn't (including when the path points to a folder or the tenant doesn't exist). Blob content is never loaded, so this is cheaper than a GET.

**PUT / DELETE / POST move** — write result
```json
{ "commit_sha": "a3f9c1d" }
```

A PUT whose `content` is byte-for-byte identical to what the file already holds is a no-op: no commit is created, no hook fires, and the response carries the current HEAD sha (the commit whose tree already contains that exact content).

**GET** `/commits` — commit list
```json
{
  "page": 1,
  "per_page": 100,
  "has_more": false,
  "commits": [
    {
      "sha": "a3f9c1d",
      "message": "update: docs/intro.md",
      "author": { "name": "Valerian Saliou", "email": "valerian@example.com" },
      "committed_at": "2026-06-16T10:00:00Z"
    }
  ]
}
```

The optional `file_path` query parameter (e.g. `?file_path=docs/intro.md`) filters the list to commits that touched that exact file. Rename history is followed: if the file was previously known under a different name, commits that touched it under the old name are included. Always pass the current (latest) path; the server resolves prior names automatically. The same `..`, `.`, and `.git` rejection rules as other path parameters apply.

**GET** `/commits/:sha` — commit detail
```json
{
  "sha": "a3f9c1d",
  "message": "update: docs/intro.md",
  "author": { "name": "Valerian Saliou", "email": "valerian@example.com" },
  "committed_at": "2026-06-16T10:00:00Z",
  "files": [
    {
      "path": "docs/intro.md",
      "change": "updated",
      "content": "# Hello world\n...",
      "diff": "@@ -1,3 +1,4 @@\n ..."
    }
  ]
}
```

`change` is one of `"created"`, `"updated"`, `"deleted"`, `"moved"`. Moved files include an additional `"from_path"` field. `content` is empty string for deleted files.

**POST** `/commits/:sha/revert`
```json
{
  "reverted_sha": "a3f9c1d",
  "commit_sha": "b8d2e4a"
}
```

## Configuration (`config.toml`)

```toml
[server]
host = "0.0.0.0"
port = 5355
api_key = "your-secret-key"
repos_path = "./dev/repositories"
# Tracing log level: "trace" | "debug" | "info" | "warn" | "error"
# Defaults to "info" if unset. Overridden by the RUST_LOG env var.
log_level = "debug"

[limits]              # optional; request-level guard rails
# Optional whitelist of file extensions (compared case-insensitively) accepted
# on PUT paths and move destinations; other extensions are rejected with 400.
# Unset means all extensions are accepted. Move sources are not checked, so
# files written before the whitelist was configured remain movable.
allowed_extensions = ["md", "mdx"]
# Safety cap on how many files one batch read request may ask for; larger
# requests are rejected with 400. Defaults to 100 if unset.
batch_read_maximum_files = 100

[hooks]
url = "https://your-receiver.example.com/hook"
events = ["file.created", "file.updated", "file.deleted", "file.moved"]
retry_attempts = 5
retry_backoff_ms = 2000

[hooks.auth]          # optional
header = "Authorization"
value = "Bearer hook-secret"

[maintenance]         # optional; these are the defaults
enabled = true
# Delay between the first write to a repository and its background
# maintenance pass (consolidating repack + reflog expiry + index refresh).
delay_secs = 86400
# When true, the maintenance repack drops unreachable objects (e.g. blobs
# orphaned by writes that failed mid-operation). When false, every object
# is carried over into the consolidated pack, so maintenance can never
# destroy data. Commit history is unaffected either way — past file
# versions, including those of deleted files, always stay reachable.
destructive_prune = false
```

Config file path defaults to `config.toml` in the working directory. Override with `CONFIG_PATH=/path/to/config.toml`.

Log verbosity priority: `RUST_LOG` env var → `log_level` in config → `"info"` default.

## Key design decisions

- **One git working tree per tenant** at `repos_path/<collection_id>/<tenant_id>/`. Repos are auto-initialised on the first write with a `"chore: initialize"` root commit — no explicit provisioning step needed.
- **Per-tenant in-memory mutex** (`DashMap<String, Arc<Mutex<()>>>`) serialises all write operations on the same repo; keyed as `"collection_id/tenant_id"`. Reads never acquire the lock.
- **All git operations run in `spawn_blocking`** so they never stall the tokio executor.
- **Hook delivery is asynchronous** — writes enqueue their hook job and return immediately, so they are never delayed by a slow hook receiver.
- **Hook events are ordered per tenant** — each tenant has a dedicated in-memory queue with a single consumer task. Jobs are enqueued while the tenant write lock is still held, so hooks are delivered in exactly the order commits were accepted by this server; a later commit can never overtake an earlier one at the receiver (even one stuck in retries). Files within a single commit are delivered one hook at a time in order.
- **Unchanged writes are no-ops** — a PUT with content identical to HEAD's blob creates no commit and fires no hook; the response returns HEAD's sha. Detection hashes the incoming content and compares blob oids, so nothing is read from or written to the object database or disk. Clients that blindly re-write unchanged files cannot pollute history with empty commits.
- **Rename = single hook** — a `POST .../move` produces one `file.moved` event with both `from` and `to` paths, preserving entity identity in downstream systems.
- **Revert = new commit** — reverts never rewrite history; they produce a new inverse commit and fire the appropriate hooks for each changed file.
- **Author identity is caller-supplied** — every write request requires an `author` object with `name` and `email`. Both are stored in the git commit and validated as non-empty.
- **Commit identifier is named `sha`** (not `sha1`) — future-proof against git's SHA-256 migration; matches the convention used by GitHub, GitLab, and Gitea.
- **`:sha` parameters accept hexadecimal only** — a full or abbreviated commit SHA (4–64 hex chars). Revspecs (`HEAD~1`, `master@{1}`, `:/pattern`) are rejected with `400` so git semantics never leak through the API and history-search DoS is impossible.
- **HEAD is authoritative, not the working tree** — existence checks for writes, deletes, and moves are answered from HEAD's tree, and every commit tree is derived from HEAD's tree plus the intended change. Leftover working-tree state from a previously failed operation can never change an operation's outcome or be silently swept into a later commit. Moved content is read from HEAD's blob, not from disk.
- **Commits are built with `TreeUpdateBuilder`, not the git index** — cost per write is proportional to the touched path depth, not the repository size, so large repos write as fast as small ones. Moves and reverts reuse existing blob oids (no content rehash). The working tree is still kept in sync with single-file fs operations so humans can inspect repos, and the on-disk index is refreshed to HEAD during maintenance so `git status` stays meaningful.
- **Background maintenance repacks and expires (pruning is opt-in)** — the first write to a repository arms a one-shot timer (`[maintenance] delay_secs`, default 24 h; `enabled = false` turns it off). When it fires, the pass takes the tenant write lock and, via libgit2 (no `git` binary needed): expires reflogs, writes one consolidated packfile, deletes all loose objects and superseded packs, refreshes the index, and clears the slot so the next write re-arms it. By default every object is carried over into the new pack, so maintenance can never destroy data; with `destructive_prune = true` only objects reachable from a ref are kept, permanently dropping orphaned garbage (e.g. blobs from writes that failed mid-operation). Commit history is safe in both modes — history is append-only, so every past file version (including versions of since-deleted files) stays reachable through its commit. Pruning needs no grace period because objects are only ever created under the same write lock the pass holds. The repack is skipped when the repo is already consolidated (no loose objects, ≤ 1 pack). Repos receiving no writes are never touched; the schedule is in-memory only and does not survive restarts. Deleting a tenant disarms its pending timer.
- **File listing never opens blobs** — the tree endpoint is served from git tree objects alone (no sizes are reported), and pagination over root-level entries is decided before any subtree is opened, so off-page directories are never walked.
- **Seek windowing lives in its own module (`seek.rs`) and scans, never decodes whole** — `SeekOptions` (query wire type, JSON-array strings since query parameters are strings) and `SeekBody` (JSON-body wire type for the batch route: native arrays, no `seek_` prefix) both parse into the validated `SeekFilter` the git layer consumes, through one shared funnel that turns every malformed value into a `400` naming the parameter as the caller spelled it. One canonical spelling per wire format — the only polymorphism, in both formats, is that the `to` filter accepts the bare `$seek_from_line_starts_with` operator unwrapped. The scan is a single forward pass over any `BufRead` that stops reading the moment the window is complete. The git layer feeds it a streaming ODB read (`git_odb_open_rstream`) when the blob is a loose object — every blob written since the last maintenance repack — so inflation halts early; packed objects cannot be streamed by libgit2 and fall back to a `Cursor` over the in-memory blob. Either way only the selected window is allocated (and only the window must be valid UTF-8 — prefix matching is byte-level). Seeked reads answer `404` when the path resolves to a folder, same as the HEAD existence endpoint.
- **Batch read is one repository pass** — the batch endpoint opens the repo and resolves HEAD's tree once, then reads every requested blob through the same windowed-read helper as the single route (streaming ODB read where possible). Results are index-aligned with the request; `null` strictly means "not found" (invalid-UTF-8 content fails the whole batch with 422 instead of masquerading as missing). All request-level validation — path sanitisation, uniqueness after sanitisation, the `limits.batch_read_maximum_files` cap (default 100), per-file seek resolution (an entry's own `seek` replaces the request-level one; validation errors name the offending entry as `files[i]: …`) — happens before the repository is touched. Reads never take the tenant write lock, so a large batch cannot stall writers.
- **Per-tenant lock entries are never removed** — not even on tenant deletion. Removing an entry would let a writer holding the old mutex run concurrently with a writer holding a freshly-created one for the same repository.
- **Timestamps are named `committed_at`** — follows the `*_at` suffix convention (Stripe, GitHub API, Rails); unambiguous about what the value represents.
- **`/move` URL suffix on POST** — axum's wildcard router cannot match a fixed suffix after `*path`, so the handler is registered on `POST /*path` and enforces the `/move` suffix internally, returning 400 otherwise.
- **Stale `.git/index.lock` cleanup** — removed at startup across all repos, and before each maintenance index refresh (removed if older than 30 s). The write path itself no longer touches the index, so a stale lock can never block writes.
- **`git2` compiled with `vendored-libgit2`** — libgit2 is bundled in the binary; no system dependency needed.

## Webhook payloads

All payloads include `tenant_id`, `commit_sha`, and `committed_at`.

**file.created / file.updated**
```json
{
  "event": "file.created",
  "tenant_id": "acme",
  "commit_sha": "a3f9c1d",
  "committed_at": "2026-06-16T10:00:00Z",
  "file": { "path": "docs/intro.md", "content": "# Hello" }
}
```

**file.deleted**
```json
{
  "event": "file.deleted",
  "tenant_id": "acme",
  "commit_sha": "a3f9c1d",
  "committed_at": "2026-06-16T10:00:00Z",
  "file": { "path": "docs/intro.md" }
}
```

**file.moved**
```json
{
  "event": "file.moved",
  "tenant_id": "acme",
  "commit_sha": "b8d2e4a",
  "committed_at": "2026-06-16T10:01:00Z",
  "from": { "path": "docs/old.md" },
  "to": { "path": "docs/new.md", "content": "# Hello" }
}
```

## Running

```sh
cargo run                                    # uses config.toml in cwd
cargo run -- -c /etc/githttp-fs.toml
RUST_LOG=debug cargo run                     # overrides log_level in config
```

## Development workflow

After applying code changes, always run `cargo fmt` before `cargo build`.

## Release procedure

To bump the version to `vX.Y.Z`:

1. Update `version` in `Cargo.toml`
2. Update the version in `README.md`
3. Update the version in `debian/rules`
4. Run `cargo build` to regenerate `Cargo.lock`
5. Commit all changes with message `vX.Y.Z`
6. Tag the commit with `vX.Y.Z`

## Docker

Two-stage build: compiles in `rust:alpine` (static musl binary), runs in `alpine:3.22`.

```sh
docker build -t githttp-fs .
docker run -p 5355:5355 \
  -v ./config.toml:/app/config.toml \
  -v ./data:/app/data \
  githttp-fs
```

## License

Mozilla Public License v2.0 (MPL v2.0) — Copyright 2026, Valerian Saliou.
